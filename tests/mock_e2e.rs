//! Offline integration test
use axum::{
    Json, Router,
    body::to_bytes,
    extract::{Request, State},
    routing::any,
};
use freya_builder::{
    BuildInput, builder,
    remote::{Config, Remote},
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
#[derive(Default)]
struct Mock {
    model_calls: usize,
    model_inputs: Vec<Value>,
    persona_specs: Vec<Value>,
    creation_attempts: usize,
    version: usize,
    current: Value,
    sessions: usize,
    personas: usize,
    persona: String,
    sim: usize,
    mode: String,
}
type Shared = Arc<Mutex<Mock>>;
fn spec() -> Value {
    json!({"servers":[{"url":"https://bank.example/v1"}],"paths":{"/close_call":{"post":{"requestBody":{"content":{"application/json":{"schema":{"type":"object","properties":{"customer_id":{"type":"string"},"outcome":{"type":"string","enum":["done"]}},"required":["customer_id","outcome"]}}}}}}}})
}
fn plan(repaired: bool) -> Value {
    let mut p = json!({"system_prompt":"Mock policy", "initial_node":"start","nodes":{"start":{"kind":"conversation","prompt":"Hello","fallback":"end","routes":[{"target":"end","when":{"kind":"always"}}]},"end":{"kind":"end_call","prompt":"Goodbye"}}});
    if repaired {
        p["nodes"]["start"]["routes"][0]["target"] = json!("check");
        p["nodes"]["check"] = json!({"kind":"code","code":"try:\n    vars['checked'] = 'yes'\nexcept Exception:\n    vars['checked'] = 'no'\n","fallback":"close","routes":[]});
        p["nodes"]["close"] = json!({"kind":"api_request","operation":"close_call","arguments":{"outcome":"done"},"success":"end","failure":"failed"});
        p["nodes"]["failed"] = json!({"kind":"end_call","prompt":"Sorry"});
    }
    json!({"diagnosis":if repaired{"Add missing embedded check and bank close action"}else{"initial/regressed candidate"},"plan":p})
}
async fn handler(
    State(shared): State<Shared>,
    request: Request,
) -> (axum::http::StatusCode, Json<Value>) {
    let path = request.uri().path().to_owned();
    let dry_run = request
        .uri()
        .query()
        .is_some_and(|q| q.contains("dryRun=true"));
    let method = request.method().clone();
    let bytes = to_bytes(request.into_body(), 1_000_000).await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(json!({}));
    let mode = shared.lock().unwrap().mode.clone();
    if mode.starts_with("slow") && path == "/v1/chat/completions" {
        tokio::time::sleep(std::time::Duration::from_millis(
            if mode == "slow_timeout" { 2000 } else { 250 },
        ))
        .await;
    }
    let mut m = shared.lock().unwrap();
    let v = if path == "/v1/chat/completions" {
        m.model_calls += 1;
        let model_input: Value =
            serde_json::from_str(body["messages"][1]["content"].as_str().unwrap()).unwrap();
        m.model_inputs.push(model_input.clone());
        if m.mode.contains("model_failure") {
            return (
                axum::http::StatusCode::BAD_GATEWAY,
                Json(json!({"secret":"mock-key"})),
            );
        }
        let content = if m.model_calls == 1 {
            json!({"requirements":model_input["policies"],"variations":[{"case_id":"fixture","style":"pieces"}]})
        } else {
            plan(m.model_calls == 3)
        };
        json!({"choices":[{"message":{"content":content.to_string()}}]})
    } else if path == "/api/v2/workspaces" {
        json!({"workspaces":[{"id":"ws"}]})
    } else if path == "/api/v2/workflow" {
        if !dry_run {
            m.creation_attempts += 1;
        }
        if m.mode == "freya_failure" || (m.mode == "create_failure" && !dry_run) {
            return (axum::http::StatusCode::SERVICE_UNAVAILABLE, Json(json!({})));
        }
        json!({"workflow":{"id":"workflow","agentId":"agent"},"valid":true})
    } else if path == "/api/v2/workflow/workflow" {
        if method == "PUT" {
            m.current = body["workflow"].clone();
            m.version += 1;
            json!({})
        } else {
            json!({"workflow":{"id":"workflow","agentId":"agent","workflow":m.current,"updatedAt":"mock-time"}})
        }
    } else if path == "/api/v2/versions/workflow/workflow" {
        json!({"versions":[{"id":format!("v{}",m.version),"versionNumber":m.version,"isPublished":m.mode == "evaluate_existing"}]})
    } else if path.ends_with("/publish") {
        json!({})
    } else if path == "/v1/practice/sessions" {
        m.sessions += 1;
        json!({"variables":{"customer_id":format!("opaque-session-{}",m.sessions),"now":"2026-10-14T11:00"}})
    } else if path.starts_with("/v1/practice/identity/") {
        json!({"identity":{"answer":"synthetic"}})
    } else if path == "/api/v2/personas" {
        m.personas += 1;
        m.persona_specs
            .extend(body["personas"].as_array().unwrap().clone());
        json!({"personas":[{"id":format!("persona{}",m.personas)}]})
    } else if path == "/api/v2/simulations" {
        m.sim += 1;
        m.persona = body["selectedPersonaIds"][0].as_str().unwrap().into();
        json!({"simulation":{"id":format!("sim{}",m.sim)}})
    } else if path.ends_with("/iterations") {
        json!({"iterations":[{"id":"iteration"}]})
    } else if path.contains("/iterations/iteration") {
        json!({"runsTotal":1,"runs":[{"personaId":m.persona,"callId":"call","outcome":if m.mode == "inconclusive" {"inconclusive"} else {"pass"},"reason":"mock transcript check"}]})
    } else if path.starts_with("/api/v2/simulations/") {
        json!({"simulation":{"status":"completed"}})
    } else if path == "/api/v2/call/call" {
        json!({"call":{"messages":[{"role":"assistant","content":"Mock transcript"}]}})
    } else if path.starts_with("/v1/practice/ledger/") {
        let pass = m.current["nodes"]["close"].is_object() && m.sessions % 2 == 1;
        json!({"outcome":if pass{"done"}else{"wrong"},"reason":null,"promises":[],"notes":[],"callbacks":[],"transfers":[],"calls":[{"tool":"close_call","customer_id":format!("opaque-session-{}",m.sessions)}]})
    } else {
        return (
            axum::http::StatusCode::NOT_FOUND,
            Json(json!({"path":path})),
        );
    };
    (
        axum::http::StatusCode::OK,
        Json(json!({"data":{"result":v}})),
    )
}
async fn server(mode: &str) -> (Config, Shared, tokio::task::JoinHandle<()>) {
    let shared = Arc::new(Mutex::new(Mock {
        mode: mode.into(),
        ..Default::default()
    }));
    let app = Router::new()
        .fallback(any(handler))
        .with_state(shared.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (
        Config {
            llm_base: format!("{base}/v1"),
            llm_key: "mock-key".into(),
            model: "scripted-mock-not-real-model".into(),
            freya_base: base.clone(),
            freya_key: "mock-key".into(),
            bank_base: base,
            bank_key: "mock-team-key-16plus".into(),
        },
        shared,
        task,
    )
}
fn input() -> BuildInput {
    BuildInput {
        policies: vec!["Close with outcome done".into()],
        openapi: spec(),
        cases: vec![
            json!({"id":"fixture","identity":{"answer":"synthetic"},"script":"Say hello","checks":["close correctly"],"expected":{"outcome":"done","promises":[],"notes":[],"callbacks":[],"transfers":[]}}),
        ],
        max_iterations: 3,
        budget_seconds: 120,
        reps: 1,
    }
}
#[tokio::test]
async fn offline_repair_regression_restore_and_export() {
    let (config, shared, task) = server("").await;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("build");
    let summary = builder::build(input(), config, out.clone(), dir.path().join("slot.json"))
        .await
        .unwrap();
    assert_eq!(summary["selected_iteration"], 2);
    assert_eq!(summary["passed"], 1);
    assert_eq!(summary["total"], 2);
    assert_eq!(shared.lock().unwrap().model_calls, 4);
    assert!(shared.lock().unwrap().current["nodes"]["close"].is_object());
    assert_eq!(
        summary["platform_snapshot_hash"],
        freya_builder::file_hash(&out.join("selected-platform.json")).unwrap()
    );
    let export = std::fs::read_to_string(out.join("selected-platform.json")).unwrap();
    assert!(export.contains("close_call"));
    assert!(export.contains("experimental-5"));
    let trace = std::fs::read_to_string(out.join("trace.json")).unwrap();
    assert!(trace.contains("build_end"));
    assert!(!trace.contains("mock-key"));
    assert!(!trace.contains("opaque-session"));
    assert!(out.join("candidate-3-evaluation.json").exists());
    // Optional durable evidence path, explicitly labelled mock.
    if let Ok(path) = std::env::var("BUILDER_MOCK_ARTIFACTS") {
        std::fs::create_dir_all(&path).unwrap();
        for e in std::fs::read_dir(out).unwrap() {
            let e = e.unwrap();
            std::fs::copy(e.path(), std::path::Path::new(&path).join(e.file_name())).unwrap();
        }
    }
    task.abort();
}
#[tokio::test]
async fn model_failures_are_bounded_and_never_selected() {
    let (config, shared, task) = server("model_failure").await;
    let dir = tempfile::tempdir().unwrap();
    assert!(
        builder::build(
            input(),
            config,
            dir.path().join("build"),
            dir.path().join("slot.json")
        )
        .await
        .is_err()
    );
    assert_eq!(shared.lock().unwrap().model_calls, 2);
    assert!(!dir.path().join("build/selected-platform.json").exists());
    task.abort();
}
#[tokio::test]
async fn failed_freya_calls_consume_bounded_iterations_not_passes() {
    let (config, shared, task) = server("freya_failure").await;
    let dir = tempfile::tempdir().unwrap();
    assert!(
        builder::build(
            input(),
            config,
            dir.path().join("build"),
            dir.path().join("slot.json")
        )
        .await
        .is_err()
    );
    assert_eq!(shared.lock().unwrap().model_calls, 4);
    assert_eq!(shared.lock().unwrap().sessions, 0);
    task.abort();
}
#[tokio::test]
async fn expired_deadline_prevents_remote_calls() {
    let (config, shared, task) = server("").await;
    let dir = tempfile::tempdir().unwrap();
    let mut remote = Remote::new(config, dir.path().into(), 0).unwrap();
    assert!(
        remote
            .model("expired", "Return JSON", &json!({}))
            .await
            .is_err()
    );
    assert_eq!(shared.lock().unwrap().model_calls, 0);
    task.abort();
}

#[tokio::test]
async fn http_service_serializes_and_rejects_paths_and_bad_json() {
    let (config, _, mock_task) = server("slow_model_failure").await;
    let dir = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let root = dir.path().to_path_buf();
    let service_task = tokio::spawn(async move {
        freya_builder::service::serve(config, root, addr)
            .await
            .unwrap();
    });
    let client = reqwest::Client::new();
    let base = format!("http://{addr}");
    for _ in 0..100 {
        if client.get(format!("{base}/status")).send().await.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let response = client
        .post(format!("{base}/builds"))
        .json(&input())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 202);
    let response = client
        .post(format!("{base}/builds"))
        .json(&input())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 409);
    let response = client
        .post(format!("{base}/builds"))
        .json(&json!({"policy_path":"/etc/passwd"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    assert!(response.json::<Value>().await.unwrap()["error"].is_string());
    for _ in 0..100 {
        let status: Value = client
            .get(format!("{base}/status"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if status["status"] == "error" {
            assert!(!status.to_string().contains("mock-key"));
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    service_task.abort();
    mock_task.abort();
}

#[tokio::test]
async fn stalled_model_is_timed_out_within_remaining_budget() {
    let (config, _, task) = server("slow_timeout").await;
    let dir = tempfile::tempdir().unwrap();
    let mut remote = Remote::new(config, dir.path().into(), 1).unwrap();
    let start = std::time::Instant::now();
    let result = remote
        .model("timeout test", "Return JSON", &json!({}))
        .await;
    assert!(result.is_err());
    assert!(start.elapsed() < std::time::Duration::from_secs(2));
    assert!(remote.trace.iter().any(|e| e["event"] == "model_error"));
    task.abort();
}

#[tokio::test]
async fn inconclusive_judge_retains_evidence_but_cannot_be_selected() {
    let (config, _, task) = server("inconclusive").await;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("build");
    let mut request = input();
    request.max_iterations = 1;
    assert!(
        builder::build(request, config, out.clone(), dir.path().join("slot.json"))
            .await
            .is_err()
    );
    let evaluation = freya_builder::read_json(&out.join("candidate-1-evaluation.json")).unwrap();
    assert_eq!(evaluation["complete"], false);
    assert!(evaluation["rows"][0]["call"].is_object());
    assert!(evaluation["rows"][0]["ledger"].is_object());
    assert!(out.join("candidate-1-platform.json").exists());
    assert!(!out.join("selected-platform.json").exists());
    task.abort();
}

#[tokio::test]
async fn ambiguous_create_never_fills_workspace_with_retries() {
    let (config, shared, task) = server("create_failure").await;
    let dir = tempfile::tempdir().unwrap();
    let slot = dir.path().join("slot.json");
    assert!(
        builder::build(input(), config, dir.path().join("build"), slot.clone())
            .await
            .is_err()
    );
    assert_eq!(shared.lock().unwrap().creation_attempts, 1);
    assert_eq!(
        freya_builder::read_json(&slot).unwrap()["state"],
        "creation_pending"
    );
    task.abort();
}

#[tokio::test]
async fn source_requirements_are_frozen_before_candidates_and_sent_to_hosted_judge() {
    let (config, shared, task) = server("").await;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("build");
    let mut request = input();
    let requirement = "Source rule: accurately read back product, amount and date, then obtain fresh explicit confirmation.";
    request.policies.push(requirement.into());
    let expected_requirements = json!(request.policies);
    builder::build(request, config, out.clone(), dir.path().join("slot.json"))
        .await
        .unwrap();
    let frozen = freya_builder::read_json(&out.join("frozen-suite.json")).unwrap();
    for test in frozen["tests"].as_array().unwrap() {
        assert_eq!(test["requirements"], expected_requirements);
    }
    let state = shared.lock().unwrap();
    assert_eq!(state.model_inputs.len(), 4);
    for candidate_input in state.model_inputs.iter().skip(1) {
        assert_eq!(candidate_input["frozen_tests"], frozen["tests"]);
    }
    assert!(!state.persona_specs.is_empty());
    for persona in &state.persona_specs {
        let criteria = persona["successCriteria"].as_str().unwrap();
        assert!(criteria.contains(requirement));
        assert!(criteria.contains("close correctly"));
        assert!(!persona["prompt"].as_str().unwrap().contains(requirement));
    }
    drop(state);
    let trace = freya_builder::read_json(&out.join("trace.json")).unwrap();
    let trace = trace.as_array().unwrap();
    let frozen_at = trace
        .iter()
        .position(|e| e["event"] == "suite_frozen")
        .unwrap();
    let candidate_at = trace
        .iter()
        .position(|e| e["event"] == "candidate_start")
        .unwrap();
    assert!(frozen_at < candidate_at);
    task.abort();
}

#[tokio::test]
async fn evaluate_cli_uses_all_repetitions_without_model_or_workflow_writes() {
    let (config, shared, task) = server("evaluate_existing").await;
    {
        let mut mock = shared.lock().unwrap();
        mock.current = plan(true)["plan"].clone();
        mock.version = 7;
    }
    let dir = tempfile::tempdir().unwrap();
    let slot = dir.path().join("slot.json");
    let cases = dir.path().join("cases.json");
    let requirements = dir.path().join("requirements.json");
    freya_builder::write_json(
        &slot,
        &json!({"workflow_id":"workflow","agent_id":"agent","version":7}),
    )
    .unwrap();
    freya_builder::write_json(&cases, &input().cases).unwrap();
    freya_builder::write_json(&requirements, &input().policies).unwrap();
    let result = tokio::process::Command::new(env!("CARGO_BIN_EXE_freya-builder"))
        .args(["evaluate", "--slot"])
        .arg(&slot)
        .arg("--cases")
        .arg(&cases)
        .arg("--requirements")
        .arg(&requirements)
        .args(["--reps", "3", "--budget-seconds", "30", "--output"])
        .arg(dir.path().join("evaluation"))
        .env("BUILDER_LLM_BASE_URL", &config.llm_base)
        .env("BUILDER_LLM_API_KEY", &config.llm_key)
        .env("FREYA_API_URL", &config.freya_base)
        .env("FREYA_API_KEY", &config.freya_key)
        .env("FREYA_BANK_HOST", &config.bank_base)
        .env("FREYA_BANK_TEAM_KEY", &config.bank_key)
        .output()
        .await
        .unwrap();
    assert!(
        !result.status.success(),
        "one failed repetition must fail the CLI gate"
    );
    assert!(String::from_utf8_lossy(&result.stderr).contains("failing cases"));
    let summary: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(summary["complete"], true);
    assert_eq!(summary["run_passed"], 2);
    assert_eq!(summary["passed"], 0);
    assert_eq!(summary["total"], 1);
    let mock = shared.lock().unwrap();
    assert_eq!(mock.model_calls, 0);
    assert_eq!(mock.creation_attempts, 0);
    assert_eq!(mock.version, 7);
    assert_eq!(mock.sessions, 3);
    assert!(!dir.path().join(".build-lock").exists());
    task.abort();
}

#[tokio::test]
async fn evaluate_cli_rejects_an_unpublished_slot_without_starting_calls() {
    let (config, shared, task) = server("").await;
    let dir = tempfile::tempdir().unwrap();
    let slot = dir.path().join("slot.json");
    let cases = dir.path().join("cases.json");
    let requirements = dir.path().join("requirements.json");
    freya_builder::write_json(
        &slot,
        &json!({"workflow_id":"workflow","agent_id":"agent","version":0}),
    )
    .unwrap();
    freya_builder::write_json(&cases, &input().cases).unwrap();
    freya_builder::write_json(&requirements, &input().policies).unwrap();
    let result = tokio::process::Command::new(env!("CARGO_BIN_EXE_freya-builder"))
        .args(["evaluate", "--slot"])
        .arg(&slot)
        .arg("--cases")
        .arg(&cases)
        .arg("--requirements")
        .arg(&requirements)
        .arg("--output")
        .arg(dir.path().join("evaluation"))
        .env("FREYA_API_URL", &config.freya_base)
        .env("FREYA_API_KEY", &config.freya_key)
        .env("FREYA_BANK_HOST", &config.bank_base)
        .env("FREYA_BANK_TEAM_KEY", &config.bank_key)
        .output()
        .await
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("published version differs"));
    assert_eq!(shared.lock().unwrap().sessions, 0);
    task.abort();
}

#[tokio::test]
async fn caller_persona_does_not_receive_expected_checks_faults_or_other_fixture_metadata() {
    let (config, shared, task) = server("").await;
    let dir = tempfile::tempdir().unwrap();
    let mut request = input();
    request.max_iterations = 1;
    request.cases[0]["identity"] = json!({"security_answer":"IDENTITY_ONLY_KNOWN"});
    request.cases[0]["variables"] = json!({"title":"Mx","first_name":"PERSONA_GIVEN","last_name":"PERSONA_FAMILY","hidden":"VARIABLE_ONLY_SECRET"});
    request.cases[0]["script"] = json!("SCRIPT_ONLY_HELLO");
    request.cases[0]["expected"]["private_oracle_note"] = json!("EXPECTED_ONLY_MARKER");
    request.cases[0]["checks"] = json!(["FIXTURE_CHECK_ONLY"]);
    request.cases[0]["faults"] = json!({"injection":"FAULT_ONLY_MARKER"});
    builder::build(
        request,
        config,
        dir.path().join("build"),
        dir.path().join("slot.json"),
    )
    .await
    .unwrap();
    let state = shared.lock().unwrap();
    assert_eq!(state.persona_specs.len(), 2);
    for persona in &state.persona_specs {
        let prompt = persona["prompt"].as_str().unwrap();
        for allowed in [
            "PERSONA_GIVEN",
            "PERSONA_FAMILY",
            "IDENTITY_ONLY_KNOWN",
            "SCRIPT_ONLY_HELLO",
        ] {
            assert!(prompt.contains(allowed));
        }
        for excluded in [
            "EXPECTED_ONLY_MARKER",
            "FIXTURE_CHECK_ONLY",
            "FAULT_ONLY_MARKER",
            "VARIABLE_ONLY_SECRET",
            "Close with outcome done",
        ] {
            assert!(!prompt.contains(excluded));
        }
        assert!(
            persona["successCriteria"]
                .as_str()
                .unwrap()
                .contains("FIXTURE_CHECK_ONLY")
        );
    }
    task.abort();
}
