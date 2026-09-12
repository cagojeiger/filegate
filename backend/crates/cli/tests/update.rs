#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod support;
use support::*;

#[test]
fn update_is_independent_of_server_configuration_and_rejects_unmanaged_install() {
    for args in [vec!["update"], vec!["update", "--check"]] {
        let output = bare()
            .args(args)
            .args(["--output", "json"])
            .env("GROVE_ENDPOINT", "invalid endpoint")
            .env("GROVE_OPERATOR_TOKEN", "invalid token")
            .args(["--token-file", "/absent/token"])
            .output()
            .unwrap();
        let result = envelope(&output, 2);
        assert!(result["endpoint"].is_null());
        assert_eq!(result["error"]["code"], "unmanaged_install");
        assert_eq!(result["error"]["outcome"], "not_applied");
    }
}

#[test]
fn update_help_is_local_and_installer_entry_is_hidden() {
    let output = bare().args(["update", "--help"]).output().unwrap();
    assert!(output.status.success());
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("--check")
    );
    let output = bare().arg("--help").output().unwrap();
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("update"));
    assert!(!help.contains("__install"));
}

#[cfg(unix)]
#[test]
fn broken_output_after_install_preserves_applied_exit_code() {
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::process::Stdio;

    let directory = tempfile::tempdir().unwrap();
    let (writer, reader) = UnixStream::pair().unwrap();
    drop(reader);
    let writer: OwnedFd = writer.into();
    let output = bare()
        .args(["__install", "--bin-dir"])
        .arg(directory.path())
        .args(["--output", "json"])
        .stdout(Stdio::from(writer))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(8));
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("CLI was changed")
    );
    assert!(
        directory
            .path()
            .join("gscli-install-receipt.json")
            .is_file()
    );
    let output = std::process::Command::new(directory.path().join("gscli"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(output.status.success());
}
