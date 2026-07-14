//! 업로드 스트림을 로컬 스풀에 통과-계측하는 단일 프리미티브 (ADR 002).
//!
//! 네이티브 중계(blobs)와 S3 표면(s3)이 공유한다 — 둘 다 "body를
//! 임시 파일에 쓰며 크기·해시를 실측하고 선언 크기를 넘는 순간 끊는" 같은
//! 일을 한다. lease/인증은 진입 시 한 번만 검사되므로 진행 중 연결의 수명은
//! 이 유휴 타임아웃이 다스린다 — 바이트를 극소량씩 흘리며 연결·임시 파일을
//! 점유하는 것을 두 표면 모두에서 끊는다.

use std::path::Path;
use std::time::Duration;

use axum::body::Body;
use futures_util::StreamExt as _;
use md5::{Digest as _, Md5};
use sha2::Sha256;
use tokio::io::AsyncWriteExt as _;

use crate::storage_access::StorageBackend;

/// 청크 사이 유휴 상한 — 두 표면 공통.
pub const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
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
        md5_hex: hex(&md5.finalize()),
        sha256_hex: sha256.map(|sha| {
            use sha2::Digest as _;
            hex(&sha.finalize())
        }),
    })
}

async fn fs_backend_abort(temp_path: &Path) {
    filegate_infra::fs::abort_write(temp_path).await;
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}
