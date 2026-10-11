use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use freya_builder::{
    BuildInput, Redactor, builder,
    compiler::{Plan, compile},
    read_json, read_text,
    remote::{Config, Remote},
    service::{self, BuildLock},
};
use serde_json::json;
use std::{net::SocketAddr, path::PathBuf};

#[derive(Parser)]
#[command(
    version,
    about = "Policy-to-Freya builder with immutable hosted synthetic tests"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}
#[derive(Subcommand)]
enum Commands {
    /// Build a contract-compatible workflow from an untrusted document folder.
    BuildFolder {
        #[arg(long)]
        case_folder: PathBuf,
        #[arg(long)]
        mock_url: String,
        #[arg(long, default_value = "output")]
        output: PathBuf,
        #[arg(long, default_value_t = 5)]
        max_iterations: usize,
        #[arg(long, default_value_t = 1800)]
        budget_seconds: u64,
        #[arg(long, default_value_t = 1)]
        reps: usize,
        /// Select this many generated tests BEFORE freezing the suite.
        #[arg(long)]
        tests_limit: Option<usize>,
        /// Generate and validate locally, without publishing or simulating.
        #[arg(long)]
        compile_only: bool,
    },
    Build {
        #[arg(long,required=true,num_args=1..)]
        policy: Vec<PathBuf>,
        #[arg(long)]
        openapi: PathBuf,
        #[arg(long, default_value = "testdata/public-test-set.json")]
        cases: PathBuf,
        #[arg(long, value_delimiter = ',')]
        only: Vec<String>,
        /// Approve using ONLY supplied script facts where fixture identity is absent.
        #[arg(long)]
        script_only_identity: bool,
        #[arg(long, default_value = "output")]
        output: PathBuf,
        #[arg(long, default_value_t = 5)]
        max_iterations: usize,
        #[arg(long, default_value_t = 1800)]
        budget_seconds: u64,
        #[arg(long, default_value_t = 1)]
        reps: usize,
    },
    Validate {
        #[arg(long)]
        plan: PathBuf,
        #[arg(long)]
        openapi: PathBuf,
        #[arg(long)]
        out: Option<PathBuf>,
    },
    Doctor {
        #[arg(long)]
        model_smoke: bool,
        #[arg(long, default_value = "output/doctor")]
        output: PathBuf,
    },
    /// Read-only platform observation; does not select or score a candidate.
    Export {
        #[arg(long)]
        slot: PathBuf,
        #[arg(long, default_value = "output/observation")]
        output: PathBuf,
    },
    /// Test a published slot without regeneration or workflow changes.
    Evaluate {
        #[arg(long)]
        slot: PathBuf,
        #[arg(long)]
        cases: PathBuf,
        /// requirements.json extracted by a prior build; frozen for this evaluation.
        #[arg(long)]
        requirements: PathBuf,
        #[arg(long, value_delimiter = ',')]
        only: Vec<String>,
        #[arg(long, default_value_t = 3)]
        reps: usize,
        #[arg(long, default_value_t = 900)]
        budget_seconds: u64,
        #[arg(long, default_value = "output/validation")]
        output: PathBuf,
    },
    Serve {
        #[arg(long, default_value = "127.0.0.1:8787")]
        bind: SocketAddr,
        #[arg(long, default_value = "output")]
        output: PathBuf,
    },
}
#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!(
            "{}",
            json!({"status":"error","error":Redactor::env().text(&e.to_string())})
        );
        std::process::exit(1);
    }
}
async fn run() -> Result<()> {
    match Cli::parse().command {
        Commands::BuildFolder {
            case_folder,
            mock_url,
            output,
            max_iterations,
            budget_seconds,
            reps,
            tests_limit,
            compile_only,
        } => {
            let _lock = BuildLock::acquire(&output)?;
            let out = output.join(freya_builder::build_id());
            let options = freya_builder::documents::Options {
                case_folder,
                mock_url,
                max_iterations,
                budget_seconds,
                reps,
                tests_limit,
                compile_only,
            };
            let result = freya_builder::documents::build(
                options,
                Config::env(),
                out.clone(),
                output.join("slot.json"),
            )
            .await?;
            println!("{}", json!({"result":result,"output":out}));
        }
        Commands::Build {
            policy,
            openapi,
            cases,
            only,
            script_only_identity,
            output,
            max_iterations,
            budget_seconds,
            reps,
        } => {
            let _lock = BuildLock::acquire(&output)?;
            let cases = read_json(&cases)?
                .as_array()
                .context("cases must be an array")?
                .iter()
                .filter(|c| only.is_empty() || only.iter().any(|id| c["id"] == *id))
                .cloned()
                .map(|mut c| {
                    if script_only_identity && !c["identity"].is_object() {
                        c["identity_mode"] = json!("script_only");
                    }
                    c
                })
                .collect();
            let input = BuildInput {
                policies: policy
                    .iter()
                    .map(|p| read_text(p))
                    .collect::<Result<Vec<_>>>()?,
                openapi: read_json(&openapi)?,
                cases,
                max_iterations,
                budget_seconds,
                reps,
            };
            let out = output.join(freya_builder::build_id());
            let result =
                builder::build(input, Config::env(), out.clone(), output.join("slot.json")).await?;
            println!("{}", json!({"result":result,"output":out}));
        }
        Commands::Validate { plan, openapi, out } => {
            let value = read_json(&plan)?;
            let plan: Plan = serde_json::from_value(value.get("plan").unwrap_or(&value).clone())?;
            let workflow = compile(&plan, &read_json(&openapi)?)?;
            if let Some(path) = out {
                freya_builder::remote::ensure_executable_safe(&workflow, &Redactor::env())?;
                freya_builder::write_json(&path, &workflow)?;
            }
            println!(
                "{}",
                json!({"status":"valid","kind":"local structural/compiler validation; not a policy proof or Freya test"})
            );
        }
        Commands::Doctor {
            model_smoke,
            output,
        } => {
            let config = Config::env();
            config.validate()?;
            freya_builder::compiler::check_python(
                "try:\n    vars['test'] = 'yes'\nexcept Exception:\n    vars['test'] = 'no'\n",
            )?;
            let mut remote = Remote::new(config, output, 150)?;
            let workspace = remote.workspace().await?;
            let model = if model_smoke {
                Some(
                    remote
                        .model(
                            "credential-free connectivity smoke",
                            "Return exactly a JSON object with ok true.",
                            &json!({"task":"connection smoke"}),
                        )
                        .await?,
                )
            } else {
                None
            };
            println!(
                "{}",
                json!({"status":"ok","workspace_id":workspace,"model_smoke":model,"redirects":"disabled","python":"AST-only validator available"})
            );
        }
        Commands::Export { slot, output } => {
            let ids = read_json(&slot)?;
            let mut remote = Remote::new(Config::env(), output.clone(), 120)?;
            remote.event(
                "observation_start",
                json!({"selection":"none; read-only observation"}),
            )?;
            remote.snapshot(&ids, "observed-platform.json").await?;
            remote.event(
                "observation_end",
                json!({"status":"exported","evaluated":false}),
            )?;
            println!(
                "{}",
                json!({"status":"exported","evaluated":false,"output":output})
            );
        }
        Commands::Evaluate {
            slot,
            cases,
            requirements,
            only,
            reps,
            budget_seconds,
            output,
        } => {
            ensure!((1..=3).contains(&reps), "reps must be 1..3");
            ensure!(
                (30..=1800).contains(&budget_seconds),
                "budget_seconds must be 30..1800"
            );
            let slot_root = slot
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| std::path::Path::new("."));
            let _lock = BuildLock::acquire(slot_root)?;
            let ids = read_json(&slot)?;
            ensure!(
                ids["workflow_id"].is_string()
                    && ids["agent_id"].is_string()
                    && ids["version"].is_u64(),
                "slot requires published workflow, agent and version"
            );
            let supplied = read_json(&cases)?;
            let cases: Vec<_> = supplied
                .as_array()
                .context("cases must be an array")?
                .iter()
                .filter(|c| only.is_empty() || only.iter().any(|id| c["id"] == *id))
                .cloned()
                .collect();
            ensure!(!cases.is_empty() && cases.len() <= 20, "need 1..20 cases");
            ensure!(
                only.iter().all(|id| cases.iter().any(|c| c["id"] == *id)),
                "requested case ID is absent"
            );
            ensure!(
                cases
                    .iter()
                    .all(|c| c["identity"].is_object() || c["identity_mode"] == "script_only"),
                "case identity or explicit script_only metadata required"
            );
            let requirements: Vec<String> = serde_json::from_value(read_json(&requirements)?)?;
            let variations = cases
                .iter()
                .map(|c| {
                    Ok(freya_builder::eval::Variation {
                        case_id: c["id"].as_str().context("case ID must be string")?.into(),
                        style: "natural".into(),
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            let tests = freya_builder::eval::freeze(&cases, &variations, &requirements)?;
            let out = output.join(freya_builder::build_id());
            let mut remote = Remote::new(Config::env(), out.clone(), budget_seconds)?;
            remote.artifact(
                "frozen-suite.json",
                &json!({"hash":freya_builder::hash(&tests),"tests":tests,"reps":reps}),
            )?;
            remote.event(
                "evaluation_start",
                json!({"ids":ids,"reps":reps,"workflow_mutation":false}),
            )?;
            let deadline = remote.deadline;
            let result = tokio::time::timeout_at(deadline, async {
                ensure_published_slot(&mut remote, &ids).await?;
                let platform = remote.snapshot(&ids, "evaluated-platform.json").await?;
                ensure!(
                    platform["workflow_id"] == ids["workflow_id"]
                        && platform["agent_id"] == ids["agent_id"],
                    "slot agent does not belong to the snapshotted workflow"
                );
                let evaluation = remote.evaluate(&ids, &tests, reps, 0).await?;
                ensure_published_slot(&mut remote, &ids).await?;
                remote.artifact("evaluation.json", &serde_json::to_value(&evaluation)?)?;
                Ok::<_, anyhow::Error>(evaluation)
            })
            .await
            .context("evaluation wall-clock deadline exceeded; remote simulation may still finish")
            .and_then(|v| v);
            match result {
                Ok(evaluation) => {
                    remote.event("evaluation_end", json!({"complete":evaluation.complete,"passed":evaluation.passed,"total":evaluation.total,"run_passed":evaluation.run_passed,"run_total":evaluation.run_total}))?;
                    println!(
                        "{}",
                        json!({"complete":evaluation.complete,"passed":evaluation.passed,"total":evaluation.total,"run_passed":evaluation.run_passed,"run_total":evaluation.run_total,"reps":reps,"output":out,"official_score":false})
                    );
                    ensure!(
                        evaluation.complete,
                        "evaluation incomplete; not a passing result"
                    );
                    ensure!(
                        evaluation.passed == evaluation.total,
                        "evaluation completed with failing cases; inspect evaluation.json"
                    );
                }
                Err(e) => {
                    remote.event(
                        "evaluation_end",
                        json!({"status":"error","error":remote.redactor.text(&e.to_string())}),
                    )?;
                    return Err(e);
                }
            }
        }
        Commands::Serve { bind, output } => service::serve(Config::env(), output, bind).await?,
    }
    Ok(())
}

async fn ensure_published_slot(remote: &mut Remote, ids: &serde_json::Value) -> Result<()> {
    let workspace = remote.workspace().await?;
    let id = ids["workflow_id"].as_str().context("missing workflow ID")?;
    let versions = remote
        .freya(
            reqwest::Method::GET,
            &format!("/api/v2/versions/workflow/{id}?workspaceId={workspace}"),
            None,
        )
        .await?;
    ensure!(
        versions["versions"]
            .as_array()
            .context("missing workflow versions")?
            .iter()
            .any(|v| v["isPublished"] == true && v["versionNumber"] == ids["version"]),
        "published version differs from slot; refusing to attribute results to the wrong workflow"
    );
    Ok(())
}
