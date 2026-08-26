//! S3 호환 표면의 크기-비선언 multipart DB 경로 통합 테스트 (spec 03).
//!
//! 네이티브 multipart와 달리 create에 크기가 없다(sentinel 0) — Complete가
//! 실측 part 합으로 declared_size를 확정한다. 여기서 검증하는 것은 DB 계층의
//! 생애주기다: create-open → part 원장 기록(비순차 포함) → finalize_multipart
//! (합·합성 ETag로 pending→active) → abort의 reclaim. 조립(fs offset)과 XML은
//! api 계층(s3/multipart.rs)의 유닛이 덮는다. 테스트마다 격리 DB(`#[sqlx::test]`).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use filegate_db::files::{self, CreateOutcome, CreateSpec, CreatedFile};
use filegate_db::registry::{self, StorageRow};
use filegate_db::s3_registry as s3;
use sqlx::PgPool;

const KEY: &str = "dir/large.bin";

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
        capacity_bytes: 1_000_000,
    }
}

async fn wire(pool: &PgPool) {
    registry::insert_storage(pool, &s3_row("s")).await.unwrap();
    registry::insert_client(pool, "c", "s").await.unwrap();
}

/// S3 multipart create-open — 크기 미상(0) + part_size 표식.
async fn open_multipart(pool: &PgPool) -> CreatedFile {
    let spec = CreateSpec {
        client_id: "c",
        declared_size: 0,
        content_type: None,
        declared_md5: None,
        lease_ttl_secs: 900,
        // part_size는 크기-비선언이라 실제 기하가 아니라 multipart 표식이다.
        part_size: Some(64 * 1024 * 1024),
    };
    match s3::create_upload(pool, spec, KEY).await.unwrap() {
        CreateOutcome::Created(created) => *created,
        CreateOutcome::NoClient => panic!("expected Created, got NoClient"),
    }
}

async fn open_native_multipart(pool: &PgPool) -> CreatedFile {
    let spec = CreateSpec {
        client_id: "c",
        declared_size: 128 * 1024 * 1024,
        content_type: None,
        declared_md5: None,
        lease_ttl_secs: 900,
        part_size: Some(64 * 1024 * 1024),
    };
    match files::create(pool, spec).await.unwrap() {
        CreateOutcome::Created(created) => *created,
        CreateOutcome::NoClient => panic!("expected Created, got NoClient"),
    }
}

async fn file_row(pool: &PgPool, id: uuid::Uuid) -> (String, i64, Option<String>) {
    sqlx::query_as("SELECT state, declared_size, etag FROM files WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

// ── create-open → parts → complete ─────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn open_records_pending_with_unknown_size(pool: PgPool) {
    wire(&pool).await;
    let created = open_multipart(&pool).await;
    // create-open은 크기 미상(0) pending이고 write lease가 붙는다.
    let (state, size, etag) = file_row(&pool, created.file_id).await;
    assert_eq!(state, "pending");
    assert_eq!(size, 0);
    assert!(etag.is_none());
    let lease = files::write_lease(&pool, created.file_id)
        .await
        .unwrap()
        .expect("write lease exists");
    assert_eq!(lease.lease_id, created.lease_id);
    assert!(
        s3::upload_matches(&pool, "c", KEY, created.file_id, true)
            .await
            .unwrap()
    );
    // 관찰 확정 후보에서 빠진다 — 완료는 선언(Complete)이다 (part_size 표식).
    assert!(
        files::observed_commit_candidates(&pool, 10)
            .await
            .unwrap()
            .is_empty()
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn upload_id_is_bound_to_client_key_and_s3_mode(pool: PgPool) {
    wire(&pool).await;
    let created = open_multipart(&pool).await;

    assert!(
        !s3::upload_matches(&pool, "c", "dir/other.bin", created.file_id, true)
            .await
            .unwrap()
    );
    assert!(
        !s3::upload_matches(&pool, "other", KEY, created.file_id, true)
            .await
            .unwrap()
    );
    assert!(
        !s3::upload_matches(&pool, "c", KEY, created.file_id, false)
            .await
            .unwrap()
    );

    let native = open_native_multipart(&pool).await;
    assert!(
        !s3::upload_matches(&pool, "c", KEY, native.file_id, true)
            .await
            .unwrap()
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn parts_recorded_out_of_order_read_back_ascending(pool: PgPool) {
    // 크기-비선언 모델: part는 동시·비순차로 온다. 원장은 번호순으로 읽혀
    // Complete의 조립(누계 offset)과 크기 합이 결정적이 된다.
    wire(&pool).await;
    let created = open_multipart(&pool).await;
    let lease_id = created.lease_id;
    // 비순차 기록: 3 → 1 → 2 (s3 백엔드 경로 = record_part_done upsert).
    files::record_part_done(&pool, lease_id, 3, 20, "cccc")
        .await
        .unwrap();
    files::record_part_done(&pool, lease_id, 1, 50, "aaaa")
        .await
        .unwrap();
    files::record_part_done(&pool, lease_id, 2, 30, "bbbb")
        .await
        .unwrap();
    let parts = files::done_parts(&pool, lease_id).await.unwrap();
    assert_eq!(
        parts,
        vec![
            (1, 50, "aaaa".to_owned()),
            (2, 30, "bbbb".to_owned()),
            (3, 20, "cccc".to_owned()),
        ]
    );
    // 같은 part 재업로드는 last-write-wins (실측 갱신).
    files::record_part_done(&pool, lease_id, 2, 33, "bbbb2")
        .await
        .unwrap();
    let parts = files::done_parts(&pool, lease_id).await.unwrap();
    assert_eq!(parts[1], (2, 33, "bbbb2".to_owned()));
}

#[sqlx::test(migrations = "./migrations")]
async fn claim_path_serializes_and_records_measured(pool: PgPool) {
    // fs 백엔드 경로 = claim_part(행 락) → done(실측). 크기-비선언이라 실측
    // 크기가 그대로 원장에 남는다 (기하 파생 없음).
    wire(&pool).await;
    let created = open_multipart(&pool).await;
    let lease_id = created.lease_id;
    let claim = files::claim_part(&pool, lease_id, 1).await.unwrap();
    claim.done(4096, "dddd").await.unwrap();
    let parts = files::done_parts(&pool, lease_id).await.unwrap();
    assert_eq!(parts, vec![(1, 4096, "dddd".to_owned())]);
}

#[sqlx::test(migrations = "./migrations")]
async fn complete_finalizes_with_summed_size_and_composite_etag(pool: PgPool) {
    wire(&pool).await;
    let created = open_multipart(&pool).await;
    let lease_id = created.lease_id;
    files::record_part_done(&pool, lease_id, 1, 50, "aaaa")
        .await
        .unwrap();
    files::record_part_done(&pool, lease_id, 2, 30, "bbbb")
        .await
        .unwrap();
    // Complete: 실측 합(80)과 합성 ETag로 pending→active. create의 sentinel
    // 0이 실측 합으로 갱신된다.
    let total = 80;
    // generic multipart 확정은 S3 세션을 건드리지 못한다.
    assert!(
        !files::finalize_multipart_commit(&pool, created.file_id, total, "hexhex-2")
            .await
            .unwrap()
    );
    assert_eq!(
        s3::claim_completion(
            &pool,
            s3::CompletionSpec {
                client_id: "c",
                key: KEY,
                file_id: created.file_id,
                multipart: true,
                expected_size: total,
                expected_etag: "hexhex-2",
                lease_ttl_secs: 900,
            },
        )
        .await
        .unwrap(),
        s3::CompletionClaim::Claimed
    );
    assert_eq!(
        s3::finalize_multipart_upload(&pool, "c", KEY, created.file_id)
            .await
            .unwrap(),
        s3::FinalizeOutcome::Finalized { displaced: None }
    );
    let (state, size, etag) = file_row(&pool, created.file_id).await;
    assert_eq!(state, "active");
    assert_eq!(size, total);
    assert_eq!(etag.as_deref(), Some("hexhex-2"));
    // write lease가 committed로 정산된다.
    let lease_state: String = sqlx::query_scalar("SELECT state FROM leases WHERE id = $1")
        .bind(lease_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(lease_state, "committed");
    // 이중 Complete는 전이 경합의 패자 — false (멱등 응답의 재료).
    assert_eq!(
        s3::finalize_multipart_upload(&pool, "c", KEY, created.file_id)
            .await
            .unwrap(),
        s3::FinalizeOutcome::NotPending
    );
    assert_eq!(
        s3::get_key(&pool, "c", KEY).await.unwrap(),
        Some(created.file_id)
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn completing_survives_finalize_failure_and_reopens_when_object_is_missing(pool: PgPool) {
    wire(&pool).await;
    let created = open_multipart(&pool).await;
    assert_eq!(
        s3::claim_completion(
            &pool,
            s3::CompletionSpec {
                client_id: "c",
                key: KEY,
                file_id: created.file_id,
                multipart: true,
                expected_size: 80,
                expected_etag: "hexhex-2",
                lease_ttl_secs: 900,
            },
        )
        .await
        .unwrap(),
        s3::CompletionClaim::Claimed
    );

    // 외부 Complete 뒤 DB finalize가 실패한 경계를 모사해 finalize를 생략한다.
    // open 작업은 막히지만 같은 예상값의 Complete 재시도는 복구 중임을 안다.
    assert!(
        !s3::upload_matches(&pool, "c", KEY, created.file_id, true)
            .await
            .unwrap()
    );
    assert!(
        s3::completion_matches(&pool, "c", KEY, created.file_id, true)
            .await
            .unwrap()
    );
    assert_eq!(
        s3::claim_completion(
            &pool,
            s3::CompletionSpec {
                client_id: "c",
                key: KEY,
                file_id: created.file_id,
                multipart: true,
                expected_size: 80,
                expected_etag: "hexhex-2",
                lease_ttl_secs: 900,
            },
        )
        .await
        .unwrap(),
        s3::CompletionClaim::Resuming
    );
    assert_eq!(
        s3::claim_abort(&pool, "c", KEY, created.file_id)
            .await
            .unwrap(),
        s3::AbortClaim::Unavailable
    );

    sqlx::query("UPDATE leases SET expires_at = now() - interval '1 hour' WHERE file_id = $1")
        .bind(created.file_id)
        .execute(&pool)
        .await
        .unwrap();
    let candidates = s3::completion_candidates(&pool, 10).await.unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].file_id, created.file_id);
    assert_eq!(candidates[0].expected_size, 80);
    assert_eq!(candidates[0].expected_etag, "hexhex-2");

    // reconciler가 실물이 없음을 관찰한 multipart는 open으로 되돌린다.
    assert!(
        s3::reopen_completion(&pool, created.file_id, 900)
            .await
            .unwrap()
    );
    assert!(
        s3::upload_matches(&pool, "c", KEY, created.file_id, true)
            .await
            .unwrap()
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn renewed_completion_cannot_be_recovered_from_a_stale_candidate(pool: PgPool) {
    wire(&pool).await;
    let created = open_multipart(&pool).await;
    assert_eq!(
        s3::claim_completion(
            &pool,
            s3::CompletionSpec {
                client_id: "c",
                key: KEY,
                file_id: created.file_id,
                multipart: true,
                expected_size: 80,
                expected_etag: "hexhex-2",
                lease_ttl_secs: 900,
            },
        )
        .await
        .unwrap(),
        s3::CompletionClaim::Claimed
    );
    sqlx::query("UPDATE leases SET expires_at = now() - interval '1 hour' WHERE file_id = $1")
        .bind(created.file_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(s3::completion_candidates(&pool, 10).await.unwrap().len(), 1);

    // 후보 조회 뒤 소유자가 heartbeat를 보내면 stale reconciler 전이는 져야 한다.
    assert!(
        s3::renew_completion_lease(&pool, created.file_id, 900)
            .await
            .unwrap()
    );
    assert!(
        !s3::reopen_completion(&pool, created.file_id, 900)
            .await
            .unwrap()
    );
    assert!(
        !s3::mark_completion_aborting(&pool, created.file_id)
            .await
            .unwrap()
    );
    assert!(
        s3::completion_matches(&pool, "c", KEY, created.file_id, true)
            .await
            .unwrap()
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn heartbeat_and_expired_recovery_have_exactly_one_winner(pool: PgPool) {
    wire(&pool).await;
    let created = open_multipart(&pool).await;
    assert_eq!(
        s3::claim_completion(
            &pool,
            s3::CompletionSpec {
                client_id: "c",
                key: KEY,
                file_id: created.file_id,
                multipart: true,
                expected_size: 80,
                expected_etag: "hexhex-2",
                lease_ttl_secs: 900,
            },
        )
        .await
        .unwrap(),
        s3::CompletionClaim::Claimed
    );
    sqlx::query("UPDATE leases SET expires_at = now() - interval '1 hour' WHERE file_id = $1")
        .bind(created.file_id)
        .execute(&pool)
        .await
        .unwrap();

    let (heartbeat, recovery) = tokio::join!(
        s3::renew_completion_lease(&pool, created.file_id, 900),
        s3::reopen_completion(&pool, created.file_id, 900),
    );
    let heartbeat_won = heartbeat.unwrap();
    let recovery_won = recovery.unwrap();
    assert_ne!(heartbeat_won, recovery_won);

    let state: String = sqlx::query_scalar("SELECT state FROM s3_uploads WHERE file_id = $1")
        .bind(created.file_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state, if heartbeat_won { "completing" } else { "open" });
}

#[sqlx::test(migrations = "./migrations")]
async fn claimed_upload_part_fences_complete_until_its_measurement_is_done(pool: PgPool) {
    wire(&pool).await;
    let created = open_multipart(&pool).await;
    files::record_part_done(&pool, created.lease_id, 1, 50, "old-etag")
        .await
        .unwrap();

    assert_eq!(
        s3::claim_upload_part(&pool, "c", KEY, created.file_id, created.lease_id, 1, 900,)
            .await
            .unwrap(),
        s3::UploadPartClaim::Claimed
    );
    assert_eq!(
        s3::claim_upload_part(&pool, "c", KEY, created.file_id, created.lease_id, 1, 900,)
            .await
            .unwrap(),
        s3::UploadPartClaim::Busy
    );
    assert_eq!(
        s3::claim_completion(
            &pool,
            s3::CompletionSpec {
                client_id: "c",
                key: KEY,
                file_id: created.file_id,
                multipart: true,
                expected_size: 50,
                expected_etag: "old-etag-1",
                lease_ttl_secs: 900,
            },
        )
        .await
        .unwrap(),
        s3::CompletionClaim::Busy
    );

    // 실패 시도는 이전 done 값을 되살려 순차 재업로드를 허용한다.
    s3::cancel_upload_part(&pool, created.file_id, created.lease_id, 1)
        .await
        .unwrap();
    assert_eq!(
        files::done_parts(&pool, created.lease_id).await.unwrap(),
        vec![(1, 50, "old-etag".to_owned())]
    );

    assert_eq!(
        s3::claim_upload_part(&pool, "c", KEY, created.file_id, created.lease_id, 1, 900,)
            .await
            .unwrap(),
        s3::UploadPartClaim::Claimed
    );
    assert!(
        s3::finish_upload_part(&pool, created.file_id, created.lease_id, 1, 60, "new-etag",)
            .await
            .unwrap()
    );
    let mut guard = match s3::begin_multipart_completion(&pool, "c", KEY, created.file_id)
        .await
        .unwrap()
    {
        s3::MultipartCompletionStart::Ready(guard) => guard,
        _ => panic!("finished part must leave completion ready"),
    };
    assert_eq!(
        guard.done_parts().await.unwrap(),
        vec![(1, 60, "new-etag".to_owned())]
    );
    assert_eq!(
        guard.claim(60, "new-etag-1", 900).await.unwrap(),
        s3::CompletionClaim::Claimed
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn upload_part_and_complete_claims_have_exactly_one_winner(pool: PgPool) {
    wire(&pool).await;
    let created = open_multipart(&pool).await;
    let (part, complete) = tokio::join!(
        s3::claim_upload_part(&pool, "c", KEY, created.file_id, created.lease_id, 1, 900,),
        s3::claim_completion(
            &pool,
            s3::CompletionSpec {
                client_id: "c",
                key: KEY,
                file_id: created.file_id,
                multipart: true,
                expected_size: 0,
                expected_etag: "empty-0",
                lease_ttl_secs: 900,
            },
        ),
    );
    match (part.unwrap(), complete.unwrap()) {
        (s3::UploadPartClaim::Claimed, s3::CompletionClaim::Busy)
        | (s3::UploadPartClaim::Unavailable, s3::CompletionClaim::Claimed) => {}
        outcome => panic!("part and complete were not fenced: {outcome:?}"),
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn upload_part_and_abort_claims_have_exactly_one_winner(pool: PgPool) {
    wire(&pool).await;
    let created = open_multipart(&pool).await;
    let (part, abort) = tokio::join!(
        s3::claim_upload_part(&pool, "c", KEY, created.file_id, created.lease_id, 1, 900,),
        s3::claim_abort(&pool, "c", KEY, created.file_id),
    );
    match (part.unwrap(), abort.unwrap()) {
        (s3::UploadPartClaim::Claimed, s3::AbortClaim::Busy)
        | (s3::UploadPartClaim::Unavailable, s3::AbortClaim::Claimed) => {}
        outcome => panic!("part and abort were not fenced: {outcome:?}"),
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn complete_and_abort_claims_have_exactly_one_winner(pool: PgPool) {
    wire(&pool).await;
    let created = open_multipart(&pool).await;
    let (complete, abort) = tokio::join!(
        s3::claim_completion(
            &pool,
            s3::CompletionSpec {
                client_id: "c",
                key: KEY,
                file_id: created.file_id,
                multipart: true,
                expected_size: 10,
                expected_etag: "e-1",
                lease_ttl_secs: 900,
            },
        ),
        s3::claim_abort(&pool, "c", KEY, created.file_id),
    );
    let complete_won = complete.unwrap() == s3::CompletionClaim::Claimed;
    let abort_won = abort.unwrap() == s3::AbortClaim::Claimed;
    assert_ne!(complete_won, abort_won);

    let state: String = sqlx::query_scalar("SELECT state FROM s3_uploads WHERE file_id = $1")
        .bind(created.file_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        state,
        if complete_won {
            "completing"
        } else {
            "aborting"
        }
    );
}

// ── abort → reclaim ────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn abort_keeps_recovery_material_until_cleanup_is_confirmed(pool: PgPool) {
    wire(&pool).await;
    let created = open_multipart(&pool).await;
    files::record_part_done(&pool, created.lease_id, 1, 50, "aaaa")
        .await
        .unwrap();
    // Abort는 먼저 aborting만 선점한다. 외부 정리가 실패한 것으로 모사해
    // finalize하지 않으면 session/location/lease가 다음 재시도 재료로 남는다.
    assert_eq!(
        s3::claim_abort(&pool, "c", KEY, created.file_id)
            .await
            .unwrap(),
        s3::AbortClaim::Claimed
    );
    let (state, _, _) = file_row(&pool, created.file_id).await;
    assert_eq!(state, "pending");
    let location: Option<uuid::Uuid> =
        sqlx::query_scalar("SELECT file_id FROM locations WHERE file_id = $1")
            .bind(created.file_id)
            .fetch_optional(&pool)
            .await
            .unwrap();
    assert_eq!(location, Some(created.file_id));
    let cleanup = s3::cleanup_candidates(&pool, 10).await.unwrap();
    assert_eq!(cleanup.len(), 1);
    assert_eq!(cleanup[0].file_id, created.file_id);
    assert_eq!(cleanup[0].write_lease_id, Some(created.lease_id));
    assert!(
        !files::reclaim_pending(&pool, created.file_id)
            .await
            .unwrap()
    );

    // 물리 정리 성공 뒤에만 DB 회수가 session/location을 제거한다.
    assert!(s3::finalize_abort(&pool, created.file_id).await.unwrap());
    let (state, _, _) = file_row(&pool, created.file_id).await;
    assert_eq!(state, "reclaimed");
    let location: Option<uuid::Uuid> =
        sqlx::query_scalar("SELECT file_id FROM locations WHERE file_id = $1")
            .bind(created.file_id)
            .fetch_optional(&pool)
            .await
            .unwrap();
    assert!(location.is_none());
    let lease_state: String = sqlx::query_scalar("SELECT state FROM leases WHERE id = $1")
        .bind(created.lease_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(lease_state, "expired");
    assert!(!s3::finalize_abort(&pool, created.file_id).await.unwrap());
    let session_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM s3_uploads WHERE file_id = $1")
            .bind(created.file_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(session_count, 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn abort_after_complete_does_not_reclaim(pool: PgPool) {
    // 이미 Complete된(active) 세션의 Abort는 회수하지 않는다 — pending만 회수.
    wire(&pool).await;
    let created = open_multipart(&pool).await;
    assert_eq!(
        s3::claim_completion(
            &pool,
            s3::CompletionSpec {
                client_id: "c",
                key: KEY,
                file_id: created.file_id,
                multipart: true,
                expected_size: 10,
                expected_etag: "e-1",
                lease_ttl_secs: 900,
            },
        )
        .await
        .unwrap(),
        s3::CompletionClaim::Claimed
    );
    s3::finalize_multipart_upload(&pool, "c", KEY, created.file_id)
        .await
        .unwrap();
    assert_eq!(
        s3::claim_abort(&pool, "c", KEY, created.file_id)
            .await
            .unwrap(),
        s3::AbortClaim::Unavailable
    );
    let (state, _, _) = file_row(&pool, created.file_id).await;
    assert_eq!(state, "active");
}

#[sqlx::test(migrations = "./migrations")]
async fn wrong_key_complete_cannot_activate_or_map_upload(pool: PgPool) {
    wire(&pool).await;
    let created = open_multipart(&pool).await;

    assert_eq!(
        s3::claim_completion(
            &pool,
            s3::CompletionSpec {
                client_id: "c",
                key: "dir/other.bin",
                file_id: created.file_id,
                multipart: true,
                expected_size: 10,
                expected_etag: "e-1",
                lease_ttl_secs: 900,
            },
        )
        .await
        .unwrap(),
        s3::CompletionClaim::Unavailable
    );
    assert_eq!(
        s3::finalize_multipart_upload(&pool, "c", "dir/other.bin", created.file_id)
            .await
            .unwrap(),
        s3::FinalizeOutcome::NotPending
    );
    let (state, size, etag) = file_row(&pool, created.file_id).await;
    assert_eq!(state, "pending");
    assert_eq!(size, 0);
    assert!(etag.is_none());
    assert!(s3::get_key(&pool, "c", KEY).await.unwrap().is_none());
    assert!(
        s3::get_key(&pool, "c", "dir/other.bin")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        s3::discard_unstarted_upload(&pool, created.file_id)
            .await
            .unwrap()
    );
}

// ── reconciler 회수 재료 ───────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn expired_multipart_is_protected_and_reclaimable(pool: PgPool) {
    // 진행 중 S3 multipart는 fs 조립 sweep에서 보호된다 (part_size 표식).
    wire(&pool).await;
    let created = open_multipart(&pool).await;
    let protected = files::active_multipart_lease_ids(&pool).await.unwrap();
    assert_eq!(protected, vec![created.lease_id]);
    // 만료되면 reconciler의 만료 회수가 줍는다 (벤더 Abort 재료 포함).
    sqlx::query("UPDATE leases SET expires_at = now() - interval '1 hour' WHERE file_id = $1")
        .bind(created.file_id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(files::expired_pending(&pool, 10).await.unwrap().is_empty());
    assert_eq!(
        s3::expired_open_uploads(&pool, 10).await.unwrap(),
        vec![created.file_id]
    );
    // 만료 시각이 지나도 lease가 아직 issued면 보호는 유지된다 — 회수(전이)가
    // 조립 파일 sweep보다 먼저다 (그래야 재개 경합에서 손상본이 안 커밋된다).
    assert_eq!(
        files::active_multipart_lease_ids(&pool).await.unwrap(),
        vec![created.lease_id]
    );
    // aborting 선점이 lease를 닫고, 물리 정리 전에도 임시는 보호 대상에서
    // 빠진다. session/location은 cleanup 후보로 계속 남는다.
    assert!(
        s3::claim_expired_abort(&pool, created.file_id)
            .await
            .unwrap()
    );
    assert!(
        files::active_multipart_lease_ids(&pool)
            .await
            .unwrap()
            .is_empty()
    );
    let cleanup = s3::cleanup_candidates(&pool, 10).await.unwrap();
    assert_eq!(cleanup.len(), 1);
    assert_eq!(cleanup[0].file_id, created.file_id);
    assert!(cleanup[0].multipart);
    assert!(cleanup[0].upload_id.is_none());
    // aborting의 expired lease는 보존 기간이 지나도 session이 소유한 복구
    // 핸들이므로 GC되지 않는다.
    sqlx::query("UPDATE leases SET created_at = now() - interval '2 days' WHERE id = $1")
        .bind(created.lease_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        files::prune_terminal_leases(&pool, 24 * 3600, 10)
            .await
            .unwrap(),
        0
    );
    assert!(
        files::write_lease(&pool, created.file_id)
            .await
            .unwrap()
            .is_some()
    );
    assert!(s3::finalize_abort(&pool, created.file_id).await.unwrap());
}
