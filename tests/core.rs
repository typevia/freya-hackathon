use freya_builder::{
    Redactor,
    builder::prefer,
    compiler::{Plan, check_python, compile},
    eval::{Evaluation, Variation, freeze, score},
};
use serde_json::{Value, json};
fn spec() -> Value {
    json!({"servers":[{"url":"https://bank.example/v1"}],"paths":{"/close_call":{"post":{"requestBody":{"content":{"application/json":{"schema":{"type":"object","properties":{"customer_id":{"type":"string"},"outcome":{"type":"string","enum":["done"]}},"required":["customer_id","outcome"]}}}}}}}})
}
fn plan() -> Value {
    json!({"system_prompt":"Follow policy","initial_node":"start","nodes":{"start":{"kind":"conversation","prompt":"Hello","fallback":"end","routes":[{"target":"end","when":{"kind":"always"}}]},"end":{"kind":"end_call","prompt":"Goodbye"}}})
}
#[test]
fn compiler_rejects_external_tools_graphs_and_model_fields() {
    let p: Plan = serde_json::from_value(plan()).unwrap();
    assert!(compile(&p, &spec()).is_ok());
    let mut v = plan();
    v["agent_config"] = json!({"model":"other"});
    assert!(serde_json::from_value::<Plan>(v).is_err());
    let mut v = plan();
    v["nodes"]["start"]["kind"] = json!("integration");
    assert!(compile(&serde_json::from_value(v).unwrap(), &spec()).is_err());
    let mut v = plan();
    v["nodes"]["start"]["fallback"] = json!("absent");
    assert!(compile(&serde_json::from_value(v).unwrap(), &spec()).is_err());
    for url in [
        "http://bank.example/v1",
        "https://bank.example/v1?key=secret",
        "https://user:secret@bank.example/v1",
        "https://bank.example/{{host}}",
    ] {
        let mut s = spec();
        s["servers"][0]["url"] = json!(url);
        if !url.contains("{{") {
            assert!(compile(&p, &s).is_err());
        }
    }
    let mut v = plan();
    v["nodes"]["start"] =
        json!({"kind":"api_request","operation":"shell_exec","success":"end","failure":"fail"});
    v["nodes"]["fail"] = json!({"kind":"end_call","prompt":"Failed"});
    assert!(compile(&serde_json::from_value(v).unwrap(), &spec()).is_err());
}
#[test]
fn embedded_code_is_parsed_not_executed() {
    assert!(check_python("import datetime\ntry:\n    vars['safe'] = str(datetime.date.fromisoformat('2026-10-14'))\nexcept Exception:\n    vars['safe'] = ''\n").is_ok());
    for code in [
        "import os\nos.system('echo bad')",
        "try:\n    vars['builder_verified'] = 'yes'\nexcept Exception:\n    pass",
        "try:\n    open('/tmp/bad','w')\nexcept Exception:\n    pass",
        "try:\n    x = vars\n    x['key'] = 'bad'\nexcept Exception:\n    pass",
        "try:\n    x = '{{hello}}'\nexcept Exception:\n    pass",
    ] {
        assert!(check_python(code).is_err());
    }
}
fn evaluation(passed: usize, total: usize, complete: bool) -> Evaluation {
    Evaluation {
        passed,
        total,
        complete,
        rows: vec![json!({}); total],
        reps: 1,
        run_total: total,
        run_passed: passed,
        suite_hash: "same-suite".into(),
    }
}
#[test]
fn selection_preserves_best_and_excludes_incomplete_and_incomparable() {
    let best = evaluation(2, 3, true);
    assert!(!prefer(&evaluation(1, 3, true), Some(&best)));
    assert!(!prefer(&evaluation(3, 3, false), Some(&best)));
    assert!(!prefer(&evaluation(4, 4, true), Some(&best)));
    assert!(prefer(&evaluation(3, 3, true), Some(&best)));
    assert!(prefer(&evaluation(0, 3, true), None));
    assert!(!prefer(&evaluation(0, 0, true), None));
}
#[test]
fn fixture_expectations_are_immutable_and_arbitrary_states_rejected() {
    let c = json!({"id":"hosted","script":"ask","checks":["check"],"expected":{"outcome":"done","promises":[],"notes":[],"callbacks":[],"transfers":[]}});
    let tests = freeze(
        std::slice::from_ref(&c),
        &[Variation {
            case_id: "hosted".into(),
            style: "pieces".into(),
        }],
        &["Follow the source policy".into()],
    )
    .unwrap();
    assert_eq!(tests.len(), 2);
    assert_eq!(tests[1].case, c);
    assert!(
        freeze(
            &[c],
            &[Variation {
                case_id: "invented".into(),
                style: "natural".into()
            }],
            &["Follow the source policy".into()]
        )
        .is_err()
    );
}
#[test]
fn ledger_checks_exact_writes_and_closing() {
    let c = json!({"expected":{"outcome":"done","reason":null,"promises":[],"notes":[],"callbacks":[],"transfers":[]}});
    let mut ledger = c["expected"].clone();
    ledger["calls"] = json!([{"tool":"close_call"}]);
    assert!(score(&c, &ledger).is_empty());
    ledger["promises"] = json!([{"amount":1,"product_id":"p","date":"2026-01-01"}]);
    assert!(!score(&c, &ledger).is_empty());
    ledger["calls"] = json!([]);
    assert!(score(&c, &ledger).iter().any(|s| s.contains("close_call")));
}
#[test]
fn redaction_covers_nested_fields_and_embedded_capabilities() {
    let r = Redactor(vec!["secret-value".into(), "session-opaque".into()]);
    let v=r.value(&json!({"customer_id":"session-opaque","messages":["key=secret-value and session-opaque"],"Authorization":"Bearer secret-value"}));
    let text = v.to_string();
    assert!(!text.contains("secret-value"));
    assert!(!text.contains("session-opaque"));
    assert!(text.contains("REDACTED"));
}

#[test]
fn process_slot_lock_is_exclusive_and_released() {
    let dir = tempfile::tempdir().unwrap();
    let first = freya_builder::service::BuildLock::acquire(dir.path()).unwrap();
    assert!(freya_builder::service::BuildLock::acquire(dir.path()).is_err());
    drop(first);
    assert!(freya_builder::service::BuildLock::acquire(dir.path()).is_ok());
}

#[test]
fn artifact_redaction_preserves_bank_schema_and_template_defaults() {
    let r = Redactor(vec!["secret".into()]);
    let v = json!({"body":{"properties":{"customer_id":{"type":"string","default":"{{customer_id}}"}}},"apiKey":null,"headers":{"Authorization":"Bearer secret"}});
    let redacted = r.value(&v);
    assert_eq!(redacted["body"], v["body"]);
    assert!(redacted["apiKey"].is_null());
    assert!(!redacted.to_string().contains("Bearer secret"));
}

#[test]
fn promise_guards_and_confirmation_provenance_are_compiler_owned() {
    let spec: Value =
        serde_json::from_str(include_str!("../testdata/practice-openapi.json")).unwrap();
    let mut plan = json!({"system_prompt":"synthetic test", "initial_node":"collect","nodes":{
        "collect":{"kind":"conversation","prompt":"Confirm the terms","extraction":[{"name":"confirmed","description":"Explicit user confirmation","values":["yes","no"]}],"routes":[{"target":"validate","when":{"kind":"always"}}],"fallback":"failed"},
        "validate":{"kind":"code","code":"try:\n    vars['valid_payment'] = 'yes'\nexcept Exception:\n    vars['valid_payment'] = 'no'\n","fallback":"write"},
        "write":{"kind":"api_request","operation":"create_promise","arguments":{"product_id":"p1","amount":1,"date":"2026-10-14"},"validation_flag":"valid_payment","confirmation_flag":"confirmed","success":"end","failure":"failed"},
        "end":{"kind":"end_call","prompt":"End"},"failed":{"kind":"end_call","prompt":"Failed"}
    }});
    let error = compile(&serde_json::from_value(plan.clone()).unwrap(), &spec).unwrap_err();
    assert!(error.to_string().contains("confirmation_flag is unsafe"));
    plan["nodes"]["write"]
        .as_object_mut()
        .unwrap()
        .remove("confirmation_flag");
    assert!(
        compile(&serde_json::from_value(plan).unwrap(), &spec)
            .unwrap_err()
            .to_string()
            .contains("dedicated confirmation")
    );
}

#[test]
fn reliable_case_scoring_requires_all_repetitions_and_same_suite() {
    use freya_builder::eval::{Test, summarize};
    let tests = vec![
        Test {
            case: json!({"id":"fixture"}),
            style: "natural".into(),
            requirements: vec!["Follow the source policy".into()],
        },
        Test {
            case: json!({"id":"fixture"}),
            style: "pieces".into(),
            requirements: vec!["Follow the source policy".into()],
        },
    ];
    let rows_a: Vec<_> = (0..2)
        .flat_map(|index| {
            (0..3)
                .map(move |rep| json!({"test_index":index,"rep":rep,"evaluated":true,"pass":rep<2}))
        })
        .collect();
    let rows_b: Vec<_> = (0..2)
        .flat_map(|index| {
            (0..3).map(
                move |rep| json!({"test_index":index,"rep":rep,"evaluated":true,"pass":index==0}),
            )
        })
        .collect();
    let a = summarize(&tests, 3, rows_a);
    let b = summarize(&tests, 3, rows_b);
    assert_eq!((a.passed, a.run_passed, a.total, a.run_total), (0, 4, 2, 6));
    assert_eq!((b.passed, b.run_passed), (1, 3));
    assert!(prefer(&b, Some(&a)));
    let mut other = b.clone();
    other.suite_hash = "different".into();
    assert!(!prefer(&other, Some(&a)));
    let mut partial = b.rows.clone();
    partial.pop();
    assert!(!summarize(&tests, 3, partial).complete);
}

#[test]
fn customer_capability_is_reserved_in_every_generated_write_channel() {
    let mut extraction = plan();
    extraction["nodes"]["start"]["extraction"] =
        json!([{"name":"customer_id","description":"overwrite"}]);
    assert!(compile(&serde_json::from_value(extraction).unwrap(), &spec()).is_err());
    let mut mapping = plan();
    mapping["nodes"]["start"] = json!({"kind":"api_request","operation":"close_call","arguments":{"outcome":"done"},"mappings":{"customer_id":"data.id"},"success":"end","failure":"failed"});
    mapping["nodes"]["failed"] = json!({"kind":"end_call"});
    assert!(compile(&serde_json::from_value(mapping).unwrap(), &spec()).is_err());
    for field in ["validation_flag", "confirmation_flag"] {
        let mut v = plan();
        v["nodes"]["start"][field] = json!("customer_id");
        assert!(compile(&serde_json::from_value(v).unwrap(), &spec()).is_err());
    }
    assert!(
        check_python("try:\n    vars['customer_id'] = 'another'\nexcept Exception:\n    pass\n")
            .is_err()
    );
}

#[test]
fn unreached_ledger_rejects_any_identity_or_account_use() {
    for outcome in ["not_reached", "no_contact", "unreached"] {
        let case = json!({"expected":{"outcome":outcome,"promises":[],"notes":[],"callbacks":[],"transfers":[]}});
        for tool in ["verify_identity", "get_account"] {
            let mut ledger = case["expected"].clone();
            ledger["calls"] = json!([{"tool":tool,"result":{"ok":true}},{"tool":"close_call"}]);
            assert!(
                score(&case, &ledger)
                    .iter()
                    .any(|s| s.contains("not on the line"))
            );
        }
    }
}

#[test]
fn terminal_shape_is_canonical_for_custom_and_silent_endings() {
    for farewell in ["Goodbye", ""] {
        let mut p = plan();
        p["nodes"]["end"]["prompt"] = json!(farewell);
        let w = compile(&serde_json::from_value(p).unwrap(), &spec()).unwrap();
        let end = &w["nodes"]["end"];
        assert_eq!(end["on_enter"], json!({"type":"silent"}));
        assert_eq!(end["disable_inactivity_monitor"], true);
        assert_eq!(end["transitions"], json!([]));
        let props = &end["terminal"]["function"]["parameters"]["properties"];
        assert_eq!(
            props["message_mode"]["default"],
            if farewell.is_empty() {
                "none"
            } else {
                "custom"
            }
        );
        if farewell.is_empty() {
            assert!(props.get("spoken_message").is_none())
        } else {
            assert_eq!(props["spoken_message"]["default"], farewell)
        }
    }
}

#[test]
fn compiler_contains_errors_after_model_try_and_blocks_all_success_routes() {
    let mut p = plan();
    p["nodes"]["start"] = json!({"kind":"code","code":"try:\n    vars['payment_valid'] = 'yes'\nexcept Exception:\n    pass\nvalue = 1 / 0\n","fallback":"end"});
    let w = compile(&serde_json::from_value(p).unwrap(), &spec()).unwrap();
    let n = &w["nodes"]["start"];
    let code = n["pre_actions"][0]["function"]["parameters"]["properties"]["code"]["default"]
        .as_str()
        .unwrap();
    assert!(code.starts_with("vars['builder_start_executed'] = 'no'\ntry:\n    try:"));
    assert!(code.contains(
        "    value = 1 / 0\n    vars['builder_start_executed'] = 'yes'\nexcept Exception:"
    ));
    assert!(code.contains("vars['builder_code_healthy'] = 'no'"));
    assert!(code.contains("vars['builder_verified'] = 'no'"));
    assert_eq!(n["passthrough_default_transition"], "builder_code_failure");
    assert!(
        n["transitions"][0]["condition"]
            .to_string()
            .contains("builder_start_executed")
    );
    assert_eq!(
        n["transitions"].as_array().unwrap().last().unwrap()["target_node"],
        "builder_code_failure"
    );
    assert_eq!(w["nodes"]["builder_code_failure"]["node_type"], "end_call");
    // Only structure is inspected: generated Python is never executed on this host.
}

#[test]
fn code_fallback_cannot_merge_into_a_guarded_target() {
    let mut p = plan();
    p["nodes"]["start"] = json!({"kind":"code","code":"try:\n    vars['ok'] = 'no'\nexcept Exception:\n    pass\n","fallback":"end","routes":[{"target":"end","when":{"kind":"equals","variable":"ok","value":"yes"}}]});
    let e = compile(&serde_json::from_value(p).unwrap(), &spec()).unwrap_err();
    assert!(e.to_string().contains("fallback duplicates"));
}

#[test]
fn template_lint_rejects_document_notation_without_changing_program_text() {
    use freya_builder::compiler::lint_templates;
    assert!(
        lint_templates("Hello {title} {last_name}")
            .unwrap_err()
            .to_string()
            .contains("double braces")
    );
    assert!(lint_templates("Hello {{title}} {{last_name}}; {{system.iso_utc_now}}").is_ok());
    assert!(lint_templates("JSON {\"field\": 1} is not a variable").is_ok());
    let mut p = plan();
    p["nodes"]["start"]["prompt"] = json!("Ask {contact_name}");
    assert!(compile(&serde_json::from_value(p).unwrap(), &spec()).is_err());
}

#[test]
fn executable_artifacts_are_exact_or_rejected_not_redacted() {
    use freya_builder::remote::{Config, Remote};
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        llm_base: "https://model.example/v1".into(),
        llm_key: "mock-secret-key".into(),
        model: "mock".into(),
        freya_base: "https://freya.example".into(),
        freya_key: "mock-freya-key".into(),
        bank_base: "https://bank.example".into(),
        bank_key: "mock-bank-key-16plus".into(),
    };
    let mut remote = Remote::new(config, dir.path().into(), 30).unwrap();
    remote.redactor.0.push("opaque-session-capability".into());
    let safe = json!({"workflow":{"agent_config":{"llmConfig":{"apiKey":null}},"properties":{"customer_id":{"type":"string","default":"{{customer_id}}"}},"secret":"public policy label"}});
    let digest = remote.executable_artifact("safe.json", &safe).unwrap();
    assert_eq!(
        freya_builder::read_json(&dir.path().join("safe.json")).unwrap(),
        safe
    );
    assert_eq!(
        digest,
        freya_builder::file_hash(&dir.path().join("safe.json")).unwrap()
    );
    for bad in [
        json!({"code":"mock-secret-key"}),
        json!({"prompt":"opaque-session-capability"}),
        json!({"apiKey":"unknown-key"}),
        json!({"customer_id":"another-session"}),
    ] {
        assert!(remote.executable_artifact("bad.json", &bad).is_err());
        assert!(!dir.path().join("bad.json").exists());
    }
}

#[test]
fn iteration_limit_is_configurable_above_default_with_a_safe_upper_bound() {
    let mut input: freya_builder::BuildInput =
        serde_json::from_value(json!({"policies":["policy"],"openapi":{},"cases":[{}]})).unwrap();
    assert_eq!(input.max_iterations, 5);
    for count in [6, 20] {
        input.max_iterations = count;
        assert!(input.validate().is_ok());
    }
    for count in [0, 21] {
        input.max_iterations = count;
        assert!(input.validate().is_err());
    }
}

#[test]
fn syntax_checker_accepts_safe_validation_exceptions_but_never_executes_them() {
    assert!(check_python("raise ValueError('invalid fixture data')\n").is_ok());
    assert!(check_python("return 1\n").is_err());
    assert!(check_python("break\n").is_err());
}

#[test]
fn composite_response_mappings_are_scalar_json_and_reset_before_each_call() {
    let mut p = plan();
    p["nodes"]["start"] = json!({"kind":"api_request","operation":"close_call","arguments":{"outcome":"done"},"mappings":{"payload":"data","items":"data.products","item":"data.products.0","ok":"data.ok"},"success":"end","failure":"failed"});
    p["nodes"]["failed"] = json!({"kind":"end_call"});
    let w = compile(&serde_json::from_value(p).unwrap(), &spec()).unwrap();
    let action = &w["nodes"]["builder_start_action"];
    for key in ["payload", "items", "item", "ok"] {
        assert!(
            action["response_mapping"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["variable"] == key && m["path"] == format!("data.builder_map_{key}"))
        );
        assert!(
            action["pre_actions"][0]["pre_process_code"]
                .as_str()
                .unwrap()
                .contains(&format!("vars['{key}'] = ''"))
        );
    }
    let post = action["pre_actions"][0]["post_process_code"]
        .as_str()
        .unwrap();
    assert!(post.contains("source_data = dict(d)"));
    assert!(post.contains("json.dumps(value) if isinstance(value, (dict, list))"));
    assert!(post.contains("part.isdigit()"));
}
