-- 배치 정책 — source storage가 소유하는 규칙 (spec 05). 조건을 만족하는 파일의
-- 이동을 생성만 하고 집행은 이동 메커니즘(spec 04)이 한다 (결정·집행 분리).
-- source 삭제는 그 정책을 함께 지운다(CASCADE); dest는 RESTRICT라 정책이
-- 가리키는 동안 dest storage 삭제를 막는다 (이동 저널 FK 방패의 연장).
CREATE TABLE placement_policies (
    id                uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    source_storage_id text NOT NULL REFERENCES storages (id) ON DELETE CASCADE,
    dest_storage_id   text NOT NULL REFERENCES storages (id),
    priority          int  NOT NULL DEFAULT 100,
    min_size          bigint,
    min_idle_secs     bigint,
    max_idle_secs     bigint,
    high_pct          int,
    low_pct           int,
    last_run_at       timestamptz,
    last_error        text,
    moves_generated   bigint NOT NULL DEFAULT 0,
    created_at        timestamptz NOT NULL DEFAULT now(),
    CHECK (source_storage_id <> dest_storage_id),
    CHECK (high_pct IS NULL OR (high_pct BETWEEN 0 AND 100)),
    CHECK (low_pct  IS NULL OR (low_pct  BETWEEN 0 AND 100)),
    CHECK (low_pct IS NULL OR high_pct IS NULL OR low_pct <= high_pct)
);
-- 평가는 source별로 우선순위 순으로 순차한다 (첫 매칭 승리).
CREATE INDEX placement_policies_source_idx ON placement_policies (source_storage_id, priority);
