//! Compiler-owned consent state. No model-authored code is executed here.
use super::*;

const FIELDS: [&str; 3] = ["product_id", "amount", "date"];
const SPOKEN: [&str; 3] = ["product_label", "amount", "date"];

fn value_expr(v: &Value) -> String {
    if let Some(s) = v.as_str().filter(|s| s.starts_with("{{")) {
        format!("vars.get('{}')", &s[2..s.len() - 2])
    } else {
        // Promise fields are strings/numbers only; JSON literals are also Python literals.
        v.to_string()
    }
}
fn snapshot(name: &str, field: &str) -> String {
    format!("builder_{name}_{field}")
}
fn same(name: &str, n: &Node) -> String {
    same_with_prefix(&format!("builder_{name}_"), n)
}
fn same_with_prefix(prefix: &str, n: &Node) -> String {
    format!(
        "{} and {} == vars.get('{prefix}product_label')",
        FIELDS
            .iter()
            .map(|f| format!(
                "{} == vars.get('{}')",
                value_expr(&n.arguments[*f]),
                format_args!("{prefix}{f}")
            ))
            .collect::<Vec<_>>()
            .join(" and "),
        value_expr(&json!(n.product_label))
    )
}
fn ready(name: &str, n: &Node) -> String {
    format!(
        "vars.get('builder_code_healthy') == 'yes' and vars.get('builder_verified') == 'yes' and vars.get('builder_account') == 'yes' and vars.get('{}') == 'yes' and vars.get('builder_{name}_token') == 'yes' and {} and vars.get('builder_{name}_product_id') not in vars.get('builder_blocked_products', [])",
        n.validation_flag,
        same(name, n)
    )
}
pub(super) fn validate(plan: &Plan, catalog: &Catalog) -> Result<()> {
    let mut owners = BTreeSet::new();
    for n in plan.nodes.values() {
        ensure!(
            n.confirmation_flag.is_empty(),
            "confirmation_flag is unsafe and no longer supported; use kind=confirmation with actions and fixed on_enter readback"
        );
        if n.kind != "confirmation" {
            ensure!(
                n.actions.is_empty(),
                "actions only supported on confirmation states"
            );
            continue;
        }
        ensure!(
            !n.actions.is_empty()
                && n.routes.is_empty()
                && n.extraction.is_empty()
                && n.code.is_empty()
                && n.operation.is_empty()
                && n.prompt.is_empty(),
            "confirmation requires actions and compiler-owned routing/extraction; no prompt/code/routes/extraction/operation"
        );
        ensure!(
            !n.success.is_empty() && !n.failure.is_empty() && n.success != n.failure,
            "confirmation needs distinct success/failure targets"
        );
        let Some(EntrySpeech::Tts { text }) = &n.on_enter else {
            bail!("confirmation requires fixed on_enter type=tts text");
        };
        let mut expected = BTreeSet::new();
        for action in &n.actions {
            let a = plan
                .nodes
                .get(action)
                .context("confirmation action missing")?;
            ensure!(
                a.kind == "api_request" && a.operation == "create_promise",
                "confirmation actions must name create_promise API nodes"
            );
            ensure!(
                owners.insert(action.clone()),
                "each promise action must belong to exactly one confirmation state"
            );
            for field in SPOKEN {
                let placeholder = format!("{{{{{action}.{field}}}}}");
                ensure!(
                    text.contains(&placeholder),
                    "readback must include {placeholder}"
                );
                expected.insert(format!("{action}.{field}"));
            }
        }
        // No mutable variables in fixed consent speech, including hidden extra terms.
        for tail in text.split("{{").skip(1) {
            let key = tail.split("}}").next().unwrap_or_default();
            ensure!(
                expected.contains(key),
                "confirmation speech may only reference action.product_label/action.amount/action.date snapshots"
            );
        }
    }
    let mut snapshot_keys = BTreeSet::new();
    for (name, n) in &plan.nodes {
        if n.operation != "create_promise" {
            continue;
        }
        // An action named e.g. first_validated must not alias first's validation
        // snapshot: validation capture would otherwise overwrite confirmed terms.
        for field in FIELDS.into_iter().chain(["product_label"]) {
            for key in [
                snapshot(name, field),
                snapshot(name, &format!("validated_{field}")),
            ] {
                ensure!(
                    snapshot_keys.insert(key.clone()),
                    "colliding generated confirmation state key: {key}"
                );
            }
        }
        ensure!(
            owners.contains(name),
            "create_promise {name} requires a dedicated confirmation state naming it in actions"
        );
        ensure!(
            n.arguments.len() == 3 && FIELDS.iter().all(|f| n.arguments.contains_key(*f)),
            "unsupported promise arguments: exactly product_id, amount, date are supported"
        );
        let schema = catalog
            .operations
            .get("create_promise")
            .context("create_promise missing from supplied OpenAPI")?;
        for f in FIELDS {
            let expected = if f == "amount" { "number" } else { "string" };
            ensure!(
                schema["properties"][f]["type"] == expected,
                "unsupported create_promise schema"
            );
        }
        ensure!(
            !n.product_label.trim().is_empty(),
            "create_promise requires product_label (account-derived display name, literal or whole variable template)"
        );
        lint_templates(&n.product_label)?;
        if n.product_label.contains("{{") {
            ensure!(
                n.product_label.starts_with("{{")
                    && n.product_label.ends_with("}}")
                    && name_ok(&n.product_label[2..n.product_label.len() - 2]),
                "product_label requires a whole variable template"
            );
        }
        // Validate bindings before generating any Python expressions.
        tool("create_promise", n, catalog)?;
    }
    Ok(())
}
pub(super) fn silent(name: &str, code: &str, target: &str) -> Value {
    json!({"name":name,"node_type":"code","passthrough":true,"on_enter":{"type":"silent"},"pre_actions":[code_tool(code)],"passthrough_default_transition":target,"transitions":[edge(target,Condition::Always)]})
}
pub(super) fn initialize(plan: &Plan) -> String {
    let mut code = "vars['builder_blocked_products'] = []\n".to_string();
    for (name, n) in &plan.nodes {
        if n.operation == "create_promise" {
            code.push_str(&format!(
                "vars['builder_{name}_token'] = 'no'\nvars['builder_{name}_authorized'] = 'no'\nvars['builder_{name}_validated'] = 'no'\n"
            ));
            for f in FIELDS.into_iter().chain(["product_label"]) {
                code.push_str(&format!("vars['{}'] = ''\n", snapshot(name, f)));
            }
        }
    }
    code
}
pub(super) fn compile_state(
    name: &str,
    n: &Node,
    plan: &Plan,
    nodes: &mut serde_json::Map<String, Value>,
) -> Result<Value> {
    let speech = format!("builder_{name}_readback");
    let grant = format!("builder_{name}_grant");
    let valid = format!("builder_{name}_ready");
    let Some(EntrySpeech::Tts { text }) = &n.on_enter else {
        bail!("missing readback");
    };
    let mut text = text.clone();
    let mut code = format!("vars['{valid}'] = 'no'\n");
    // Reset before attempting any snapshot: failures can never preserve old consent.
    for action in &n.actions {
        code.push_str(&format!("vars['builder_{action}_token'] = 'no'\n"));
    }
    code.push_str(&format!("try:\n    import json\n    builder_history = vars.get('system.conversation', [])\n    if isinstance(builder_history, str):\n        builder_history = json.loads(builder_history)\n    assert isinstance(builder_history, list)\n    vars['builder_{name}_history_size'] = len(builder_history)\n    builder_products = []\n"));
    for action in &n.actions {
        let a = &plan.nodes[action];
        code.push_str(&format!(
            "    assert vars.get('{}') == 'yes' and vars.get('builder_{action}_validated') == 'yes' and {}\n",
            a.validation_flag, validated_same(action, a)
        ));
        code.push_str(&format!("    vars['builder_{action}_product_label'] = {}\n    assert isinstance(vars['builder_{action}_product_label'], str) and vars['builder_{action}_product_label'].strip()\n", value_expr(&json!(a.product_label))));
        text = text.replace(
            &format!("{{{{{action}.product_label}}}}"),
            &format!("{{{{builder_{action}_product_label}}}}"),
        );
        for f in FIELDS {
            code.push_str(&format!(
                "    vars['{}'] = {}\n",
                snapshot(action, f),
                value_expr(&a.arguments[f])
            ));
            text = text.replace(
                &format!("{{{{{action}.{f}}}}}"),
                &format!("{{{{{}}}}}", snapshot(action, f)),
            );
        }
        code.push_str(&format!("    assert isinstance(vars['builder_{action}_product_id'], str) and vars['builder_{action}_product_id'].strip()\n    assert isinstance(vars['builder_{action}_date'], str) and vars['builder_{action}_date'].strip()\n    assert type(vars['builder_{action}_amount']) in (int, float) and 0 < vars['builder_{action}_amount'] < float('inf')\n    assert vars['builder_{action}_product_id'] not in vars.get('builder_blocked_products', [])\n    assert vars['builder_{action}_product_id'] not in builder_products\n    builder_products.append(vars['builder_{action}_product_id'])\n"));
    }
    let mut parts = Vec::new();
    let mut remaining = text.as_str();
    while let Some(start) = remaining.find("{{") {
        parts.push(serde_json::to_string(&remaining[..start])?);
        let tail = &remaining[start + 2..];
        let end = tail.find("}}").context("invalid readback template")?;
        parts.push(format!("str(vars['{}'])", &tail[..end]));
        remaining = &tail[end + 2..];
    }
    parts.push(serde_json::to_string(remaining)?);
    code.push_str(&format!(
        "    vars['builder_{name}_text'] = {}\n",
        parts.join(" + ")
    ));
    code.push_str(&format!("    assert vars.get('builder_code_healthy') == 'yes' and vars.get('builder_verified') == 'yes' and vars.get('builder_account') == 'yes'\n    vars['{valid}'] = 'yes'\nexcept Exception:\n    vars['{valid}'] = 'no'\n"));
    let mut entry = silent(name, &code, &n.failure);
    entry["transitions"] = json!([
        edge(&speech, eq(&valid)),
        edge(&n.failure, Condition::Always)
    ]);
    nodes.insert(speech.clone(), json!({"name":speech,"node_type":"conversation","on_enter":{"type":"tts","text":format!("{{{{builder_{name}_text}}}}")},"on_enter_first_entry_only":false,"task_message":"The fixed readback is the only offer. Do not invent or alter terms. Unclear or changed terms require repair and a new complete readback.","max_node_turns":1,"max_turn_transition":grant,"transitions":[edge(&grant,Condition::Always)]}));
    // system.conversation is verified runtime evidence, not an inferred utterance.
    // Fail closed for absent/unrecognized transcript shapes or non-adjacent readback.
    let mut grant_code = format!(
        "import json\nimport re\nvars['{valid}'] = 'no'\nbuilder_explicit = False\ntry:\n    builder_history = vars.get('system.conversation')\n    if isinstance(builder_history, str):\n        builder_history = json.loads(builder_history)\n    assert isinstance(builder_history, list) and len(builder_history) >= vars.get('builder_{name}_history_size', 0) + 2\n    builder_previous = builder_history[-2]\n    builder_latest = builder_history[-1]\n    assert builder_previous.get('role') == 'assistant' and builder_latest.get('role') == 'user'\n    builder_spoken = ' '.join(builder_previous.get('content', '').split())\n    builder_expected = ' '.join(vars.get('builder_{name}_text', '').split())\n    builder_reply = re.sub(r'[.,!?]', '', builder_latest.get('content', '').lower()).strip()\n    builder_reply = ' '.join(builder_reply.split())\n    builder_explicit = bool(builder_expected) and builder_spoken == builder_expected and builder_reply in ('yes', 'yes i confirm', 'i confirm', 'yes that is correct', 'yes correct')\nexcept Exception:\n    builder_explicit = False\nif builder_explicit and {}:\n",
        n.actions
            .iter()
            .map(|a| format!("({})", same(a, &plan.nodes[a])))
            .collect::<Vec<_>>()
            .join(" and ")
    );
    for action in &n.actions {
        grant_code.push_str(&format!("    vars['builder_{action}_token'] = 'yes'\n"));
    }
    grant_code.push_str(&format!("    vars['{valid}'] = 'yes'\n"));
    let mut grant_node = silent(&grant, &grant_code, &n.failure);
    grant_node["transitions"] = json!([
        edge(&n.success, eq(&valid)),
        edge(&n.failure, Condition::Always)
    ]);
    nodes.insert(grant, grant_node);
    Ok(entry)
}
pub(super) fn bind_snapshots(name: &str, n: &Node, tool: &mut Value) {
    for f in n.arguments.keys() {
        tool["function"]["parameters"]["properties"]["body"]["properties"][f]["default"] =
            json!(format!("{{{{{}}}}}", snapshot(name, f)));
    }
    // Explicit false is documented as not carried out. Missing/transport outcomes are
    // uncertain and must not be retried automatically (no backend idempotency key).
    let post = tool["post_process_code"].as_str().unwrap();
    tool["post_process_code"] = json!(format!(
        "{post}\nresult['data']['builder_retry_safe'] = 'yes' if result['data'].get('ok') is False else 'no'\n"
    ));
}
pub(super) fn attempt(name: &str, n: &Node) -> String {
    format!(
        "vars['builder_{name}_authorized'] = 'no'\nif {}:\n    vars['builder_{name}_authorized'] = 'yes'\n    vars['builder_{name}_token'] = 'no'\n    vars['builder_{name}_retry_safe'] = 'no'\n    vars['builder_blocked_products'].append(vars['builder_{name}_product_id'])\nelse:\n    vars['builder_{name}_token'] = 'no'\n",
        ready(name, n)
    )
}
pub(super) fn finish_attempt(name: &str, success: bool) -> String {
    let mut code = format!("vars['builder_{name}_authorized'] = 'no'\n");
    if !success {
        code.push_str(&format!("if vars.get('builder_{name}_retry_safe') == 'yes':\n    vars['builder_blocked_products'] = [p for p in vars.get('builder_blocked_products', []) if p != vars.get('builder_{name}_product_id')]\n    vars['builder_{name}_token'] = 'yes'\n"));
    }
    code
}
pub(super) fn invalidate_changed(plan: &Plan) -> String {
    let mut code = String::new();
    for (name, n) in &plan.nodes {
        if n.operation == "create_promise" {
            code.push_str(&format!(
                "if not ({}):\n    vars['builder_{name}_token'] = 'no'\n",
                same(name, n)
            ));
        }
    }
    code
}
pub(super) fn proposal_reset(n: &Node, plan: &Plan) -> String {
    let mut code = String::new();
    for (name, a) in &plan.nodes {
        if a.operation == "create_promise"
            && a.arguments
                .values()
                .chain(std::iter::once(&json!(a.product_label)))
                .any(|v| {
                    n.extraction
                        .iter()
                        .any(|e| v.as_str() == Some(&format!("{{{{{}}}}}", e.name)))
                })
        {
            code.push_str(&format!("vars['builder_{name}_token'] = 'no'\n"));
        }
    }
    code
}

fn validated_same(name: &str, n: &Node) -> String {
    same_with_prefix(&format!("builder_{name}_validated_"), n)
}

/// Bind a successful generated policy validation to exactly the values it checked.
/// No other generated code node or API mapping can write its validation flag.
pub(super) fn capture_validated(plan: &Plan, writes: &BTreeSet<String>) -> String {
    let mut code = String::new();
    for (name, n) in &plan.nodes {
        if n.operation != "create_promise" || !writes.contains(&n.validation_flag) {
            continue;
        }
        code.push_str(&format!("vars['builder_{name}_validated'] = 'no'\nif vars.get('builder_code_healthy') == 'yes' and vars.get('{}') == 'yes':\n", n.validation_flag));
        for f in FIELDS {
            code.push_str(&format!(
                "    vars['builder_{name}_validated_{f}'] = {}\n",
                value_expr(&n.arguments[f])
            ));
        }
        code.push_str(&format!("    vars['builder_{name}_validated_product_label'] = {}\n    vars['builder_{name}_validated'] = 'yes'\n", value_expr(&json!(n.product_label))));
    }
    code
}
