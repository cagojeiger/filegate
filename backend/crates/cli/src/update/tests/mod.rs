#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod install;
pub(super) mod recovery;
mod transfer;

use super::*;
use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    response::Response,
};
use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex, MutexGuard};

static INSTALLATIONS: Mutex<()> = Mutex::new(());

struct Installation {
    _serial: MutexGuard<'static, ()>,
    _root: tempfile::TempDir,
    binary: PathBuf,
}

impl Installation {
    fn new(version: &str) -> Self {
        // File-lock and injected-failure cases share process-level test hooks.
        let serial = INSTALLATIONS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        fs::write(&source, script(version)).unwrap();
        let bin = root.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let binary = storage::install(&source, &bin, target().unwrap()).unwrap();
        Self {
            _serial: serial,
            _root: root,
            binary,
        }
    }

    fn receipt(&self) -> PathBuf {
        self.binary
            .parent()
            .unwrap()
            .join("gscli-install-receipt.json")
    }

    fn assert_no_candidates(&self) {
        let mut entries: Vec<_> = fs::read_dir(self.binary.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        entries.sort();
        assert_eq!(
            entries,
            [".gscli-update.lock", "gscli", "gscli-install-receipt.json"]
        );
    }
}

fn script(version: &str) -> Vec<u8> {
    format!("#!/bin/sh\nprintf 'gscli {version}\\n'\n").into_bytes()
}

fn manifest(version: &str, binary: &[u8]) -> Value {
    json!({"schema_version":1,"repository":REPOSITORY,"version":version,"assets":{
        target().unwrap(): {"name":format!("gscli-{}", target().unwrap()),"size":binary.len(),"sha256":format!("{:x}", Sha256::digest(binary))}
    }})
}

struct Server {
    base: String,
    seen: Arc<Mutex<Vec<(String, bool)>>>,
    task: tokio::task::JoinHandle<()>,
}

#[derive(Clone)]
struct Replies {
    metadata: Vec<u8>,
    binary: Vec<u8>,
    code: u16,
    location: Option<&'static str>,
    delay: Duration,
    seen: Arc<Mutex<Vec<(String, bool)>>>,
}

impl Server {
    async fn new(metadata: Value, binary: Vec<u8>) -> Self {
        Self::custom(
            serde_json::to_vec(&metadata).unwrap(),
            binary,
            200,
            None,
            Duration::ZERO,
        )
        .await
    }

    async fn custom(
        metadata: Vec<u8>,
        binary: Vec<u8>,
        code: u16,
        location: Option<&'static str>,
        delay: Duration,
    ) -> Self {
        async fn reply(State(state): State<Replies>, request: Request) -> Response {
            let path = request.uri().path().to_owned();
            state.seen.lock().unwrap().push((
                path.clone(),
                request.headers().contains_key("authorization"),
            ));
            tokio::time::sleep(state.delay).await;
            let mut response = Response::builder().status(state.code);
            if let Some(location) = state.location {
                response = response.header("location", location);
            }
            let bytes = if path == "/manifest" {
                state.metadata
            } else {
                state.binary
            };
            response.body(Body::from(bytes)).unwrap()
        }
        let seen = Arc::new(Mutex::new(Vec::new()));
        let state = Replies {
            metadata,
            binary,
            code,
            location,
            delay,
            seen: seen.clone(),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, Router::new().fallback(reply).with_state(state))
                .await
                .unwrap();
        });
        Self { base, seen, task }
    }

    fn source(&self) -> download::Source {
        download::Source::fixture(&self.base, 5).unwrap()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
