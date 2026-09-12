use super::*;

#[tokio::test]
#[ignore = "requires GSCLI_TEST_BINARY pointing to the native release artifact"]
async fn native_release_artifact_updates_and_runs_with_existing_receipt() {
    let binary = std::env::var_os("GSCLI_TEST_BINARY").expect("native release artifact required");
    let bytes = fs::read(binary).unwrap();
    let version = env!("CARGO_PKG_VERSION");
    let server = Server::new(manifest(version, &bytes), bytes).await;
    let install = Installation::new("0.0.0");
    let result = update(&install.binary, false, &server.source())
        .await
        .unwrap();
    assert_eq!(result.status, "updated");
    assert_eq!(
        executable_version(&install.binary, Duration::from_secs(5))
            .await
            .unwrap(),
        Version::parse(version).unwrap()
    );
    storage::validate_receipt(&install.binary, target().unwrap()).unwrap();
    install.assert_no_candidates();
}

#[tokio::test]
async fn check_only_preserves_installation_and_never_downloads_binary() {
    let install = Installation::new("1.0.0");
    let bytes = script("2.0.0");
    let server = Server::new(manifest("2.0.0", &bytes), bytes).await;
    let before = fs::read(&install.binary).unwrap();
    let receipt = fs::read(install.receipt()).unwrap();
    let result = update(&install.binary, true, &server.source())
        .await
        .unwrap();
    assert_eq!(result.status, "update_available");
    assert!(!result.updated);
    assert_eq!(result.current_version, "1.0.0");
    assert_eq!(fs::read(&install.binary).unwrap(), before);
    assert_eq!(fs::read(install.receipt()).unwrap(), receipt);
    assert_eq!(
        *server.seen.lock().unwrap(),
        [("/manifest".to_owned(), false)]
    );
    install.assert_no_candidates();
}

#[tokio::test]
async fn update_replaces_binary_keeps_static_receipt_and_pins_asset_version() {
    let install = Installation::new("1.0.0");
    let bytes = script("2.0.0");
    let server = Server::new(manifest("2.0.0", &bytes), bytes.clone()).await;
    let receipt = fs::read(install.receipt()).unwrap();
    let result = update(&install.binary, false, &server.source())
        .await
        .unwrap();
    assert_eq!(result.status, "updated");
    assert!(result.updated);
    assert_eq!(fs::read(&install.binary).unwrap(), bytes);
    assert_eq!(fs::read(install.receipt()).unwrap(), receipt);
    assert_eq!(
        executable_version(&install.binary, Duration::from_secs(1))
            .await
            .unwrap(),
        Version::new(2, 0, 0)
    );
    assert_eq!(
        *server.seen.lock().unwrap(),
        [
            ("/manifest".to_owned(), false),
            (format!("/v2.0.0/gscli-{}", target().unwrap()), false)
        ]
    );
    install.assert_no_candidates();
}

#[tokio::test]
async fn on_disk_version_prevents_stale_process_downgrade_or_reinstall() {
    for latest in ["2.0.0", "3.0.0"] {
        let install = Installation::new("3.0.0");
        let bytes = script(latest);
        let server = Server::new(manifest(latest, &bytes), bytes).await;
        let result = update(&install.binary, false, &server.source())
            .await
            .unwrap();
        assert_eq!(result.status, "up_to_date");
        assert_eq!(result.current_version, "3.0.0");
        assert!(!result.updated);
        assert_eq!(server.seen.lock().unwrap().len(), 1);
        assert_eq!(fs::read(&install.binary).unwrap(), script("3.0.0"));
    }
}

#[tokio::test]
async fn updater_and_installer_share_lock_and_release_on_drop() {
    let install = Installation::new("1.0.0");
    let bytes = script("2.0.0");
    let server = Server::new(manifest("2.0.0", &bytes), bytes).await;
    let guard = storage::lock(&install.binary).unwrap();
    let error = update(&install.binary, false, &server.source())
        .await
        .err()
        .unwrap();
    assert_eq!(error.code, "update_in_progress");
    assert_eq!(error.exit, 6);
    assert!(server.seen.lock().unwrap().is_empty());
    let error = storage::install(
        &install.binary,
        install.binary.parent().unwrap(),
        target().unwrap(),
    )
    .err()
    .unwrap();
    assert_eq!(error.code, "update_in_progress");
    drop(guard);
    assert!(
        update(&install.binary, false, &server.source())
            .await
            .unwrap()
            .updated
    );
}

#[tokio::test]
async fn missing_wrong_and_symlink_receipts_make_no_network_requests() {
    let bytes = script("2.0.0");
    let server = Server::new(manifest("2.0.0", &bytes), bytes).await;
    for kind in [
        "missing",
        "path",
        "repository",
        "target",
        "symlink",
        "oversized",
    ] {
        let install = Installation::new("1.0.0");
        let path = install.receipt();
        let mut receipt: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        match kind {
            "missing" => fs::remove_file(&path).unwrap(),
            "symlink" => {
                fs::remove_file(&path).unwrap();
                std::os::unix::fs::symlink(&install.binary, &path).unwrap();
            }
            "oversized" => fs::write(&path, vec![b' '; 16385]).unwrap(),
            field => {
                let field = if field == "path" {
                    "install_path"
                } else {
                    field
                };
                receipt[field] = json!("invalid");
                fs::write(&path, serde_json::to_vec(&receipt).unwrap()).unwrap();
            }
        }
        let error = update(&install.binary, true, &server.source())
            .await
            .err()
            .unwrap();
        assert_eq!(error.code, "unmanaged_install", "{kind}");
    }
    assert!(server.seen.lock().unwrap().is_empty());
}

#[test]
fn installer_rejects_symlink_receipt_before_replacing_binary() {
    let install = Installation::new("1.0.0");
    fs::remove_file(install.receipt()).unwrap();
    std::os::unix::fs::symlink(&install.binary, install.receipt()).unwrap();
    let error = storage::install(
        &install.binary,
        install.binary.parent().unwrap(),
        target().unwrap(),
    )
    .err()
    .unwrap();
    assert_eq!(error.code, "unmanaged_install");
    assert_eq!(fs::read(&install.binary).unwrap(), script("1.0.0"));
}

#[tokio::test]
async fn hanging_and_noisy_version_probes_are_bounded() {
    let install = Installation::new("1.0.0");
    for script in [
        "#!/bin/sh\nwhile :; do :; done\n",
        "#!/bin/sh\nwhile :; do echo noisy; done\n",
    ] {
        fs::write(&install.binary, script).unwrap();
        fs::set_permissions(&install.binary, fs::Permissions::from_mode(0o755)).unwrap();
        let error = executable_version(&install.binary, Duration::from_millis(50))
            .await
            .err()
            .unwrap();
        assert_eq!(error.code, "update_probe_failed");
    }
}
