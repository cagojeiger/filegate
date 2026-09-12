#![allow(clippy::unwrap_used)]
mod support;

use serde_json::{Value, json};
use support::*;

#[test]
fn credential_secret_is_written_once_to_a_private_file() {
    let secret = "issued-secret-that-must-not-reach-stdout";
    let server = Server::routes(vec![(
        "POST",
        "/api/admin/v1/clients/app/s3-credentials",
        Reply::status(
            201,
            json!({"access_key_id":"fgakpublic","secret_key":secret}),
        ),
    )]);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("credential.json");
    let output = server
        .command()
        .args(["credential", "create", "--client", "app", "--secret-out"])
        .arg(&path)
        .output()
        .unwrap();
    let result = envelope(&output, 0);
    assert_eq!(at(&result, "/data/access_key_id"), "fgakpublic");
    assert_eq!(at(&result, "/data/file_state"), "saved");
    assert!(!String::from_utf8_lossy(&output.stdout).contains(secret));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(secret));

    let stored: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(at(&stored, "/schema_version"), 1);
    assert_eq!(at(&stored, "/client_id"), "app");
    assert_eq!(at(&stored, "/access_key_id"), "fgakpublic");
    assert_eq!(at(&stored, "/secret_key"), secret);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let seen = server.seen();
    assert_eq!(seen.len(), 1);
    assert!(seen.first().unwrap().body.is_empty());
}

#[test]
fn existing_secret_paths_are_rejected_before_issuance() {
    let server = Server::new(vec![]);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("credential.json");
    std::fs::write(&path, "keep").unwrap();
    let output = server
        .command()
        .args(["credential", "create", "--client", "app", "--secret-out"])
        .arg(&path)
        .output()
        .unwrap();
    envelope(&output, 2);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "keep");

    #[cfg(unix)]
    {
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        let output = server
            .command()
            .args(["credential", "create", "--client", "app", "--secret-out"])
            .arg(link)
            .output()
            .unwrap();
        envelope(&output, 2);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "keep");
    }
    assert!(server.seen().is_empty());
}

#[test]
fn rejected_issuance_removes_the_reserved_empty_file() {
    let server = Server::routes(vec![(
        "POST",
        "/api/admin/v1/clients/missing/s3-credentials",
        Reply::error(404),
    )]);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("credential.json");
    let output = server
        .command()
        .args([
            "credential",
            "create",
            "--client",
            "missing",
            "--secret-out",
        ])
        .arg(&path)
        .output()
        .unwrap();
    let result = envelope(&output, 4);
    assert_eq!(at(&result, "/error/outcome"), "not_applied");
    assert!(!path.exists());
}

#[test]
fn uncertain_or_malformed_issuance_preserves_the_marker_file() {
    for (reply, outcome, access_key_id) in [
        (Reply::error(500), "unknown", None),
        (
            Reply::status(201, json!({"unexpected":true})),
            "applied",
            None,
        ),
        (
            Reply::status(201, json!({"access_key_id":"fgakpublic","secret_key":""})),
            "applied",
            Some("fgakpublic"),
        ),
    ] {
        let server = Server::routes(vec![(
            "POST",
            "/api/admin/v1/clients/app/s3-credentials",
            reply,
        )]);
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("credential.json");
        let output = server
            .command()
            .args(["credential", "create", "--client", "app", "--secret-out"])
            .arg(&path)
            .output()
            .unwrap();
        let result = envelope(&output, 8);
        assert_eq!(at(&result, "/error/outcome"), outcome);
        assert_eq!(at(&result, "/data/file_state"), "empty");
        assert_eq!(at(&result, "/data/access_key_id").as_str(), access_key_id);
        assert!(path.exists());
        assert!(std::fs::read(&path).unwrap().is_empty());
        assert_eq!(server.seen().len(), 1);
    }
}
