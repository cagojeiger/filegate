//! 배치 정책의 평가 — reconciler가 회차마다 돈다 (spec 04).
//!
//! DB만 만진다: 점유를 읽고 조건에 맞는 파일을 골라 이동 저널에 의도를
//! 넣는다. 바이트는 워커가 만지므로 여기는 밀리초에 끝난다.
//!
//! 두 가지가 이 평가를 폭주하지 않게 잡는다.
//!
//! **이미 걸린 이동을 점유에서 뺀다.** 이동 중인 파일은 아직 source에 있어
//! `active_bytes`에 잡히지만 후보에서는 빠진다. 빼지 않으면 매 회차가 "아직
//! 안 줄었다"고 보고 또 생성해, 집행이 따라가지 못하는 만큼 과녁을 지나친다.
//!
//! **dest의 여유도 회차 안에서 깎아 나간다.** 한 번 찍은 스냅샷만 보면 거의
//! 찬 dest에 계속 밀어 넣어 그쪽을 넘겨버린다.

use std::collections::{HashMap, HashSet};

use filegate_db::{PgPool, policies, usage};
use uuid::Uuid;

/// 한 회차에 전체 정책이 만드는 이동의 상한 — 벤더 요청 예산의 보호선이다.
const MOVES_PER_TICK: i64 = 50;

/// 이 안에 이동된 파일은 후보에서 뺀다 — 정책 사이를 오가는 것을 막는다.
const COOLDOWN_SECS: i64 = 3600;

pub async fn evaluate(pool: &PgPool) {
    let policies = match policies::all(pool).await {
        Ok(rows) if rows.is_empty() => return,
        Ok(rows) => rows,
        Err(error) => {
            tracing::error!(event = "reconciler.scan_failed", kind = "policy", %error);
            return;
        }
    };
    let usage = match usage::by_storage(pool).await {
        Ok(rows) => rows,
        Err(error) => {
            tracing::error!(event = "reconciler.scan_failed", kind = "policy_usage", %error);
            return;
        }
    };
    let in_flight: HashMap<String, i64> = match policies::in_flight_bytes(pool).await {
        Ok(rows) => rows.into_iter().collect(),
        Err(error) => {
            tracing::error!(event = "reconciler.scan_failed", kind = "policy_inflight", %error);
            return;
        }
    };

    // 회차 안에서 깎아 나가는 추정치. source는 줄고 dest는 는다.
    let mut projected: HashMap<String, i64> = usage
        .iter()
        .map(|row| {
            let queued = in_flight.get(&row.storage_id).copied().unwrap_or(0);
            (row.storage_id.clone(), (row.active_bytes - queued).max(0))
        })
        .collect();
    let capacity: HashMap<String, i64> = usage
        .iter()
        .map(|row| (row.storage_id.clone(), row.capacity_bytes))
        .collect();

    // 앞선 정책이 집은 파일은 뒤 정책의 후보에서 뺀다 (first-match).
    let mut claimed: HashSet<Uuid> = HashSet::new();
    let mut budget = MOVES_PER_TICK;

    for policy in &policies {
        if budget <= 0 {
            break;
        }
        let generated = evaluate_one(
            pool,
            policy,
            &mut projected,
            &capacity,
            &mut claimed,
            &mut budget,
        )
        .await;
        if let Err(error) = policies::record_run(pool, policy.id, generated).await {
            tracing::error!(event = "reconciler.gc_failed", kind = "policy_run", %error);
        }
        if generated > 0 {
            tracing::info!(
                event = "policy.generated",
                policy = %policy.id,
                source = %policy.source_storage_id,
                dest = %policy.dest_storage_id,
                count = generated,
            );
        }
    }
}

/// 정책 하나를 평가해 생성한 이동 수를 낸다. 중간에 DB가 실패해도 그때까지
/// 만든 수를 낸다 — 이미 커밋된 이동을 0으로 보고하지 않는다.
async fn evaluate_one(
    pool: &PgPool,
    policy: &policies::PolicyRow,
    projected: &mut HashMap<String, i64>,
    capacity: &HashMap<String, i64>,
    claimed: &mut HashSet<Uuid>,
    budget: &mut i64,
) -> i64 {
    let source_capacity = capacity
        .get(&policy.source_storage_id)
        .copied()
        .unwrap_or(0);
    let source_now = projected
        .get(&policy.source_storage_id)
        .copied()
        .unwrap_or(0);

    // 압박 게이트 — high를 넘어야 켜지고 low에 닿으면 멈춘다. 게이트가 없는
    // 정책은 조건에 맞는 것을 계속 내린다 (목표 없음).
    let target = match (policy.high_pct, policy.low_pct) {
        (Some(high), Some(low)) => {
            // capacity가 없으면 비율을 잴 수 없다 — 그 정책은 아무것도 안 한다.
            if source_capacity <= 0 || source_now <= pct_of(source_capacity, high) {
                return 0;
            }
            Some(pct_of(source_capacity, low))
        }
        _ => None,
    };

    let candidates = match policies::candidates(pool, policy, COOLDOWN_SECS, *budget).await {
        Ok(rows) => rows,
        Err(error) => {
            tracing::error!(event = "reconciler.scan_failed", kind = "policy_candidates", %error);
            return 0;
        }
    };

    let mut generated = 0;
    for candidate in candidates {
        if *budget <= 0 || claimed.contains(&candidate.file_id) {
            continue;
        }
        // 목표에 닿았으면 멈춘다 — 넘겨서 비우지 않는다.
        let source_left = projected
            .get(&policy.source_storage_id)
            .copied()
            .unwrap_or(0);
        if target.is_some_and(|target| source_left <= target) {
            break;
        }
        // dest에 자리가 없으면 이 정책은 더 내릴 곳이 없다.
        let dest_capacity = capacity.get(&policy.dest_storage_id).copied().unwrap_or(0);
        let dest_used = projected.get(&policy.dest_storage_id).copied().unwrap_or(0);
        if dest_capacity > 0 && dest_used + candidate.declared_size > dest_capacity {
            break;
        }

        match policies::enqueue_move(pool, candidate.file_id, &policy.dest_storage_id).await {
            Ok(true) => {}
            // 그 사이 상태가 바뀌었다 — 다음 회차가 다시 본다.
            Ok(false) => continue,
            Err(error) => {
                tracing::error!(event = "reconciler.enqueue_failed", kind = "policy", %error);
                return generated;
            }
        }
        claimed.insert(candidate.file_id);
        *budget -= 1;
        generated += 1;
        *projected
            .entry(policy.source_storage_id.clone())
            .or_insert(0) -= candidate.declared_size;
        *projected.entry(policy.dest_storage_id.clone()).or_insert(0) += candidate.declared_size;
    }
    generated
}

/// capacity의 몇 퍼센트. i128로 셈해 큰 capacity에서도 넘치지 않는다.
fn pct_of(capacity: i64, pct: i32) -> i64 {
    let value = i128::from(capacity) * i128::from(pct) / 100;
    i64::try_from(value).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pct_of_floors_and_survives_huge_capacities() {
        assert_eq!(pct_of(100, 80), 80);
        assert_eq!(pct_of(10, 25), 2); // 내림
        assert_eq!(pct_of(0, 80), 0);
        // capacity * 100이 i64를 넘어도 i128 셈이라 안전하다.
        assert_eq!(pct_of(i64::MAX, 100), i64::MAX);
        assert!(pct_of(i64::MAX, 50) < i64::MAX);
    }
}
