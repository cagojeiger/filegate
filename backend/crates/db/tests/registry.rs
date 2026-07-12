//! 등록부 라이프사이클 통합 테스트 — create/update/delete의 예외, FK 무결성,
//! write_violation 분류, 그리고 회계(예약·정산·해제). 테스트마다 격리 DB를
//! 만드는 `#[sqlx::test]`로 돌린다 (DATABASE_URL 필요, 없으면 매크로가 스킵).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use filegate_db::registry::{self, BindingRow, StorageRow, WriteOp, WriteViolation};
use sqlx::PgPool;

// ── 픽스처 ──────────────────────────────────────────────────

/// 유효한 클라이언트 키 해시 — `sha256:` + 64 hex (client_keys CHECK).
const HASH: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// fs storage — root_path 하나가 접근 계약의 전부, s3 필드는 전부 None,
/// force_relay는 반드시 false (storages_fs_fields CHECK).
fn fs_row(id: &str, capacity: i64) -> StorageRow {
    StorageRow {
        id: id.to_owned(),
        kind: "fs".to_owned(),
        force_relay: false,
        root_path: Some("/data".to_owned()),
        endpoint: None,
        public_endpoint: None,
        region: None,
        bucket: None,
        force_path_style: false,
        access_key: None,
        secret_key_ciphertext: None,
        secret_key_nonce: None,
        enc_key_id: None,
        capacity_bytes: capacity,
    }
}

fn s3_row(id: &str, capacity: i64) -> StorageRow {
    StorageRow {
        id: id.to_owned(),
        kind: "s3".to_owned(),
        force_relay: false,
        root_path: None,
        endpoint: Some("http://minio:9000".to_owned()),
        public_endpoint: Some("http://minio:9000".to_owned()),
        region: Some("us-east-1".to_owned()),
        bucket: Some("b".to_owned()),
        force_path_style: true,
        access_key: Some("ak".to_owned()),
        secret_key_ciphertext: Some(vec![1, 2, 3]),
        secret_key_nonce: Some(vec![0_u8; 12]),
        enc_key_id: Some("v1".to_owned()),
        capacity_bytes: capacity,
    }
}

// ── 인프라 검증용 최소 테스트 ────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn insert_storage_registers_the_row(pool: PgPool) {
    registry::insert_storage(&pool, &s3_row("st1", 1000))
        .await
        .unwrap();
    assert!(registry::get_storage(&pool, "st1").await.unwrap().is_some());
}

#[sqlx::test(migrations = "./migrations")]
async fn duplicate_storage_id_is_a_duplicate_violation(pool: PgPool) {
    registry::insert_storage(&pool, &s3_row("dup", 1000))
        .await
        .unwrap();
    let err = registry::insert_storage(&pool, &s3_row("dup", 1000))
        .await
        .unwrap_err();
    assert_eq!(
        registry::write_violation(&err, WriteOp::Insert),
        Some(WriteViolation::Duplicate)
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn binding_to_missing_client_is_missing_ref(pool: PgPool) {
    registry::insert_storage(&pool, &s3_row("st", 1000))
        .await
        .unwrap();
    let err = registry::insert_binding(
        &pool,
        &BindingRow {
            client_id: "ghost".to_owned(),
            intent: "att".to_owned(),
            storage_id: "st".to_owned(),
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(
        registry::write_violation(&err, WriteOp::Insert),
        Some(WriteViolation::MissingRef(_))
    ));
}

// ── create / CHECK 위반 ──────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn fs_storage_registers(pool: PgPool) {
    registry::insert_storage(&pool, &fs_row("nas", 1000))
        .await
        .unwrap();
    let got = registry::get_storage(&pool, "nas").await.unwrap().unwrap();
    assert_eq!(got.kind, "fs");
    assert_eq!(got.root_path.as_deref(), Some("/data"));
    assert!(got.bucket.is_none());
}

#[sqlx::test(migrations = "./migrations")]
async fn bad_slug_id_is_invalid(pool: PgPool) {
    let err = registry::insert_storage(&pool, &s3_row("Bad_Id", 1000))
        .await
        .unwrap_err();
    assert_eq!(
        registry::write_violation(&err, WriteOp::Insert),
        Some(WriteViolation::Invalid)
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn negative_capacity_is_invalid(pool: PgPool) {
    let err = registry::insert_storage(&pool, &s3_row("neg", -1))
        .await
        .unwrap_err();
    assert_eq!(
        registry::write_violation(&err, WriteOp::Insert),
        Some(WriteViolation::Invalid)
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn s3_without_bucket_is_invalid(pool: PgPool) {
    let mut row = s3_row("nobucket", 1000);
    row.bucket = None;
    let err = registry::insert_storage(&pool, &row).await.unwrap_err();
    assert_eq!(
        registry::write_violation(&err, WriteOp::Insert),
        Some(WriteViolation::Invalid)
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn bad_key_hash_format_is_invalid(pool: PgPool) {
    registry::insert_client(&pool, "c").await.unwrap();
    let err = registry::insert_client_key(&pool, "c", "nothash")
        .await
        .unwrap_err();
    assert_eq!(
        registry::write_violation(&err, WriteOp::Insert),
        Some(WriteViolation::Invalid)
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn duplicate_client_id_is_a_duplicate_violation(pool: PgPool) {
    registry::insert_client(&pool, "c").await.unwrap();
    let err = registry::insert_client(&pool, "c").await.unwrap_err();
    assert_eq!(
        registry::write_violation(&err, WriteOp::Insert),
        Some(WriteViolation::Duplicate)
    );
}

// ── clients / keys ───────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn key_hash_resolves_to_owning_client(pool: PgPool) {
    registry::insert_client(&pool, "c").await.unwrap();
    registry::insert_client_key(&pool, "c", HASH).await.unwrap();
    assert_eq!(
        registry::client_id_for_key_hash(&pool, HASH).await.unwrap(),
        Some("c".to_owned())
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn key_for_missing_client_is_missing_ref(pool: PgPool) {
    let err = registry::insert_client_key(&pool, "ghost", HASH)
        .await
        .unwrap_err();
    assert!(matches!(
        registry::write_violation(&err, WriteOp::Insert),
        Some(WriteViolation::MissingRef(_))
    ));
}

#[sqlx::test(migrations = "./migrations")]
async fn client_key_exists_checks_ownership(pool: PgPool) {
    registry::insert_client(&pool, "owner").await.unwrap();
    registry::insert_client(&pool, "other").await.unwrap();
    registry::insert_client_key(&pool, "owner", HASH)
        .await
        .unwrap();
    assert!(registry::client_key_exists(&pool, "owner", HASH)
        .await
        .unwrap());
    assert!(!registry::client_key_exists(&pool, "other", HASH)
        .await
        .unwrap());
}

#[sqlx::test(migrations = "./migrations")]
async fn deleting_client_cascades_its_keys(pool: PgPool) {
    registry::insert_client(&pool, "c").await.unwrap();
    registry::insert_client_key(&pool, "c", HASH).await.unwrap();
    registry::delete_client(&pool, "c").await.unwrap();
    assert!(registry::list_client_keys(&pool, "c")
        .await
        .unwrap()
        .is_empty());
    assert!(!registry::client_key_exists(&pool, "c", HASH).await.unwrap());
}

// ── bindings / FK ────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn binding_to_missing_storage_is_missing_ref(pool: PgPool) {
    registry::insert_client(&pool, "c").await.unwrap();
    let err = registry::insert_binding(
        &pool,
        &BindingRow {
            client_id: "c".to_owned(),
            intent: "att".to_owned(),
            storage_id: "ghost".to_owned(),
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(
        registry::write_violation(&err, WriteOp::Insert),
        Some(WriteViolation::MissingRef(_))
    ));
}

#[sqlx::test(migrations = "./migrations")]
async fn binding_round_trip_and_repoint(pool: PgPool) {
    registry::insert_client(&pool, "c").await.unwrap();
    registry::insert_storage(&pool, &s3_row("s1", 1000))
        .await
        .unwrap();
    registry::insert_storage(&pool, &s3_row("s2", 1000))
        .await
        .unwrap();
    let mut binding = BindingRow {
        client_id: "c".to_owned(),
        intent: "att".to_owned(),
        storage_id: "s1".to_owned(),
    };
    registry::insert_binding(&pool, &binding).await.unwrap();
    assert_eq!(
        registry::get_binding(&pool, "c", "att")
            .await
            .unwrap()
            .unwrap()
            .storage_id,
        "s1"
    );

    binding.storage_id = "s2".to_owned();
    assert!(registry::update_binding(&pool, &binding).await.unwrap());
    assert_eq!(
        registry::get_binding(&pool, "c", "att")
            .await
            .unwrap()
            .unwrap()
            .storage_id,
        "s2"
    );

    let absent = BindingRow {
        client_id: "c".to_owned(),
        intent: "nope".to_owned(),
        storage_id: "s2".to_owned(),
    };
    assert!(!registry::update_binding(&pool, &absent).await.unwrap());
}

#[sqlx::test(migrations = "./migrations")]
async fn repointing_binding_to_missing_storage_is_missing_ref(pool: PgPool) {
    registry::insert_client(&pool, "c").await.unwrap();
    registry::insert_storage(&pool, &s3_row("s1", 1000))
        .await
        .unwrap();
    let mut binding = BindingRow {
        client_id: "c".to_owned(),
        intent: "att".to_owned(),
        storage_id: "s1".to_owned(),
    };
    registry::insert_binding(&pool, &binding).await.unwrap();
    binding.storage_id = "ghost".to_owned();
    let err = registry::update_binding(&pool, &binding).await.unwrap_err();
    assert!(matches!(
        registry::write_violation(&err, WriteOp::Insert),
        Some(WriteViolation::MissingRef(_))
    ));
}

// ── delete 가드 (RESTRICT) ───────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn client_with_binding_cannot_be_deleted(pool: PgPool) {
    registry::insert_client(&pool, "c").await.unwrap();
    registry::insert_storage(&pool, &s3_row("s", 1000))
        .await
        .unwrap();
    registry::insert_binding(
        &pool,
        &BindingRow {
            client_id: "c".to_owned(),
            intent: "att".to_owned(),
            storage_id: "s".to_owned(),
        },
    )
    .await
    .unwrap();
    let err = registry::delete_client(&pool, "c").await.unwrap_err();
    assert_eq!(
        registry::write_violation(&err, WriteOp::Delete),
        Some(WriteViolation::InUse)
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn storage_with_binding_cannot_be_deleted(pool: PgPool) {
    registry::insert_client(&pool, "c").await.unwrap();
    registry::insert_storage(&pool, &s3_row("s", 1000))
        .await
        .unwrap();
    registry::insert_binding(
        &pool,
        &BindingRow {
            client_id: "c".to_owned(),
            intent: "att".to_owned(),
            storage_id: "s".to_owned(),
        },
    )
    .await
    .unwrap();
    let err = registry::delete_storage(&pool, "s").await.unwrap_err();
    assert_eq!(
        registry::write_violation(&err, WriteOp::Delete),
        Some(WriteViolation::InUse)
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn deleting_binding_frees_both_nodes(pool: PgPool) {
    registry::insert_client(&pool, "c").await.unwrap();
    registry::insert_storage(&pool, &s3_row("s", 1000))
        .await
        .unwrap();
    registry::insert_binding(
        &pool,
        &BindingRow {
            client_id: "c".to_owned(),
            intent: "att".to_owned(),
            storage_id: "s".to_owned(),
        },
    )
    .await
    .unwrap();
    registry::delete_binding(&pool, "c", "att").await.unwrap();
    registry::delete_storage(&pool, "s").await.unwrap();
    registry::delete_client(&pool, "c").await.unwrap();
    assert!(registry::get_storage(&pool, "s").await.unwrap().is_none());
    assert!(!registry::client_exists(&pool, "c").await.unwrap());
}

#[sqlx::test(migrations = "./migrations")]
async fn deleting_absent_ids_is_idempotent(pool: PgPool) {
    registry::delete_storage(&pool, "ghost").await.unwrap();
    registry::delete_client(&pool, "ghost").await.unwrap();
}
