#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod support;

use serde_json::json;
use support::*;

#[test]
fn http_failures_map_to_exit_codes_without_echoing_bodies_or_tokens() {
    for (status, exit) in [
        (401, 3),
        (403, 3),
        (404, 4),
        (409, 6),
        (400, 7),
        (429, 7),
        (500, 5),
        (503, 5),
        (204, 5),
    ] {
        let server = Server::new(vec![("/api/admin/v1/clients", Reply::error(status))]);
        let output = server.run(&["client", "list"]);
        let result = envelope(&output, exit);
        assert_eq!(result["error"]["http_status"], status);
        assert_eq!(result["error"]["outcome"], "not_applied");
        assert!(result["data"].is_null());
        assert!(!String::from_utf8_lossy(&output.stdout).contains(TOKEN));
        assert!(!String::from_utf8_lossy(&output.stderr).contains(TOKEN));
        assert_eq!(server.seen().len(), 1, "no automatic retries");
    }
}

#[test]
fn redirects_never_forward_operator_credentials() {
    let target = Server::new(vec![("/", Reply::json(json!([])))]);
    let mut redirect = Reply::error(302);
    redirect.location = Some(target.endpoint.clone());
    let server = Server::new(vec![("/api/admin/v1/clients", redirect)]);
    let result = envelope(&server.run(&["client", "list"]), 5);
    assert_eq!(result["error"]["code"], "redirect");
    assert!(target.seen().is_empty());
}

#[test]
fn malformed_and_wrong_shape_success_responses_are_failures() {
    for reply in [
        Reply {
            body: format!("bad JSON {TOKEN}"),
            ..Reply::json(json!([]))
        },
        Reply::json(json!({"clients":[]})),
        Reply::json(json!([{"id":"not-a-string"}])),
    ] {
        let server = Server::new(vec![("/api/admin/v1/clients", reply)]);
        let output = server.run(&["client", "list"]);
        let result = envelope(&output, 5);
        assert_eq!(result["error"]["code"], "invalid_response");
        assert!(!String::from_utf8_lossy(&output.stdout).contains(TOKEN));
    }
}

#[test]
fn response_size_is_bounded() {
    let reply = Reply {
        body: " ".repeat(8 * 1024 * 1024 + 1),
        ..Reply::json(json!([]))
    };
    let server = Server::new(vec![("/api/admin/v1/clients", reply)]);
    let result = envelope(&server.run(&["client", "list"]), 5);
    assert_eq!(result["error"]["code"], "response_too_large");
}

#[test]
fn connection_failure_is_reported_without_internal_transport_details() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let output = bare()
        .env("GROVE_OPERATOR_TOKEN", TOKEN)
        .args([
            "--endpoint",
            &endpoint,
            "--output",
            "json",
            "client",
            "list",
        ])
        .output()
        .unwrap();
    let result = envelope(&output, 5);
    assert_eq!(result["error"]["code"], "transport");
}
