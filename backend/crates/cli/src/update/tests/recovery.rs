use super::*;
use std::cell::Cell;

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Operation {
    Sync,
    Receipt,
}

thread_local! {
    // Synchronous storage calls stay on the test thread; parallel tests are isolated.
    static FAILURE: Cell<Option<(Operation, usize)>> = const { Cell::new(None) };
}

struct FailOnce;

impl FailOnce {
    fn on(operation: Operation, call: usize) -> Self {
        assert!(call > 0);
        FAILURE.with(|failure| {
            assert!(failure.get().is_none());
            failure.set(Some((operation, call)));
        });
        Self
    }
}

impl Drop for FailOnce {
    fn drop(&mut self) {
        FAILURE.with(|failure| failure.set(None));
    }
}

pub(crate) fn fail_if_requested(operation: Operation) -> std::io::Result<()> {
    FAILURE.with(|failure| match failure.get() {
        Some((expected, call)) if expected == operation => {
            if call == 1 {
                failure.set(None);
                Err(std::io::Error::other("injected installation failure"))
            } else {
                failure.set(Some((expected, call - 1)));
                Ok(())
            }
        }
        _ => Ok(()),
    })
}

fn assert_applied(error: &Error) {
    assert_eq!(error.code, "update_applied_incomplete");
    assert_eq!(error.exit, 8);
    assert_eq!(error.outcome, "applied");
    FAILURE.with(|failure| assert!(failure.get().is_none()));
}

#[tokio::test]
async fn update_sync_failure_reports_applied_and_reinstall_recovers() {
    let install = Installation::new("1.0.0");
    let bytes = script("2.0.0");
    let receipt = fs::read(install.receipt()).unwrap();
    let server = Server::new(manifest("2.0.0", &bytes), bytes.clone()).await;
    let fault = FailOnce::on(Operation::Sync, 1);
    let error = update(&install.binary, false, &server.source())
        .await
        .err()
        .unwrap();
    assert_applied(&error);
    drop(fault);
    assert_eq!(fs::read(&install.binary).unwrap(), bytes);
    assert_eq!(fs::read(install.receipt()).unwrap(), receipt);
    install.assert_no_candidates();

    storage::install(
        &install.binary,
        install.binary.parent().unwrap(),
        target().unwrap(),
    )
    .unwrap();
    storage::validate_receipt(&install.binary, target().unwrap()).unwrap();
    assert_eq!(
        update(&install.binary, true, &server.source())
            .await
            .unwrap()
            .status,
        "up_to_date"
    );
    install.assert_no_candidates();
}

#[test]
fn installer_post_replace_failures_report_applied_and_reinstall_recovers() {
    for existing in [false, true] {
        for (operation, call) in [
            (Operation::Sync, 1),
            (Operation::Receipt, 1),
            (Operation::Sync, 2),
        ] {
            let install = Installation::new("1.0.0");
            if !existing {
                fs::remove_file(&install.binary).unwrap();
                fs::remove_file(install.receipt()).unwrap();
            }
            let source = install._root.path().join("source");
            let bytes = script("2.0.0");
            fs::write(&source, &bytes).unwrap();
            let directory = install.binary.parent().unwrap();
            let fault = FailOnce::on(operation, call);
            let error = storage::install(&source, directory, target().unwrap()).unwrap_err();
            assert_applied(&error);
            drop(fault);
            assert_eq!(fs::read(&install.binary).unwrap(), bytes);
            assert_eq!(install.receipt().exists(), existing || call == 2);
            let lock = storage::lock(&install.binary).unwrap();
            drop(lock);
            let entries = fs::read_dir(directory).unwrap().count();
            assert_eq!(entries, if install.receipt().exists() { 3 } else { 2 });

            storage::install(&source, directory, target().unwrap()).unwrap();
            storage::validate_receipt(&install.binary, target().unwrap()).unwrap();
            let output = std::process::Command::new(&install.binary)
                .arg("--version")
                .output()
                .unwrap();
            assert!(output.status.success());
            assert_eq!(output.stdout, b"gscli 2.0.0\n");
            install.assert_no_candidates();
        }
    }
}
