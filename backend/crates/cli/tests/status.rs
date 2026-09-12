#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod support;

use serde_json::json;
use support::*;

#[test]
fn status_checks_five_contracts_and_only_sends_token_to_admin_paths() {
    let server = Server::new(status_replies());
    let result = envelope(&server.run(&["status"]), 0);
    assert_eq!(result["data"]["server_version"], "0.3.10");
    assert_eq!(result["data"]["registry"]["state"], "ok");
    assert_eq!(result["data"]["registry"]["storage_count"], 1);
    assert_eq!(result["data"]["registry"]["client_count"], 1);
    assert_eq!(result["data"]["storage_access"], "not_checked");
    let seen = server.seen();
    assert_eq!(seen.len(), 5);
    for request in seen {
        assert_eq!(
            request.authorization.is_some(),
            request.path.starts_with("/api/admin/v1/")
        );
    }
}

#[test]
fn empty_registry_is_healthy() {
    let mut replies = status_replies();
    replies[3].1 = Reply::json(json!([]));
    replies[4].1 = Reply::json(json!([]));
    let server = Server::new(replies);
    let result = envelope(&server.run(&["status"]), 0);
    assert_eq!(result["data"]["registry"]["storage_count"], 0);
    assert_eq!(result["data"]["registry"]["client_count"], 0);
}

#[test]
fn status_table_includes_endpoint_and_physical_check_limit() {
    let server = Server::new(status_replies());
    let output = server
        .command()
        .args(["status", "--output", "table"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains(&server.endpoint));
    assert!(text.contains("not_checked"));
    assert!(text.contains("0.3.10"));
}

#[test]
fn failures_keep_partial_results_and_authentication_has_exit_precedence() {
    let mut replies = status_replies();
    replies[2].1 = Reply::error(503);
    replies[3].1 = Reply::error(403);
    let server = Server::new(replies);
    let result = envelope(&server.run(&["status"]), 3);
    assert_eq!(result["error"]["http_status"], 403);
    assert_eq!(result["data"]["health"], "ok");
    assert_eq!(result["data"]["readiness"], "failed");
    assert_eq!(result["data"]["registry"]["usage"], "failed");
    assert_eq!(result["data"]["registry"]["clients"], "ok");
    assert_eq!(server.seen().len(), 5);
}

#[test]
fn malformed_payloads_are_unknown_but_explicit_unhealthy_payloads_are_failed() {
    let mut replies = status_replies();
    replies[1].1 = Reply::json(json!({"unrelated":"ok"}));
    replies[2].1 = Reply::json(json!({"status":"not-ready"}));
    replies[3].1 = Reply::json(json!({}));
    let server = Server::new(replies);
    let result = envelope(&server.run(&["status"]), 5);
    assert_eq!(result["data"]["health"], "unknown");
    assert_eq!(result["data"]["readiness"], "failed");
    assert_eq!(result["data"]["registry"]["state"], "unknown");
}

#[test]
fn cli_brand_does_not_replace_the_existing_server_identity_contract() {
    let mut replies = status_replies();
    replies[0].1 = Reply::json(json!({"name":"different-service","version":"0.3.10"}));
    let server = Server::new(replies);
    let result = envelope(&server.run(&["status"]), 5);
    assert_eq!(result["data"]["identity"], "failed");
    assert!(result["data"]["server_version"].is_null());
}

#[test]
fn timeout_is_one_command_budget_and_unstarted_checks_stay_unknown() {
    let mut replies = status_replies();
    replies[1].1.delay_ms = 3000;
    let server = Server::new(replies);
    let started = std::time::Instant::now();
    let result = envelope(&server.run(&["status", "--timeout", "1"]), 5);
    assert!(started.elapsed() < std::time::Duration::from_secs(3));
    assert_eq!(result["data"]["identity"], "ok");
    assert_eq!(result["data"]["health"], "unknown");
    assert_eq!(result["data"]["readiness"], "unknown");
    assert_eq!(result["data"]["registry"]["state"], "unknown");
    assert_eq!(server.seen().len(), 2);
}
