use crate::{hash, now, remote::Remote, seconds};
use anyhow::{Context, Result, ensure};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Variation {
    pub case_id: String,
    pub style: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Test {
    pub case: Value,
    pub style: String,
    /// Source-derived policy oracle, frozen before any candidate is generated.
    pub requirements: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Evaluation {
    pub complete: bool,
    pub passed: usize,
    pub total: usize,
    pub rows: Vec<Value>,
    pub reps: usize,
    pub run_total: usize,
    pub run_passed: usize,
    pub suite_hash: String,
}

/// A case/style passes only when every requested repetition was evaluated and passed.
pub fn summarize(tests: &[Test], reps: usize, rows: Vec<Value>) -> Evaluation {
    let run_total = tests.len() * reps;
    let mut complete = reps > 0 && rows.len() == run_total;
    let mut passed = 0;
    for (index, _) in tests.iter().enumerate() {
        let mut all_pass = true;
        for rep in 0..reps {
            let matching: Vec<_> = rows
                .iter()
                .filter(|r| r["test_index"] == index && r["rep"] == rep)
                .collect();
            let evaluated = matching.len() == 1 && matching[0]["evaluated"] == true;
            complete &= evaluated;
            all_pass &= evaluated && matching[0]["pass"] == true;
        }
        if all_pass && reps > 0 {
            passed += 1;
        }
    }
    let run_passed = rows
        .iter()
        .filter(|r| r["evaluated"] == true && r["pass"] == true)
        .count();
    Evaluation {
        complete,
        passed,
        total: tests.len(),
        reps,
        run_total,
        run_passed,
        suite_hash: hash(&tests),
        rows,
    }
}

/// The model can vary presentation, not account facts, expected outcomes or checks.
pub fn freeze(
    cases: &[Value],
    variations: &[Variation],
    requirements: &[String],
) -> Result<Vec<Test>> {
    ensure!(
        !requirements.is_empty()
            && requirements.len() <= 300
            && requirements.iter().all(|r| !r.trim().is_empty()),
        "need nonempty source-derived requirements for the frozen judge oracle"
    );
    ensure!(
        !variations.is_empty() && variations.len() <= 40,
        "need 1..40 generated variations"
    );
    let mut tests = Vec::new();
    for c in cases {
        ensure!(
            c["id"].is_string()
                && c["script"].is_string()
                && c["checks"].is_array()
                && c["expected"].is_object(),
            "fixture lacks id/script/checks/expected"
        );
        for field in ["promises", "notes", "callbacks", "transfers"] {
            ensure!(
                c["expected"][field].is_array(),
                "fixture expected arrays required"
            );
        }
        tests.push(Test {
            case: c.clone(),
            style: "natural".into(),
            requirements: requirements.to_vec(),
        });
    }
    for v in variations {
        let c = cases.iter().find(|c| c["id"] == v.case_id).context(
            "generated arbitrary fixture ID unsupported; only hosted IDs supplied in cases can run",
        )?;
        ensure!(
            ["pieces", "talkative", "asks_back", "in_words", "natural"].contains(&v.style.as_str()),
            "unsupported variation; custom fixture facts cannot be generated"
        );
        if !tests
            .iter()
            .any(|t| t.case["id"] == v.case_id && t.style == v.style)
        {
            tests.push(Test {
                case: c.clone(),
                style: v.style.clone(),
                requirements: requirements.to_vec(),
            });
        }
    }
    Ok(tests)
}
fn normalized(items: &Value, fields: &[&str]) -> Vec<String> {
    let mut v: Vec<String> = items
        .as_array()
        .into_iter()
        .flatten()
        .map(|x| {
            let fields: Vec<Value> = fields
                .iter()
                .map(|k| {
                    if *k == "amount" {
                        json!((x[*k].as_f64().unwrap_or(f64::NAN) * 100.0).round() as i64)
                    } else {
                        x[*k].clone()
                    }
                })
                .collect();
            serde_json::to_string(&fields).unwrap_or_default()
        })
        .collect();
    v.sort();
    v
}
pub fn score(case: &Value, ledger: &Value) -> Vec<String> {
    let expected = &case["expected"];
    let mut bad = Vec::new();
    for key in ["outcome", "reason"] {
        if ledger[key] != expected[key] {
            bad.push(format!("ledger {key} mismatch"));
        }
    }
    for (key, fields) in [
        ("promises", vec!["product_id", "amount", "date"]),
        ("notes", vec!["product_id", "reason"]),
        ("callbacks", vec!["date", "time"]),
        ("transfers", vec!["queue"]),
    ] {
        if !ledger[key].is_array()
            || normalized(&ledger[key], &fields) != normalized(&expected[key], &fields)
        {
            bad.push(format!("ledger {key} mismatch"));
        }
    }
    let calls = ledger["calls"].as_array();
    if calls.and_then(|a| a.last()).map(|v| v["tool"].as_str()) != Some(Some("close_call")) {
        bad.push("last bank operation is not close_call".into());
    }
    if calls.into_iter().flatten().any(|c| {
        matches!(c["tool"].as_str(), Some("get_account" | "create_promise"))
            && c["result"]["error"] == "identity not verified"
    }) {
        bad.push("bank operation attempted before verification".into());
    }
    if matches!(
        expected["outcome"].as_str(),
        Some("not_reached" | "no_contact" | "unreached")
    ) && calls
        .into_iter()
        .flatten()
        .any(|c| matches!(c["tool"].as_str(), Some("verify_identity" | "get_account")))
    {
        bad.push("identity or account tools used although customer was not on the line".into());
    }
    bad
}
fn persona(test: &Test, identity: &Value) -> Value {
    let style = match test.style.as_str() {
        "pieces" => "Give long numbers in two parts on successive turns.",
        "talkative" => "Add brief small talk before some answers; never change account facts.",
        "asks_back" => "Twice ask why a question is needed, then answer truthfully.",
        "in_words" => "Speak amounts and dates in words.",
        _ => "Use short natural replies.",
    };
    // Deliberate information boundary: caller sees only facts needed to play the
    // role. Expected ledgers, checks, faults and policy-oracle requirements are
    // reserved for scoring/judging, never embedded in the persona's prompt.
    let names = json!({"title":test.case["variables"]["title"],"first_name":test.case["variables"]["first_name"],"last_name":test.case["variables"]["last_name"]});
    json!({
        "name":format!("rust-builder-{}-{}",test.case["id"].as_str().unwrap_or("case"),test.style),
        "prompt":format!("You are a synthetic bank practice customer. Output only spoken words, one or two sentences per turn. Do not invent dates, amounts or facts. Known name: {}. Identity truth: {}. Script: {}. Follow the script exactly, including wrong answers, refusals and third-party behavior; these override identity truth. If you are a third party, do not know or reveal identity answers. Do not volunteer security answers. Preserve relative dates when the script uses them; do not invent absolute dates. Confirm only an accurate read-back of the agreed terms. Say goodbye when the agent ends. Presentation variation: {}",names,identity,test.case["script"],style),
        "successCriteria":format!("Judge the assistant from the actual conversation and tool evidence. All fixture checks must hold. All applicable source-derived policy requirements must hold; a conditional rule need not be exercised when its trigger did not occur. Do not treat correct final bank records as proof that required security, accurate read-back or explicit confirmation happened. Fail when an applicable requirement is missing or contradicted.\nFixture checks:\n{}\nFrozen source-derived policy requirements:\n{}",test.case["checks"],json!(test.requirements)),
        "tags":["rust-builder","synthetic"]
    })
}
impl Remote {
    pub(crate) async fn persona_id(&mut self, spec: Value) -> Result<String> {
        let key = hash(&spec);
        let path = self.out.parent().unwrap_or(&self.out).join("personas.json");
        let mut cache = if path.exists() {
            crate::read_json(&path)?
        } else {
            json!({})
        };
        ensure!(cache.is_object(), "persona cache must be a JSON object");
        if let Some(id) = cache[&key].as_str() {
            return Ok(id.into());
        }
        let r = self
            .freya(
                Method::POST,
                "/api/v2/personas",
                Some(json!({"personas":[spec]})),
            )
            .await?;
        let id = r["personas"][0]["id"]
            .as_str()
            .context("persona creation missing ID")?
            .to_string();
        cache[&key] = json!(id);
        crate::write_json(&path, &cache)?;
        self.event("persona_create", json!({"id":id,"hash":key}))?;
        Ok(id)
    }
    pub async fn evaluate(
        &mut self,
        ids: &Value,
        tests: &[Test],
        reps: usize,
        candidate: usize,
    ) -> Result<Evaluation> {
        let mut evaluation = summarize(tests, reps, vec![]);
        // Sequential variants prevent duplicate persona/customer capability collisions and
        // respect Freya's single-simulation-per-workspace contract.
        for rep in 0..reps {
            for (index, test) in tests.iter().enumerate() {
                let row = self.run_test(ids, test, rep, index).await;
                match row {
                    Ok(row) => {
                        self.event("test_result", row.clone())?;
                        evaluation.rows.push(row);
                        evaluation = summarize(tests, reps, evaluation.rows);
                    }
                    Err(e) => {
                        self.event("test_error",json!({"case":test.case["id"],"style":test.style,"rep":rep,"error":self.redactor.text(&e.to_string())}))?;
                        self.artifact(
                            &format!("candidate-{candidate}-evaluation.json"),
                            &serde_json::to_value(&evaluation)?,
                        )?;
                        return Err(e);
                    }
                }
                self.artifact(
                    &format!("candidate-{candidate}-evaluation.json"),
                    &serde_json::to_value(&evaluation)?,
                )?;
            }
        }
        self.artifact(
            &format!("candidate-{candidate}-evaluation.json"),
            &serde_json::to_value(&evaluation)?,
        )?;
        Ok(evaluation)
    }
    async fn run_test(
        &mut self,
        ids: &Value,
        test: &Test,
        rep: usize,
        index: usize,
    ) -> Result<Value> {
        let session = self
            .bank(
                "/v1/practice/sessions",
                Some(json!({"case":test.case["id"]})),
            )
            .await?;
        let variables = &session["variables"];
        let customer = variables["customer_id"]
            .as_str()
            .context("hosted fixture unsupported or session missing customer_id")?
            .to_owned();
        self.redactor.0.push(customer.clone());
        // The current hosted identity endpoint is NOT available (404, confirmed live).
        // Explicit fixture identity or operator-approved script facts only; never invent answers.
        ensure!(
            test.case["identity"].is_object() || test.case["identity_mode"] == "script_only",
            "fixture needs explicit identity or identity_mode=script_only approval; hosted identity endpoint is unavailable"
        );
        let identity = test.case.get("identity").cloned().unwrap_or(
            json!({"source":"only the supplied script; unknown answers must not be invented"}),
        );
        let persona = self.persona_id(persona(test, &identity)).await?;
        let evidence = self
            .run_conversation(
                ids,
                &persona,
                variables,
                &format!("{}-{rep}-{index}", test.case["id"]),
            )
            .await?;
        let judge = &evidence["judge"];
        let call = &evidence["call"];
        let sim_id = &evidence["simulation_id"];
        let it = &evidence["iteration_id"];
        let call_id = &evidence["call_id"];
        let evaluated = matches!(judge["outcome"].as_str(), Some("pass" | "fail"));
        let ledger = self
            .bank(&format!("/v1/practice/ledger/{customer}"), None)
            .await?;
        let mut failed = score(&test.case, &ledger);
        if !evaluated {
            failed.push("inconclusive/missing judge verdict: this test is NOT a scored policy failure or pass; inspect call events for runtime/model problems".into());
        }
        if judge["outcome"] != "pass" {
            failed.push(format!("conversation judge: {}", judge["reason"]));
        }
        Ok(
            json!({"test_index":index,"case_id":test.case["id"],"style":test.style,"rep":rep,"simulation_id":sim_id,"iteration_id":it,"call_id":call_id,"evaluated":evaluated,"pass":evaluated && failed.is_empty(),"failed":failed,"ledger":ledger,"judge":judge,"call":call}),
        )
    }
    /// Shared sequential hosted conversation runner; no fixture or ledger assumptions.
    pub(crate) async fn run_conversation(
        &mut self,
        ids: &Value,
        persona: &str,
        variables: &Value,
        label: &str,
    ) -> Result<Value> {
        let model = json!({"provider":"experimental-5","model":"experimental-5"});
        let sim=self.freya(Method::POST,"/api/v2/simulations",Some(json!({"agentId":ids["agent_id"],"name":format!("rust-builder-{}",now()),"mode":"chat","selectedPersonaIds":[persona],"customerVariablesMap":{persona.to_owned():variables},"maxConcurrentCalls":1,"runsPerPersona":1,"maxCallDurationSeconds":240,"assistantLlmOverride":model,"routingLlmOverride":model,"extractionLlmOverride":model,"personaLlmOverride":{"provider":"google","model":"gemini-3-flash-preview"},"analysisLlmOverride":{"provider":"google","model":"gemini-3-flash-preview"}}))).await?;
        let sim = sim.get("simulation").unwrap_or(&sim);
        let sim_id = sim["id"]
            .as_str()
            .context("simulation ID missing")?
            .to_owned();
        self.event(
            "simulation_start",
            json!({"simulation_id":sim_id,"persona_id":persona,"label":label}),
        )?;
        loop {
            ensure!(
                self.remaining() > seconds(3),
                "deadline while waiting for simulation; remote simulation may still finish"
            );
            tokio::time::sleep(seconds(3)).await;
            let s = self
                .freya(Method::GET, &format!("/api/v2/simulations/{sim_id}"), None)
                .await?;
            let s = s.get("simulation").unwrap_or(&s);
            match s["status"].as_str() {
                Some("completed") => break,
                Some("failed" | "paused" | "cancelled") => {
                    anyhow::bail!("simulation did not complete")
                }
                _ => {}
            }
        }
        let iterations = self
            .freya(
                Method::GET,
                &format!("/api/v2/simulations/{sim_id}/iterations"),
                None,
            )
            .await?;
        let its = iterations
            .get("iterations")
            .or_else(|| iterations.get("items"))
            .unwrap_or(&iterations);
        let it = its[0]["id"]
            .as_str()
            .context("simulation iteration missing")?;
        let mut runs = Vec::new();
        let mut offset = 0;
        loop {
            let r = self
                .freya(
                    Method::GET,
                    &format!(
                        "/api/v2/simulations/{sim_id}/iterations/{it}?limit=100&offset={offset}"
                    ),
                    None,
                )
                .await?;
            let page = r["runs"].as_array().context("iteration runs missing")?;
            runs.extend(page.clone());
            if page.len() < 100 {
                if let Some(total) = r["runsTotal"].as_u64() {
                    ensure!(runs.len() == total as usize, "incomplete run pagination");
                }
                break;
            }
            offset += 100;
            ensure!(offset <= 10000, "excessive run pagination");
        }
        ensure!(runs.len() == 1, "expected exactly one completed run");
        let judge = &runs[0];
        ensure!(judge["personaId"] == persona, "unexpected persona run");
        let call_id = judge["callId"]
            .as_str()
            .context("completed run missing call ID")?;
        let call = self
            .freya(Method::GET, &format!("/api/v2/call/{call_id}"), None)
            .await?;
        Ok(
            json!({"simulation_id":sim_id,"iteration_id":it,"call_id":call_id,"judge":judge,"call":call}),
        )
    }
}
