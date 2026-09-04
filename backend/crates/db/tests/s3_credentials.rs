//! S3 surface DB integration tests.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

#[path = "support/s3_single.rs"]
mod support;

use filegate_db::s3_registry as s3;
use sqlx::PgPool;
use support::{CT, add_cred, wire};

// ── 자격증명 ────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn credential_maps_access_key_to_client(pool: PgPool) {
    wire(&pool).await;
    add_cred(&pool, "fgak0123456789abcdef", "c").await.unwrap();
    let found = s3::get_credential(&pool, "fgak0123456789abcdef")
        .await
        .unwrap()
        .unwrap();
    // 검증이 복호할 재료가 그대로 돌아온다 (client + 암호문 셋).
    assert_eq!(found.client_id, "c");
    assert_eq!(found.secret_ciphertext, CT);
    assert_eq!(found.enc_key_id, "v1");
    // 모르는 access key는 None — 403의 재료.
    assert!(
        s3::get_credential(&pool, "fgakffffffffffffffff")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        s3::list_credentials(&pool, "c").await.unwrap(),
        vec!["fgak0123456789abcdef".to_owned()]
    );
    // 폐기 — 멱등: 두 번째는 0행.
    assert_eq!(
        s3::delete_credential(&pool, "c", "fgak0123456789abcdef")
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        s3::delete_credential(&pool, "c", "fgak0123456789abcdef")
            .await
            .unwrap(),
        0
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn credential_requires_registered_client_and_slug_form(pool: PgPool) {
    wire(&pool).await;
    // 미등록 client — FK가 거부한다.
    assert!(
        add_cred(&pool, "fgak0123456789abcdef", "ghost")
            .await
            .is_err()
    );
    // 형태 위반(대문자) — CHECK가 거부한다.
    assert!(add_cred(&pool, "FGAK0123456789ABCDEF", "c").await.is_err());
}
