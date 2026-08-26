//! S3 호환 표면 등록부 통합 테스트 (spec 03) — 자격증명 매핑과 논리 키의
//! 덮어쓰기·정리 시맨틱. 테스트마다 격리 DB(`#[sqlx::test]`).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use filegate_db::files::{self, CreateOutcome, CreateSpec, CreatedFile};
use filegate_db::registry::{self, StorageRow};
use filegate_db::s3_registry as s3;

// db 계층은 암호문을 저장만 한다 (복호는 core::Crypto). 더미 암호 재료.
const CT: &[u8] = &[1, 2, 3, 4];
const NONCE: &[u8] = &[0_u8; 12];

async fn add_cred(pool: &PgPool, access_key_id: &str, client_id: &str) -> Result<(), sqlx::Error> {
    s3::insert_credential(pool, access_key_id, client_id, CT, NONCE, "v1").await
}
use sqlx::PgPool;

// ── 픽스처 ──────────────────────────────────────────────────

fn s3_row(id: &str) -> StorageRow {
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
        capacity_bytes: 1000,
    }
}

async fn wire(pool: &PgPool) {
    registry::insert_storage(pool, &s3_row("s")).await.unwrap();
    registry::insert_client(pool, "c", "s").await.unwrap();
}

async fn create_ok(pool: &PgPool) -> CreatedFile {
    let spec = CreateSpec {
        client_id: "c",
        declared_size: 100,
        content_type: None,
        declared_md5: None,
        lease_ttl_secs: 900,
        part_size: None,
    };
    match files::create(pool, spec).await.unwrap() {
        CreateOutcome::Created(created) => *created,
        CreateOutcome::NoClient => panic!("expected Created, got NoClient"),
    }
}

async fn create_s3_upload(pool: &PgPool, key: &str) -> CreatedFile {
    let spec = CreateSpec {
        client_id: "c",
        declared_size: 100,
        content_type: None,
        declared_md5: None,
        lease_ttl_secs: 900,
        part_size: None,
    };
    match s3::create_upload(pool, spec, key).await.unwrap() {
        CreateOutcome::Created(created) => *created,
        CreateOutcome::NoClient => panic!("expected Created, got NoClient"),
    }
}

async fn claim_single(pool: &PgPool, key: &str, file_id: uuid::Uuid, etag: &str) {
    assert_eq!(
        s3::claim_completion(
            pool,
            s3::CompletionSpec {
                client_id: "c",
                key,
                file_id,
                multipart: false,
                expected_size: 100,
                expected_etag: etag,
                lease_ttl_secs: 900,
            },
        )
        .await
        .unwrap(),
        s3::CompletionClaim::Claimed
    );
}

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

// ── 진행 중 S3 업로드 ─────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn single_upload_is_bound_and_excluded_from_observer(pool: PgPool) {
    wire(&pool).await;
    let upload = create_s3_upload(&pool, "dir/a.bin").await;

    assert!(
        s3::upload_matches(&pool, "c", "dir/a.bin", upload.file_id, false)
            .await
            .unwrap()
    );
    assert!(
        !s3::upload_matches(&pool, "c", "dir/b.bin", upload.file_id, false)
            .await
            .unwrap()
    );
    assert!(
        !s3::upload_matches(&pool, "c", "dir/a.bin", upload.file_id, true)
            .await
            .unwrap()
    );
    // S3 PutObject는 요청 경로가 키 매핑까지 확정한다. generic observer나
    // generic finalize가 먼저 active로 만들 수 없다.
    assert!(
        files::observed_commit_candidates(&pool, 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        !files::finalize_commit(&pool, upload.file_id, "etag")
            .await
            .unwrap()
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn single_finalize_maps_key_and_detaches_old_file_atomically(pool: PgPool) {
    wire(&pool).await;
    let old = create_ok(&pool).await;
    files::finalize_commit(&pool, old.file_id, "old-etag")
        .await
        .unwrap();
    s3::upsert_key(&pool, "c", "k", old.file_id).await.unwrap();

    let upload = create_s3_upload(&pool, "k").await;
    claim_single(&pool, "k", upload.file_id, "new-etag").await;
    let outcome = s3::finalize_single_upload(&pool, "c", "k", upload.file_id)
        .await
        .unwrap();
    assert_eq!(
        outcome,
        s3::FinalizeOutcome::Finalized {
            displaced: Some(old.file_id)
        }
    );
    assert_eq!(
        s3::get_key(&pool, "c", "k").await.unwrap(),
        Some(upload.file_id)
    );
    assert_eq!(file_state(&pool, old.file_id).await, "deleted");
    assert_eq!(file_state(&pool, upload.file_id).await, "active");
    let session_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM s3_uploads WHERE file_id = $1")
            .bind(upload.file_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(session_count, 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn wrong_key_finalize_leaves_upload_pending_and_unmapped(pool: PgPool) {
    wire(&pool).await;
    let upload = create_s3_upload(&pool, "bound-key").await;

    assert_eq!(
        s3::claim_completion(
            &pool,
            s3::CompletionSpec {
                client_id: "c",
                key: "other-key",
                file_id: upload.file_id,
                multipart: false,
                expected_size: 100,
                expected_etag: "etag",
                lease_ttl_secs: 900,
            },
        )
        .await
        .unwrap(),
        s3::CompletionClaim::Unavailable
    );
    assert_eq!(
        s3::finalize_single_upload(&pool, "c", "other-key", upload.file_id)
            .await
            .unwrap(),
        s3::FinalizeOutcome::NotPending
    );
    assert_eq!(file_state(&pool, upload.file_id).await, "pending");
    assert!(
        s3::get_key(&pool, "c", "bound-key")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        s3::get_key(&pool, "c", "other-key")
            .await
            .unwrap()
            .is_none()
    );

    // generic 회수도 S3 세션을 지울 수 없다. 외부 작업 전임을 아는 S3 create
    // 정리만 session/location을 함께 제거한다.
    assert!(!files::reclaim_pending(&pool, upload.file_id).await.unwrap());
    assert!(
        s3::discard_unstarted_upload(&pool, upload.file_id)
            .await
            .unwrap()
    );
    let session_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM s3_uploads WHERE file_id = $1")
            .bind(upload.file_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(session_count, 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn concurrent_first_put_keeps_only_the_mapped_file_active(pool: PgPool) {
    wire(&pool).await;
    let a = create_s3_upload(&pool, "same-new-key").await;
    let b = create_s3_upload(&pool, "same-new-key").await;
    claim_single(&pool, "same-new-key", a.file_id, "etag-a").await;
    claim_single(&pool, "same-new-key", b.file_id, "etag-b").await;

    let (a_outcome, b_outcome) = tokio::join!(
        s3::finalize_single_upload(&pool, "c", "same-new-key", a.file_id),
        s3::finalize_single_upload(&pool, "c", "same-new-key", b.file_id),
    );
    assert!(matches!(
        a_outcome.unwrap(),
        s3::FinalizeOutcome::Finalized { .. }
    ));
    assert!(matches!(
        b_outcome.unwrap(),
        s3::FinalizeOutcome::Finalized { .. }
    ));

    let mapped = s3::get_key(&pool, "c", "same-new-key")
        .await
        .unwrap()
        .unwrap();
    let other = if mapped == a.file_id {
        b.file_id
    } else {
        a.file_id
    };
    assert_eq!(file_state(&pool, mapped).await, "active");
    assert_eq!(file_state(&pool, other).await, "deleted");
}

#[sqlx::test(migrations = "./migrations")]
async fn failed_single_completion_retains_cleanup_material(pool: PgPool) {
    wire(&pool).await;
    let upload = create_s3_upload(&pool, "retry-key").await;
    claim_single(&pool, "retry-key", upload.file_id, "etag").await;

    // 외부 PUT 뒤 DB finalize 실패를 모사해 completing 상태로 둔다. lease가
    // 지나면 reconciler가 예상값과 location을 모두 가진 후보를 다시 얻는다.
    sqlx::query("UPDATE leases SET expires_at = now() - interval '1 hour' WHERE file_id = $1")
        .bind(upload.file_id)
        .execute(&pool)
        .await
        .unwrap();
    let completions = s3::completion_candidates(&pool, 10).await.unwrap();
    assert_eq!(completions.len(), 1);
    assert_eq!(completions[0].file_id, upload.file_id);
    assert_eq!(completions[0].expected_size, 100);
    assert_eq!(completions[0].expected_etag, "etag");

    // 실물이 없거나 불일치한 경우에도 바로 DB 정보를 버리지 않고 aborting
    // cleanup 후보로 옮긴다. 물리 Delete 성공 뒤 finalize_abort가 끝낸다.
    assert!(
        s3::mark_completion_aborting(&pool, upload.file_id)
            .await
            .unwrap()
    );
    let cleanup = s3::cleanup_candidates(&pool, 10).await.unwrap();
    assert_eq!(cleanup.len(), 1);
    assert_eq!(cleanup[0].file_id, upload.file_id);
    assert!(s3::finalize_abort(&pool, upload.file_id).await.unwrap());
    assert_eq!(file_state(&pool, upload.file_id).await, "reclaimed");
}

// ── 논리 키 매핑 ────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn key_overwrite_returns_displaced_file(pool: PgPool) {
    wire(&pool).await;
    let first = create_ok(&pool).await;
    let second = create_ok(&pool).await;
    // 첫 매핑 — 밀려난 파일 없음.
    assert!(
        s3::upsert_key(&pool, "c", "dir/a.bin", first.file_id)
            .await
            .unwrap()
            .is_none()
    );
    // 같은 file_id 재기록은 덮어쓰기가 아니다 — None.
    assert!(
        s3::upsert_key(&pool, "c", "dir/a.bin", first.file_id)
            .await
            .unwrap()
            .is_none()
    );
    // 다른 file로 교체 — 밀려난 옛 file_id가 돌아온다 (delete 결정의 재료).
    assert_eq!(
        s3::upsert_key(&pool, "c", "dir/a.bin", second.file_id)
            .await
            .unwrap(),
        Some(first.file_id)
    );
    assert_eq!(
        s3::get_key(&pool, "c", "dir/a.bin").await.unwrap(),
        Some(second.file_id)
    );
    // 제거 — 지워진 file_id 반환, 멱등.
    assert_eq!(
        s3::delete_key(&pool, "c", "dir/a.bin").await.unwrap(),
        Some(second.file_id)
    );
    assert!(
        s3::delete_key(&pool, "c", "dir/a.bin")
            .await
            .unwrap()
            .is_none()
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn overwrite_and_delete_detach_the_active_file(pool: PgPool) {
    // overwrite/delete는 밀려난·지워진 active file을 같은 트랜잭션에서
    // detach한다 — 매핑만 바뀌고 파일이 active로 남으면 도달 불가 고아가 된다.
    wire(&pool).await;
    let a = create_ok(&pool).await;
    let b = create_ok(&pool).await;
    files::finalize_commit(&pool, a.file_id, "etag-a")
        .await
        .unwrap();
    files::finalize_commit(&pool, b.file_id, "etag-b")
        .await
        .unwrap();

    s3::upsert_key(&pool, "c", "k", a.file_id).await.unwrap();
    // A를 B로 덮어쓰면 A가 detach된다 (B는 그대로 active).
    assert_eq!(
        s3::upsert_key(&pool, "c", "k", b.file_id).await.unwrap(),
        Some(a.file_id)
    );
    assert_eq!(file_state(&pool, a.file_id).await, "deleted");
    assert_eq!(file_state(&pool, b.file_id).await, "active");
    // 키를 지우면 B도 detach된다.
    assert_eq!(
        s3::delete_key(&pool, "c", "k").await.unwrap(),
        Some(b.file_id)
    );
    assert_eq!(file_state(&pool, b.file_id).await, "deleted");
}

async fn file_state(pool: &PgPool, id: uuid::Uuid) -> String {
    sqlx::query_scalar("SELECT state FROM files WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "./migrations")]
async fn key_mapping_dies_with_the_file_row(pool: PgPool) {
    // 종착 행 보존 정리(spec 00)가 file을 지울 때 매핑도 CASCADE로 사라진다
    // — 매달린 매핑이 남지 않는다 (마이그레이션 0004).
    wire(&pool).await;
    let file = create_ok(&pool).await;
    s3::upsert_key(&pool, "c", "dir/b.bin", file.file_id)
        .await
        .unwrap();
    // reclaim → lease GC → 보존 경과 → prune (lifecycle.rs와 같은 절차).
    sqlx::query("UPDATE leases SET expires_at = now() - interval '1 hour' WHERE file_id = $1")
        .bind(file.file_id)
        .execute(&pool)
        .await
        .unwrap();
    let candidates = files::expired_pending(&pool, 10).await.unwrap();
    assert!(
        files::finalize_reclaim(&pool, &candidates[0])
            .await
            .unwrap()
    );
    files::prune_terminal_leases(&pool, 0, 10).await.unwrap();
    sqlx::query("UPDATE files SET created_at = now() - interval '91 days' WHERE id = $1")
        .bind(file.file_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        files::prune_terminal_files(&pool, 90 * 24 * 3600, 10)
            .await
            .unwrap(),
        1
    );
    assert!(
        s3::get_key(&pool, "c", "dir/b.bin")
            .await
            .unwrap()
            .is_none()
    );
}
