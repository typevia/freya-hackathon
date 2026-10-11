use crate::{Redactor, hash, now, seconds, write_json};
use anyhow::{Context, Result, bail, ensure};
use reqwest::{Client, Method};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::time::Instant;

/// Reuse only a completed, exact model request. Changed documents/settings miss the cache.
pub fn matching_model_cache(
    events: &Value,
    model: &str,
    purpose: &str,
    messages: &Value,
    effort: &Option<String>,
    max_tokens: u64,
) -> Option<Value> {
    let mut pending: Option<Value> = None;
    for event in events.as_array()? {
        let detail = &event["detail"];
        if detail["purpose"] != purpose {
            continue;
        }
        match event["event"].as_str() {
            Some("model_start") => {
                pending = if detail["model"] == model
                    && detail["messages"] == *messages
                    && detail["reasoning_effort"] == json!(effort)
                    && detail["max_completion_tokens"] == max_tokens
                {
                    Some(event.clone())
                } else {
                    None
                };
            }
            Some("model_error") => pending = None,
            Some("model_end") => {
                if let Some(start) = pending.take()
                    && detail["model"] == model
                    && detail.get("response").is_some()
                {
                    return Some(json!({"original_call":start,"original_result":event}));
                }
            }
            _ => {}
        }
    }
    None
}

#[derive(Clone)]
pub struct Config {
    pub llm_base: String,
    pub llm_key: String,
    pub model: String,
    pub freya_base: String,
    pub freya_key: String,
    pub bank_base: String,
    pub bank_key: String,
}
impl Config {
    pub fn env() -> Self {
        fn env(k: &str, d: &str) -> String {
            std::env::var(k).unwrap_or_else(|_| d.into())
        }
        Self {
            llm_base: env("BUILDER_LLM_BASE_URL", "https://api.openai.com/v1"),
            llm_key: env("BUILDER_LLM_API_KEY", ""),
            model: env("BUILDER_LLM_MODEL", "gpt-6-astra"),
            freya_base: env("FREYA_API_URL", "https://staging-app.freyavoice.ai"),
            freya_key: env("FREYA_API_KEY", ""),
            bank_base: env(
                "FREYA_BANK_HOST",
                "https://freya-bank-mock.freyavoice.workers.dev",
            ),
            bank_key: env("FREYA_BANK_TEAM_KEY", ""),
        }
    }
    pub fn validate(&self) -> Result<()> {
        self.validate_services()?;
        ensure!(
            !self.freya_key.is_empty() && self.bank_key.len() >= 16,
            "Freya key and bank team key (16+ chars) required"
        );
        Ok(())
    }
    fn validate_services(&self) -> Result<()> {
        for base in [&self.llm_base, &self.freya_base, &self.bank_base] {
            let u = reqwest::Url::parse(base).context("invalid service base URL")?;
            ensure!(
                u.username().is_empty()
                    && u.password().is_none()
                    && u.query().is_none()
                    && u.fragment().is_none(),
                "service URLs cannot contain credentials/query/fragment"
            );
            ensure!(
                u.scheme() == "https"
                    || (u.scheme() == "http"
                        && matches!(u.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))),
                "plaintext is restricted to loopback"
            );
        }
        Ok(())
    }
}
pub struct Remote {
    pub config: Config,
    client: Client,
    pub deadline: Instant,
    pub redactor: Redactor,
    pub trace: Vec<Value>,
    pub out: PathBuf,
    pub workspace: Option<String>,
    document_mode: bool,
}
impl Remote {
    pub fn new(config: Config, out: PathBuf, budget: u64) -> Result<Self> {
        Self::new_inner(config, out, budget, false)
    }
    /// Document builds never use the practice bank session/ledger client.
    pub fn new_document(config: Config, out: PathBuf, budget: u64) -> Result<Self> {
        Self::new_inner(config, out, budget, true)
    }
    fn new_inner(config: Config, out: PathBuf, budget: u64, document_mode: bool) -> Result<Self> {
        if document_mode {
            config.validate_services()?;
            ensure!(!config.freya_key.is_empty(), "Freya key required");
        } else {
            config.validate()?;
        }
        let redactor = Redactor(vec![
            config.llm_key.clone(),
            config.freya_key.clone(),
            config.bank_key.clone(),
        ]);
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(seconds(10))
            .timeout(seconds(120))
            .user_agent("Mozilla/5.0 (Rust Freya Builder)")
            .build()?;
        Ok(Self {
            config,
            client,
            deadline: Instant::now() + seconds(budget),
            redactor,
            trace: vec![],
            out,
            workspace: None,
            document_mode,
        })
    }
    pub fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }
    pub fn event(&mut self, kind: &str, detail: Value) -> Result<()> {
        self.trace.push(
            self.redactor
                .value(&json!({"at":now(),"event":kind,"detail":detail})),
        );
        write_json(&self.out.join("trace.json"), &self.trace)
    }
    pub fn artifact(&self, name: &str, value: &Value) -> Result<()> {
        write_json(&self.out.join(name), &self.redactor.value(value))
    }
    /// Executable artifacts are never transformed by diagnostic redaction.
    pub fn executable_artifact(&self, name: &str, value: &Value) -> Result<String> {
        ensure_executable_safe_mode(value, &self.redactor, self.document_mode)?;
        let path = self.out.join(name);
        write_json(&path, value)?;
        crate::file_hash(&path)
    }
    async fn request(
        &self,
        base: &str,
        key: &str,
        bank: bool,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value> {
        ensure!(self.remaining() > Duration::ZERO, "build deadline exceeded");
        let mut req = self
            .client
            .request(method, format!("{}{}", base.trim_end_matches('/'), path))
            .timeout(
                self.remaining()
                    .min(seconds(if path == "/chat/completions" {
                        600
                    } else {
                        120
                    })),
            );
        if bank {
            req = req.header("x-team-key", key)
        } else if !key.is_empty() {
            req = req.bearer_auth(key)
        }
        if let Some(body) = body {
            req = req.json(&body)
        }
        let mut res = req.send().await.map_err(|e| {
            anyhow::anyhow!(
                "remote transport failure: {}",
                if e.is_timeout() {
                    "timeout"
                } else {
                    "connection/request"
                }
            )
        })?;
        let status = res.status();
        let mut bytes = Vec::new();
        while let Some(chunk) = res.chunk().await.context("remote response interrupted")? {
            ensure!(
                bytes.len() + chunk.len() <= 8_000_000,
                "remote response exceeds 8 MB"
            );
            bytes.extend_from_slice(&chunk);
        }
        // Do not echo remote errors: they can contain auth headers or session capabilities.
        ensure!(
            status.is_success(),
            "remote HTTP status {}",
            status.as_u16()
        );
        let v: Value = serde_json::from_slice(&bytes).context("invalid remote JSON")?;
        if v.get("success") == Some(&json!(false)) {
            bail!("remote reported unsuccessful response")
        }
        Ok(v.pointer("/data/result").cloned().unwrap_or(v))
    }
    pub async fn freya(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value> {
        self.request(
            &self.config.freya_base,
            &self.config.freya_key,
            false,
            method,
            path,
            body,
        )
        .await
    }
    pub async fn bank(&self, path: &str, body: Option<Value>) -> Result<Value> {
        self.request(
            &self.config.bank_base,
            &self.config.bank_key,
            true,
            if body.is_some() {
                Method::POST
            } else {
                Method::GET
            },
            path,
            body,
        )
        .await
    }
    pub async fn model(&mut self, purpose: &str, system: &str, input: &Value) -> Result<Value> {
        let messages = json!([{"role":"system","content":system},{"role":"user","content":serde_json::to_string(input)?}]);
        ensure!(
            messages.to_string().len() < 700_000,
            "model context exceeds bound"
        );
        let effort = std::env::var("BUILDER_REASONING_EFFORT").ok();
        if let Some(effort) = &effort {
            ensure!(
                ["minimal", "low", "medium", "high"].contains(&effort.as_str()),
                "unsupported builder reasoning effort"
            );
        }
        let max_tokens = std::env::var("BUILDER_MAX_COMPLETION_TOKENS")
            .ok()
            .map(|s| s.parse::<u64>().context("invalid builder token limit"))
            .transpose()?
            .unwrap_or(18000);
        ensure!(
            (1000..=32000).contains(&max_tokens),
            "builder token limit must be 1000..32000"
        );
        if let Ok(path) = std::env::var("BUILDER_MODEL_CACHE_TRACE") {
            let path = Path::new(&path);
            ensure!(
                std::fs::metadata(path)?.len() <= 32_000_000,
                "model cache trace exceeds32MB"
            );
            let events: Value = serde_json::from_slice(&std::fs::read(path)?)?;
            if let Some(cached) = matching_model_cache(
                &events,
                &self.config.model,
                purpose,
                &messages,
                &effort,
                max_tokens,
            ) {
                self.event("model_cache_hit", json!({"purpose":purpose,"model":self.config.model,"source_trace":path,"source_trace_sha256":crate::file_hash(path)?,"original_call":cached["original_call"],"original_result":cached["original_result"],"note":"Exact completed model call reused without editing its response; no new inference claimed."}))?;
                return Ok(cached["original_result"]["detail"]["response"].clone());
            }
        }
        let mut body = json!({"model":self.config.model,"messages":messages,"response_format":{"type":"json_object"},"max_completion_tokens":max_tokens});
        if let Some(effort) = &effort {
            body["reasoning_effort"] = json!(effort);
        }
        for attempt in 1..=2 {
            self.event("model_start",json!({"purpose":purpose,"model":self.config.model,"attempt":attempt,"messages":messages,"reasoning_effort":effort,"max_completion_tokens":max_tokens}))?;
            let result = self
                .request(
                    &self.config.llm_base,
                    &self.config.llm_key,
                    false,
                    Method::POST,
                    "/chat/completions",
                    Some(body.clone()),
                )
                .await;
            let parsed = result.and_then(|v| {
                let text = v["choices"][0]["message"]["content"]
                    .as_str()
                    .context("model response lacks content")?;
                let text = text
                    .trim()
                    .trim_start_matches("```json")
                    .trim_start_matches("```")
                    .trim_end_matches("```")
                    .trim();
                serde_json::from_str::<Value>(text).context("model did not return JSON")
            });
            match parsed {
                Ok(v) => {
                    self.event(
                        "model_end",
                        json!({"purpose":purpose,"model":self.config.model,"response":v}),
                    )?;
                    return Ok(v);
                }
                Err(e) => {
                    self.event("model_error",json!({"purpose":purpose,"model":self.config.model,"error":self.redactor.text(&e.to_string())}))?;
                    if attempt == 2 {
                        return Err(e);
                    }
                }
            }
        }
        bail!("model retries exhausted")
    }
    pub async fn workspace(&mut self) -> Result<String> {
        if let Some(id) = &self.workspace {
            return Ok(id.clone());
        }
        let v = self.freya(Method::GET, "/api/v2/workspaces", None).await?;
        let list = v.get("workspaces").or_else(|| v.get("items")).unwrap_or(&v);
        let id = list[0]["id"].as_str().context("no workspace")?.to_string();
        self.workspace = Some(id.clone());
        Ok(id)
    }
    pub async fn validate_workflow(&mut self, w: &Value) -> Result<()> {
        let r = self
            .freya(
                Method::POST,
                "/api/v2/workflow?dryRun=true",
                Some(json!({"name":"rust-builder-validation","workflow":w})),
            )
            .await?;
        self.event("freya_dry_run", r.clone())?;
        ensure!(
            r.get("valid") != Some(&json!(false)),
            "Freya dry run rejected workflow: {}",
            self.redactor.value(&r)
        );
        Ok(())
    }
    pub async fn publish(&mut self, w: &Value, slot: &Path) -> Result<Value> {
        let ids = if slot.exists() {
            let ids = crate::read_json(slot)?;
            ensure!(
                ids["state"] != "creation_pending",
                "workflow creation outcome is unknown; reconcile slot with Freya before retrying (no duplicate create)"
            );
            ids
        } else {
            write_json(
                slot,
                &json!({"state":"creation_pending","started_at":now()}),
            )?;
            let r = self
                .freya(
                    Method::POST,
                    "/api/v2/workflow",
                    Some(
                        json!({"name":format!("rust-builder-{}", crate::build_id()),"workflow":w}),
                    ),
                )
                .await?;
            let r = r.get("workflow").unwrap_or(&r);
            let ids = json!({"workflow_id":r["id"],"agent_id":r["agentId"]});
            ensure!(
                ids["workflow_id"].is_string() && ids["agent_id"].is_string(),
                "workflow creation missing IDs"
            );
            write_json(slot, &ids)?;
            self.event("freya_create", ids.clone())?;
            ids
        };
        let id = ids["workflow_id"]
            .as_str()
            .context("invalid slot workflow id")?;
        let ws = self.workspace().await?;
        self.freya(
            Method::PUT,
            &format!("/api/v2/workflow/{id}"),
            Some(json!({"workflow":w})),
        )
        .await?;
        let get_path = format!("/api/v2/workflow/{id}?workspaceId={ws}");
        let r = self.freya(Method::GET, &get_path, None).await?;
        let mut platform = r["workflow"]["workflow"].clone();
        ensure!(platform.is_object(), "platform workflow missing");
        if platform.pointer("/agent_config/llmConfig/provider") != Some(&json!("experimental-5"))
            || platform.pointer("/agent_config/llmConfig/model") != Some(&json!("experimental-5"))
        {
            if !platform["agent_config"].is_object() {
                platform["agent_config"] = json!({});
            }
            if !platform["agent_config"]["llmConfig"].is_object() {
                platform["agent_config"]["llmConfig"] = json!({});
            }
            platform["agent_config"]["llmConfig"]["provider"] = json!("experimental-5");
            platform["agent_config"]["llmConfig"]["model"] = json!("experimental-5");
            self.freya(
                Method::PUT,
                &format!("/api/v2/workflow/{id}"),
                Some(json!({"workflow":platform})),
            )
            .await?;
        }
        let versions = self
            .freya(
                Method::GET,
                &format!("/api/v2/versions/workflow/{id}?workspaceId={ws}"),
                None,
            )
            .await?;
        let newest = versions["versions"]
            .as_array()
            .context("missing versions")?
            .iter()
            .max_by_key(|x| x["versionNumber"].as_u64().unwrap_or(0))
            .context("no workflow versions")?;
        if newest["isPublished"] != json!(true) {
            self.freya(
                Method::POST,
                &format!("/api/v2/versions/workflow/{id}/publish"),
                Some(json!({"versionId":newest["id"]})),
            )
            .await?;
        }
        let mut result = ids;
        result["version"] = newest["versionNumber"].clone();
        write_json(slot, &result)?;
        self.event(
            "freya_publish",
            json!({"ids":result,"candidate_hash":hash(w)}),
        )?;
        Ok(result)
    }
    pub async fn export(&mut self, ids: &Value) -> Result<Value> {
        self.snapshot(ids, "selected-platform.json").await
    }
    pub async fn snapshot(&mut self, ids: &Value, filename: &str) -> Result<Value> {
        let id = ids["workflow_id"].as_str().context("missing workflow id")?;
        let ws = self.workspace().await?;
        let r = self
            .freya(
                Method::GET,
                &format!("/api/v2/workflow/{id}?workspaceId={ws}"),
                None,
            )
            .await?;
        let w = &r["workflow"];
        ensure!(
            w["workflow"].is_object(),
            "export missing platform workflow"
        );
        let export = json!({"workflow_id":w["id"],"agent_id":w["agentId"],"updated_at":w["updatedAt"],"version":ids["version"],"workflow":w["workflow"]});
        let snapshot_hash = self.executable_artifact(filename, &export)?;
        self.event(
            "freya_export",
            json!({"ids":ids,"artifact":filename,"sha256":snapshot_hash}),
        )?;
        Ok(export)
    }
}

/// Reject contamination rather than changing the semantics of exported code/config.
pub fn ensure_executable_safe(value: &Value, redactor: &Redactor) -> Result<()> {
    ensure_executable_safe_mode(value, redactor, false)
}
pub fn ensure_document_executable_safe(value: &Value, redactor: &Redactor) -> Result<()> {
    ensure_executable_safe_mode(value, redactor, true)
}
fn ensure_executable_safe_mode(
    value: &Value,
    redactor: &Redactor,
    document_mode: bool,
) -> Result<()> {
    match value {
        Value::String(s) => ensure!(
            redactor.text(s) == *s,
            "executable artifact contains a credential or session capability"
        ),
        Value::Array(values) => {
            for v in values {
                ensure_executable_safe_mode(v, redactor, document_mode)?;
            }
        }
        Value::Object(object) => {
            for (key, v) in object {
                let sensitive = [
                    "apikey",
                    "api_key",
                    "authorization",
                    "x-team-key",
                    "access_token",
                    "refresh_token",
                    "password",
                    "customer_id",
                ]
                .contains(&key.to_lowercase().as_str());
                if sensitive && !(document_mode && key == "customer_id") {
                    let literal = v
                        .as_str()
                        .or_else(|| v.get("default").and_then(Value::as_str));
                    ensure!(
                        literal
                            .is_none_or(|s| s.is_empty()
                                || (key == "customer_id" && s == "{{customer_id}}")),
                        "executable artifact contains a baked credential or session capability field"
                    );
                }
                ensure_executable_safe_mode(v, redactor, document_mode)?;
            }
        }
        _ => {}
    }
    Ok(())
}
