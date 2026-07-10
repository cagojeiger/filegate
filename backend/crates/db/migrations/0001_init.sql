-- 도메인 스키마 — spec 00의 상태 기계를 DB 제약으로 표현한다.
--
-- 상태 전이의 집행은 repo의 조건부 갱신 몫이다 (docs/stack). 여기서는 값
-- 도메인, 상태-시각 정합, 음수 회계를 DB가 거부하게 한다.

-- 파일 정체성. 상태: pending(미확정) → active(확정) → deleted(purge 대기).
-- purge 후에도 행은 deleted로 남는다 — stat이 계속 답한다 (spec 00).
CREATE TABLE files (
    id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    client_id     text NOT NULL,
    intent        text NOT NULL,
    state         text NOT NULL DEFAULT 'pending'
                  CHECK (state IN ('pending', 'active', 'deleted')),
    declared_size bigint NOT NULL CHECK (declared_size >= 0),
    content_type  text,
    declared_md5  text,
    etag          text, -- commit 시점 기록. 변조 판정 기준 (spec 00)
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
CREATE TABLE locations (
    file_id    uuid PRIMARY KEY REFERENCES files (id),
    storage_id text NOT NULL,
    object_key text NOT NULL, -- filegate가 발급한 불투명 키 (ADR 001)
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (storage_id, object_key)
);

-- lease 원장 — 접근 추적·감사의 단일 근원 (ADR 002). 중계 토큰도 여기 얹힌다.
-- 생애: issued → committed | expired | canceled (spec 00).
CREATE TABLE leases (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    file_id    uuid NOT NULL REFERENCES files (id),
    kind       text NOT NULL CHECK (kind IN ('write', 'read')),
    state      text NOT NULL DEFAULT 'issued'
               CHECK (state IN ('issued', 'committed', 'expired', 'canceled')),
    expires_at timestamptz NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);

-- reconciler의 만료 회수 스캔과 파일별 감사 조회.
CREATE INDEX leases_expiry_idx ON leases (expires_at) WHERE state = 'issued';
CREATE INDEX leases_file_idx ON leases (file_id);

-- storage별 capacity 회계 — usage의 세 버킷 (spec 00). 상한은 등록부에 산다.
-- 예약은 create, 정산은 commit, 해제는 만료 회수·purge (단일 트랜잭션 안에서).
CREATE TABLE storage_usage (
    storage_id          text PRIMARY KEY,
    reserved_bytes      bigint NOT NULL DEFAULT 0 CHECK (reserved_bytes >= 0),
    active_bytes        bigint NOT NULL DEFAULT 0 CHECK (active_bytes >= 0),
    purge_pending_bytes bigint NOT NULL DEFAULT 0 CHECK (purge_pending_bytes >= 0),
    updated_at          timestamptz NOT NULL DEFAULT now()
);
