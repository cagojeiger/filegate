//! `filegate status` — 배포 자가 점검.
//!
//! 서버를 띄우지 않고 Config로 DB에 붙어 등록부·스토리지 접근을 직접 점검하고
//! 사람이 읽을 요약을 stdout에 찍는다. 전부 정상이면 exit 0, 스토리지 접근이
//! 하나라도 실패하면 non-zero (kubectl exec 헬스체크·스크립트용). HTTP 서버가
//! 죽어 있어도 동작한다 — readyz의 DB-only 체크보다 강하다(실제 접근 재검증).

use std::process::ExitCode;

use filegate_core::{Config, ExposeSecret};

pub async fn run() -> anyhow::Result<ExitCode> {
    let config = Config::load()?;
    let crypto = config.security.crypto()?;
    let pool = filegate_db::connect(
        config.database.url.expose_secret(),
        config.database.max_connections,
    )
    .await?;

    // 부팅과 같은 재검증을 storage별로(abort 없이) 돌린다.
    let checks = crate::admin::check_registered(&pool, &crypto).await?;
    let usage = filegate_db::usage::by_storage(&pool).await?;
    let clients = filegate_db::registry::list_clients(&pool).await?;
    let moves = filegate_db::moves::status_summary(&pool).await?;
    let policies = filegate_db::policies::status_summary(&pool).await?;
    pool.close().await;

    println!("filegate {}   db ok", env!("CARGO_PKG_VERSION"));
    println!();
    println!("STORAGES ({})", checks.len());
    for check in &checks {
        // ok면 사용량/용량, 실패면 사유를 같은 자리에 붙인다.
        let tail = if check.ok() {
            usage
                .iter()
                .find(|u| u.storage_id == check.id)
                .map(|u| {
                    format!(
                        "{} / {}",
                        human_bytes(u.active_bytes),
                        capacity(u.capacity_bytes)
                    )
                })
                .unwrap_or_default()
        } else {
            check.detail.clone().unwrap_or_default()
        };
        let mark = if check.ok() { "ok" } else { "FAIL" };
        println!("  {:<16} {:<4} {:<5} {}", check.id, check.kind, mark, tail);
    }
    println!();
    println!("CLIENTS  {}", clients.len());
    println!();
    // 요약만 — 멈춘 이동의 상세는 admin GET /moves가 담당한다.
    println!("MOVES    active {} · failed {}", moves.active, moves.failed);
    // 정책은 관찰만 — last_error는 다음 tick이 다시 시도하는 soft 신호라
    // exit를 가르지 않는다. 상세는 admin GET /storages/{id}/policies가 담당한다.
    println!("POLICIES {} · errors {}", policies.count, policies.failing);

    // 스토리지 접근 실패든 멈춘 이동이든 배포는 unhealthy다 (운영자 개입 신호).
    let all_ok = checks.iter().all(|check| check.ok()) && moves.failed == 0;
    Ok(if all_ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// 바이트를 사람이 읽을 단위로. 0 이하는 호출부에서 걸러 넘어오지 않는다.
fn human_bytes(bytes: i64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    let suffix = UNITS.get(unit).copied().unwrap_or("B");
    if unit == 0 {
        format!("{bytes} {suffix}")
    } else {
        format!("{value:.1} {suffix}")
    }
}

/// 용량 상한. 0 이하는 무제한으로 본다.
fn capacity(bytes: i64) -> String {
    if bytes > 0 {
        human_bytes(bytes)
    } else {
        "—".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_bytes_scales_units() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(human_bytes(5 * 1024 * 1024 * 1024), "5.0 GiB");
    }

    #[test]
    fn capacity_treats_nonpositive_as_unlimited() {
        assert_eq!(capacity(0), "—");
        assert_eq!(capacity(-1), "—");
        assert_eq!(capacity(1024), "1.0 KiB");
    }
}
