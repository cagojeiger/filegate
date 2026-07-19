-- 도메인 스키마 — spec 00·02의 상태 기계와 원장을 DB 제약으로 표현한다.
--
-- 상태 전이의 집행은 repo의 조건부 갱신 몫이다 (docs/stack). 여기서는 값
-- 도메인, 상태-시각 정합, 음수 회계를 DB가 거부하게 한다.

-- 파일 정체성. 상태: pending(미확정) → active(확정) → deleted(purge 대기).
-- pending은 lease 만료로 reclaimed가 될 수 있다 — 회수와 늦은 commit의
-- 경합을 조건부 전이 하나로 끊는 종착 상태다. purge 후에도 행은 deleted로
-- 남는다 — stat이 계속 답한다 (spec 00).
CREATE TABLE files (
    id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    client_id     text NOT NULL,
    state         text NOT NULL DEFAULT 'pending'
                  CHECK (state IN ('pending', 'active', 'deleted', 'reclaimed')),
    declared_size bigint NOT NULL CHECK (declared_size >= 0),
    content_type  text,
    declared_md5  text,
    etag          text, -- commit 시점 기록. 변조 판정 기준 (spec 00)
    -- multipart 업로드별 동결 part 크기 (spec 02). NULL = 단일 PUT.
    -- 운영자 설정이 바뀌어도 진행 중 업로드의 offset 파생이 흔들리지 않는다.
    part_size     bigint CHECK (part_size > 0),
    created_at    timestamptz NOT NULL DEFAULT now(),
    committed_at  timestamptz,
    deleted_at    timestamptz,
    -- active는 확정 시각을, deleted는 확정·삭제 시각을 반드시 가진다.
    CHECK (state <> 'active' OR committed_at IS NOT NULL),
    CHECK (state <> 'deleted' OR (committed_at IS NOT NULL AND deleted_at IS NOT NULL))
);

-- 소유 조회(클라이언트는 자기 file만)와 reconciler 스캔(pending 회수·purge 대기).
CREATE INDEX files_client_idx ON files (client_id);
CREATE INDEX files_nonactive_idx ON files (state, created_at) WHERE state <> 'active';

-- 파일의 현재 물리 위치 — 가변 포인터 (ADR 001: file/location 분리).
-- v0는 파일당 위치 하나. 이동은 포인터 교체, purge는 행 삭제.
-- object_key는 만들 때 규칙(spec 00 물리 배치)으로 조합해 저장한다 —
-- 읽기·삭제는 항상 저장된 키를 따르므로 규칙이 바뀌어도 기존 객체는 동작한다.
CREATE TABLE locations (
    file_id    uuid PRIMARY KEY REFERENCES files (id),
    storage_id text NOT NULL,
    object_key text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (storage_id, object_key)
);

-- lease 원장 — 접근 추적·감사의 단일 근원 (ADR 002). 중계 토큰도 여기 얹힌다.
-- 생애: issued → committed | expired | canceled (spec 00).
--   secret_hash: 중계 바이트 엔드포인트 인증 — raw는 URL에만 산다.
--   uploaded_size/md5: 중계 쓰기의 스트림 중 실측 — commit이 대조한다.
--   upload_id: 직결 multipart의 벤더 세션 핸들 (spec 02) — 파생 불가능한
--              외부 값이라 저장한다. 회수가 Abort에 쓴다.
CREATE TABLE leases (
    id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    file_id       uuid NOT NULL REFERENCES files (id),
    kind          text NOT NULL CHECK (kind IN ('write', 'read')),
    state         text NOT NULL DEFAULT 'issued'
                  CHECK (state IN ('issued', 'committed', 'expired', 'canceled')),
    expires_at    timestamptz NOT NULL,
    secret_hash   text CHECK (secret_hash ~ '^sha256:[0-9a-f]{64}$'),
    -- multipart relay의 write secret raw — parts() 발급이 매번 같은 secret으로
    -- URL을 조립해야 하므로(회전 금지, spec 02), 업로드 중에만 원문을 보관한다.
    -- 종료(commit·회수) 시 NULL로 지운다. 단일 PUT relay는 URL 1회 발급이라
    -- 이 컬럼을 쓰지 않는다(해시만).
    write_secret  text,
    uploaded_size bigint CHECK (uploaded_size >= 0),
    uploaded_md5  text,
    upload_id     text,
    created_at    timestamptz NOT NULL DEFAULT now()
);

-- reconciler의 만료 회수 스캔과 파일별 감사 조회.
CREATE INDEX leases_expiry_idx ON leases (expires_at) WHERE state = 'issued';
CREATE INDEX leases_file_idx ON leases (file_id);

-- multipart part 실측 (spec 02). 기하(개수·offset·명목 크기)는 저장하지
-- 않는다 — declared_size + 동결 part_size에서 전부 파생된다. 여기 남는 것은
-- 실측(크기·체크섬)과 승격 직렬화 상태뿐이다. claimed 행이 같은 part 동시
-- PUT의 인터리브 손상을 막는 가드다 (단일 PUT의 temp 충돌과 같은 처방).
CREATE TABLE lease_parts (
    lease_id      uuid NOT NULL REFERENCES leases (id) ON DELETE CASCADE,
    part_no       integer NOT NULL CHECK (part_no >= 1),
    state         text NOT NULL DEFAULT 'claimed'
                  CHECK (state IN ('claimed', 'done')),
    uploaded_size bigint CHECK (uploaded_size >= 0),
    uploaded_md5  text,
    -- done은 실측을 반드시 가진다.
    CHECK (state <> 'done' OR (uploaded_size IS NOT NULL AND uploaded_md5 IS NOT NULL)),
    PRIMARY KEY (lease_id, part_no)
);

-- 대여 이력 — 관찰용 durable 로그 (통계 분포·파일별 개별 이력·idle 판단).
-- leases는 인증·수명 원장이라 짧게 살고 GC되지만, 이력은 여기 남는다.
-- 발급과 같은 트랜잭션에 기록되므로 lease와 항상 짝이다. FK는 걸지
-- 않는다 — 이력은 등록부·파일 삭제와 독립적으로 생존해야 하는 로그고,
-- 로그가 등록부 삭제를 막아서도 안 된다. 보존은 reconciler가 다스린다.
CREATE TABLE lease_history (
    at         timestamptz NOT NULL DEFAULT now(),
    file_id    uuid NOT NULL,
    storage_id text NOT NULL,
    client_id  text NOT NULL,
    kind       text NOT NULL CHECK (kind IN ('write', 'read')),
    size       bigint NOT NULL CHECK (size >= 0)
);

-- 보존 prune과 시계열 조회.
CREATE INDEX lease_history_at_idx ON lease_history (at);
-- 파일별 개별 이력과 idle 판단 (마지막 대여 시각).
CREATE INDEX lease_history_file_idx ON lease_history (file_id, at);

-- 일별 사용량 스냅샷 — stock(점유)의 시계열 박제 (spec 00). flow와 달리
-- 점유의 과거는 소급 계산이 불가하므로(purge가 행을 지운다) reconciler가
-- 매일 UTC 자정 이후 첫 tick에 어제 종점을 기록한다. lease_history처럼
-- FK는 걸지 않는다 — 지워진 storage·client의 과거 점유도 이력으로 남고,
-- 이력이 등록부 삭제를 막아서도 안 된다. 보존은 무기한 — 조합당 하루
-- 1행이라 성장이 유계다.
CREATE TABLE usage_snapshot (
    day          date   NOT NULL,
    storage_id   text   NOT NULL,
    client_id    text   NOT NULL,
    active_bytes bigint NOT NULL CHECK (active_bytes >= 0),
    active_files bigint NOT NULL CHECK (active_files >= 0),
    PRIMARY KEY (day, storage_id, client_id)
);

-- 회계 카운터는 두지 않는다 (spec 00) — capacity는 집행이 아니라 관찰이다.
-- 사용량은 조회 시점에 files·locations에서 집계한다: purge가 location을
-- 실제로 지우므로 "남은 행 = 현재 점유"고, 파생값을 저장하지 않으니
-- 어긋날 것도 없다. 시계열 관찰은 lease_history(flow)와
-- usage_snapshot(stock)이 담당한다.
