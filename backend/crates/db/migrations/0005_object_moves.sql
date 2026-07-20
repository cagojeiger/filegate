-- 이동 저널 — 파일 하나의 storage 이동(복사→검증→스왑→지연삭제)의 유일한 상태.
-- 완료는 행 삭제다. FK가 이동 중 storage 삭제를 거부한다 (spec 01 삭제 방패).
CREATE TABLE object_moves (
    file_id           uuid PRIMARY KEY REFERENCES files (id),
    source_storage_id text NOT NULL REFERENCES storages (id),
    dest_storage_id   text NOT NULL REFERENCES storages (id),
    object_key        text NOT NULL,
    state             text NOT NULL DEFAULT 'requested'
                      CHECK (state IN ('requested', 'canceled', 'swapped', 'failed')),
    -- 집행 우선순위 — 낮을수록 먼저. 운영자 수동 이동(0)이 정책 이동(100)을
    -- 항상 추월한다. 정책 간 서열도 이 값으로 표현한다.
    priority          smallint NOT NULL DEFAULT 100,
    attempts          int  NOT NULL DEFAULT 0,
    next_attempt_at   timestamptz NOT NULL DEFAULT now(),
    delete_after      timestamptz,
    last_error        text,
    created_at        timestamptz NOT NULL DEFAULT now(),
    CHECK (source_storage_id <> dest_storage_id),
    CHECK (state <> 'swapped' OR delete_after IS NOT NULL)
);
CREATE INDEX object_moves_due_idx ON object_moves (state, priority, next_attempt_at);

-- 이동 결과 원장 — 종결 시 reconciler가 같은 tx로 박제한다 (lease_history와 같은
-- 원칙). 파일·storage 행이 정리된 뒤에도 홀로 읽히도록 FK 없이 박제한다.
CREATE TABLE move_history (
    id                uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    file_id           uuid NOT NULL,
    client_id         text NOT NULL,
    source_storage_id text NOT NULL,
    dest_storage_id   text NOT NULL,
    object_key        text NOT NULL,
    size_bytes        bigint NOT NULL,
    outcome           text NOT NULL CHECK (outcome IN ('moved', 'lost', 'canceled')),
    attempts          int  NOT NULL,
    last_error        text,
    requested_at      timestamptz NOT NULL,
    finished_at       timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX move_history_file_idx  ON move_history (file_id, finished_at);
CREATE INDEX move_history_prune_idx ON move_history (finished_at);
