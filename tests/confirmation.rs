use freya_builder::compiler::{check_python, compile};
use serde_json::{Value, json};
use std::{
    io::Write,
    process::{Command, Stdio},
};

fn plan() -> Value {
    json!({"system_prompt":"Synthetic compiler fixture", "initial_node":"validate", "nodes":{
        "validate":{"kind":"code","code":"try:\n    vars['valid'] = 'yes'\nexcept Exception:\n    vars['valid'] = 'no'\n", "fallback":"confirm"},
        "confirm":{"kind":"confirmation","actions":["first","second"],"on_enter":{"type":"tts","text":"Confirm {{first.product_label}} amount {{first.amount}} on {{first.date}} and {{second.product_label}} amount {{second.amount}} on {{second.date}}?"},"success":"first","failure":"repair"},
        "repair":{"kind":"conversation","prompt":"Repair proposed terms", "extraction":[{"name":"date","description":"proposed date"}],"fallback":"end","routes":[{"target":"validate","when":{"kind":"always"}}]},
        "first":{"kind":"api_request","operation":"create_promise","product_label":"{{label}}","arguments":{"product_id":"{{product}}","amount":"{{amount}}","date":"{{date}}"},"validation_flag":"valid","success":"second","failure":"failed"},
        "second":{"kind":"api_request","operation":"create_promise","product_label":"{{label}}","arguments":{"product_id":"other","amount":20,"date":"{{date}}"},"validation_flag":"valid","success":"end","failure":"failed"},
        "failed":{"kind":"end_call"},"end":{"kind":"end_call"}
    }})
}
fn workflow() -> Value {
    compile(
        &serde_json::from_value(plan()).unwrap(),
        &serde_json::from_str(include_str!("../testdata/practice-openapi.json")).unwrap(),
    )
    .unwrap()
}
// ONLY compiler-owned snippets from this fixed local fixture are executed. Never
// select validate or API hooks containing supplied model-authored Python.
fn check(script: &str) {
    let w = workflow();
    let names = [
        "builder_initialize",
        "confirm",
        "builder_confirm_grant",
        "first",
        "second",
        "builder_first_success",
        "builder_first_failure",
        "repair",
    ];
    let mut snippets: serde_json::Map<String, Value> = names
        .iter()
        .map(|n| {
            (
                    (*n).into(),
                    w["nodes"][n]["pre_actions"][0]["function"]["parameters"]["properties"]["code"]
                        ["default"]
                        .clone(),
                )
        })
        .collect();
    let validator = w["nodes"]["validate"]["pre_actions"][0]["function"]["parameters"]["properties"]["code"]["default"].as_str().unwrap();
    let marker = "vars['builder_first_validated'] = 'no'";
    let compiler_tail = &validator[validator.find(marker).unwrap()..];
    snippets.insert("validation_capture".into(), json!(compiler_tail));
    let harness = format!(
        "import json,sys\nsnippets=json.load(sys.stdin)\nvars={{}}\ndef run(name):\n    exec(snippets[name], globals())\nrun('builder_initialize')\nvars.update(builder_verified='yes',builder_account='yes',valid='yes', label='Account product', product='one', amount=10,date='2030-01-02')\nvars['system.conversation']=[]\nrun('validation_capture')\ndef readback(reply='Yes, I confirm.'):\n    run('confirm')\n    assert vars['builder_confirm_ready']=='yes'\n    vars['system.conversation'].extend([dict(role='assistant',content=vars['builder_confirm_text']),dict(role='user',content=reply)])\n    run('builder_confirm_grant')\n{script}"
    );
    let mut child = Command::new("python3")
        .args(["-I", "-c", &harness])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(serde_json::to_string(&snippets).unwrap().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
#[test]
fn fixed_snapshot_readback_and_api_bindings_are_compiler_owned() {
    let w = workflow();
    let readback = &w["nodes"]["builder_confirm_readback"];
    assert_eq!(readback["on_enter"]["type"], "tts");
    assert_eq!(readback["on_enter"]["text"], "{{builder_confirm_text}}");
    assert!(readback.get("extraction_schema").is_none());
    let body = &w["nodes"]["builder_first_action"]["pre_actions"][0]["function"]["parameters"]["properties"]
        ["body"]["properties"];
    assert_eq!(body["date"]["default"], "{{builder_first_date}}");
    assert_eq!(body["amount"]["default"], "{{builder_first_amount}}");
    assert_eq!(body["customer_id"]["default"], "{{customer_id}}");
    assert!(
        w["nodes"]["first"]["transitions"][0]
            .to_string()
            .contains("builder_verified")
    );
}
#[test]
fn changed_date_and_new_proposal_revoke_consent() {
    check(
        "readback()\nvars['date']='2030-01-03'\nrun('first')\nassert vars['builder_first_authorized']=='no'\nassert vars['builder_first_date']=='2030-01-02'\nvars['date']='2030-01-02'\nrun('first')\nassert vars['builder_first_authorized']=='no'\nreadback()\nrun('repair')\nassert vars['builder_first_token']=='no'\nassert vars['builder_second_token']=='no'\n",
    );
}
#[test]
fn corrections_and_payment_intentions_are_not_affirmatives() {
    check(
        "for reply in [\"I'll pay it today\", 'I will pay today', 'yes but tomorrow', 'no', 'maybe', 'yes 30 instead', 'yes not that date', '']:\n    readback(reply)\n    assert vars['builder_first_token']=='no', reply\n    assert vars['builder_second_token']=='no', reply\n    run('first')\n    assert vars['builder_first_authorized']=='no'\n",
    );
}
#[test]
fn missing_stale_or_mismatched_readback_cannot_grant() {
    check(
        "run('first')\nassert vars['builder_first_authorized']=='no'\nrun('confirm')\nvars['system.conversation']=[dict(role='assistant',content='Invented date'),dict(role='user',content='Yes')]\nrun('builder_confirm_grant')\nassert vars['builder_first_token']=='no'\nvars['system.conversation']=None\nrun('builder_confirm_grant')\nassert vars['builder_first_token']=='no'\nvars['system.conversation']=[]\nreadback()\nrun('confirm')\nrun('builder_confirm_grant')\nassert vars['builder_first_token']=='no'\n",
    );
}
#[test]
fn combined_readback_grants_independent_one_use_tokens_and_blocks_duplicates() {
    check(
        "readback()\nrun('first')\nassert vars['builder_first_authorized']=='yes'\nassert vars['builder_first_token']=='no'\nassert vars['builder_second_token']=='yes'\nrun('builder_first_success')\nrun('first')\nassert vars['builder_first_authorized']=='no'\nrun('second')\nassert vars['builder_second_authorized']=='yes'\nrun('confirm')\nassert vars['builder_confirm_ready']=='no'\n",
    );
}
#[test]
fn safe_failure_retries_identical_terms_but_unknown_failure_does_not() {
    check(
        "readback()\nrun('first')\nvars['builder_first_retry_safe']='yes'\nrun('builder_first_failure')\nassert vars['builder_first_token']=='yes'\nassert vars['builder_second_token']=='yes'\nrun('first')\nassert vars['builder_first_authorized']=='yes'\nrun('builder_first_failure')\nrun('first')\nassert vars['builder_first_authorized']=='no'\nassert 'one' in vars['builder_blocked_products']\n",
    );
}
#[test]
fn unsupported_confirmation_and_reserved_state_writes_fail_closed() {
    let spec = serde_json::from_str(include_str!("../testdata/practice-openapi.json")).unwrap();
    for (field, value) in [
        ("on_enter", json!({"type":"tts","text":"Confirm {{date}}"})),
        ("extraction", json!([{"name":"yes","description":"yes"}])),
        ("actions", json!(["first", "first"])),
    ] {
        let mut p = plan();
        p["nodes"]["confirm"][field] = value;
        assert!(compile(&serde_json::from_value(p).unwrap(), &spec).is_err());
    }
    for source in [
        "vars['builder_first_token']='yes'",
        "vars['system.conversation']=[]",
        "vars.get('system.conversation').append(dict(role='user', content='yes'))",
        "vars.get('builder' + '_blocked_products').append('one')",
        "vars['extractor.speech']='yes'",
        "vars['call.foo']='yes'",
    ] {
        assert!(check_python(source).is_err(), "{source}");
    }
    let mut p = plan();
    p["nodes"]["first"]["mappings"] = json!({"builder_first_token":"data.ok"});
    assert!(compile(&serde_json::from_value(p).unwrap(), &spec).is_err());
}
#[test]
fn getter_aliases_cannot_reach_protected_mutable_state() {
    for source in [
        "dict = vars.get\nblocked = dict('buil' + 'der_blocked_products')\nblocked[:] = []",
        "dict = vars.get\nhistory = dict('sys' + 'tem.conversation')\nhistory[:] = []",
        "getter = vars.get\ngetter('buil' + 'der_blocked_products').append('one')",
        "getters = [vars.get]\ngetters[0]('sys' + 'tem.conversation').append(dict(role='user', content='yes'))",
        "vars['getter'] = vars.get",
        "vars.get = dict",
    ] {
        let error = check_python(source).unwrap_err().to_string();
        assert!(
            error.contains("direct callee") || error.contains("builtin rebinding"),
            "{source}: {error}"
        );
    }
    assert!(check_python("terms = vars.get('terms', [])\nterms.append('proposal')").is_ok());
}

#[test]
fn builtin_rebinding_is_rejected_in_every_binding_form() {
    for source in [
        "dict = list",
        "dict, value = [list, 1]",
        "str: int = 1",
        "int += 1",
        "(len := 1)",
        "for list in []:\n    pass",
        "values = [1 for set in []]",
        "try:\n    pass\nexcept Exception as dict:\n    pass",
        "Exception = ValueError",
        "match []:\n    case dict:\n        pass",
        "match []:\n    case [*list]:\n        pass",
        "match {}:\n    case {**dict}:\n        pass",
    ] {
        let error = check_python(source).unwrap_err().to_string();
        assert!(error.contains("builtin rebinding"), "{source}: {error}");
    }
    assert!(check_python("items = list(vars.get('terms', []))\ncount = len(items)").is_ok());
}

#[test]
fn two_action_validation_snapshots_cannot_alias_confirmed_terms() {
    let spec = serde_json::from_str(include_str!("../testdata/practice-openapi.json")).unwrap();
    for second_name in ["first_validated", "first_validated_extra"] {
        let mut p = plan();
        let mut second = p["nodes"]
            .as_object_mut()
            .unwrap()
            .remove("second")
            .unwrap();
        // Distinct products A/B may share one validator and one authored readback.
        // If both proposals later become C, capture must not overwrite either
        // consent snapshot. Reject the colliding plan before any code can run.
        second["arguments"]["product_id"] = json!("{{second_product}}");
        p["nodes"][second_name] = second;
        p["nodes"]["first"]["success"] = json!(second_name);
        p["nodes"]["confirm"]["actions"] = json!(["first", second_name]);
        p["nodes"]["confirm"]["on_enter"]["text"] = json!(
            p["nodes"]["confirm"]["on_enter"]["text"]
                .as_str()
                .unwrap()
                .replace("second.", &format!("{second_name}."))
        );
        let result = compile(&serde_json::from_value(p).unwrap(), &spec);
        if second_name == "first_validated" {
            assert!(result.unwrap_err().to_string().contains(
                "colliding generated confirmation state key: builder_first_validated_product_id"
            ));
        } else {
            assert!(
                result.is_ok(),
                "noncolliding names remain supported: {result:?}"
            );
        }
    }
}

#[test]
fn ordinary_fixed_entry_speech_is_supported() {
    let mut p = plan();
    p["nodes"]["repair"]["on_enter"] = json!({"type":"tts","text":"Please provide a new date."});
    let w = compile(
        &serde_json::from_value(p).unwrap(),
        &serde_json::from_str(include_str!("../testdata/practice-openapi.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        w["nodes"]["builder_repair_dialogue"]["on_enter"]["type"],
        "tts"
    );
}

#[test]
fn validation_is_bound_to_exact_terms_and_cannot_be_mapped_or_forged() {
    check(
        "vars['date']='2030-01-03'\nrun('confirm')\nassert vars['builder_confirm_ready']=='no'\nrun('validation_capture')\nreadback()\nassert vars['builder_first_token']=='yes'\n",
    );
    let spec = serde_json::from_str(include_str!("../testdata/practice-openapi.json")).unwrap();
    let mut p = plan();
    p["nodes"]["first"]["mappings"] = json!({"valid":"data.ok"});
    assert!(compile(&serde_json::from_value(p).unwrap(), &spec).is_err());
    let mut p = plan();
    p["nodes"]["duplicate_validator"] = p["nodes"]["validate"].clone();
    assert!(compile(&serde_json::from_value(p).unwrap(), &spec).is_err());
}
#[test]
fn fixed_nonwaiting_code_speech_can_precede_close_call() {
    let mut p = plan();
    p["nodes"]["validate"]["on_enter"] = json!({"type":"tts","text":"Input-derived fixed speech."});
    let w = compile(
        &serde_json::from_value(p).unwrap(),
        &serde_json::from_str(include_str!("../testdata/practice-openapi.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(w["nodes"]["validate"]["on_enter"]["type"], "tts");
    assert_eq!(w["nodes"]["validate"]["passthrough"], true);
}

#[test]
fn missing_promise_operation_returns_error_not_panic() {
    let mut spec: Value = serde_json::from_str(include_str!("../testdata/practice-openapi.json")).unwrap();
    spec["paths"]
        .as_object_mut()
        .unwrap()
        .remove("/create_promise");
    assert!(compile(&serde_json::from_value(plan()).unwrap(), &spec).is_err());
}

#[test]
fn json_transcripts_are_supported_and_verification_account_gates_remain() {
    check(
        "readback()\nvars['builder_verified']='no'\nrun('first')\nassert vars['builder_first_authorized']=='no'\nvars['builder_verified']='yes'\nreadback()\nvars['builder_account']='no'\nrun('first')\nassert vars['builder_first_authorized']=='no'\nvars['builder_account']='yes'\nrun('confirm')\nvars['system.conversation'].extend([dict(role='assistant',content=vars['builder_confirm_text']),dict(role='user',content='YES!')])\nvars['system.conversation']=json.dumps(vars['system.conversation'])\nrun('builder_confirm_grant')\nassert vars['builder_first_token']=='yes'\n",
    );
}
