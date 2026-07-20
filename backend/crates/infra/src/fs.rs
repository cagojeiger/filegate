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
) -> anyhow::Result<Option<(impl tokio::io::AsyncRead + Send + Unpin + use<>, i64)>> {
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
        if let Some(rest) = name.strip_prefix(".fg-tmp-mp-") {
            // rest는 네이티브 조립 파일이면 `{lease}`, S3 multipart part 파일이면
            // `{lease}-p{N}`이다. UUID는 hex+하이픈이라 `p`가 없으므로 `-p`가
            // lease id와 part 번호를 명확히 가른다 (네이티브는 `-p`가 없어 rest
            // 전체가 lease). 활성 lease면 조립 중 part든 조립 파일이든 보호한다.
            let lease = rest.split_once("-p").map_or(rest, |(lease, _)| lease);
            if protected_mp_leases.contains(lease) {
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

/// S3 multipart part의 임시 파일 경로 (spec 03) — 크기-비선언 모델이라
/// 도착 시점엔 offset을 모른다. 그래서 각 part를 자기 파일에 계측 보관하고
/// (조립은 Complete로 미룬다), `.fg-tmp-mp-{lease}-p{N}`로 짓는다: `.fg-tmp-`
/// 접두로 버려지면 mtime sweep이 줍고, `mp-{lease}` 부분으로 활성 lease 보호
/// (sweep_stale_temps)가 조립 중 part를 지키게 한다.
pub fn multipart_part_temp(root: &Path, lease_id: &str, part_no: i32) -> PathBuf {
    root.join(format!(".fg-tmp-mp-{lease_id}-p{part_no}"))
}

/// 스풀 임시를 최종 part 임시로 원자 교체한다 (S3 multipart part 승격).
/// rename이라 같은 part의 재업로드가 last-write-wins로 덮어쓰고, 이전 더
/// 큰 part의 꼬리 바이트가 남지 않는다 (truncate 있는 write_part_at과 달리
/// 여기선 조립 전이라 part 파일이 통째로 교체돼야 한다).
pub async fn rename_into(source: &Path, target: &Path) -> anyhow::Result<()> {
    fs::rename(source, target).await?;
    Ok(())
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

/// 조립 임시를 정확한 길이로 자른다 — 조립은 결정적 이름을 truncate 없이
/// 쓰므로, 실패한 이전 시도가 더 긴 꼬리를 남겼을 수 있다. 확정 직전 실측
/// 합으로 잘라 그 꼬리가 객체로 새는 것을 막는다.
pub async fn truncate_to(target: &Path, len: u64) -> anyhow::Result<()> {
    let file = fs::OpenOptions::new().write(true).open(target).await?;
    file.set_len(len).await?;
    file.sync_all().await?;
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use std::collections::HashSet;
    use std::time::Duration;

    use super::*;

    async fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fg-fstest-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir).await;
        fs::create_dir_all(&dir).await.unwrap();
        dir
    }

    #[tokio::test]
    async fn multipart_part_temp_carries_lease_and_part_number() {
        let root = Path::new("/data/x");
        let path = multipart_part_temp(root, "lease-abc", 7);
        assert_eq!(path, root.join(".fg-tmp-mp-lease-abc-p7"));
    }

    #[tokio::test]
    async fn out_of_order_parts_assemble_by_cumulative_offset() {
        // S3 multipart: part는 비순차로 오지만, Complete가 번호순 실측 누계
        // offset으로 조립하면 원본 바이트열이 재구성된다. 물리 쓰기 순서는
        // 무관하다 (offset이 절대값이므로).
        let dir = scratch("assemble").await;
        let p1 = dir.join("src-p1");
        let p2 = dir.join("src-p2");
        let p3 = dir.join("src-p3");
        fs::write(&p1, b"AAA").await.unwrap(); // 크기 3, offset 0
        fs::write(&p2, b"BB").await.unwrap(); // 크기 2, offset 3
        fs::write(&p3, b"CCCC").await.unwrap(); // 크기 4, offset 5
        let assembly = multipart_temp(&dir, "lease1");
        // 도착 순서 = 2, 3, 1 (비순차). offset은 번호순 누계 (0, 3, 5).
        write_part_at(&assembly, 3, &p2).await.unwrap();
        write_part_at(&assembly, 5, &p3).await.unwrap();
        write_part_at(&assembly, 0, &p1).await.unwrap();
        let assembled = fs::read(&assembly).await.unwrap();
        assert_eq!(assembled, b"AAABBCCCC");
        fs::remove_dir_all(&dir).await.ok();
    }

    #[tokio::test]
    async fn truncate_to_drops_stale_tail_from_a_prior_attempt() {
        // 이전 실패 시도가 더 긴 꼬리를 남겼다 — 짧은 재시도가 앞부분만
        // 덮어써도 truncate_to가 실측 합으로 잘라 꼬리가 새지 않는다.
        let dir = scratch("truncate").await;
        let assembly = multipart_temp(&dir, "lease1");
        fs::write(&assembly, b"OLDLONGTAIL").await.unwrap(); // 11바이트 잔재
        let p1 = dir.join("src-p1");
        fs::write(&p1, b"NEW").await.unwrap();
        write_part_at(&assembly, 0, &p1).await.unwrap(); // 앞 3바이트만 갱신
        truncate_to(&assembly, 3).await.unwrap();
        assert_eq!(fs::read(&assembly).await.unwrap(), b"NEW");
        fs::remove_dir_all(&dir).await.ok();
    }

    #[tokio::test]
    async fn sweep_protects_active_multipart_part_files() {
        // `.fg-tmp-mp-{lease}-p{N}` part 파일은 활성 lease면 mtime과 무관하게
        // 보호된다 — UUID엔 'p'가 없어 `-p`가 lease와 part 번호를 가른다.
        let dir = scratch("sweep").await;
        let active = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let stale = "11111111-2222-3333-4444-555555555555";
        fs::write(dir.join(format!(".fg-tmp-mp-{active}-p1")), b"x")
            .await
            .unwrap();
        fs::write(dir.join(format!(".fg-tmp-mp-{active}")), b"x") // 조립 파일
            .await
            .unwrap();
        fs::write(dir.join(format!(".fg-tmp-mp-{stale}-p1")), b"x")
            .await
            .unwrap();
        // 늙었다고 간주되도록 max_age를 0에 가깝게 두고 잠깐 재운다.
        tokio::time::sleep(Duration::from_millis(15)).await;
        let mut protected = HashSet::new();
        protected.insert(active.to_owned());
        let removed = sweep_stale_temps(&dir, Duration::from_millis(1), &protected)
            .await
            .unwrap();
        // 보호되지 않은 stale part 하나만 지워진다.
        assert_eq!(removed, 1);
        assert!(
            fs::try_exists(dir.join(format!(".fg-tmp-mp-{active}-p1")))
                .await
                .unwrap()
        );
        assert!(
            fs::try_exists(dir.join(format!(".fg-tmp-mp-{active}")))
                .await
                .unwrap()
        );
        assert!(
            !fs::try_exists(dir.join(format!(".fg-tmp-mp-{stale}-p1")))
                .await
                .unwrap()
        );
        fs::remove_dir_all(&dir).await.ok();
    }
}
