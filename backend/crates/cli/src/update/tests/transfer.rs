use super::*;

#[tokio::test]
async fn corrupt_and_wrong_version_candidates_preserve_old_binary() {
    for kind in ["checksum", "short", "long", "version", "exit"] {
        let install = Installation::new("1.0.0");
        let mut bytes = script("2.0.0");
        let mut metadata = manifest("2.0.0", &bytes);
        let asset = &mut metadata["assets"][target().unwrap()];
        match kind {
            "checksum" => asset["sha256"] = json!("0".repeat(64)),
            "short" => asset["size"] = json!(bytes.len() + 1),
            "long" => asset["size"] = json!(bytes.len() - 1),
            "version" => {
                bytes = script("9.0.0");
                metadata = manifest("2.0.0", &bytes);
            }
            _ => {
                bytes = b"#!/bin/sh\nexit 1\n".to_vec();
                metadata = manifest("2.0.0", &bytes);
            }
        }
        let server = Server::new(metadata, bytes).await;
        let error = update(&install.binary, false, &server.source())
            .await
            .err()
            .unwrap();
        assert_eq!(error.outcome, "not_applied", "{kind}");
        assert_eq!(
            fs::read(&install.binary).unwrap(),
            script("1.0.0"),
            "{kind}"
        );
        install.assert_no_candidates();
    }
}

#[test]
fn manifest_rejects_wrong_origin_platform_schema_and_unstable_versions() {
    let bytes = script("2.0.0");
    for kind in [
        "schema",
        "repository",
        "missing",
        "name",
        "hash",
        "size",
        "zero",
        "prerelease",
        "build",
        "leading_zero",
    ] {
        let mut metadata = manifest("2.0.0", &bytes);
        match kind {
            "schema" => metadata["schema_version"] = json!(2),
            "repository" => metadata["repository"] = json!("attacker/other"),
            "missing" => metadata["assets"] = json!({}),
            "name" => metadata["assets"][target().unwrap()]["name"] = json!("../bad"),
            "hash" => metadata["assets"][target().unwrap()]["sha256"] = json!("bad"),
            "size" => metadata["assets"][target().unwrap()]["size"] = json!(MAX_BINARY + 1),
            "zero" => metadata["assets"][target().unwrap()]["size"] = json!(0),
            "prerelease" => metadata["version"] = json!("2.0.0-beta.1"),
            "build" => metadata["version"] = json!("2.0.0+test"),
            _ => metadata["version"] = json!("02.0.0"),
        }
        let parsed: Manifest = serde_json::from_value(metadata).unwrap();
        assert!(parsed.validate(target().unwrap()).is_err(), "{kind}");
    }
}

#[tokio::test]
async fn invalid_large_rejected_or_redirected_metadata_never_replaces_binary() {
    for (bytes, status, location, expected) in [
        (b"not json".to_vec(), 200, None, "invalid_update_manifest"),
        (vec![b' '; MAX_MANIFEST + 1], 200, None, "update_too_large"),
        (
            b"sensitive upstream error".to_vec(),
            403,
            None,
            "update_download_rejected",
        ),
        (
            vec![],
            302,
            Some("https://example.invalid/evil"),
            "update_download_rejected",
        ),
        (
            vec![],
            302,
            Some("http://github.com/evil"),
            "update_download_rejected",
        ),
    ] {
        let install = Installation::new("1.0.0");
        let server = Server::custom(bytes, vec![], status, location, Duration::ZERO).await;
        let error = update(&install.binary, false, &server.source())
            .await
            .err()
            .unwrap();
        assert_eq!(error.code, expected);
        assert!(!error.message.contains("sensitive"));
        assert_eq!(fs::read(&install.binary).unwrap(), script("1.0.0"));
        assert_eq!(server.seen.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn network_timeout_leaves_installation_and_releases_lock() {
    let install = Installation::new("1.0.0");
    let server = Server::custom(vec![], vec![], 200, None, Duration::from_secs(2)).await;
    let source = download::Source::fixture(&server.base, 1).unwrap();
    let error = update(&install.binary, false, &source).await.err().unwrap();
    assert_eq!(error.code, "timeout");
    assert_eq!(fs::read(&install.binary).unwrap(), script("1.0.0"));
    assert!(storage::lock(&install.binary).is_ok());
    install.assert_no_candidates();
}

#[tokio::test]
async fn streaming_download_limit_is_enforced_without_content_length() {
    use tokio::io::AsyncWriteExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0; 1024];
        assert!(socket.read(&mut request).await.unwrap() > 0);
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nabcde\r\n0\r\n\r\n",
            )
            .await
            .unwrap();
    });
    let source = download::Source::fixture(&base, 5).unwrap();
    assert_eq!(
        source.get(&base, 4).await.err().unwrap().code,
        "update_too_large"
    );
    task.await.unwrap();
}
