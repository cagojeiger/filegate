-- 집행 큐 — 판단자가 넣고, 파드마다 뜬 집행자가 집어간다 (spec 04).
--
-- 큐가 필요한 이유는 배타성의 단위를 바꾸는 것이다. advisory lock은 파드
-- 하나를 고를 뿐이라 파드를 늘려도 집행 용량이 늘지 않는다. 행 단위 claim은
-- 작업을 나눠주므로 용량이 파드 수에 비례한다.
--
-- 갈래가 셋이고, 저장소에 할 수 있는 동사와 1:1이다:
--   observe  확인    pending 실물이 선언과 맞는지 본다 (HEAD)
--   copy     만들기  staging 자리를 채우고 정본을 교체한다 (GET+PUT)
--   delete   지우기  dropped 실물을 없앤다 (DELETE)
--
-- 상태 전이만 하는 일(소프트 삭제 집행, 만료 중단)은 여기 오지 않는다 —
-- 실물을 안 만지므로 판단자가 벌크 SQL로 직접 한다.
CREATE TABLE tasks (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    kind       text NOT NULL CHECK (kind IN ('observe', 'copy', 'delete')),

    -- 대상은 갈래에 따라 다르다. observe·copy는 파일이고, delete는 실물
    -- 주소다 — 지울 실물은 이미 어떤 파일의 정본도 아니므로 파일로 가리킬
    -- 이유가 없다.
    file_id    uuid REFERENCES files (id) ON DELETE CASCADE,
    storage_id text REFERENCES storages (id),
    object_key text,

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
    -- 갈래마다 대상이 정해져 있다.
    CHECK (CASE kind
             WHEN 'delete' THEN storage_id IS NOT NULL AND object_key IS NOT NULL
             ELSE file_id IS NOT NULL
           END)
);

-- 매 회차의 재도출이 중복을 만들지 않는다 (멱등 enqueue).
CREATE UNIQUE INDEX tasks_file_idx ON tasks (kind, file_id) WHERE file_id IS NOT NULL;
CREATE UNIQUE INDEX tasks_object_idx ON tasks (storage_id, object_key) WHERE object_key IS NOT NULL;

-- 집행자의 dequeue: 집을 수 있는 것 중 가장 오래 기다린 것.
CREATE INDEX tasks_ready_idx ON tasks (run_at) WHERE state = 'queued';
-- 판단자의 claim 만료 회수 — 죽은 파드가 쥔 채 남은 것.
CREATE INDEX tasks_claimed_idx ON tasks (claimed_at) WHERE state = 'active';
