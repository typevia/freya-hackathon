//! Invented, domain-neutral contract shapes; no competition case input is used.
use freya_builder::{
    compiler::check_generic_python,
    document_validation::{self, validate},
    documents::{self, Endpoint},
    remote::{Config, Remote},
};
use serde_json::{Value, json};

fn endpoint() -> Endpoint {
    Endpoint {
        path: "/v2/item.lookup".into(),
        method: "POST".into(),
        source: "contract.md".into(),
        request_schema: json!({"type":"object","properties":{"request":{"type":"object","properties":{"key":{"type":"string"}}}}}),
        response_schema: json!({"type":"object","properties":{"payload":{"type":"object"}}}),
        effect: "read".into(),
    }
}
fn workflow() -> Value {
    json!({"initial_node":"start","system_prompt":"Use only supplied facts.","nodes":{"start":{"name":"start","node_type":"conversation","transitions":[],"tools":[{"type":"function","function":{"name":"item.lookup","handler":"__function__:api_call","parameters":{"type":"object","properties":{"url":{"type":"string","default":"https://mock.example/v2/item.lookup"},"method":{"type":"string","default":"POST"},"headers":{"type":"object","properties":{"Content-Type":{"type":"string","default":"application/json"}}},"body":{"type":"object","properties":{"request":{"type":"object","properties":{"key":{"type":"string","default":"{{item_key}}"}}}}}}}},"pre_process_code":"config['body'] = {'request': {'key': vars.get('item_key')}}","post_process_code":"import json\nif isinstance(result.get('data'), dict):\n    result['data']['payload_json'] = json.dumps(result['data'].get('payload', {}))\nresult.pop('header', None)","response_mapping":[{"path":"data.payload_json","variable":"payload_json"}]}]}}})
}
#[test]
fn nested_bodies_dotted_endpoints_and_custom_envelopes_are_supported() {
    validate(&workflow(), &[endpoint()], "https://mock.example").unwrap();
    let mut engine = workflow();
    let tools = engine["nodes"]["start"]
        .as_object_mut()
        .unwrap()
        .remove("tools")
        .unwrap();
    engine["nodes"]["start"]["node_type"] = json!("api_request");
    engine["nodes"]["start"]["pre_actions"] = tools;
    engine["nodes"]["start"]["response_mapping"] =
        engine["nodes"]["start"]["pre_actions"][0]["response_mapping"].clone();
    engine["nodes"]["start"]["pre_actions"][0]
        .as_object_mut()
        .unwrap()
        .remove("response_mapping");
    validate(&engine, &[endpoint()], "https://mock.example").unwrap();
}
#[test]
fn transport_normalization_bakes_user_agent_without_changing_nested_bodies_or_headers() {
    let mut workflow = workflow();
    let body = workflow
        .pointer("/nodes/start/tools/0/function/parameters/properties/body")
        .unwrap()
        .clone();
    document_validation::normalize_transport(&mut workflow).unwrap();
    let headers = workflow
        .pointer("/nodes/start/tools/0/function/parameters/properties/headers/properties")
        .unwrap();
    assert_eq!(
        headers["User-Agent"]["default"],
        "Mozilla/5.0 (Freya workflow)"
    );
    assert_eq!(headers["Content-Type"]["default"], "application/json");
    assert_eq!(
        workflow.pointer("/nodes/start/tools/0/function/parameters/properties/body"),
        Some(&body)
    );
    validate(&workflow, &[endpoint()], "https://mock.example").unwrap();
    let normalized = workflow.clone();
    document_validation::normalize_transport(&mut workflow).unwrap();
    assert_eq!(workflow, normalized);
}
#[test]
fn external_urls_unknown_methods_handlers_and_dynamic_headers_fail_closed() {
    for (pointer, replacement) in [
        (
            "/nodes/start/tools/0/function/parameters/properties/url/default",
            json!("https://elsewhere.example/v2/item.lookup"),
        ),
        (
            "/nodes/start/tools/0/function/parameters/properties/url/default",
            json!("https://mock.example/v2/{{operation}}"),
        ),
        (
            "/nodes/start/tools/0/function/parameters/properties/method/default",
            json!("DELETE"),
        ),
        (
            "/nodes/start/tools/0/function/handler",
            json!("__function__:shell"),
        ),
        (
            "/nodes/start/tools/0/function/parameters/properties/headers",
            json!({"type":"object","properties":{"Authorization":{"type":"string","default":"{{credential}}"}}}),
        ),
    ] {
        let mut bad = workflow();
        *bad.pointer_mut(pointer).unwrap() = replacement;
        assert!(
            validate(&bad, &[endpoint()], "https://mock.example").is_err(),
            "{pointer}"
        );
    }
    for base in [
        "http://mock.example",
        "https://user:pass@mock.example",
        "https://mock.example/?token=x",
    ] {
        assert!(document_validation::mock_base(base).is_err());
    }
    for path in ["//evil.example", "/a/../b", "/a/%2e/b", "/a?target=x"] {
        assert!(document_validation::endpoint_path(path).is_err());
    }
}
#[test]
fn generic_hooks_cannot_change_pinned_config_or_execute_host_code() {
    for code in [
        "config['url'] = 'https://elsewhere.example'",
        "config['method'] = 'DELETE'",
        "config['headers'] = {}",
        "alias = config\nalias['url'] = 'x'",
        "config.update({'url': 'x'})",
        "config = {'body': {}}",
        "import os\nos.system('echo unsafe')",
        "open('/tmp/generic-builder-must-not-execute', 'w')",
        "exec('pass')",
        "vars.get('secret')",
        "vars['authorization'] = 'x'",
    ] {
        assert!(check_generic_python(code, "pre").is_err(), "{code}");
    }
    assert!(!std::path::Path::new("/tmp/generic-builder-must-not-execute").exists());
    check_generic_python("import json\nconfig['body'] = {'request': {'nested': json.loads(vars.get('request_json', '{}'))}}", "pre").unwrap();
    check_generic_python(
        "vars['customer_id'] = str(vars.get('customer_id', ''))",
        "code",
    )
    .unwrap();
    assert!(check_generic_python("result['data'] = {}", "code").is_err());
    assert!(check_generic_python("config['body'] = {}", "post").is_err());
}
#[test]
fn missing_scalar_serialization_and_bad_mapping_paths_are_rejected() {
    let mut bad = workflow();
    bad["nodes"]["start"]["tools"][0]["post_process_code"] = json!("result.pop('header', None)");
    assert!(validate(&bad, &[endpoint()], "https://mock.example").is_err());
    let mut bad = workflow();
    bad["nodes"]["start"]["tools"][0]["response_mapping"][0]["path"] = json!("payload");
    assert!(validate(&bad, &[endpoint()], "https://mock.example").is_err());
}
#[test]
fn folder_collects_named_changes_companions_and_bounded_data_without_policy_truncation() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("notes")).unwrap();
    std::fs::write(
        dir.path().join("contract.md"),
        "POST /v2/item.lookup nested request envelope",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("notes/change.eml"),
        "An authorized later correction, supplied as source data.",
    )
    .unwrap();
    std::fs::write(dir.path().join("manual.pdf"), b"not parsed on builder host").unwrap();
    std::fs::write(
        dir.path().join("manual.transcript.txt"),
        "Entire extracted policy text.",
    )
    .unwrap();
    std::fs::write(dir.path().join(".env"), "DO_NOT_INCLUDE=secret").unwrap();
    let data: Vec<_> = (0..10000)
        .map(|i| json!({"item_key":format!("record-{i}"),"quantity":i}))
        .collect();
    std::fs::write(
        dir.path().join("records.json"),
        serde_json::to_vec(&data).unwrap(),
    )
    .unwrap();
    let folder = documents::read_folder(dir.path()).unwrap();
    assert_eq!(folder.documents.len(), 4);
    assert!(
        folder
            .documents
            .iter()
            .any(|d| d.name == "notes/change.eml")
    );
    let data = folder
        .documents
        .iter()
        .find(|d| d.name == "records.json")
        .unwrap();
    assert_eq!(data.content["count"], 10000);
    assert_eq!(data.content["samples"].as_array().unwrap().len(), 3);
    assert!(serde_json::to_string(&folder).unwrap().len() < 10000);
    assert!(
        !serde_json::to_string(&folder)
            .unwrap()
            .contains("DO_NOT_INCLUDE")
    );
    std::fs::write(dir.path().join("too-large.txt"), "x".repeat(400001)).unwrap();
    assert!(documents::read_folder(dir.path()).is_err());
}
#[test]
fn pdf_without_companion_and_symlink_are_explicit_errors() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("missing.pdf"), b"policy").unwrap();
    assert!(documents::read_folder(dir.path()).is_err());
    #[cfg(unix)]
    {
        std::fs::remove_file(dir.path().join("missing.pdf")).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", dir.path().join("unsafe.txt")).unwrap();
        assert!(documents::read_folder(dir.path()).is_err());
    }
}
#[test]
fn generic_artifact_boundary_has_no_practice_identifier_assumption() {
    let redactor = freya_builder::Redactor(vec!["invented-test-secret".into()]);
    freya_builder::remote::ensure_document_executable_safe(
        &json!({"customer_id":"ordinary-source-fact"}),
        &redactor,
    )
    .unwrap();
    assert!(
        freya_builder::remote::ensure_executable_safe(
            &json!({"customer_id":"ordinary-source-fact"}),
            &redactor
        )
        .is_err()
    );
    for value in [
        json!({"prompt":"invented-test-secret"}),
        json!({"api_key":"not-allowed"}),
    ] {
        assert!(freya_builder::remote::ensure_document_executable_safe(&value, &redactor).is_err());
    }
    let config = Config {
        llm_base: "https://model.example/v1".into(),
        llm_key: String::new(),
        model: "unchanged-config-model".into(),
        freya_base: "https://platform.example".into(),
        freya_key: "test-platform-secret".into(),
        bank_base: "https://mock.example".into(),
        bank_key: String::new(),
    };
    let dir = tempfile::tempdir().unwrap();
    assert!(Remote::new_document(config.clone(), dir.path().into(), 30).is_ok());
    assert!(Remote::new(config, dir.path().into(), 30).is_err());
}
