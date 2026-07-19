-- 이동 저널 — 파일 하나의 storage 이동(복사→검증→스왑→지연삭제)의 유일한 상태.
-- 완료는 행 삭제다. FK가 이동 중 storage 삭제를 거부한다 (spec 01 삭제 방패).
CREATE TABLE object_moves (
    file_id           uuid PRIMARY KEY REFERENCES files (id),
    source_storage_id text NOT NULL REFERENCES storages (id),
    dest_storage_id   text NOT NULL REFERENCES storages (id),
    object_key        text NOT NULL,
    state             text NOT NULL DEFAULT 'requested'
                      CHECK (state IN ('requested', 'swapped', 'failed')),
    attempts          int  NOT NULL DEFAULT 0,
    next_attempt_at   timestamptz NOT NULL DEFAULT now(),
    delete_after      timestamptz,
    last_error        text,
    created_at        timestamptz NOT NULL DEFAULT now(),
    CHECK (source_storage_id <> dest_storage_id),
    CHECK (state <> 'swapped' OR delete_after IS NOT NULL)
);
CREATE INDEX object_moves_due_idx ON object_moves (state, next_attempt_at);
