//! A small, deliberately constrained IR. URLs, handlers, models and lifecycle hooks
//! are not model-controlled. Python is parsed, never executed on the builder host.
#[path = "confirmation.rs"]
mod confirmation;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    process::{Command, Stdio},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub system_prompt: String,
    pub initial_node: String,
    pub nodes: BTreeMap<String, Node>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Node {
    pub kind: String,
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub extraction: Vec<Extraction>,
    #[serde(default)]
    pub routes: Vec<Route>,
    #[serde(default)]
    pub fallback: String,
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub operation: String,
    #[serde(default)]
    pub arguments: BTreeMap<String, Value>,
    #[serde(default)]
    pub mappings: BTreeMap<String, String>,
    #[serde(default)]
    pub success: String,
    #[serde(default)]
    pub failure: String,
    #[serde(default)]
    pub validation_flag: String,
    #[serde(default)]
    pub confirmation_flag: String,
    #[serde(default)]
    pub on_enter: Option<EntrySpeech>,
    #[serde(default)]
    pub actions: Vec<String>,
    #[serde(default)]
    pub product_label: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum EntrySpeech {
    Tts { text: String },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Extraction {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub values: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    pub target: String,
    pub when: Condition,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Condition {
    Always,
    Equals { variable: String, value: String },
    All { conditions: Vec<Condition> },
    Any { conditions: Vec<Condition> },
    Intent { description: String },
}
impl Condition {
    fn raw(&self) -> Value {
        match self {
            Self::Always => json!({"operator":"always"}),
            Self::Equals { variable, value } => {
                json!({"variable":variable,"operator":"equals","operand":value})
            }
            Self::All { conditions } => {
                json!({"logic":"and","conditions":conditions.iter().map(Self::raw).collect::<Vec<_>>()})
            }
            Self::Any { conditions } => {
                json!({"logic":"or","conditions":conditions.iter().map(Self::raw).collect::<Vec<_>>()})
            }
            Self::Intent { description } => {
                json!({"operator":"matches_intent","description":description})
            }
        }
    }
    fn compile(&self) -> Value {
        match self {
            Self::Always | Self::Intent { .. } | Self::Any { .. } => self.raw(),
            _ => json!({"logic":"or","conditions":[self.raw()]}),
        }
    }
    fn deterministic(&self) -> bool {
        match self {
            Self::Intent { .. } => false,
            Self::All { conditions } | Self::Any { conditions } => {
                conditions.iter().all(Self::deterministic)
            }
            _ => true,
        }
    }
}
#[derive(Clone)]
pub struct Catalog {
    pub base: String,
    pub operations: BTreeMap<String, Value>,
}
impl Catalog {
    pub fn new(spec: &Value) -> Result<Self> {
        let base = spec["servers"][0]["url"]
            .as_str()
            .context("OpenAPI requires server URL")?
            .trim_end_matches('/')
            .to_string();
        ensure!(
            !base.contains(['{', '}']),
            "bank server URL cannot be templated"
        );
        let url = reqwest::Url::parse(&base)?;
        ensure!(
            url.scheme() == "https"
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "bank server must be an HTTPS URL without credentials/query/fragment"
        );
        let mut operations = BTreeMap::new();
        for (path, item) in spec["paths"].as_object().context("missing OpenAPI paths")? {
            let name = path.trim_start_matches('/');
            ensure!(
                name_ok(name) && path == &format!("/{name}"),
                "only simple operation paths supported"
            );
            let op = &item["post"];
            ensure!(
                op.is_object() && item.as_object().is_some_and(|m| m.len() == 1),
                "only POST bank operations supported"
            );
            let schema = &op["requestBody"]["content"]["application/json"]["schema"];
            ensure!(
                schema["properties"].is_object(),
                "inline body schema required"
            );
            for field in schema["properties"]
                .as_object()
                .context("body properties")?
                .values()
            {
                ensure!(
                    matches!(
                        field["type"].as_str(),
                        Some("string" | "number" | "integer" | "boolean")
                    ) && field.get("$ref").is_none(),
                    "only inline scalar body fields are supported"
                );
            }
            ensure!(
                schema["properties"]["customer_id"].is_object(),
                "bank operation must be scoped by customer_id"
            );
            operations.insert(name.into(), schema.clone());
        }
        ensure!(
            !operations.is_empty() && operations.len() <= 30,
            "unsupported operation count"
        );
        Ok(Self { base, operations })
    }
}
fn name_ok(s: &str) -> bool {
    s.bytes()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c == b'_')
        && s.len() <= 64
        && s.bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
        && !s.starts_with("builder_")
}
fn reference(s: &str) -> Result<()> {
    ensure!(name_ok(s), "invalid or reserved identifier: {s}");
    Ok(())
}

fn writable_reference(s: &str) -> Result<()> {
    reference(s)?;
    ensure!(
        s != "customer_id",
        "customer_id is a read-only session capability; generated writes are forbidden"
    );
    Ok(())
}

/// Catch source-document {variable} notation before Freya sees an unresolved name.
pub fn lint_templates(text: &str) -> Result<()> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'{' {
            i += 1;
            continue;
        }
        if bytes.get(i + 1) == Some(&b'{') {
            let rest = &text[i + 2..];
            let end = rest
                .find("}}")
                .context("unclosed Freya double-brace template")?;
            ensure!(
                !rest[..end].contains(['{', '}']) && !rest[..end].is_empty(),
                "invalid Freya template"
            );
            i += end + 4;
        } else {
            if let Some(end) = text[i + 1..].find('}') {
                let variable = &text[i + 1..i + 1 + end];
                ensure!(
                    variable.is_empty()
                        || !variable
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || "_.:-".contains(c)),
                    "single-brace placeholder {{{variable}}} is not a Freya template; use double braces: {{{{{variable}}}}}"
                );
            }
            i += 1;
        }
    }
    Ok(())
}

fn terminal_node(name: &str, farewell: &str) -> Value {
    let mut properties = json!({"message_mode":{"type":"string","default":if farewell.is_empty(){"none"}else{"custom"}}});
    if !farewell.is_empty() {
        properties["spoken_message"] = json!({"type":"string","default":farewell});
    }
    json!({"name":name,"node_type":"end_call","on_enter":{"type":"silent"},"disable_inactivity_monitor":true,"transitions":[],"terminal":{"type":"function","function":{"name":name,"handler":"__function__:end_call","parameters":{"type":"object","properties":properties}}}})
}

/// Fixed AST checker has no execution path for supplied Python. A conservative
/// allowlist reduces risk; the remote Freya sandbox remains the security boundary.
pub fn check_python(code: &str) -> Result<()> {
    python_writes(code).map(|_| ())
}
fn python_writes(code: &str) -> Result<BTreeSet<String>> {
    python_check_mode(code, "legacy")
}
/// Generic sandbox validation permits body-only pre-hooks and response post-hooks.
pub fn check_generic_python(code: &str, mode: &str) -> Result<()> {
    ensure!(
        ["code", "pre", "post"].contains(&mode),
        "unknown Python validation mode"
    );
    python_check_mode(code, mode).map(|_| ())
}
fn python_check_mode(code: &str, mode: &str) -> Result<BTreeSet<String>> {
    ensure!(
        !code.is_empty()
            && code.len() <= 24000
            && !code.contains("{{")
            && (mode != "legacy" || !code.contains("}}")),
        "invalid embedded Python length/template"
    );
    let mut child = Command::new("python3")
        .args(["-I", "-c", include_str!("python_check.py"), mode])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("python3 is required for AST validation")?;
    child
        .stdin
        .take()
        .context("AST checker stdin")?
        .write_all(code.as_bytes())?;
    let output = child.wait_with_output()?;
    ensure!(
        output.status.success(),
        "embedded Python rejected by AST allowlist (never executed locally): {}",
        String::from_utf8_lossy(&output.stdout)
            .chars()
            .take(1000)
            .collect::<String>()
    );
    serde_json::from_slice(&output.stdout).context("AST checker write-set output")
}
fn tool(operation: &str, n: &Node, catalog: &Catalog) -> Result<Value> {
    let schema = catalog
        .operations
        .get(operation)
        .context("operation not in supplied OpenAPI")?;
    let mut body = schema.clone();
    let props = body["properties"]
        .as_object_mut()
        .context("body properties")?;
    for (key, val) in &n.arguments {
        ensure!(
            key != "customer_id" && props.contains_key(key),
            "unknown/reserved bank argument {key}"
        );
        ensure!(
            val.is_string() || val.is_number() || val.is_boolean(),
            "body arguments must be scalars"
        );
        let template = val.as_str().is_some_and(|s| s.contains("{{"));
        if !template {
            let correct_type = match props[key]["type"].as_str() {
                Some("string") => val.is_string(),
                Some("number") => val.is_number(),
                Some("integer") => val.as_i64().is_some() || val.as_u64().is_some(),
                Some("boolean") => val.is_boolean(),
                _ => false,
            };
            ensure!(
                correct_type,
                "bank argument {key} does not match OpenAPI type"
            );
        }
        if let Some(s) = val.as_str() {
            lint_templates(s)?;
            if s.contains("{{") {
                ensure!(
                    s.starts_with("{{") && s.ends_with("}}") && name_ok(&s[2..s.len() - 2]),
                    "only whole variable substitutions allowed"
                );
            } else if let Some(en) = props[key].get("enum").and_then(Value::as_array) {
                ensure!(en.contains(val), "argument outside OpenAPI enum");
            }
        }
        props.get_mut(key).context("body property")?["default"] = val.clone();
    }
    props.get_mut("customer_id").context("customer_id schema")?["default"] =
        json!("{{customer_id}}");
    for required in schema["required"]
        .as_array()
        .context("required body fields")?
    {
        let r = required.as_str().context("required field string")?;
        ensure!(
            r == "customer_id" || n.arguments.contains_key(r),
            "missing bound bank argument {r}"
        );
    }
    // API nodes have no model to fill fields: remove unsupplied optional fields.
    body["properties"]
        .as_object_mut()
        .context("body")?
        .retain(|k, _| k == "customer_id" || n.arguments.contains_key(k));
    let mut post = String::from(
        "try:\n    result.pop('header', None)\n    d = result.get('data')\n    if not isinstance(d, dict):\n        d = dict()\n        result['data'] = d\n    d['builder_ok'] = 'yes' if d.get('ok') is True else 'no'\n    d['what_to_do_next'] = 'Report the actual result truthfully, then continue.' if d['builder_ok'] == 'yes' else 'The action failed; do not claim success.'\n",
    );
    if operation == "verify_identity" {
        post.push_str("    d['builder_verified'] = 'yes' if d.get('ok') is True and d.get('all_verified') is True else 'no'\n");
    }
    // Freya response_mapping retains scalars, but can silently omit objects/lists.
    // Convert declared composite mappings to JSON text before the engine flattens data.
    if !n.mappings.is_empty() {
        let mappings = serde_json::to_string(&n.mappings)?;
        post.push_str(&format!("    import json\n    source_data = dict(d)\n    for variable, path in {mappings}.items():\n        value = source_data\n        for part in path.split('.')[1:]:\n            if isinstance(value, dict):\n                value = value.get(part)\n            elif isinstance(value, list) and part.isdigit() and int(part) < len(value):\n                value = value[int(part)]\n            else:\n                value = None\n                break\n        d['builder_map_' + variable] = json.dumps(value) if isinstance(value, (dict, list)) else (value if value is not None else '')\n"));
    }
    post.push_str("except Exception:\n    result['data'] = dict(builder_ok='no')\n");
    Ok(
        json!({"type":"function","config":{"timeoutMs":15000,"maxRetries":0},"function":{"name":operation,"handler":"__function__:api_call","description":format!("Perform {operation} once with the bound arguments."),"parameters":{"type":"object","properties":{"url":{"type":"string","default":format!("{}/{operation}",catalog.base)},"method":{"type":"string","default":"POST"},"connect_retries":{"type":"number","default":0},"headers":{"type":"object","properties":{"Content-Type":{"type":"string","default":"application/json"}}},"body":body}}},"post_process_code":post}),
    )
}
fn edge(target: &str, c: Condition) -> Value {
    json!({"target_node":target,"description":"State transition","condition":c.compile()})
}
fn eq(variable: &str) -> Condition {
    Condition::Equals {
        variable: variable.into(),
        value: "yes".into(),
    }
}
fn code_tool(code: &str) -> Value {
    json!({"type":"function","function":{"name":"code_exec","handler":"__function__:code_exec","parameters":{"type":"object","properties":{"code":{"type":"string","default":code},"timeout_ms":{"type":"number","default":3000}}}}})
}

pub fn compile(plan: &Plan, spec: &Value) -> Result<Value> {
    let catalog = Catalog::new(spec)?;
    lint_templates(&plan.system_prompt)?;
    ensure!(
        !plan.nodes.is_empty()
            && plan.nodes.len() <= 80
            && plan.nodes.contains_key(&plan.initial_node),
        "invalid initial node/node count"
    );
    confirmation::validate(plan, &catalog)?;
    let mut refs: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut nodes = serde_json::Map::new();
    let validation_flags: BTreeSet<String> = plan
        .nodes
        .values()
        .filter(|n| n.operation == "create_promise")
        .map(|n| n.validation_flag.clone())
        .collect();
    let mut code_writes = BTreeMap::new();
    for (name, n) in &plan.nodes {
        if n.kind == "code" {
            code_writes.insert(
                name.clone(),
                python_writes(&n.code).map_err(|e| anyhow::anyhow!("code node {name}: {e}"))?,
            );
        }
    }
    for flag in &validation_flags {
        ensure!(
            code_writes
                .values()
                .filter(|writes| writes.contains(flag))
                .count()
                == 1,
            "validation flag {flag} must have exactly one code-node writer; use separate flags for separate validators"
        );
    }
    let mut extraction_vars = BTreeSet::new();
    for n in plan.nodes.values() {
        lint_templates(&n.prompt)?;
        if let Some(EntrySpeech::Tts { text }) = &n.on_enter {
            lint_templates(text)?;
            ensure!(
                matches!(n.kind.as_str(), "conversation" | "confirmation" | "code"),
                "fixed entry speech only on conversation/confirmation/code"
            );
        }
        for flag in [&n.validation_flag, &n.confirmation_flag] {
            if !flag.is_empty() {
                writable_reference(flag)?;
            }
        }
        for e in &n.extraction {
            writable_reference(&e.name)?;
            lint_templates(&e.description)?;
            extraction_vars.insert(e.name.clone());
        }
    }
    for (name, n) in &plan.nodes {
        reference(name)?;
        let mut targets = Vec::new();
        let mut seen = BTreeSet::new();
        for r in &n.routes {
            ensure!(
                seen.insert(&r.target),
                "duplicate transition targets can weaken gates"
            );
            targets.push(r.target.clone());
        }
        let routes: Vec<_> = n
            .routes
            .iter()
            .map(|r| edge(&r.target, r.when.clone()))
            .collect();
        let mut node = json!({"name":name,"node_type":n.kind,"transitions":routes});
        match n.kind.as_str() {
            "confirmation" => {
                targets.extend([n.success.clone(), n.failure.clone()]);
                node = confirmation::compile_state(name, n, plan, &mut nodes)?;
            }
            "conversation" => {
                ensure!(
                    !n.prompt.is_empty() && !n.fallback.is_empty(),
                    "conversation requires prompt/fallback"
                );
                targets.push(n.fallback.clone());
                node["task_message"] = json!(format!(
                    "{}\nNo tools are available in this step. Never claim an action succeeded before its actual result.",
                    n.prompt
                ));
                if let Some(speech) = &n.on_enter {
                    node["on_enter"] = serde_json::to_value(speech)?;
                }
                node["max_node_turns"] = json!(8);
                node["max_turn_transition"] = json!(n.fallback);
                node["extraction_schema"] =
                    json!(n.extraction.iter().map(|e| {
                    let mut v=json!({"name":e.name,"type":"string","description":e.description,"min_confidence":0.7});
                    if !e.values.is_empty() { v["enum_values"]=json!(e.values); } v
                }).collect::<Vec<_>>());
                ensure!(
                    n.code.is_empty() && n.operation.is_empty(),
                    "conversation cannot execute actions"
                );
            }
            "code" => {
                ensure!(
                    !n.fallback.is_empty() && n.routes.iter().all(|r| r.when.deterministic()),
                    "code needs deterministic routes/fallback"
                );
                targets.push(n.fallback.clone());
                let mut routing = n.routes.clone();
                if !routing.iter().any(|r| matches!(r.when, Condition::Always)) {
                    ensure!(
                        !routing.iter().any(|r| r.target == n.fallback),
                        "code fallback duplicates a guarded route target; choose a distinct fallback or explicit always route"
                    );
                    routing.push(Route {
                        target: n.fallback.clone(),
                        when: Condition::Always,
                    });
                }
                // This boundary catches failures ANYWHERE in generated source, not merely
                // inside a model-authored try. It never executes on the builder host.
                let executed = format!("builder_{name}_executed");
                let mut wrapped = format!("vars['{executed}'] = 'no'\ntry:\n");
                for line in n.code.lines() {
                    wrapped.push_str("    ");
                    wrapped.push_str(line);
                    wrapped.push('\n');
                }
                wrapped.push_str(&format!("    vars['{executed}'] = 'yes'\nexcept Exception:\n    vars['{executed}'] = 'no'\n    vars['builder_code_healthy'] = 'no'\n    vars['builder_verified'] = 'no'\n    vars['builder_account'] = 'no'\n"));
                for action in plan
                    .nodes
                    .values()
                    .filter(|n| n.operation == "create_promise")
                {
                    wrapped.push_str(&format!("    vars['{}'] = 'no'\n", action.validation_flag));
                }
                wrapped.push_str(&confirmation::capture_validated(plan, &code_writes[name]));
                wrapped.push_str(&confirmation::invalidate_changed(plan));
                node["pre_actions"] = json!([code_tool(&wrapped)]);
                node["passthrough"] = json!(true);
                node["on_enter"] = match &n.on_enter {
                    Some(speech) => serde_json::to_value(speech)?,
                    None => json!({"type":"silent"}),
                };
                node["passthrough_default_transition"] = json!("builder_code_failure");
                let mut guarded_routes: Vec<Value> = routing
                    .into_iter()
                    .map(|r| {
                        edge(
                            &r.target,
                            Condition::All {
                                conditions: vec![eq(&executed), eq("builder_code_healthy"), r.when],
                            },
                        )
                    })
                    .collect();
                guarded_routes.push(edge("builder_code_failure", Condition::Always));
                node["transitions"] = json!(guarded_routes);
            }
            "api_request" => {
                ensure!(
                    n.routes.is_empty() && n.extraction.is_empty() && n.code.is_empty(),
                    "API actions use success/failure only"
                );
                ensure!(
                    n.success != n.failure && !n.success.is_empty() && !n.failure.is_empty(),
                    "distinct API success/failure targets required"
                );
                targets.extend([n.success.clone(), n.failure.clone()]);
                let mut bank_tool = tool(&n.operation, n, &catalog)?;
                if n.operation == "create_promise" {
                    confirmation::bind_snapshots(name, n, &mut bank_tool);
                }
                node["pre_actions"] = json!([bank_tool]);
                node["passthrough"] = json!(true);
                node["on_enter"] = json!({"type":"silent"});
                node["passthrough_default_transition"] = json!(n.failure);
                let ok = format!("builder_{name}_ok");
                let mut reset = format!("vars['{ok}'] = 'no'\n");
                if n.operation == "verify_identity" {
                    reset.push_str("vars['builder_verified'] = 'no'\n");
                }
                if n.operation == "get_account" {
                    reset.push_str("vars['builder_account'] = 'no'\n");
                }
                let mut mappings = vec![json!({"path":"data.builder_ok","variable":ok})];
                if n.operation == "create_promise" {
                    mappings.push(json!({"path":"data.builder_retry_safe","variable":format!("builder_{name}_retry_safe")}));
                }
                for (variable, path) in &n.mappings {
                    writable_reference(variable)?;
                    ensure!(
                        !validation_flags.contains(variable),
                        "API mappings cannot forge code validation"
                    );
                    ensure!(
                        (path == "data" || path.starts_with("data.")) && !path.contains(['{', '}']),
                        "mapping must use an API data path, without templates"
                    );
                    reset.push_str(&format!("vars['{variable}'] = ''\n"));
                    mappings.push(
                        json!({"path":format!("data.builder_map_{variable}"),"variable":variable}),
                    );
                }
                if n.operation == "verify_identity" {
                    mappings.push(
                        json!({"path":"data.builder_verified","variable":"builder_verified"}),
                    );
                }
                if n.operation == "get_account" {
                    mappings.push(json!({"path":"data.builder_ok","variable":"builder_account"}));
                }
                node["pre_actions"][0]["pre_process_code"] = json!(reset);
                node["response_mapping"] = json!(mappings);
                node["transitions"] = json!([
                    edge(&n.success, eq(&ok)),
                    edge(&n.failure, Condition::Always)
                ]);
                let mut guards = vec![eq("builder_code_healthy")];
                if n.operation == "get_account" || n.operation == "create_promise" {
                    guards.push(eq("builder_verified"));
                }
                if n.operation == "create_promise" {
                    guards.push(eq("builder_account"));
                    writable_reference(&n.validation_flag)?;
                    ensure!(
                        !extraction_vars.contains(&n.validation_flag),
                        "promise validation must be code-owned, not extracted"
                    );
                    guards.extend([
                        eq(&n.validation_flag),
                        eq(&format!("builder_{name}_authorized")),
                    ]);
                    let good = format!("builder_{name}_success");
                    let bad = format!("builder_{name}_failure");
                    for (key, target) in [(&good, &n.success), (&bad, &n.failure)] {
                        let cleanup = confirmation::finish_attempt(name, key == &good);
                        nodes.insert(key.clone(), json!({"name":key,"node_type":"code","passthrough":true,"on_enter":{"type":"silent"},"pre_actions":[code_tool(&cleanup)],"passthrough_default_transition":target,"transitions":[edge(target,Condition::Always)]}));
                    }
                    node["passthrough_default_transition"] = json!(bad);
                    node["transitions"] =
                        json!([edge(&good, eq(&ok)), edge(&bad, Condition::Always)]);
                }
                if !guards.is_empty() {
                    // All incoming references resolve to this compiler-owned gate.
                    let action = format!("builder_{name}_action");
                    node["name"] = json!(action);
                    nodes.insert(action.clone(), node);
                    let gate_code = if n.operation == "create_promise" {
                        confirmation::attempt(name, n)
                    } else {
                        "vars['builder_gate_seen'] = 'yes'\n".into()
                    };
                    node = json!({"name":name,"node_type":"code","passthrough":true,"on_enter":{"type":"silent"},"pre_actions":[code_tool(&gate_code)],"passthrough_default_transition":n.failure,"transitions":[edge(&action,Condition::All{conditions:guards}),edge(&n.failure,Condition::Always)]});
                }
            }
            "end_call" => {
                ensure!(n.routes.is_empty(), "end node cannot route");
                node = terminal_node(name, &n.prompt);
            }
            _ => bail!("unsupported node kind"),
        }
        ensure!(
            n.kind == "conversation" || n.extraction.is_empty(),
            "extraction only in conversation"
        );
        for target in &targets {
            ensure!(
                plan.nodes.contains_key(target),
                "missing graph target {target}"
            );
        }
        refs.insert(name.clone(), targets);
        // Extraction into proposal bindings starts a new negotiation and revokes only
        // the affected action tokens before any user answer is interpreted.
        if n.kind == "conversation" {
            let reset = confirmation::proposal_reset(n, plan);
            if !reset.is_empty() {
                let dialogue = format!("builder_{name}_dialogue");
                node["name"] = json!(dialogue);
                nodes.insert(dialogue.clone(), node);
                node = confirmation::silent(name, &reset, &dialogue);
            }
        }
        nodes.insert(name.clone(), node);
    }
    // Every reachable node must be able to terminate. Turn caps bound conversation loops,
    // but this is not a semantic proof that generated conditions/code will make progress.
    let mut terminating: BTreeSet<String> = plan
        .nodes
        .iter()
        .filter(|(_, n)| n.kind == "end_call")
        .map(|(k, _)| k.clone())
        .collect();
    loop {
        let old = terminating.len();
        for (k, vs) in &refs {
            if vs.iter().any(|v| terminating.contains(v)) {
                terminating.insert(k.clone());
            }
        }
        if old == terminating.len() {
            break;
        }
    }
    ensure!(
        terminating.len() == plan.nodes.len(),
        "some nodes have no terminating path"
    );
    // Initialize compiler-owned authority flags, ignoring any incoming call defaults.
    let mut initialize = String::from(
        "vars['builder_code_healthy'] = 'yes'\nvars['builder_verified'] = 'no'\nvars['builder_account'] = 'no'\n",
    );
    for (name, n) in &plan.nodes {
        if n.kind == "code" {
            initialize.push_str(&format!("vars['builder_{name}_executed'] = 'no'\n"));
        }
        if n.kind == "api_request" {
            initialize.push_str(&format!("vars['builder_{name}_ok'] = 'no'\n"));
        }
        if n.operation == "create_promise" {
            initialize.push_str(&format!("vars['{}'] = 'no'\n", n.validation_flag));
        }
    }
    initialize.push_str(&confirmation::initialize(plan));
    nodes.insert(
        "builder_code_failure".into(),
        terminal_node("builder_code_failure", ""),
    );
    nodes.insert("builder_initialize".into(),json!({"name":"builder_initialize","node_type":"code","passthrough":true,"on_enter":{"type":"silent"},"pre_actions":[code_tool(&initialize)],"passthrough_default_transition":plan.initial_node,"transitions":[edge(&plan.initial_node,Condition::Always)]}));
    Ok(
        json!({"system_prompt":plan.system_prompt,"initial_node":"builder_initialize","nodes":nodes}),
    )
}
