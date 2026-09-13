#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod support;

use serde_json::{Value, json};
use support::*;

fn fs_storage(id: &str, capacity: i64) -> Value {
    json!({
        "id": id,
        "kind": "fs",
        "force_relay": false,
        "root_path": "/srv/files",
        "endpoint": null,
        "public_endpoint": null,
        "region": null,
        "bucket": null,
        "force_path_style": false,
        "access_key": null,
        "capacity_bytes": capacity
    })
}

#[test]
fn storage_create_replace_and_delete_use_distinct_contracts() {
    let server = Server::routes(vec![
        (
            "POST",
            "/api/admin/v1/storages",
            Reply::status(201, fs_storage("archive", 1024)),
        ),
        (
            "PUT",
            "/api/admin/v1/storages/archive",
            Reply::status(200, fs_storage("archive", 2048)),
        ),
        (
            "DELETE",
            "/api/admin/v1/storages/archive",
            Reply::empty(204),
        ),
    ]);
    let directory = tempfile::tempdir().unwrap();
    let create = directory.path().join("create.json");
    let replace = directory.path().join("replace.json");
    std::fs::write(
        &create,
        r#"{"kind":"fs","root_path":"/srv/files","capacity_bytes":1024}"#,
    )
    .unwrap();
    std::fs::write(
        &replace,
        r#"{"kind":"fs","root_path":"/srv/files","capacity_bytes":2048}"#,
    )
    .unwrap();

    let created = server
        .command()
        .args(["storage", "create", "archive", "--from"])
        .arg(&create)
        .output()
        .unwrap();
    let result = envelope(&created, 0);
    assert_eq!(result["command"], "storage.create");
    assert_eq!(result["data"]["capacity_bytes"], 1024);

    let replaced = server
        .command()
        .args(["storage", "replace", "archive", "--from"])
        .arg(&replace)
        .arg("--yes")
        .output()
        .unwrap();
    let result = envelope(&replaced, 0);
    assert_eq!(result["command"], "storage.replace");
    assert_eq!(result["data"]["capacity_bytes"], 2048);

    let result = envelope(&server.run(&["storage", "delete", "archive", "--yes"]), 0);
    assert_eq!(result["data"]["resource"], "storage");
    assert_eq!(result["data"]["id"], "archive");

    let seen = server.seen();
    assert_eq!(seen.len(), 3);
    let create_body: Value = serde_json::from_str(&seen[0].body).unwrap();
    let replace_body: Value = serde_json::from_str(&seen[1].body).unwrap();
    assert_eq!(create_body["id"], "archive");
    assert!(replace_body.get("id").is_none());
    assert!(seen[2].body.is_empty());
    assert!(
        seen.iter().all(|request| {
            request.authorization.as_deref() == Some(&format!("Bearer {TOKEN}"))
        })
    );
}

#[test]
fn s3_vendor_secret_is_sent_but_never_rendered() {
    let secret = "vendor-secret-that-must-not-be-rendered";
    let server = Server::routes(vec![(
        "POST",
        "/api/admin/v1/storages",
        Reply::status(201, storage()),
    )]);
    let input = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(
        input.path(),
        json!({
            "endpoint":"https://storage.example",
            "public_endpoint":"https://public.example",
            "region":"auto",
            "bucket":"data",
            "force_path_style":true,
            "access_key":"public-access-key",
            "secret_key":secret,
            "capacity_bytes":0
        })
        .to_string(),
    )
    .unwrap();
    let output = server
        .command()
        .args(["storage", "create", "r2", "--from"])
        .arg(input.path())
        .output()
        .unwrap();
    let result = envelope(&output, 0);
    assert_eq!(result["data"]["id"], "r2");
    assert!(!String::from_utf8_lossy(&output.stdout).contains(secret));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(secret));
    let body: Value = serde_json::from_str(&server.seen()[0].body).unwrap();
    assert_eq!(body["kind"], "s3");
    assert_eq!(body["secret_key"], secret);
}
