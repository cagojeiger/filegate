#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod support;

use serde_json::{Value, json};
use support::*;

#[test]
fn client_and_key_lifecycle_hashes_raw_key_locally() {
    let hash = "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    let key_path = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(key_path.path(), "abc\n").unwrap();
    let key_delete_path = format!("/api/admin/v1/clients/app/keys/{hash}");
    let server = Server::routes(vec![
        (
            "POST",
            "/api/admin/v1/clients",
            Reply::status(201, json!({"id":"app","storage_id":"archive"})),
        ),
        (
            "POST",
            "/api/admin/v1/clients/app/keys",
            Reply::status(201, json!({"client_id":"app","key_hash":hash})),
        ),
        ("DELETE", &key_delete_path, Reply::empty(204)),
        (
            "DELETE",
            "/api/admin/v1/clients/app/s3-credentials/fgakpublic",
            Reply::empty(204),
        ),
        ("DELETE", "/api/admin/v1/clients/app", Reply::empty(204)),
    ]);

    envelope(
        &server.run(&["client", "create", "app", "--storage", "archive"]),
        0,
    );
    let registered = server
        .command()
        .args(["client-key", "register", "--client", "app", "--key-file"])
        .arg(key_path.path())
        .output()
        .unwrap();
    let result = envelope(&registered, 0);
    assert_eq!(result["data"]["key_hash"], hash);
    assert!(!String::from_utf8_lossy(&registered.stdout).contains("abc"));

    envelope(
        &server.run(&["client-key", "delete", "--client", "app", hash, "--yes"]),
        0,
    );
    envelope(
        &server.run(&[
            "credential",
            "delete",
            "--client",
            "app",
            "fgakpublic",
            "--yes",
        ]),
        0,
    );
    envelope(&server.run(&["client", "delete", "app", "--yes"]), 0);

    let seen = server.seen();
    let client_body: Value = serde_json::from_str(&seen[0].body).unwrap();
    let key_body: Value = serde_json::from_str(&seen[1].body).unwrap();
    assert_eq!(client_body, json!({"id":"app","storage_id":"archive"}));
    assert_eq!(key_body, json!({"key_hash":hash}));
    assert!(!seen[1].body.contains("abc"));
    assert_eq!(
        seen.iter()
            .filter(|request| request.method == "POST")
            .count(),
        2
    );
}
