-- 등록부 스키마 — 소유자가 다른 세 계층 (ADR 004, spec 01).
--
-- providers(물리 접근 계약) / profiles(운영자 배치 카탈로그) /
-- clients(서비스 어휘: 키 + intents). 연결은 한 방향:
-- clients.intents → profiles → providers.
--
-- 참조 무결성은 쓰기 시점에 DB가 집행한다 (ADR 004): 사용 중인
-- 등록의 삭제는 FK가 거부하고, 클라이언트 자신의 소유물(키·intent)만
-- 클라이언트와 함께 사라진다. id는 운영자가 정하는 안정 슬러그다.

-- 물리 저장 공간 접근 계약. 행 생성은 운영자 API만 한다 — 시크릿이
-- 암호문(AES-256-GCM, AAD=id, 마스터 키는 env)이라 SQL로 못 만든다.
-- enc_key_id는 이 행을 잠근 마스터 키 세대 라벨 — 복호는 라벨로
-- 키를 고른다 (spec 01 회전 런북).
CREATE TABLE providers (
    id                    text PRIMARY KEY
                          CHECK (id ~ '^[a-z0-9]([a-z0-9-]{0,62}[a-z0-9])?$'),
    endpoint              text NOT NULL,
    region                text NOT NULL,
    bucket                text NOT NULL,
    force_path_style      boolean NOT NULL DEFAULT false,
    access_key            text NOT NULL,
    secret_key_ciphertext bytea NOT NULL,
    secret_key_nonce      bytea NOT NULL CHECK (octet_length(secret_key_nonce) = 12),
    enc_key_id            text NOT NULL,
    -- capacity 상한은 등록의 일부다 (ADR 004). 기본값 없음 — 등록자가 정한다.
    capacity_bytes        bigint NOT NULL CHECK (capacity_bytes >= 0),
    created_at            timestamptz NOT NULL DEFAULT now(),
    updated_at            timestamptz NOT NULL DEFAULT now()
);

-- 운영자 배치 카탈로그. v0는 provider 하나를 가리키는 명시 선언뿐 (spec 01).
-- 자동 배치가 오면 후보 풀·전략 컬럼이 여기 늘어난다.
CREATE TABLE profiles (
    id          text PRIMARY KEY
                CHECK (id ~ '^[a-z0-9]([a-z0-9-]{0,62}[a-z0-9])?$'),
    provider_id text NOT NULL REFERENCES providers (id), -- 참조되는 provider는 삭제 거부
    created_at  timestamptz NOT NULL DEFAULT now()
);

-- 서비스 신원. 어휘(intents)와 키의 네임스페이스다.
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

-- intent는 클라이언트 네임스페이스 안에 살고 profile을 가리킨다 (ADR 004).
-- provider를 직접 가리키지 않는다 — 배치를 바꿔도 서비스 계약은 유지된다.
CREATE TABLE intents (
    client_id  text NOT NULL REFERENCES clients (id) ON DELETE CASCADE,
    name       text NOT NULL
               CHECK (name ~ '^[a-z0-9]([a-z0-9-]{0,62}[a-z0-9])?$'),
    profile_id text NOT NULL REFERENCES profiles (id), -- 참조되는 profile은 삭제 거부
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (client_id, name)
);

-- 도메인 테이블(0001)의 느슨한 text id를 등록부에 묶는다.
-- 파일을 가진 client, 위치·회계가 남은 provider는 삭제가 거부된다 (ADR 004).
ALTER TABLE files
    ADD CONSTRAINT files_client_fk FOREIGN KEY (client_id) REFERENCES clients (id);
ALTER TABLE locations
    ADD CONSTRAINT locations_provider_fk FOREIGN KEY (provider_id) REFERENCES providers (id);
ALTER TABLE provider_usage
    ADD CONSTRAINT provider_usage_provider_fk FOREIGN KEY (provider_id) REFERENCES providers (id);
