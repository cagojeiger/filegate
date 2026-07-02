use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Extension, Json, Router};
use filegate_core::{Config, Error};
use filegate_service::{CreateFileInput, FileService};
use serde::Deserialize;
use uuid::Uuid;

#[derive(Clone)]
struct AppState {
    svc: Arc<FileService>,
    /// api key -> client name
    keys: Arc<HashMap<String, String>>,
}

/// Authenticated caller, injected by the auth middleware.
#[derive(Clone)]
struct Caller(String);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let config_path =
        std::env::var("FILEGATE_CONFIG").unwrap_or_else(|_| "config/filegate.yaml".into());
    let cfg = Arc::new(Config::load(&config_path)?);
    let database_url = std::env::var("FILEGATE_DATABASE_URL")
        .map_err(|_| anyhow::anyhow!("missing env var FILEGATE_DATABASE_URL"))?;

    let pool = filegate_db::connect(&database_url).await?;
    let providers = filegate_infra::build_registry(&cfg);
    let svc = Arc::new(FileService { pool, cfg: cfg.clone(), providers });

    filegate_service::reconciler::spawn(svc.clone(), Duration::from_secs(30));

    let keys: HashMap<String, String> = cfg
        .clients
        .iter()
        .map(|(name, c)| (c.api_key.clone(), name.clone()))
        .collect();
    let state = AppState { svc, keys: Arc::new(keys) };

    let protected = Router::new()
        .route("/v1/files", post(create_file))
        .route("/v1/files/{id}", get(get_file))
        .route("/v1/files/{id}", delete(detach_file))
        .route("/v1/files/{id}/leases", post(issue_read_lease))
        .route("/v1/leases/{id}/commit", post(commit_lease))
        .route("/v1/usage", get(usage))
        .layer(middleware::from_fn_with_state(state.clone(), auth));

    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .merge(protected)
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind(&state.svc.cfg.listen_addr).await?;
    tracing::info!("filegate listening on {}", state.svc.cfg.listen_addr);
    axum::serve(listener, app).await?;
    Ok(())
}

async fn auth(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut req: axum::extract::Request,
    next: Next,
) -> Response {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    match token.and_then(|t| state.keys.get(t)) {
        Some(client) => {
            req.extensions_mut().insert(Caller(client.clone()));
            next.run(req).await
        }
        None => (StatusCode::UNAUTHORIZED, "invalid or missing api key").into_response(),
    }
}

#[derive(Deserialize)]
struct CreateFileRequest {
    intent: String,
    size: i64,
    #[serde(default)]
    content_type: Option<String>,
    #[serde(default = "empty_object")]
    metadata: serde_json::Value,
}

fn empty_object() -> serde_json::Value {
    serde_json::json!({})
}

async fn create_file(
    State(state): State<AppState>,
    Extension(Caller(client)): Extension<Caller>,
    Json(req): Json<CreateFileRequest>,
) -> Result<Response, ApiError> {
    let out = state
        .svc
        .create_file(
            &client,
            CreateFileInput {
                intent: req.intent,
                declared_size: req.size,
                content_type: req.content_type,
                client_metadata: req.metadata,
            },
        )
        .await?;
    Ok((StatusCode::CREATED, Json(out)).into_response())
}

async fn commit_lease(
    State(state): State<AppState>,
    Extension(Caller(client)): Extension<Caller>,
    Path(id): Path<Uuid>,
) -> Result<Response, ApiError> {
    let out = state.svc.commit_lease(&client, id).await?;
    Ok(Json(out).into_response())
}

async fn issue_read_lease(
    State(state): State<AppState>,
    Extension(Caller(client)): Extension<Caller>,
    Path(id): Path<Uuid>,
) -> Result<Response, ApiError> {
    let out = state.svc.issue_read_lease(&client, id).await?;
    Ok((StatusCode::CREATED, Json(out)).into_response())
}

async fn get_file(
    State(state): State<AppState>,
    Extension(Caller(client)): Extension<Caller>,
    Path(id): Path<Uuid>,
) -> Result<Response, ApiError> {
    let out = state.svc.get_file(&client, id).await?;
    Ok(Json(out).into_response())
}

async fn detach_file(
    State(state): State<AppState>,
    Extension(Caller(client)): Extension<Caller>,
    Path(id): Path<Uuid>,
) -> Result<Response, ApiError> {
    state.svc.detach_file(&client, id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn usage(
    State(state): State<AppState>,
    Extension(Caller(client)): Extension<Caller>,
) -> Result<Response, ApiError> {
    let out = state.svc.usage(&client).await?;
    Ok(Json(out).into_response())
}

/// Domain error -> HTTP status. The only place that knows both worlds.
struct ApiError(Error);

impl From<Error> for ApiError {
    fn from(e: Error) -> Self {
        Self(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match &self.0 {
            Error::NotFound => StatusCode::NOT_FOUND,
            Error::Forbidden(_) => StatusCode::FORBIDDEN,
            Error::Validation(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Error::QuotaExceeded(_) => StatusCode::TOO_MANY_REQUESTS,
            Error::LeaseState(_) => StatusCode::CONFLICT,
            Error::Config(_) | Error::Provider(_) | Error::Db(_) => {
                tracing::error!("internal error: {}", self.0);
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };
        let body = serde_json::json!({ "error": self.0.to_string() });
        (status, Json(body)).into_response()
    }
}
