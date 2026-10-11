//! Structural and sandbox boundary for document-derived workflows, independent of the practice IR.
use crate::{compiler::check_generic_python, documents::Endpoint};
use anyhow::{Context, Result, ensure};
use serde_json::Value;

const WORKFLOW_USER_AGENT: &str = "Mozilla/5.0 (Freya workflow)";

/// A stable browser-compatible transport identity avoids edge middleware's rejection
/// of generic HTTP library user agents. Other header declarations are left intact.
pub fn normalize_transport(workflow: &mut Value) -> Result<()> {
    match workflow {
        Value::Object(object) => {
            if object
                .get("function")
                .and_then(|f| f.get("handler"))
                .and_then(Value::as_str)
                == Some("__function__:api_call")
            {
                let properties = object
                    .get_mut("function")
                    .and_then(|f| f.pointer_mut("/parameters/properties"))
                    .and_then(Value::as_object_mut)
                    .context("API parameter properties missing")?;
                let headers = properties
                    .entry("headers")
                    .or_insert_with(|| serde_json::json!({"type":"object","properties":{}}));
                let header_properties = headers
                    .get_mut("properties")
                    .and_then(Value::as_object_mut)
                    .context("API headers require schema properties")?;
                header_properties.retain(|key, _| !key.eq_ignore_ascii_case("user-agent"));
                header_properties.insert(
                    "User-Agent".into(),
                    serde_json::json!({"type":"string","default":WORKFLOW_USER_AGENT}),
                );
            }
            for value in object.values_mut() {
                normalize_transport(value)?;
            }
        }
        Value::Array(values) => {
            for value in values {
                normalize_transport(value)?;
            }
        }
        _ => {}
    }
    Ok(())
}

pub fn mock_base(base: &str) -> Result<String> {
    let url = reqwest::Url::parse(base).context("invalid mock URL")?;
    ensure!(
        url.scheme() == "https" && url.host_str().is_some(),
        "mock URL must be HTTPS"
    );
    ensure!(
        url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "mock URL cannot contain credentials/query/fragment"
    );
    ensure!(
        !base.contains(['{', '}', '\\', '%']) && !base.split('/').any(|p| p == "." || p == ".."),
        "mock URL must be a literal canonical base"
    );
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

pub fn endpoint_path(path: &str) -> Result<()> {
    ensure!(
        path.starts_with('/')
            && !path.starts_with("//")
            && !path.contains("//")
            && path.len() <= 500,
        "endpoint must be an absolute service path"
    );
    ensure!(
        path.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"/._-".contains(&c))
            && !path.split('/').any(|s| s == "." || s == ".."),
        "endpoint must be literal, without traversal, query, templates or encoded characters"
    );
    Ok(())
}

fn keys(value: &Value, allowed: &[&str], context: &str) -> Result<()> {
    let object = value
        .as_object()
        .with_context(|| format!("{context} must be an object"))?;
    for key in object.keys() {
        ensure!(
            allowed.contains(&key.as_str()),
            "unsupported {context} field: {key}"
        );
    }
    Ok(())
}
fn no_null(value: &Value) -> Result<()> {
    match value {
        Value::Null => anyhow::bail!("workflow null values are unsupported; omit optional fields"),
        Value::Array(a) => {
            for v in a {
                no_null(v)?;
            }
        }
        Value::Object(o) => {
            for v in o.values() {
                no_null(v)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// GET responses contain nullable/default UI metadata absent from write schemas.
/// Verify execution fields against the already validated draft without editing the export.
pub fn validate_snapshot(snapshot: &Value, draft: &Value) -> Result<()> {
    fn contains(actual: &Value, expected: &Value, path: &str) -> Result<()> {
        match expected {
            Value::Object(fields) => {
                ensure!(
                    actual.is_object(),
                    "published workflow changed object at {path}"
                );
                for (key, value) in fields {
                    contains(&actual[key], value, &format!("{path}.{key}"))?;
                }
                for key in ["pre_process_code", "post_process_code"] {
                    if !fields.contains_key(key) {
                        ensure!(
                            actual[key].as_str().is_none_or(|s| s.is_empty()),
                            "unexpected published hook at {path}"
                        );
                    }
                }
            }
            Value::Array(items) => {
                let values = actual
                    .as_array()
                    .with_context(|| format!("published workflow changed array at {path}"))?;
                ensure!(
                    values.len() == items.len(),
                    "published workflow changed array length at {path}"
                );
                for (i, value) in items.iter().enumerate() {
                    contains(&values[i], value, &format!("{path}[{i}]"))?;
                }
            }
            _ => ensure!(
                actual == expected,
                "published workflow changed value at {path}"
            ),
        }
        Ok(())
    }
    fn signatures(value: &Value, result: &mut Vec<String>) {
        match value {
            Value::Object(fields) => {
                if let Some(handler) = fields.get("handler").and_then(Value::as_str)
                    && fields.contains_key("name")
                    && fields.contains_key("parameters")
                    && handler != "__function__:end_call"
                {
                    result.push(format!("{}:{}", handler, value["name"]));
                }
                for child in fields.values() {
                    signatures(child, result);
                }
            }
            Value::Array(values) => {
                for child in values {
                    signatures(child, result);
                }
            }
            _ => {}
        }
    }
    contains(snapshot, draft, "workflow")?;
    let mut actual = Vec::new();
    let mut expected = Vec::new();
    signatures(snapshot, &mut actual);
    signatures(draft, &mut expected);
    actual.sort();
    expected.sort();
    ensure!(
        actual == expected,
        "published workflow added or removed executable tools"
    );
    ensure!(
        snapshot.pointer("/agent_config/llmConfig/provider")
            == Some(&serde_json::json!("experimental-5"))
            && snapshot.pointer("/agent_config/llmConfig/model")
                == Some(&serde_json::json!("experimental-5")),
        "published model differs from mandated model"
    );
    Ok(())
}

pub fn validate(workflow: &Value, endpoints: &[Endpoint], base: &str) -> Result<()> {
    let base = mock_base(base)?;
    ensure!(
        workflow.to_string().len() <= 500_000,
        "workflow exceeds 500 KB"
    );
    no_null(workflow)?;
    keys(
        workflow,
        &[
            "initial_node",
            "system_prompt",
            "nodes",
            "global_nodes",
            "test_variables",
            "context_variables",
            "tools",
            "agent_config",
            "nodePositions",
        ],
        "workflow",
    )?;
    if let Some(config) = workflow.get("agent_config") {
        ensure!(
            config.pointer("/llmConfig/provider") == Some(&serde_json::json!("experimental-5"))
                && config.pointer("/llmConfig/model") == Some(&serde_json::json!("experimental-5")),
            "workflow model must remain experimental-5"
        );
    }
    let nodes = workflow["nodes"]
        .as_object()
        .context("nodes must be an object")?;
    let globals = workflow.get("global_nodes").and_then(Value::as_object);
    ensure!(
        !nodes.is_empty() && nodes.len() <= 100 && globals.is_none_or(|g| g.len() <= 30),
        "invalid node count"
    );
    let initial = workflow["initial_node"]
        .as_str()
        .context("initial_node must be a string")?;
    ensure!(nodes.contains_key(initial), "initial node missing");
    let exists = |target: &str| {
        nodes.contains_key(target) || globals.is_some_and(|g| g.contains_key(target))
    };
    for (name, node) in nodes.iter().chain(globals.into_iter().flatten()) {
        keys(
            node,
            &[
                "name",
                "node_type",
                "task_message",
                "role_message",
                "on_enter",
                "on_enter_first_entry_only",
                "transitions",
                "routing_prompt",
                "loop_condition",
                "loop_prompt",
                "extraction_schema",
                "tools",
                "pre_actions",
                "response_mapping",
                "passthrough",
                "passthrough_default_transition",
                "max_node_turns",
                "max_turn_transition",
                "terminal",
                "disable_inactivity_monitor",
                "suppress_tool_results",
                "truncation_strategy",
                "truncation_window_size",
                "reasoning_override",
                "condition",
                "description",
                "excluded_nodes",
                "is_tool_call",
            ],
            "node",
        )?;
        ensure!(node["name"] == *name, "node name must match its key");
        let kind = node["node_type"].as_str().context("node_type missing")?;
        ensure!(
            ["conversation", "api_request", "code", "end_call"].contains(&kind),
            "unsupported action node type"
        );
        if let Some(entry) = node.get("on_enter") {
            ensure!(
                matches!(entry["type"].as_str(), Some("llm" | "tts" | "silent")),
                "unsupported node entry action"
            );
            keys(entry, &["type", "text", "prompt"], "on_enter")?;
        }
        let exists = |target: &str| {
            exists(target)
                || (target == "__previous__" && globals.is_some_and(|g| g.contains_key(name)))
        };
        for field in ["passthrough_default_transition", "max_turn_transition"] {
            if let Some(target) = node.get(field) {
                ensure!(
                    target.as_str().is_some_and(&exists),
                    "unknown transition target"
                );
            }
        }
        let transitions = node["transitions"]
            .as_array()
            .context("transitions array required")?;
        for transition in transitions {
            ensure!(
                transition["target_node"].as_str().is_some_and(&exists),
                "unknown transition target"
            );
        }
        if node["passthrough"] == true {
            ensure!(
                transitions
                    .iter()
                    .any(|t| t.pointer("/condition/operator").and_then(Value::as_str)
                        == Some("always")),
                "passthrough requires an always fallback transition"
            );
        }
        for field in ["tools", "pre_actions"] {
            if let Some(tools) = node.get(field) {
                let tools = tools
                    .as_array()
                    .context("tools/pre_actions must be arrays")?;
                ensure!(
                    tools.is_empty()
                        || (field == "tools" && kind == "conversation")
                        || (field == "pre_actions" && matches!(kind, "api_request" | "code")),
                    "platform strips tools on incompatible node types"
                );
                for tool in tools {
                    validate_tool(
                        tool,
                        endpoints,
                        &base,
                        false,
                        if field == "pre_actions" {
                            node.get("response_mapping")
                        } else {
                            tool.get("response_mapping")
                        },
                    )?;
                }
            }
        }
        if let Some(terminal) = node.get("terminal") {
            ensure!(kind == "end_call", "terminal only allowed on end_call");
            validate_tool(terminal, endpoints, &base, true, None)?;
        }
        mappings(node.get("response_mapping"))?;
    }
    if let Some(tools) = workflow.get("tools") {
        for tool in tools
            .as_array()
            .context("workflow tools must be an array")?
        {
            validate_tool(tool, endpoints, &base, false, tool.get("response_mapping"))?;
        }
    }
    Ok(())
}
fn mappings(value: Option<&Value>) -> Result<()> {
    if let Some(value) = value {
        for mapping in value
            .as_array()
            .context("response_mapping must be an array")?
        {
            keys(mapping, &["path", "variable"], "response mapping")?;
            let path = mapping["path"].as_str().context("mapping path missing")?;
            ensure!(
                path.starts_with("data.") || ["status", "status_code", "message"].contains(&path),
                "mapping must start at actual API envelope (data.*)"
            );
            ensure!(
                mapping["variable"].as_str().is_some_and(|s| !s.is_empty()),
                "mapping variable missing"
            );
        }
    }
    Ok(())
}
fn validate_tool(
    tool: &Value,
    endpoints: &[Endpoint],
    base: &str,
    terminal: bool,
    response_mapping: Option<&Value>,
) -> Result<()> {
    keys(
        tool,
        &[
            "type",
            "config",
            "function",
            "response_mapping",
            "pre_process_code",
            "post_process_code",
        ],
        "tool",
    )?;
    ensure!(tool["type"] == "function", "only function tools permitted");
    if let Some(config) = tool.get("config") {
        keys(
            config,
            &["timeoutMs", "maxRetries", "interruptible", "alwaysRunsAt"],
            "tool config",
        )?;
    }
    let function = &tool["function"];
    keys(
        function,
        &["name", "handler", "description", "parameters"],
        "function",
    )?;
    ensure!(
        function["name"]
            .as_str()
            .is_some_and(|n| !n.is_empty() && n.len() <= 128),
        "tool name missing/excessive"
    );
    let handler = function["handler"]
        .as_str()
        .context("tool handler missing")?;
    ensure!(
        if terminal {
            handler == "__function__:end_call"
        } else {
            matches!(handler, "__function__:api_call" | "__function__:code_exec")
        },
        "unsupported tool handler; no integrations/external execution"
    );
    let parameters = &function["parameters"];
    ensure!(
        parameters["type"] == "object",
        "function parameters must be object schema"
    );
    let props = parameters["properties"]
        .as_object()
        .context("parameter properties missing")?;
    if handler == "__function__:api_call" {
        for key in props.keys() {
            ensure!(
                ["url", "method", "headers", "body", "connect_retries"].contains(&key.as_str()),
                "unsupported API configuration parameter"
            );
        }
        let url = parameters
            .pointer("/properties/url/default")
            .and_then(Value::as_str)
            .context("API URL must be a baked literal default")?;
        let method = parameters
            .pointer("/properties/method/default")
            .and_then(Value::as_str)
            .context("API method must be a baked literal default")?;
        ensure!(
            endpoints
                .iter()
                .any(|e| e.method == method && url == format!("{base}{}", e.path)),
            "API URL/method not in frozen source contract and pinned HTTPS mock base"
        );
        if let Some(headers) = props.get("headers") {
            keys(headers, &["type", "properties"], "headers schema")?;
            ensure!(headers["type"] == "object", "headers must be object schema");
            for (name, schema) in headers["properties"]
                .as_object()
                .context("header properties missing")?
            {
                ensure!(
                    (["content-type", "accept"].contains(&name.to_ascii_lowercase().as_str())
                        && schema["default"] == "application/json")
                        || (name.eq_ignore_ascii_case("user-agent")
                            && schema["default"] == WORKFLOW_USER_AGENT),
                    "only fixed JSON media headers and normalized User-Agent allowed; no credentials/header templates"
                );
            }
        }
        // Raw object/list mappings are dropped by the platform. Always provide an explicit
        // scalar JSON serialization in the post-hook, in addition to any individual scalars.
        let post = tool["post_process_code"]
            .as_str()
            .context("API needs post-hook scalar JSON serialization")?;
        ensure!(
            post.contains("json.dumps"),
            "API post-hook must serialize object/list data into mapped scalar JSON"
        );
        ensure!(
            response_mapping
                .and_then(Value::as_array)
                .is_some_and(|mappings| mappings.iter().any(|mapping| {
                    mapping["path"].as_str().is_some_and(|path| {
                        path.starts_with("data.")
                            && path.rsplit('.').next().is_some_and(|field| {
                                post.contains(&format!("'{field}'"))
                                    || post.contains(&format!("\"{field}\""))
                            })
                    })
                })),
            "API requires a data.* scalar mapping corresponding to its post-hook output; pre_actions mappings belong on the node"
        );
    }
    if handler == "__function__:code_exec" {
        let code = parameters
            .pointer("/properties/code/default")
            .and_then(Value::as_str)
            .context("code must have baked source default")?;
        check_generic_python(code, "code")?;
    }
    for (field, mode) in [("pre_process_code", "pre"), ("post_process_code", "post")] {
        if let Some(code) = tool.get(field) {
            ensure!(
                handler == "__function__:api_call",
                "hooks only supported on API tools"
            );
            let code = code.as_str().context("hook must be a string")?;
            if !code.trim().is_empty() {
                check_generic_python(code, mode)?;
            }
        }
    }
    mappings(tool.get("response_mapping"))?;
    Ok(())
}
