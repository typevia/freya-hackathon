use crate::{BuildInput, Redactor, builder, remote::Config};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    http::StatusCode,
    routing::{get, post},
};
use serde_json::{Value, json};
use std::{net::SocketAddr, path::PathBuf, sync::Arc};
use tokio::sync::{Mutex, Semaphore};

#[derive(Clone)]
struct App {
    config: Config,
    root: PathBuf,
    lock: Arc<Semaphore>,
    status: Arc<Mutex<Value>>,
}
pub async fn serve(config: Config, root: PathBuf, bind: SocketAddr) -> anyhow::Result<()> {
    anyhow::ensure!(
        bind.ip().is_loopback(),
        "HTTP service must bind to loopback (no authentication implemented)"
    );
    let state = App {
        config,
        root,
        lock: Arc::new(Semaphore::new(1)),
        status: Arc::new(Mutex::new(json!({"status":"idle"}))),
    };
    let app = Router::new()
        .route("/builds", post(start))
        .route("/status", get(status))
        .layer(DefaultBodyLimit::max(1_000_000))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(bind).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
async fn status(State(app): State<App>) -> Json<Value> {
    Json(app.status.lock().await.clone())
}
async fn start(
    State(app): State<App>,
    input: Result<Json<BuildInput>, axum::extract::rejection::JsonRejection>,
) -> (StatusCode, Json<Value>) {
    let input = match input {
        Ok(Json(input)) => input,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error":"invalid JSON build request or body exceeds 1 MB"})),
            );
        }
    };
    if let Err(e) = input.validate() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":Redactor::env().text(&e.to_string())})),
        );
    }
    let permit = match app.lock.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            return (
                StatusCode::CONFLICT,
                Json(json!({"error":"one build is already running"})),
            );
        }
    };
    let id = crate::build_id();
    *app.status.lock().await = json!({"status":"running","id":id});
    let response = json!({"status":"accepted","id":id});
    tokio::spawn(async move {
        let _permit = permit;
        let out = app.root.join(&id);
        let slot = app.root.join("slot.json");
        // Atomic directory lock also serializes independent CLI/server processes.
        let result = match BuildLock::acquire(&app.root) {
            Ok(_lock) => builder::build(input, app.config, out, slot).await,
            Err(e) => Err(e),
        };
        *app.status.lock().await = match result {
            Ok(summary) => json!({"status":"completed","id":id,"summary":summary}),
            Err(e) => {
                json!({"status":"error","id":id,"error":Redactor::env().text(&e.to_string())})
            }
        };
    });
    (StatusCode::ACCEPTED, Json(response))
}
/// Crash-stale locks are deliberately not broken automatically: remote simulation
/// completion must be checked before the operator removes the lock directory.
pub struct BuildLock(PathBuf);
impl BuildLock {
    pub fn acquire(root: &std::path::Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(root)?;
        let path = root.join(".build-lock");
        std::fs::create_dir(&path).map_err(|_| {
            anyhow::anyhow!(
                "build slot locked; another build may be active (see README.md for crash recovery)"
            )
        })?;
        Ok(Self(path))
    }
}
impl Drop for BuildLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.0);
    }
}
