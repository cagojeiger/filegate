//! lease TTL 정책 — 표면 무관. 쓰기·읽기 lease의 수명은 표면이 아니라
//! 정책이 정한다: 네이티브 표면과 S3 표면이 같은 값을 쓴다. 한쪽만 바꿔
//! 두 표면의 lease 수명이 어긋나는 일을 막는다.

use std::future::Future;
use std::time::Duration;

use filegate_db::{PgPool, files, s3_registry};
use uuid::Uuid;

/// 쓰기 lease TTL — 짧게 둔다 (spec 00: 쓰기 URL은 확정 후에도 만료 전까지
/// 유효하므로, 변조 창을 줄이는 건 TTL이다).
pub const WRITE_LEASE_TTL: Duration = Duration::from_secs(15 * 60);

/// 읽기 lease TTL. 발급된 직결 URL은 만료로만 소멸한다 (ADR 002).
pub const READ_LEASE_TTL: Duration = Duration::from_secs(15 * 60);

#[derive(Clone, Copy)]
enum WriteOwnership {
    NativeCompletion,
    NativeUploadPart { lease_id: Uuid, part_no: i32 },
    S3Completion,
    UploadPart { lease_id: Uuid, part_no: i32 },
}

impl WriteOwnership {
    fn operation(&self) -> &'static str {
        match self {
            Self::NativeCompletion => "native_completion",
            Self::NativeUploadPart { .. } => "native_upload_part",
            Self::S3Completion => "s3_completion",
            Self::UploadPart { .. } => "upload_part",
        }
    }
}

/// 긴 외부 저장소 작업 동안 write lease를 갱신한다. 대상이 사라졌다면 복구
/// 전이가 소유권을 가져간 것이므로 future를 취소한다. 일시적 DB 오류는
/// reconciler도 진행할 수 없는 구간이라 기록하고 다음 tick에서 다시 시도한다.
async fn run_with_write_heartbeat<T>(
    pool: &PgPool,
    file_id: Uuid,
    ownership: WriteOwnership,
    operation: impl Future<Output = T>,
) -> Option<T> {
    let heartbeat_every = Duration::from_secs(WRITE_LEASE_TTL.as_secs() / 3);
    let mut heartbeat = tokio::time::interval_at(
        tokio::time::Instant::now() + heartbeat_every,
        heartbeat_every,
    );
    tokio::pin!(operation);

    loop {
        tokio::select! {
            biased;
            result = &mut operation => return Some(result),
            _ = heartbeat.tick() => {
                let renewed = match ownership {
                    WriteOwnership::NativeCompletion => files::renew_completion_lease(
                        pool,
                        file_id,
                        WRITE_LEASE_TTL.as_secs() as i64,
                    ).await,
                    WriteOwnership::NativeUploadPart { lease_id, part_no } => {
                        files::renew_relay_part_lease(
                            pool,
                            file_id,
                            lease_id,
                            part_no,
                            WRITE_LEASE_TTL.as_secs() as i64,
                        ).await
                    }
                    WriteOwnership::S3Completion => s3_registry::renew_completion_lease(
                        pool,
                        file_id,
                        WRITE_LEASE_TTL.as_secs() as i64,
                    ).await,
                    WriteOwnership::UploadPart { lease_id, part_no } => {
                        s3_registry::renew_upload_part_lease(
                            pool,
                            file_id,
                            lease_id,
                            part_no,
                            WRITE_LEASE_TTL.as_secs() as i64,
                        ).await
                    }
                };
                match renewed {
                    Ok(true) => {}
                    Ok(false) => {
                        tracing::warn!(
                            event = "s3.write_ownership_lost",
                            operation = ownership.operation(),
                            file = %file_id,
                        );
                        return None;
                    }
                    Err(error) => tracing::warn!(
                        event = "s3.write_heartbeat_failed",
                        operation = ownership.operation(),
                        file = %file_id,
                        %error,
                    ),
                }
            }
        }
    }
}

pub async fn run_with_completion_heartbeat<T>(
    pool: &PgPool,
    file_id: Uuid,
    operation: impl Future<Output = T>,
) -> Option<T> {
    run_with_write_heartbeat(pool, file_id, WriteOwnership::S3Completion, operation).await
}

pub async fn run_with_native_completion_heartbeat<T>(
    pool: &PgPool,
    file_id: Uuid,
    operation: impl Future<Output = T>,
) -> Option<T> {
    run_with_write_heartbeat(pool, file_id, WriteOwnership::NativeCompletion, operation).await
}

pub async fn run_with_native_upload_part_heartbeat<T>(
    pool: &PgPool,
    file_id: Uuid,
    lease_id: Uuid,
    part_no: i32,
    operation: impl Future<Output = T>,
) -> Option<T> {
    run_with_write_heartbeat(
        pool,
        file_id,
        WriteOwnership::NativeUploadPart { lease_id, part_no },
        operation,
    )
    .await
}

pub async fn run_with_upload_part_heartbeat<T>(
    pool: &PgPool,
    file_id: Uuid,
    lease_id: Uuid,
    part_no: i32,
    operation: impl Future<Output = T>,
) -> Option<T> {
    run_with_write_heartbeat(
        pool,
        file_id,
        WriteOwnership::UploadPart { lease_id, part_no },
        operation,
    )
    .await
}

/// best-effort 읽기 감사 — 직결 read는 감사용 lease 원장 한 줄일 뿐이다
/// (ADR 002, 네이티브·S3 한 장부). URL/응답은 이미 완성돼 유효하므로 DB
/// 실패로 버리지 않고 경고만 남긴다. 중계 read는 lease_id가 필요하므로
/// 이 헬퍼가 아니라 issue_read_lease를 직접 쓴다. 두 표면이 같은 TTL·
/// 이벤트·secret 없음(None)을 쓰도록 한 곳에 고정한다.
pub async fn audit_read(
    pool: &PgPool,
    file_id: Uuid,
    storage_id: &str,
    client_id: &str,
    size: i64,
) {
    if let Err(error) = files::issue_read_lease(
        pool,
        file_id,
        READ_LEASE_TTL.as_secs() as i64,
        None,
        storage_id,
        client_id,
        size,
    )
    .await
    {
        tracing::warn!(event = "file.read_audit_failed", file = %file_id, %error);
    }
}
