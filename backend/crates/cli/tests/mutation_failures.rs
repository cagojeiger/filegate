#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod support;

use serde_json::json;
use support::*;

fn create(server: &Server) -> std::process::Output {
    server.run(&["client", "create", "app", "--storage", "archive"])
}

#[test]
fn explicit_client_errors_are_not_applied() {
    for (status, exit) in [(401, 3), (404, 4), (409, 6), (422, 7)] {
        let server = Server::routes(vec![(
            "POST",
            "/api/admin/v1/clients",
            Reply::error(status),
        )]);
        let result = envelope(&create(&server), exit);
        assert_eq!(result["error"]["http_status"], status);
        assert_eq!(result["error"]["outcome"], "not_applied");
        assert_eq!(server.seen().len(), 1);
    }
}

#[test]
fn server_failure_and_request_timeout_are_unknown_without_retries() {
    for status in [302, 500] {
        let server = Server::routes(vec![(
            "POST",
            "/api/admin/v1/clients",
            Reply::error(status),
        )]);
        let result = envelope(&create(&server), 8);
        assert_eq!(result["error"]["outcome"], "unknown");
        assert_eq!(server.seen().len(), 1);
    }

    let mut delayed = Reply::status(201, json!({"id":"app","storage_id":"archive"}));
    delayed.delay_ms = 1200;
    let server = Server::routes(vec![("POST", "/api/admin/v1/clients", delayed)]);
    let output = server
        .command()
        .args([
            "--timeout",
            "1",
            "client",
            "create",
            "app",
            "--storage",
            "archive",
        ])
        .output()
        .unwrap();
    let result = envelope(&output, 8);
    assert_eq!(result["error"]["code"], "timeout");
    assert_eq!(result["error"]["outcome"], "unknown");
    assert_eq!(server.seen().len(), 1);
}

#[test]
fn malformed_or_unexpected_success_is_reported_as_applied() {
    for reply in [
        Reply::status(201, json!({"id":"other","storage_id":"archive"})),
        Reply::status(201, json!({"unexpected":true})),
        Reply::status(200, json!({"id":"app","storage_id":"archive"})),
    ] {
        let server = Server::routes(vec![("POST", "/api/admin/v1/clients", reply)]);
        let result = envelope(&create(&server), 8);
        assert_eq!(result["error"]["outcome"], "applied");
        assert_eq!(server.seen().len(), 1);
    }
}

#[cfg(unix)]
#[test]
fn successful_mutation_with_broken_output_preserves_exit_eight() {
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::process::Stdio;

    let server = Server::routes(vec![(
        "POST",
        "/api/admin/v1/clients",
        Reply::status(201, json!({"id":"app","storage_id":"archive"})),
    )]);
    let (writer, reader) = UnixStream::pair().unwrap();
    drop(reader);
    let writer: OwnedFd = writer.into();
    let output = server
        .command()
        .args(["client", "create", "app", "--storage", "archive"])
        .stdout(Stdio::from(writer))
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(8));
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("command state may have changed")
    );
    assert_eq!(server.seen().len(), 1);
}
