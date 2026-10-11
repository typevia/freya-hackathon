use freya_builder::{compiler::check_python, remote::matching_model_cache};
use serde_json::json;

#[test]
fn cache_requires_exact_complete_request_and_records_original_evidence() {
    let messages = json!([{"role":"user","content":"document A"}]);
    let start = json!({"event":"model_start","at":1,"detail":{"model":"demo","purpose":"generate","messages":messages,"reasoning_effort":"low","max_completion_tokens":1000}});
    let end = json!({"event":"model_end","at":2,"detail":{"model":"demo","purpose":"generate","response":{"workflow":"unchanged"}}});
    let effort = Some("low".to_string());
    let events = json!([start, end]);
    let cached =
        matching_model_cache(&events, "demo", "generate", &messages, &effort, 1000).unwrap();
    assert_eq!(
        cached["original_result"]["detail"]["response"],
        json!({"workflow":"unchanged"})
    );
    assert!(matching_model_cache(&events, "other", "generate", &messages, &effort, 1000).is_none());
    assert!(
        matching_model_cache(
            &events,
            "demo",
            "generate",
            &json!([{"role":"user","content":"document B"}]),
            &effort,
            1000
        )
        .is_none()
    );
    assert!(matching_model_cache(&events, "demo", "generate", &messages, &None, 1000).is_none());
    assert!(matching_model_cache(&events, "demo", "generate", &messages, &effort, 2000).is_none());
    assert!(
        matching_model_cache(
            &json!([start]),
            "demo",
            "generate",
            &messages,
            &effort,
            1000
        )
        .is_none()
    );
    assert!(
        matching_model_cache(
            &json!([start,{"event":"model_error","detail":{"purpose":"generate"}},end]),
            "demo",
            "generate",
            &messages,
            &effort,
            1000
        )
        .is_none()
    );
}

#[test]
fn server_defaults_are_allowed_but_execution_changes_are_not() {
    let draft = json!({"initial_node":"n","nodes":{"n":{"name":"n","node_type":"conversation","transitions":[]}}});
    let mut snapshot = draft.clone();
    snapshot["nodes"]["n"]["max_turn_transition"] = serde_json::Value::Null;
    snapshot["nodes"]["n"]["assertions"] = json!([]);
    snapshot["agent_config"] = json!({"llmConfig":{"provider":"experimental-5","model":"experimental-5","apiKey":null},"routingLlmConfig":null});
    assert!(freya_builder::document_validation::validate_snapshot(&snapshot, &draft).is_ok());
    let mut changed = snapshot.clone();
    changed["nodes"]["n"]["name"] = json!("wrong");
    assert!(freya_builder::document_validation::validate_snapshot(&changed, &draft).is_err());
    let mut changed = snapshot.clone();
    changed["nodes"]["n"]["pre_process_code"] = json!("unexpected()");
    assert!(freya_builder::document_validation::validate_snapshot(&changed, &draft).is_err());
    snapshot["agent_config"]["tools"] =
        json!([{"name":"extra","handler":"__function__:api_call","parameters":{}}]);
    assert!(freya_builder::document_validation::validate_snapshot(&snapshot, &draft).is_err());
}

#[test]
fn pure_text_padding_unicode_and_timezone_methods_are_supported() {
    assert!(check_python("import datetime\ntry:\n    vars['text'] = 'A'.casefold().ljust(3).rstrip().rjust(4).zfill(5)\n    vars['offset'] = str(datetime.datetime.fromisoformat('2030-01-01T00:00:00+00:00').utcoffset())\nexcept Exception:\n    vars['text'] = ''\n").is_ok());
}
