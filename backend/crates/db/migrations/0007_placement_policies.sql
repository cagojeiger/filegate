-- 배치 정책 — 어떤 파일을 언제 옮길지 자동으로 결정하는 규칙.
--
-- 정책은 배치를 **제안만** 한다. 조건에 맞는 파일의 staging 자리를 여는 것이
-- 전부이고, 채우고 교체하는 일은 집행자가 한다 (ADR 007). 바이트를 만지지
-- 않으므로 판단자의 몫이다.
--
-- 정책은 storage가 소유한다. "이 storage가 차면 저기로 내린다"는 그 storage의
-- 성질이지 파일이나 클라이언트의 성질이 아니다. storage를 지우면 그 정책도
-- 함께 사라진다.
CREATE TABLE placement_policies (
    id                uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    source_storage_id text NOT NULL REFERENCES storages (id) ON DELETE CASCADE,
    dest_storage_id   text NOT NULL REFERENCES storages (id),
    -- 같은 source에 정책이 여럿이면 낮은 값이 먼저 고른다. 앞선 정책이 집은
    -- 파일은 뒤 정책의 후보에서 빠진다 (first-match).
    priority          int  NOT NULL DEFAULT 100,

    -- 후보 조건. 전부 nullable이고 지정된 것만 AND로 걸린다.
    min_size          bigint CHECK (min_size >= 0),
    min_idle_secs     bigint CHECK (min_idle_secs >= 0),

    -- 압박 게이트 (히스테리시스). 점유가 high를 넘으면 켜지고 low에 닿으면
    -- 멈춘다 — 둘을 같게 두면 경계에서 끝없이 오간다. 둘 다 NULL이면 조건에
    -- 맞는 파일을 압박과 무관하게 계속 내린다.
    high_pct          int CHECK (high_pct BETWEEN 1 AND 100),
    low_pct           int CHECK (low_pct BETWEEN 0 AND 100),

    -- 관측: 마지막 평가 시각과 누적 생성 수.
    last_run_at       timestamptz,
    moves_generated   bigint NOT NULL DEFAULT 0,
    created_at        timestamptz NOT NULL DEFAULT now(),

    CHECK (source_storage_id <> dest_storage_id),
    -- 게이트는 짝으로만 의미가 있고, low가 high보다 낮아야 멈출 수 있다.
    CHECK ((high_pct IS NULL) = (low_pct IS NULL)),
    CHECK (high_pct IS NULL OR low_pct < high_pct)
);

CREATE INDEX placement_policies_source_idx
    ON placement_policies (source_storage_id, priority);
