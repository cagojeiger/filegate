-- 등록부 스키마 — 독립 노드 둘과 엣지 하나 (ADR 004, spec 01).
--
-- storages(물리 접근 계약)와 clients(서비스 신원)는 서로를 모른다.
-- bindings(client의 intent 이름 → storage)만이 둘을 참조한다.
--
-- 참조 무결성은 쓰기 시점에 DB가 집행한다 (ADR 004): binding이 남아 있는
-- storage·client의 삭제는 FK가 거부한다 — 연결을 먼저 지워야 노드를 지운다.
-- 클라이언트의 소유물(키)만 클라이언트와 함께 사라진다. id는 운영자가
-- 정하는 안정 슬러그다 — fs 경로와 object_key에 들어가므로 슬러그 CHECK가
-- 경로 안전도 겸한다 (spec 00 물리 배치).

-- 물리 저장 공간 접근 계약. 행 생성은 운영자 API만 한다.
-- 종류가 둘이다 (ADR 001: capability는 선언식):
--   s3: 접속 필드 필수, 시크릿은 암호문(AES-256-GCM, AAD=id).
--       enc_key_id는 마스터 키 세대 라벨 (spec 01 회전 런북).
--       force_relay로 직결 대신 중계를 강제할 수 있다.
--   fs: root_path 하나가 접근 계약의 전부 — 시크릿 없는 storage.
--       presigned 개념이 없으므로 항상 중계다.
CREATE TABLE storages (
    id                    text PRIMARY KEY
                          CHECK (id ~ '^[a-z0-9]([a-z0-9-]{0,62}[a-z0-9])?$'),
    kind                  text NOT NULL DEFAULT 's3' CHECK (kind IN ('s3', 'fs')),
    force_relay           boolean NOT NULL DEFAULT false,
    root_path             text,
    endpoint              text,
    -- 내부 접근 주소와 외부 서명/전송 주소의 분리 (ADR 001).
    public_endpoint       text,
    region                text,
    bucket                text,
    force_path_style      boolean NOT NULL DEFAULT false,
    access_key            text,
    secret_key_ciphertext bytea,
    secret_key_nonce      bytea CHECK (octet_length(secret_key_nonce) = 12),
    enc_key_id            text,
    -- capacity 상한은 등록의 일부다 (ADR 004). 기본값 없음 — 등록자가 정한다.
    capacity_bytes        bigint NOT NULL CHECK (capacity_bytes >= 0),
    created_at            timestamptz NOT NULL DEFAULT now(),
    updated_at            timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT storages_s3_fields CHECK (
        kind <> 's3' OR (
            endpoint IS NOT NULL AND public_endpoint IS NOT NULL
            AND region IS NOT NULL AND bucket IS NOT NULL
            AND access_key IS NOT NULL AND secret_key_ciphertext IS NOT NULL
            AND secret_key_nonce IS NOT NULL AND enc_key_id IS NOT NULL
            AND root_path IS NULL
        )
    ),
    CONSTRAINT storages_fs_fields CHECK (
        kind <> 'fs' OR (
            root_path IS NOT NULL
            AND endpoint IS NULL AND public_endpoint IS NULL
            AND region IS NULL AND bucket IS NULL
            AND access_key IS NULL AND secret_key_ciphertext IS NULL
            AND secret_key_nonce IS NULL AND enc_key_id IS NULL
            AND force_relay = false -- fs는 선언 없이도 항상 중계
        )
    )
);

-- 서비스 신원. 키의 소유자이자 intent 이름의 네임스페이스다.
CREATE TABLE clients (
    id         text PRIMARY KEY
               CHECK (id ~ '^[a-z0-9]([a-z0-9-]{0,62}[a-z0-9])?$'),
    created_at timestamptz NOT NULL DEFAULT now()
);

-- 클라이언트 키 — sha256 해시만 저장한다 (spec 01: raw는 서버에 도달하지
-- 않는다). 회전 = 행 추가·삭제. 키는 클라이언트의 소유물이라 함께 사라진다.
CREATE TABLE client_keys (
    key_hash   text PRIMARY KEY CHECK (key_hash ~ '^sha256:[0-9a-f]{64}$'),
    client_id  text NOT NULL REFERENCES clients (id) ON DELETE CASCADE,
    created_at timestamptz NOT NULL DEFAULT now()
);

-- 인증 후 클라이언트의 키 목록 조회용 (회전 시 관리).
CREATE INDEX client_keys_client_idx ON client_keys (client_id);

-- 엣지: 클라이언트의 intent 이름을 storage에 잇는다 (ADR 004).
-- 서비스 계약은 intent 이름이고, storage 포인터는 운영자가 바꾼다 —
-- 배치를 바꿔도 서비스 계약은 유지된다. 양끝 노드는 이 엣지가 남아
-- 있는 동안 삭제가 거부된다 (FK 기본 = RESTRICT 동작).
CREATE TABLE bindings (
    client_id  text NOT NULL REFERENCES clients (id),
    intent     text NOT NULL
               CHECK (intent ~ '^[a-z0-9]([a-z0-9-]{0,62}[a-z0-9])?$'),
    storage_id text NOT NULL REFERENCES storages (id),
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (client_id, intent)
);

-- storage 삭제 시 FK RESTRICT 검사가 이 인덱스를 탄다 (PK는 client 쪽만 커버).
CREATE INDEX bindings_storage_idx ON bindings (storage_id);

-- 도메인 테이블(0001)의 느슨한 text id를 등록부에 묶는다.
-- 파일을 가진 client, 위치(실물)가 남은 storage는 삭제가 거부된다 (ADR 004).
ALTER TABLE files
    ADD CONSTRAINT files_client_fk FOREIGN KEY (client_id) REFERENCES clients (id);
ALTER TABLE locations
    ADD CONSTRAINT locations_storage_fk FOREIGN KEY (storage_id) REFERENCES storages (id);
