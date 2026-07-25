-- 배치 — 실물 하나당 한 행 (ADR 007).
--
-- 0001의 locations는 파일당 한 행이었다. 그래서 이동 중 "한 파일에 두 자리"를
-- 표현할 수 없었고, 옮기고 남은 실물이 장부 밖으로 떨어졌다. 키를 실물 주소로
-- 뒤집고 역할을 두면 그 자리가 생긴다.
--
--   primary   정본. 읽기가 본다. 파일당 하나
--   staging   채워질 자리. 아직 참조되지 않는다 (이동의 의도이기도 하다)
--   dropped   버려졌다. 실물만 남았고 지워지길 기다린다
--
-- 불변식: 실물이 있으면 행이 있다. 행을 지우는 것은 실물을 지운 집행자뿐이다.
-- 요청 경로와 판단자는 INSERT·UPDATE만 한다 (ADR 007 권한 구분).

ALTER TABLE locations RENAME TO placements;

ALTER TABLE placements
    ADD COLUMN role text NOT NULL DEFAULT 'primary'
        CHECK (role IN ('primary', 'staging', 'dropped')),
    -- dropped의 실물을 지울 수 있는 시각. 발급된 읽기 URL은 저장소가 서명해
    -- DB와 무관하게 자기 수명까지 유효하므로, 그 수명이 지나야 지운다.
    ADD COLUMN drop_after timestamptz,
    -- multipart 잔여물 회수 재료. 버려질 때 실려 온다 — lease가 GC된 뒤에도
    -- 벤더 세션 중단과 조립 임시파일 정리가 가능해야 한다.
    ADD COLUMN upload_id text,
    ADD COLUMN lease_id  uuid;

-- 키를 실물 주소로. 파일당 한 행이 아니라 실물당 한 행이다.
ALTER TABLE placements DROP CONSTRAINT locations_pkey;
ALTER TABLE placements DROP CONSTRAINT locations_storage_id_object_key_key;
ALTER TABLE placements ADD PRIMARY KEY (storage_id, object_key);

-- 파일당 정본은 하나. 이동 중에는 primary + staging 둘이 공존한다.
CREATE UNIQUE INDEX placements_primary_idx ON placements (file_id) WHERE role = 'primary';

-- 파일별 조회(읽기·이동 판정)와 storage별 집계.
CREATE INDEX placements_file_idx ON placements (file_id);
CREATE INDEX placements_storage_idx ON placements (storage_id, role);

-- 파일당 준비 중 자리도 하나 — 이동은 파일당 하나뿐이다.
CREATE UNIQUE INDEX placements_staging_idx ON placements (file_id) WHERE role = 'staging';
CREATE INDEX placements_dropped_idx ON placements (drop_after) WHERE role = 'dropped';

-- dropped는 언제 지울 수 있는지를 반드시 안다.
ALTER TABLE placements ADD CONSTRAINT placements_drop_after_check
    CHECK (role <> 'dropped' OR drop_after IS NOT NULL);

-- 배치 변경의 원장 — 행이 사라진 뒤에도 무엇이 어디로 갔는지 남는다.
-- FK를 걸지 않는다: 이력은 파일·등록부 삭제와 독립적으로 생존해야 하는
-- 로그이고, 로그가 등록부 삭제를 막아서도 안 된다 (lease_history와 같은 결).
CREATE TABLE move_history (
    at        timestamptz NOT NULL DEFAULT now(),
    file_id   uuid   NOT NULL,
    source_storage_id text NOT NULL,
    dest_storage_id   text NOT NULL,
    size      bigint NOT NULL CHECK (size >= 0),
    -- moved: 정본이 옮겨졌다. lost: 옮기려다 졌다 (요청 경로가 이겼다).
    outcome   text   NOT NULL CHECK (outcome IN ('moved', 'lost'))
);

CREATE INDEX move_history_at_idx ON move_history (at);
-- 정책의 쿨다운 조회 — 최근에 옮긴 파일을 후보에서 뺀다.
CREATE INDEX move_history_file_idx ON move_history (file_id, at);
