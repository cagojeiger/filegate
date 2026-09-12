#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    response::Response,
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    process::{Command, Output},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};
use tokio::sync::oneshot;

pub const TOKEN: &str = "test-operator-sensitive-value";

#[derive(Clone)]
pub struct Reply {
    pub status: u16,
    pub body: String,
    pub location: Option<String>,
    pub delay_ms: u64,
}

impl Reply {
    pub fn json(value: Value) -> Self {
        Self {
            status: 200,
            body: value.to_string(),
            location: None,
            delay_ms: 0,
        }
    }
    pub fn error(status: u16) -> Self {
        Self {
            status,
            body: format!("sensitive backend text: {TOKEN}"),
            location: None,
            delay_ms: 0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Seen {
    pub method: String,
    pub path: String,
    pub authorization: Option<String>,
}

#[derive(Clone)]
struct App {
    replies: Arc<HashMap<String, Reply>>,
    seen: Arc<Mutex<Vec<Seen>>>,
}

pub struct Server {
    pub endpoint: String,
    seen: Arc<Mutex<Vec<Seen>>>,
    stop: Option<oneshot::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Server {
    pub fn new(replies: Vec<(&str, Reply)>) -> Self {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let app = App {
            replies: Arc::new(
                replies
                    .into_iter()
                    .map(|(p, r)| (p.to_owned(), r))
                    .collect(),
            ),
            seen: seen.clone(),
        };
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let (stop, stopped) = oneshot::channel();
        let thread = thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                    let router = Router::new().fallback(reply).with_state(app);
                    tokio::select! {
                        result = axum::serve(listener, router) => result.unwrap(),
                        _ = stopped => {},
                    }
                });
        });
        Self {
            endpoint,
            seen,
            stop: Some(stop),
            thread: Some(thread),
        }
    }
    pub fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
    pub fn command(&self) -> Command {
        let mut command = bare();
        command
            .args(["--endpoint", &self.endpoint, "--output", "json"])
            .env("GROVE_OPERATOR_TOKEN", TOKEN);
        command
    }
    pub fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

async fn reply(State(app): State<App>, request: Request) -> Response {
    let path = request.uri().to_string();
    app.seen.lock().unwrap().push(Seen {
        method: request.method().to_string(),
        path: path.clone(),
        authorization: request
            .headers()
            .get("authorization")
            .map(|v| v.to_str().unwrap().to_owned()),
    });
    let reply = app
        .replies
        .get(&path)
        .cloned()
        .unwrap_or_else(|| Reply::error(500));
    tokio::time::sleep(Duration::from_millis(reply.delay_ms)).await;
    let mut response = Response::builder()
        .status(reply.status)
        .header("content-type", "application/json");
    if let Some(location) = reply.location {
        response = response.header("location", location);
    }
    response.body(Body::from(reply.body)).unwrap()
}

pub fn bare() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_gscli"));
    command
        .env_remove("GROVE_ENDPOINT")
        .env_remove("GROVE_OPERATOR_TOKEN")
        .env_remove("FILEGATE_DATABASE_URL")
        .env_remove("DATABASE_URL")
        .env_remove("FILEGATE_MASTER_KEY")
        .env_remove("FILEGATE_ENC_ROOT_SECRET")
        .env_remove("FILEGATE_ENC_ROOT_SECRET_PREV")
        .env_remove("FILEGATE_OPERATOR_TOKENS")
        .env_remove("HTTP_PROXY")
        .env_remove("HTTPS_PROXY")
        .env_remove("ALL_PROXY")
        .env_remove("http_proxy")
        .env_remove("https_proxy")
        .env_remove("all_proxy");
    command
}

pub fn envelope(output: &Output, code: i32) -> Value {
    assert_eq!(
        output.status.code(),
        Some(code),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

pub fn usage() -> Value {
    json!({"storage_id":"r2","kind":"s3","capacity_bytes":0,"reserved_bytes":0,"active_bytes":9007199254740993_i64,"purge_pending_bytes":0,"remaining_bytes":-9007199254740993_i64,"reserved_files":0,"active_files":2,"purge_pending_files":0})
}

pub fn storage() -> Value {
    json!({"id":"r2","kind":"s3","force_relay":false,"root_path":null,"endpoint":"https://storage.example","public_endpoint":"https://public.example","region":"auto","bucket":"data","force_path_style":true,"access_key":"public-access-key","capacity_bytes":0,"secret_key":"DO-NOT-PRINT","secret_key_ciphertext":"DO-NOT-PRINT","enc_key_id":"DO-NOT-PRINT"})
}

pub fn status_replies() -> Vec<(&'static str, Reply)> {
    vec![
        (
            "/",
            Reply::json(json!({"name":"filegate","version":"0.3.10"})),
        ),
        ("/healthz", Reply::json(json!({"status":"ok"}))),
        ("/readyz", Reply::json(json!({"status":"ready"}))),
        ("/api/admin/v1/usage", Reply::json(json!([usage()]))),
        ("/api/admin/v1/clients", Reply::json(json!(["notegate"]))),
    ]
}
