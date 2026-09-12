#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod support;

use serde_json::json;
use support::*;

#[test]
fn help_and_version_need_no_server_configuration() {
    for args in [vec!["--help"], vec!["--version"], vec!["storage", "--help"]] {
        let output = bare().args(args).output().unwrap();
        assert!(output.status.success());
        assert!(output.stderr.is_empty());
        assert!(!output.stdout.is_empty());
    }
    let output = bare().arg("--version").output().unwrap();
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        concat!("gscli ", env!("CARGO_PKG_VERSION"), "\n")
    );
}

#[test]
fn invalid_commands_and_values_make_no_http_requests() {
    let server = Server::new(vec![]);
    for args in [
        vec!["storage", "delete", "r2"],
        vec!["client", "show"],
        vec!["credential", "list"],
        vec!["client", "show", ".."],
        vec!["usage", "history", "--days", "0"],
        vec!["usage", "history", "--days", "3651"],
        vec!["status", "--timeout", "0"],
        vec!["status", "--output", "yaml"],
        vec!["status", "extra"],
    ] {
        let output = server.run(&args);
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty(), "parser errors use stderr");
    }
    assert!(server.seen().is_empty());
}

#[test]
fn endpoint_rejects_paths_credentials_fragments_and_nonliteral_http_hosts() {
    for endpoint in [
        "http://example.com",
        "http://127.1",
        "http://2130706433",
        "http://localhost.evil",
        "https://user:secret@example.com",
        "https://example.com?token=secret",
        "https://example.com/#secret",
        "https://example.com/api",
        "https://example.com/../",
        "https://example.com/%2e/",
        "https://example.com//",
        " https://example.com",
        "https://example.com\\x",
        "file:///tmp/test",
        "http://localhost:080/",
    ] {
        let output = bare()
            .args(["--endpoint", endpoint, "--output", "json", "status"])
            .env("GROVE_OPERATOR_TOKEN", TOKEN)
            .output()
            .unwrap();
        let result = envelope(&output, 2);
        assert!(result["endpoint"].is_null());
        assert!(!String::from_utf8_lossy(&output.stdout).contains("secret"));
    }
}

#[test]
fn endpoint_flag_overrides_environment_and_trailing_slash_is_accepted() {
    let server = Server::new(vec![("/api/admin/v1/clients", Reply::json(json!([])))]);
    let output = bare()
        .env("GROVE_ENDPOINT", "https://unused.invalid")
        .env("GROVE_OPERATOR_TOKEN", TOKEN)
        .args([
            "client",
            "list",
            "--output",
            "json",
            "--endpoint",
            &format!("{}/", server.endpoint),
        ])
        .output()
        .unwrap();
    envelope(&output, 0);
    assert_eq!(server.seen().len(), 1);
    let output = bare()
        .env("GROVE_ENDPOINT", &server.endpoint)
        .env("GROVE_OPERATOR_TOKEN", TOKEN)
        .args(["client", "list", "--output", "json"])
        .output()
        .unwrap();
    envelope(&output, 0);
}

#[test]
fn token_file_precedes_environment_and_accepts_one_final_newline() {
    let server = Server::new(vec![("/api/admin/v1/clients", Reply::json(json!([])))]);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("operator-token");
    for ending in ["", "\n", "\r\n"] {
        std::fs::write(&path, format!("from-file{ending}")).unwrap();
        let output = server
            .command()
            .args(["client", "list", "--token-file"])
            .arg(&path)
            .output()
            .unwrap();
        envelope(&output, 0);
    }
    assert!(
        server
            .seen()
            .iter()
            .all(|r| r.authorization.as_deref() == Some("Bearer from-file"))
    );
}

#[test]
fn missing_or_invalid_tokens_are_rejected_before_http_without_leaks() {
    let server = Server::new(vec![]);
    for token in ["", "has space", "secret\nnext", "secret\r", "비밀"] {
        let output = server
            .command()
            .env("GROVE_OPERATOR_TOKEN", token)
            .args(["status"])
            .output()
            .unwrap();
        envelope(&output, 2);
        assert!(!String::from_utf8_lossy(&output.stdout).contains("secret"));
    }
    let output = server
        .command()
        .env_remove("GROVE_OPERATOR_TOKEN")
        .args(["status"])
        .output()
        .unwrap();
    envelope(&output, 2);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("token");
    for bytes in [b"secret\n\n".as_slice(), b"\xff", b""] {
        std::fs::write(&path, bytes).unwrap();
        let output = server
            .command()
            .args(["status", "--token-file"])
            .arg(&path)
            .output()
            .unwrap();
        envelope(&output, 2);
    }
    std::fs::write(&path, "s".repeat(8193)).unwrap();
    let output = server
        .command()
        .args(["status", "--token-file"])
        .arg(&path)
        .output()
        .unwrap();
    envelope(&output, 2);
    let output = server
        .command()
        .args(["status", "--token-file"])
        .arg(dir.path())
        .output()
        .unwrap();
    envelope(&output, 2);
    assert!(server.seen().is_empty());
}

#[test]
fn working_directory_dotenv_is_not_loaded() {
    let server = Server::new(vec![]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".env"),
        format!(
            "GROVE_ENDPOINT={}\nGROVE_OPERATOR_TOKEN={TOKEN}\n",
            server.endpoint
        ),
    )
    .unwrap();
    let output = bare()
        .current_dir(dir.path())
        .args(["--output", "json", "status"])
        .output()
        .unwrap();
    envelope(&output, 2);
    assert!(server.seen().is_empty());
}
