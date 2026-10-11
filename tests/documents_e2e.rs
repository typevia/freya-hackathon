//! Offline protocol integration for generic document ingestion, not a live service or case fixture.
use axum::{
    Json, Router,
    body::to_bytes,
    extract::{Request, State},
    routing::any,
};
use freya_builder::{
    documents::{self, Options},
    remote::Config,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
#[derive(Default)]
struct Mock {
    requests: Vec<String>,
    model_inputs: Vec<Value>,
    personas: Vec<Value>,
    simulations: Vec<Value>,
    workflow: Value,
    version: usize,
    creates: usize,
    model_calls: usize,
    active_persona: String,
    mode: String,
}
fn analysis(empty: bool) -> Value {
    json!({"requirements":["contract.md: use the actual nested service response and obtain confirmation before effects"],"endpoints":[{"path":"/v2/item.lookup","method":"POST","source":"contract.md","request_schema":{"type":"object"},"response_schema":{"type":"object"},"effect":"read"}],"authority_notes":["notes.md corrects the draft, not the rejected suggestion"],"tests":if empty {json!([])} else {json!([{"name":"one","prompt":"Ask about the supplied item alpha.","variables":{"item_key":"alpha","nested":{"locale":"xx"}},"checks":["report only observed data"],"source":"example.md"},{"name":"two","prompt":"Decline the proposed action.","variables":{"item_key":"alpha"},"checks":["do not claim an unperformed action"],"source":"example.md"}])}})
}
fn workflow(regressed: bool) -> Value {
    json!({"initial_node":"start","system_prompt":if regressed {"regressed"}else{"initial"},"nodes":{"start":{"name":"start","node_type":"conversation","task_message":"Use supplied service facts.","transitions":[]}}})
}
async fn handler(
    State(shared): State<Arc<Mutex<Mock>>>,
    req: Request,
) -> (axum::http::StatusCode, Json<Value>) {
    let path = req.uri().path().to_owned();
    let dry = req.uri().query().is_some_and(|q| q.contains("dryRun=true"));
    let method = req.method().clone();
    let body: Value = serde_json::from_slice(&to_bytes(req.into_body(), 2_000_000).await.unwrap())
        .unwrap_or(json!({}));
    let mut m = shared.lock().unwrap();
    m.requests.push(path.clone());
    let result = match path.as_str() {
        "/v1/chat/completions" => {
            m.model_calls += 1;
            m.model_inputs.push(
                serde_json::from_str(body["messages"][1]["content"].as_str().unwrap()).unwrap(),
            );
            let result = if m.model_calls == 1 {
                analysis(m.mode == "empty")
            } else {
                json!({"workflow":workflow(m.model_calls > 2)})
            };
            json!({"choices":[{"message":{"content":result.to_string()}}]})
        }
        "/api/v2/workspaces" => json!({"workspaces":[{"id":"ws"}]}),
        "/api/v2/workflow" => {
            if !dry {
                m.creates += 1;
            }
            json!({"valid":true,"workflow":{"id":"workflow","agentId":"agent"}})
        }
        "/api/v2/workflow/workflow" => {
            if method == "PUT" {
                m.workflow = body["workflow"].clone();
                m.version += 1;
                json!({})
            } else {
                json!({"workflow":{"id":"workflow","agentId":"agent","updatedAt":"server-snapshot","workflow":m.workflow}})
            }
        }
        "/api/v2/versions/workflow/workflow" => {
            json!({"versions":[{"id":format!("v{}",m.version),"versionNumber":m.version,"isPublished":false}]})
        }
        "/api/v2/versions/workflow/workflow/publish" => json!({}),
        "/api/v2/personas" => {
            m.personas.push(body["personas"][0].clone());
            json!({"personas":[{"id":format!("persona{}",m.personas.len())}]})
        }
        "/api/v2/simulations" => {
            m.active_persona = body["selectedPersonaIds"][0].as_str().unwrap().into();
            m.simulations.push(body);
            json!({"simulation":{"id":"sim"}})
        }
        "/api/v2/simulations/sim" => json!({"simulation":{"status":"completed"}}),
        "/api/v2/simulations/sim/iterations" => json!({"iterations":[{"id":"iteration"}]}),
        "/api/v2/simulations/sim/iterations/iteration" => {
            let outcome = if m.mode == "inconclusive" {
                "inconclusive"
            } else if m.workflow["system_prompt"] == "initial" && m.active_persona == "persona1" {
                "pass"
            } else {
                "fail"
            };
            json!({"runsTotal":1,"runs":[{"personaId":m.active_persona,"callId":"call","outcome":outcome,"reason":"offline observed evidence"}]})
        }
        "/api/v2/call/call" => {
            json!({"call":{"transcript":"Full failure transcript with a distinctive diagnostic.","messages":[{"role":"system","message":{"error":"nested response unavailable","variable_table":{"large":"state"}}}]}})
        }
        _ => {
            return (
                axum::http::StatusCode::NOT_FOUND,
                Json(json!({"unexpected":path})),
            );
        }
    };
    (
        axum::http::StatusCode::OK,
        Json(json!({"data":{"result":result}})),
    )
}
async fn setup(
    mode: &str,
) -> (
    tempfile::TempDir,
    Options,
    Config,
    Arc<Mutex<Mock>>,
    tokio::task::JoinHandle<()>,
) {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("sources");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(
        source.join("contract.md"),
        "POST /v2/item.lookup. The request and response have nested envelopes.",
    )
    .unwrap();
    std::fs::write(
        source.join("notes.md"),
        "Later correction. A rejected suggestion is not policy.",
    )
    .unwrap();
    std::fs::write(source.join("example.md"), "Example caller asks about alpha. Initial variables: item_key=alpha; nested locale=xx. Another caller declines.").unwrap();
    let shared = Arc::new(Mutex::new(Mock {
        mode: mode.into(),
        ..Default::default()
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .fallback(any(handler))
        .with_state(shared.clone());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let config = Config {
        llm_base: format!("{base}/v1"),
        llm_key: "offline-model-key".into(),
        model: "offline-scripted-model".into(),
        freya_base: base.clone(),
        freya_key: "offline-platform-key".into(),
        bank_base: base,
        bank_key: String::new(),
    };
    let options = Options {
        case_folder: source,
        mock_url: "https://mock.example".into(),
        max_iterations: 2,
        budget_seconds: 120,
        reps: 1,
        tests_limit: None,
        compile_only: false,
    };
    (dir, options, config, shared, task)
}
#[tokio::test]
async fn generic_repairs_freeze_tests_restore_best_and_export_actual_snapshot_without_practice_calls()
 {
    let (dir, options, config, shared, task) = setup("").await;
    let out = dir.path().join("build");
    let summary = documents::build(options, config, out.clone(), dir.path().join("slot.json"))
        .await
        .unwrap();
    assert_eq!(summary["selected_iteration"], 1);
    assert_eq!(summary["passed"], 1);
    assert_eq!(summary["total"], 2);
    let m = shared.lock().unwrap();
    assert_eq!(m.creates, 1);
    assert_eq!(m.simulations.len(), 4);
    assert_eq!(m.workflow["system_prompt"], "initial");
    assert!(
        !m.requests
            .iter()
            .any(|p| p.contains("practice") || p.contains("ledger"))
    );
    assert_eq!(
        m.model_inputs[1]["analysis"]["tests"],
        m.model_inputs[2]["analysis"]["tests"]
    );
    assert!(
        m.model_inputs[2]["feedback"]
            .to_string()
            .contains("distinctive diagnostic")
    );
    assert!(
        m.model_inputs[2]["feedback"]
            .to_string()
            .contains("nested response unavailable")
    );
    assert!(
        !m.personas[0]["prompt"]
            .as_str()
            .unwrap()
            .contains("report only observed data")
    );
    assert!(
        m.personas[0]["successCriteria"]
            .as_str()
            .unwrap()
            .contains("report only observed data")
    );
    assert_eq!(
        m.simulations[0]["customerVariablesMap"]["persona1"]["nested"]["locale"],
        "xx"
    );
    for field in [
        "assistantLlmOverride",
        "routingLlmOverride",
        "extractionLlmOverride",
    ] {
        assert_eq!(m.simulations[0][field]["model"], "experimental-5");
    }
    let selected = freya_builder::read_json(&out.join("selected-workflow.json")).unwrap();
    assert_eq!(selected, m.workflow);
    assert_eq!(
        freya_builder::read_json(&out.join("selected-platform.json")).unwrap()["updated_at"],
        "server-snapshot"
    );
    assert!(
        !std::fs::read_to_string(out.join("trace.json"))
            .unwrap()
            .contains("offline-platform-key")
    );
    task.abort();
}
#[tokio::test]
async fn compile_only_limits_before_freeze_and_never_publishes_or_claims_a_score() {
    let (dir, mut options, config, shared, task) = setup("").await;
    options.compile_only = true;
    options.tests_limit = Some(1);
    let out = dir.path().join("build");
    let summary = documents::build(options, config, out.clone(), dir.path().join("slot.json"))
        .await
        .unwrap();
    assert_eq!(summary["status"], "compile_only_untested");
    assert_eq!(summary["evaluated"], false);
    assert!(summary.get("passed").is_none());
    assert!(out.join("selected-workflow.json").exists());
    assert!(!out.join("selected-platform.json").exists());
    let frozen = freya_builder::read_json(&out.join("frozen-suite.json")).unwrap();
    assert_eq!(frozen["available_tests"], 2);
    assert_eq!(frozen["tests"].as_array().unwrap().len(), 1);
    assert!(
        shared
            .lock()
            .unwrap()
            .requests
            .iter()
            .all(|p| p == "/v1/chat/completions")
    );
    task.abort();
}
#[tokio::test]
async fn no_grounded_tests_exports_explicitly_untested_without_simulations() {
    let (dir, options, config, shared, task) = setup("empty").await;
    let out = dir.path().join("build");
    let summary = documents::build(options, config, out.clone(), dir.path().join("slot.json"))
        .await
        .unwrap();
    assert_eq!(summary["status"], "exported_untested");
    assert_eq!(summary["evaluated"], false);
    assert!(summary.get("passed").is_none());
    assert!(out.join("selected-platform.json").exists());
    assert!(shared.lock().unwrap().simulations.is_empty());
    task.abort();
}
#[tokio::test]
async fn inconclusive_evaluations_retain_candidates_but_never_select_a_winner() {
    let (dir, mut options, config, shared, task) = setup("inconclusive").await;
    options.max_iterations = 1;
    options.tests_limit = Some(1);
    let out = dir.path().join("build");
    assert!(
        documents::build(options, config, out.clone(), dir.path().join("slot.json"))
            .await
            .is_err()
    );
    assert!(out.join("candidate-1-platform.json").exists());
    assert!(out.join("last-untested-platform.json").exists());
    assert!(!out.join("selected-platform.json").exists());
    assert_eq!(
        freya_builder::read_json(&out.join("summary.json")).unwrap()["status"],
        "error"
    );
    assert_eq!(shared.lock().unwrap().simulations.len(), 1);
    task.abort();
}
