//! 업로드 스트림을 로컬 스풀에 통과-계측하는 단일 프리미티브 (ADR 002).
//!
//! 네이티브 중계(blobs)와 S3 표면(s3)이 공유한다 — 둘 다 "body를
//! 임시 파일에 쓰며 크기·해시를 실측하고 선언 크기를 넘는 순간 끊는" 같은
//! 일을 한다. lease/인증은 진입 시 한 번만 검사되므로 진행 중 연결의 수명은
//! 이 유휴 타임아웃이 다스린다 — 바이트를 극소량씩 흘리며 연결·임시 파일을
//! 점유하는 것을 두 표면 모두에서 끊는다.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use futures_util::StreamExt as _;
use md5::{Digest as _, Md5};
use sha2::Sha256;
use tokio::io::AsyncWriteExt as _;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::storage_access::StorageBackend;

/// 청크 사이 유휴 상한 — 두 표면 공통.
pub const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// S3 중계 스풀 동시성 상한. 중계는 body를 공유 임시 볼륨(temp_dir)에
/// 통과-스풀하므로, 동시 스풀 수를 묶지 않으면 인증된 다수 업로드가 볼륨을
/// 채워 같은 파드의 다른 전송까지 무너뜨린다(자원 고갈 DoS). fs 백엔드는
/// 자기 대상 마운트에 직접 쓰므로 이 상한 밖이다.
pub const SPOOL_CONCURRENCY_LIMIT: usize = 16;

/// S3 백엔드일 때만 스풀 슬롯을 잡는다 — permit이 살아있는 동안 스풀+중계가
/// 진행되고, 스코프를 벗어나면(정상·에러 무관) 자동 반납된다. 세마포어는
/// close하지 않지만, 만약 close됐다면 스로틀을 건너뛴다(기능 보존).
pub async fn acquire_spool_slot(
    backend: &StorageBackend,
    slots: &Arc<Semaphore>,
) -> Option<OwnedSemaphorePermit> {
    match backend {
        StorageBackend::S3 { .. } => slots.clone().acquire_owned().await.ok(),
        StorageBackend::Fs { .. } => None,
    }
}
/// 스트림 버퍼 크기 — 다운로드 재청크와 업로드 스풀 쓰기가 공유한다.
/// 기본 4KiB로 두면 GiB급 전송이 수십만 번의 블로킹 풀 왕복이 된다.
pub const STREAM_BUF_SIZE: usize = 256 * 1024;

/// 쓰기 스풀 목적지: fs는 대상 root의 임시 파일(같은 마운트 rename),
/// s3 중계는 OS 로컬 스풀을 거친다.
pub fn spool_root(backend: &StorageBackend) -> std::path::PathBuf {
    match backend {
        StorageBackend::Fs { root } => root.clone(),
        StorageBackend::S3 { .. } => std::env::temp_dir(),
    }
}

/// 스풀 실측 결과. sha256은 요청했을 때만 (S3 표면의 x-amz-content-sha256
/// 대조용) — 네이티브 중계는 md5만 쓴다.
pub struct Measured {
    pub written: i64,
    pub md5_hex: String,
    pub sha256_hex: Option<String>,
}

/// 스풀 실패의 종류 — 호출자가 각자의 표면 에러(ApiError·S3 XML)로 번역한다.
/// 이 프리미티브는 자기 실패 시 임시 파일을 지운다 (abort_write는 멱등이라
/// 호출자가 이중 abort해도 무해하다). 미달(written < declared) 검사는
/// 호출자 몫이다 — 표면마다 에러 코드가 다르므로.
pub enum SpoolError {
    /// 청크 사이 유휴가 상한을 넘었다 (slow-loris).
    Idle,
    /// 스트림이 도중에 끊겼다.
    Aborted,
    /// 선언 크기를 넘겼다 — 도중에 끊는다.
    TooLarge,
    /// 스풀 쓰기 실패.
    Io(std::io::Error),
}

/// body를 writer에 쓰며 크기·MD5(+선택 SHA256)를 실측하고, 선언 크기를 넘는
/// 순간 끊는다. 유휴·단절·초과·IO 실패는 임시 파일을 지우고 에러로 돌아간다.
pub async fn spool_to_temp(
    body: Body,
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
    temp_path: &Path,
    declared_size: i64,
    want_sha256: bool,
) -> Result<Measured, SpoolError> {
    let mut md5 = Md5::new();
    let mut sha256 = want_sha256.then(Sha256::new);
    let mut written: i64 = 0;
    let mut stream = body.into_data_stream();
    loop {
        let chunk = match tokio::time::timeout(STREAM_IDLE_TIMEOUT, stream.next()).await {
            Err(_) => {
                fs_backend_abort(temp_path).await;
                return Err(SpoolError::Idle);
            }
            Ok(None) => break,
            Ok(Some(Err(_))) => {
                fs_backend_abort(temp_path).await;
                return Err(SpoolError::Aborted);
            }
            Ok(Some(Ok(chunk))) => chunk,
        };
        written += chunk.len() as i64;
        if written > declared_size {
            fs_backend_abort(temp_path).await;
            return Err(SpoolError::TooLarge);
        }
        md5.update(&chunk);
        if let Some(sha) = sha256.as_mut() {
            use sha2::Digest as _;
            sha.update(&chunk);
        }
        if let Err(error) = writer.write_all(&chunk).await {
            fs_backend_abort(temp_path).await;
            return Err(SpoolError::Io(error));
        }
    }
    Ok(Measured {
        written,
        md5_hex: hex::encode(md5.finalize()),
        sha256_hex: sha256.map(|sha| {
            use sha2::Digest as _;
            hex::encode(sha.finalize())
        }),
    })
}

async fn fs_backend_abort(temp_path: &Path) {
    filegate_infra::fs::abort_write(temp_path).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_s3_spec() -> filegate_infra::S3StorageSpec {
        filegate_infra::S3StorageSpec {
            endpoint: "http://m:9000".to_owned(),
            public_endpoint: "http://m:9000".to_owned(),
            region: "us-east-1".to_owned(),
            bucket: "b".to_owned(),
            force_path_style: true,
            access_key: "ak".to_owned(),
            secret_key: filegate_core::SecretString::from("sk".to_owned()),
        }
    }

    #[test]
    fn spool_root_targets_root_for_fs_and_temp_dir_for_s3() {
        let fs = StorageBackend::Fs {
            root: std::path::PathBuf::from("/data/x"),
        };
        assert_eq!(spool_root(&fs), std::path::PathBuf::from("/data/x"));
        // s3 중계는 OS 로컬 스풀(임시 디렉토리)을 거친다.
        let s3 = StorageBackend::S3 {
            spec: dummy_s3_spec(),
            force_relay: true,
        };
        assert_eq!(spool_root(&s3), std::env::temp_dir());
    }
}
