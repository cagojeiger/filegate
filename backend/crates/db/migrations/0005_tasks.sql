-- 집행 큐 — reconciler가 상태에서 도출해 넣고, 워커가 집어 집행한다.
--
-- 큐가 필요한 이유는 하나다: 워커를 파드마다 두어 용량이 파드 수에 비례해
-- 늘어나게 하려면, "이 일은 내가 집었다"를 적을 곳이 있어야 한다. 그 자리가
-- 이 행이다. 락으로는 안 된다 — 락은 파드 하나를 고를 뿐 작업을 나누지
-- 못한다.
--
-- 넣는 쪽은 상태에서 파생한다. 요청 경로는 이 테이블을 건드리지 않는다 —
-- enqueue를 빠뜨릴 주체가 없어야 level-triggered의 견고함이 남는다.
-- 놓친 일은 다음 회차가 같은 상태를 보고 다시 넣는다.
CREATE TABLE tasks (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    kind       text NOT NULL CHECK (kind IN ('observe', 'reclaim', 'purge')),
    file_id    uuid NOT NULL REFERENCES files (id) ON DELETE CASCADE,
    state      text NOT NULL DEFAULT 'queued' CHECK (state IN ('queued', 'active')),
    -- 집행 시도 횟수. 종착 상태는 두지 않는다 — 상태에서 파생된 일은 잘못된
    -- 요청이 아니라 항상 유효하고, 실패는 저장소·네트워크의 일시 장애다.
    -- 무한히 재시도하되 backoff로 간격을 벌리고, 누적 횟수가 곧 "막혔다"는
    -- 신호다 (자가치유가 원칙, 관측은 attempts).
    attempts   int NOT NULL DEFAULT 0,
    -- 이 시각 전에는 집지 않는다 — 실패 backoff가 여기 얹힌다.
    run_at     timestamptz NOT NULL DEFAULT now(),
    claimed_at timestamptz,
    claimed_by text,
    last_error text,
    created_at timestamptz NOT NULL DEFAULT now(),
    -- active는 누가 언제 집었는지 반드시 가진다 — 파드가 죽었을 때 회수의
    -- 유일한 근거다.
    CHECK (state <> 'active' OR (claimed_at IS NOT NULL AND claimed_by IS NOT NULL)),
    -- 파일당 갈래당 하나. 매 회차의 재도출이 중복을 만들지 않는다 (멱등 enqueue).
    UNIQUE (kind, file_id)
);

-- 워커의 dequeue — 집을 수 있는 것 중 가장 오래 기다린 것.
CREATE INDEX tasks_ready_idx ON tasks (run_at) WHERE state = 'queued';
-- reconciler의 claim 만료 회수 — 죽은 파드가 쥔 채 남은 것.
CREATE INDEX tasks_claimed_idx ON tasks (claimed_at) WHERE state = 'active';
