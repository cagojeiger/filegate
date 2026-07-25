-- 이동의 의도와 진행 — 큐가 아니라 상태다.
--
-- 재시도·backoff·claim·회수는 tasks가 이미 한다. 여기 남는 것은 tasks가
-- 알 수 없는 것뿐이다: 어디로 옮기는가(의도)와 스왑이 끝났는가(진행).
-- reconciler가 이 상태에서 집행 작업을 도출한다 — files에서 purge를
-- 도출하는 것과 같은 결이다.
--
--   state='requested'                      → task kind='move'
--   state='swapped' AND delete_after < now → task kind='move_cleanup'
--
-- 이동은 상태에서 파생되지 않는 유일한 작업이다. "이 파일을 저기로"는
-- 운영자나 정책의 결정이라 어딘가 적지 않으면 재시작 후 사라진다. 그 자리가
-- 이 행이고, 그래서 요청 경로는 tasks가 아니라 여기에 쓴다 (spec 04 불변식 1).
CREATE TABLE object_moves (
    -- 파일당 진행 중 이동은 하나다.
    file_id           uuid PRIMARY KEY REFERENCES files (id) ON DELETE CASCADE,
    source_storage_id text NOT NULL REFERENCES storages (id),
    dest_storage_id   text NOT NULL REFERENCES storages (id),
    -- 키는 storage 무관이라 그대로 재사용한다 — 대상이 결정적이므로 재복사가
    -- 멱등이고, 스왑은 포인터 교체만으로 끝난다.
    object_key        text NOT NULL,
    state             text NOT NULL DEFAULT 'requested'
                      CHECK (state IN ('requested', 'swapped')),
    -- 스왑 뒤 source 실물을 지울 수 있는 시각. 발급된 읽기 URL의 수명이
    -- 지나야 한다 — 그 전에 지우면 유효한 URL이 404가 된다.
    delete_after      timestamptz,
    created_at        timestamptz NOT NULL DEFAULT now(),
    CHECK (source_storage_id <> dest_storage_id),
    CHECK (state <> 'swapped' OR delete_after IS NOT NULL)
);

-- reconciler의 도출 — 집행 대기와 삭제 대기.
CREATE INDEX object_moves_pending_idx ON object_moves (state, delete_after);

-- 종결의 박제 — 행이 사라진 뒤에도 무엇이 어디로 갔는지 남는다. FK를 걸지
-- 않는다: 이력은 파일·등록부 삭제와 독립적으로 생존해야 하는 로그고, 로그가
-- 등록부 삭제를 막아서도 안 된다 (lease_history와 같은 결).
CREATE TABLE move_history (
    at        timestamptz NOT NULL DEFAULT now(),
    file_id   uuid   NOT NULL,
    source_storage_id text NOT NULL,
    dest_storage_id   text NOT NULL,
    size      bigint NOT NULL CHECK (size >= 0),
    -- moved: 스왑·삭제까지 끝남. lost: 요청 경로가 이겨 이동이 조용히 짐.
    outcome   text   NOT NULL CHECK (outcome IN ('moved', 'lost'))
);

CREATE INDEX move_history_at_idx ON move_history (at);

-- 이동 집행과 스왑 뒤 정리 — 둘 다 저장소 백엔드를 만지므로 워커의 몫이다.
ALTER TABLE tasks DROP CONSTRAINT tasks_kind_check;
ALTER TABLE tasks ADD CONSTRAINT tasks_kind_check
    CHECK (kind IN ('observe', 'reclaim', 'purge', 'move', 'move_cleanup'));
