pub mod builder;
pub mod compiler;
pub mod document_validation;
pub mod documents;
pub mod eval;
pub mod remote;
pub mod service;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub fn hash(v: &impl Serialize) -> String {
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(v).unwrap_or_default())
    )
}
pub fn file_hash(path: &Path) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(std::fs::read(path)?)))
}
pub fn build_id() -> String {
    format!(
        "build-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    )
}
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
pub fn read_json(path: &Path) -> Result<Value> {
    serde_json::from_str(&read_text(path)?).context("invalid input JSON")
}
pub fn read_text(path: &Path) -> Result<String> {
    ensure!(
        std::fs::metadata(path).context("input unavailable")?.len() <= 1_000_000,
        "input exceeds 1 MB"
    );
    std::fs::read_to_string(path).context("cannot read input")
}
pub fn write_json(path: &Path, v: &impl Serialize) -> Result<()> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(v)?)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

#[derive(Clone, Default)]
pub struct Redactor(pub Vec<String>);
impl Redactor {
    pub fn env() -> Self {
        Self(
            [
                "BUILDER_LLM_API_KEY",
                "FREYA_API_KEY",
                "FREYA_BANK_TEAM_KEY",
            ]
            .iter()
            .filter_map(|k| std::env::var(k).ok())
            .filter(|s| !s.is_empty())
            .collect(),
        )
    }
    pub fn text(&self, s: &str) -> String {
        let mut out = s.to_owned();
        let mut secrets = self.0.clone();
        secrets.sort_by_key(|x| std::cmp::Reverse(x.len()));
        for secret in secrets {
            if !secret.is_empty() {
                out = out.replace(&secret, "[REDACTED]");
            }
        }
        out
    }
    pub fn value(&self, v: &Value) -> Value {
        match v {
            Value::String(s) => Value::String(self.text(s)),
            Value::Array(a) => Value::Array(a.iter().map(|x| self.value(x)).collect()),
            Value::Object(m) => Value::Object(
                m.iter()
                    .map(|(k, v)| {
                        (
                            k.clone(),
                            if [
                                "customer_id",
                                "authorization",
                                "x-team-key",
                                "api_key",
                                "apikey",
                                "access_token",
                                "refresh_token",
                                "token",
                                "password",
                                "secret",
                            ]
                            .contains(&k.to_lowercase().as_str())
                                && v.as_str()
                                    .is_some_and(|s| !s.is_empty() && s != "{{customer_id}}")
                            {
                                Value::String("[REDACTED]".into())
                            } else {
                                self.value(v)
                            },
                        )
                    })
                    .collect(),
            ),
            _ => v.clone(),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildInput {
    pub policies: Vec<String>,
    pub openapi: Value,
    pub cases: Vec<Value>,
    #[serde(default = "default_iterations")]
    pub max_iterations: usize,
    #[serde(default = "default_budget")]
    pub budget_seconds: u64,
    #[serde(default = "default_reps")]
    pub reps: usize,
}
fn default_iterations() -> usize {
    5
}
fn default_budget() -> u64 {
    1800
}
fn default_reps() -> usize {
    1
}
impl BuildInput {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.policies.is_empty()
                && self.policies.len() <= 10
                && self.policies.iter().all(|s| !s.trim().is_empty()),
            "need 1..10 nonempty policies"
        );
        ensure!(
            serde_json::to_vec(self)?.len() <= 1_000_000,
            "combined input exceeds 1 MB"
        );
        ensure!(
            (1..=20).contains(&self.max_iterations),
            "max_iterations must be 1..20"
        );
        ensure!(
            (30..=1800).contains(&self.budget_seconds),
            "budget_seconds must be 30..1800"
        );
        ensure!(
            (1..=3).contains(&self.reps) && !self.cases.is_empty() && self.cases.len() <= 20,
            "need 1..20 hosted fixtures and 1..3 reps"
        );
        Ok(())
    }
}
pub fn seconds(n: u64) -> Duration {
    Duration::from_secs(n)
}
