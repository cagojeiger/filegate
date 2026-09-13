#![allow(clippy::unwrap_used)]
mod support;

use std::io::Write;
use std::process::Stdio;

use serde_json::json;
use support::*;

#[test]
fn invalid_storage_documents_fail_before_http() {
    let server = Server::new(vec![]);
    let directory = tempfile::tempdir().unwrap();
    for (name, contents) in [
        ("array", "[]"),
        (
            "id",
            r#"{"id":"inside","kind":"fs","root_path":"/tmp","capacity_bytes":1}"#,
        ),
        (
            "unknown",
            r#"{"kind":"fs","root_path":"/tmp","capacity_bytes":1,"typo":true}"#,
        ),
        ("missing", r#"{"kind":"fs","root_path":"/tmp"}"#),
        ("kind", r#"{"kind":"other","capacity_bytes":1}"#),
    ] {
        let path = directory.path().join(name);
        std::fs::write(&path, contents).unwrap();
        let output = server
            .command()
            .args(["storage", "create", "archive", "--from"])
            .arg(path)
            .output()
            .unwrap();
        let result = envelope(&output, 2);
        assert_eq!(at(&result, "/error/outcome"), "not_applied");
    }
    let oversized = directory.path().join("oversized");
    std::fs::write(&oversized, vec![b' '; 1024 * 1024 + 1]).unwrap();
    let output = server
        .command()
        .args(["storage", "create", "archive", "--from"])
        .arg(oversized)
        .output()
        .unwrap();
    envelope(&output, 2);
    assert!(server.seen().is_empty());
}

#[test]
fn storage_document_can_be_read_from_stdin() {
    let response = json!({
        "id":"archive","kind":"fs","force_relay":false,"root_path":"/tmp",
        "endpoint":null,"public_endpoint":null,"region":null,"bucket":null,
        "force_path_style":false,"access_key":null,"capacity_bytes":1
    });
    let server = Server::routes(vec![(
        "POST",
        "/api/admin/v1/storages",
        Reply::status(201, response),
    )]);
    let mut child = server
        .command()
        .args(["storage", "create", "archive", "--from", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"kind":"fs","root_path":"/tmp","capacity_bytes":1}"#)
        .unwrap();
    envelope(&child.wait_with_output().unwrap(), 0);
    assert_eq!(server.seen().len(), 1);
}

#[test]
fn invalid_key_files_and_hashes_make_no_request() {
    let server = Server::new(vec![]);
    let directory = tempfile::tempdir().unwrap();
    for (name, contents) in [("empty", ""), ("space", "has space"), ("lines", "a\nb")] {
        let path = directory.path().join(name);
        std::fs::write(&path, contents).unwrap();
        let output = server
            .command()
            .args(["client-key", "register", "--client", "app", "--key-file"])
            .arg(path)
            .output()
            .unwrap();
        envelope(&output, 2);
    }
    for hash in ["sha256:abc", &format!("sha256:{}", "A".repeat(64))] {
        let output = server.run(&["client-key", "delete", "--client", "app", hash, "--yes"]);
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
    }
    let output = server.run(&[
        "credential",
        "delete",
        "--client",
        "app",
        "NOT-AN-ACCESS-KEY",
        "--yes",
    ]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(server.seen().is_empty());
}

#[test]
fn secret_output_cannot_be_stdout() {
    let server = Server::new(vec![]);
    let output = server.run(&[
        "credential",
        "create",
        "--client",
        "app",
        "--secret-out",
        "-",
    ]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(server.seen().is_empty());
}
