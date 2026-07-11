//! 로컬/NFS 파일시스템 storage adapter (ADR 001: 파일시스템도 storage다).
//!
//! presigned 개념이 없으므로 항상 중계다 — 바이트는 filegate의 바이트
//! 엔드포인트를 지나 여기로 온다. 쓰기는 임시 경로 + rename 원자성
//! (spec 00): 같은 마운트 안에서만 성립하므로 멀티 파드는 같은 마운트를
//! 공유해야 한다 (docs/stack).

use std::path::{Path, PathBuf};

use tokio::fs;
use tokio::io::AsyncWriteExt;

#[derive(Debug, Clone)]
pub struct FsStorage {
    pub root: PathBuf,
}

/// 접근 검증 — s3의 head_bucket 등가물. 디렉토리 존재 + 쓰기 가능을
/// 프로브 파일로 확인한다 (등록 거부 또는 부팅 중단, ADR 001).
pub async fn connect(root_path: &str) -> anyhow::Result<FsStorage> {
    let root = PathBuf::from(root_path);
    let meta = fs::metadata(&root)
        .await
        .map_err(|e| anyhow::anyhow!("root_path '{root_path}' not accessible: {e}"))?;
    if !meta.is_dir() {
        anyhow::bail!("root_path '{root_path}' is not a directory");
    }
    let probe = root.join(".filegate-probe");
    fs::write(&probe, b"probe")
        .await
        .map_err(|e| anyhow::anyhow!("root_path '{root_path}' not writable: {e}"))?;
    fs::remove_file(&probe).await.ok();
    Ok(FsStorage { root })
}

fn object_path(root: &Path, object_key: &str) -> PathBuf {
    root.join(object_key)
}

/// 쓰기 시작 — 임시 파일을 연다. 완결은 rename(commit_write), 실패는
/// abort_write가 지운다.
pub async fn begin_write(root: &Path, temp_name: &str) -> anyhow::Result<(PathBuf, fs::File)> {
    let temp = root.join(format!(".tmp-{temp_name}"));
    let file = fs::File::create(&temp).await?;
    Ok((temp, file))
}

/// 임시 → 실체 경로 rename (원자적, 같은 마운트 전제).
pub async fn commit_write(
    mut file: fs::File,
    temp: &Path,
    root: &Path,
    object_key: &str,
) -> anyhow::Result<()> {
    file.flush().await?;
    file.sync_all().await?;
    drop(file);
    fs::rename(temp, object_path(root, object_key)).await?;
    Ok(())
}

pub async fn abort_write(temp: &Path) {
    let _ = fs::remove_file(temp).await;
}

/// 읽기 스트림 — 파일과 크기. 없으면 None (presigned GET 404 등가).
pub async fn open_read(root: &Path, object_key: &str) -> anyhow::Result<Option<(fs::File, i64)>> {
    let path = object_path(root, object_key);
    match fs::File::open(&path).await {
        Ok(file) => {
            let len = file.metadata().await?.len() as i64;
            Ok(Some((file, len)))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// 물리 삭제 — 없는 파일도 성공 (purge는 멱등, spec 00).
pub async fn delete(root: &Path, object_key: &str) -> anyhow::Result<()> {
    match fs::remove_file(object_path(root, object_key)).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}
