//! 로컬/NFS 파일시스템 storage adapter (ADR 001: 파일시스템도 storage다).
//!
//! presigned 개념이 없으므로 항상 중계다 — 바이트는 filegate의 바이트
//! 엔드포인트를 지나 여기로 온다. 쓰기는 임시 경로 + rename 원자성
//! (spec 00): 같은 마운트 안에서만 성립하므로 멀티 파드는 같은 마운트를
//! 공유해야 한다 (docs/stack).

use std::path::{Path, PathBuf};

use tokio::fs;
use tokio::io::AsyncWriteExt;

/// 접근 검증 — s3의 head_bucket 등가물. 디렉토리 존재 + 쓰기 가능을
/// 프로브 파일로 확인한다 (등록 거부 또는 부팅 중단, ADR 001). fs는
/// 캐시할 커넥션이 없어 핸들을 돌려주지 않는다 — 실제 작업은 root 경로를
/// 직접 받는다 (s3의 connect가 client를 돌려주는 것과 다른 점).
pub async fn connect(root_path: &str) -> anyhow::Result<()> {
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
    Ok(())
}

/// object_key를 root 아래 경로로 잇는다 — 방어선: 절대 경로나 `..` 성분은
/// root를 벗어난다(`Path::join`은 절대 경로를 주면 root를 버린다). 키는
/// filegate 생성값이라 오늘은 안전하지만, s3 어댑터가 구조적으로 탈출
/// 불가한 것과 대칭이 되도록 여기서도 봉인한다.
fn object_path(root: &Path, object_key: &str) -> anyhow::Result<PathBuf> {
    let key = Path::new(object_key);
    let escapes = key.is_absolute()
        || key
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir));
    if escapes {
        anyhow::bail!("object_key '{object_key}' escapes the storage root");
    }
    Ok(root.join(key))
}

/// 쓰기 시작 — 임시 파일을 연다. 완결은 rename(commit_write), 실패는
/// abort_write가 지운다. 접두사 `.fg-tmp-`는 공유 /tmp에서도 filegate
/// 것임을 식별하게 한다 — 나이(mtime) 기반 sweep의 대상 표식 (spec 00).
pub async fn begin_write(root: &Path, temp_name: &str) -> anyhow::Result<(PathBuf, fs::File)> {
    let temp = root.join(format!(".fg-tmp-{temp_name}"));
    let file = fs::File::create(&temp).await?;
    Ok((temp, file))
}

/// 임시 → 실체 경로 rename (원자적, 같은 마운트 전제).
/// 키가 경로를 가지므로(spec 00 물리 배치) 부모 디렉토리를 먼저 보장한다.
pub async fn commit_write(
    mut file: fs::File,
    temp: &Path,
    root: &Path,
    object_key: &str,
) -> anyhow::Result<()> {
    file.flush().await?;
    file.sync_all().await?;
    drop(file);
    let target = object_path(root, object_key)?;
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).await?;
    }
    fs::rename(temp, target).await?;
    Ok(())
}

pub async fn abort_write(temp: &Path) {
    let _ = fs::remove_file(temp).await;
}

/// 읽기 스트림 — 파일과 크기. 없으면 None (presigned GET 404 등가).
pub async fn open_read(root: &Path, object_key: &str) -> anyhow::Result<Option<(fs::File, i64)>> {
    let path = object_path(root, object_key)?;
    match fs::File::open(&path).await {
        Ok(file) => {
            let len = file.metadata().await?.len() as i64;
            Ok(Some((file, len)))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// 부분 읽기 스트림 (spec 03 Range) — seek 후 길이 제한. 구간은 호출자가
/// 파일 크기로 검증한 폐구간 [start, end]다. 없으면 None.
pub async fn open_read_range(
    root: &Path,
    object_key: &str,
    start: i64,
    end: i64,
) -> anyhow::Result<Option<(impl tokio::io::AsyncRead + Send + Unpin, i64)>> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let path = object_path(root, object_key)?;
    match fs::File::open(&path).await {
        Ok(mut file) => {
            file.seek(std::io::SeekFrom::Start(start as u64)).await?;
            let len = end - start + 1;
            Ok(Some((file.take(len as u64), len)))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// 장부 밖 임시 정리 (spec 00 물리 배치): 디렉토리 최상위의 `.fg-tmp-*` 중
/// mtime이 max_age를 넘은 것을 지운다. 단일 PUT temp는 DB를 보지 않는다 —
/// 진행 중 업로드는 어리므로 걸리지 않고, 크래시가 남긴 것만 늙어서 걸린다.
///
/// multipart 조립 파일(.fg-tmp-mp-{lease})은 예외다: part 재개가 물리 쓰기
/// 없이 lease만 갱신할 수 있어 mtime 노화가 진행 중과 크래시를 못 가른다.
/// 그래서 활성 lease 목록(`protected_mp_leases`, 호출자가 DB에서 조회)에
/// 있는 조립 파일은 mtime과 무관하게 건너뛴다 — 활성 조립 파일은 고아가
/// 아니고, 지우면 재개된 part 쓰기가 파일을 재생성해 손상본이 커밋된다.
pub async fn sweep_stale_temps(
    dir: &Path,
    max_age: std::time::Duration,
    protected_mp_leases: &std::collections::HashSet<String>,
) -> anyhow::Result<u32> {
    let mut entries = fs::read_dir(dir).await?;
    let mut removed = 0u32;
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with(".fg-tmp-") {
            continue;
        }
        if let Some(lease_id) = name.strip_prefix(".fg-tmp-mp-") {
            if protected_mp_leases.contains(lease_id) {
                continue;
            }
        }
        let Ok(meta) = entry.metadata().await else {
            continue;
        };
        let stale = meta
            .modified()
            .ok()
            .and_then(|mtime| mtime.elapsed().ok())
            .is_some_and(|age| age > max_age);
        if stale && fs::remove_file(entry.path()).await.is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

/// 물리 삭제 — 없는 파일도 성공 (purge는 멱등, spec 00).
pub async fn delete(root: &Path, object_key: &str) -> anyhow::Result<()> {
    match fs::remove_file(object_path(root, object_key)?).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

// ---- multipart (spec 02) ----

/// multipart 대상 임시 파일 경로 — 결정적 이름이라 승격·commit·회수가
/// 같은 파일을 본다. `.fg-tmp-` 접두사를 상속하므로 버려지면 mtime sweep이
/// 줍는다 (진행 중엔 part 쓰기가 mtime을 갱신해 걸리지 않는다).
pub fn multipart_temp(root: &Path, lease_id: &str) -> PathBuf {
    root.join(format!(".fg-tmp-mp-{lease_id}"))
}

/// part 승격 — part 스풀을 대상 임시 파일의 자기 offset에 기록한다.
/// 범위가 겹치지 않아 병렬·멀티 pod 안전하고, 같은 part의 동시 승격
/// 직렬화는 호출자의 part claim(행 락) 몫이다 (spec 02).
pub async fn write_part_at(target: &Path, offset: u64, source: &Path) -> anyhow::Result<()> {
    use tokio::io::AsyncSeekExt;
    // truncate 금지 — 다른 part들의 offset 기록이 이미 이 파일에 있다.
    let mut dst = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(target)
        .await?;
    dst.seek(std::io::SeekFrom::Start(offset)).await?;
    let mut src = fs::File::open(source).await?;
    tokio::io::copy(&mut src, &mut dst).await?;
    dst.flush().await?;
    dst.sync_all().await?;
    Ok(())
}

/// 경로 기반 확정 — multipart 대상 임시 파일을 실체 경로로 rename.
/// (단일 PUT의 commit_write와 같은 계약, 핸들 대신 경로를 받는 변형.)
pub async fn commit_path(root: &Path, temp: &Path, object_key: &str) -> anyhow::Result<()> {
    let target = object_path(root, object_key)?;
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).await?;
    }
    fs::rename(temp, target).await?;
    Ok(())
}
