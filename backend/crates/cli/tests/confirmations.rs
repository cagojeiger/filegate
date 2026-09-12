#![allow(clippy::unwrap_used)]
mod support;

use support::*;

#[test]
fn destructive_commands_require_yes_without_a_tty() {
    let server = Server::new(vec![]);
    let input = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(
        input.path(),
        r#"{"kind":"fs","root_path":"/tmp","capacity_bytes":1}"#,
    )
    .unwrap();
    let replace_path = input.path().to_str().unwrap();
    let hash = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    for args in [
        vec!["storage", "delete", "archive"],
        vec!["storage", "replace", "archive", "--from", replace_path],
        vec!["client", "delete", "app"],
        vec!["credential", "delete", "--client", "app", "fgakpublic"],
        vec!["client-key", "delete", "--client", "app", hash],
    ] {
        let result = envelope(&server.run(&args), 2);
        assert_eq!(at(&result, "/error/outcome"), "not_applied");
    }
    assert!(server.seen().is_empty());
}
