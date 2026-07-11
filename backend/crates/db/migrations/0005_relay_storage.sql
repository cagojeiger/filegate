-- 중계(relay) 모드 — presigned를 못 쓰는 storage를 위한 호환 기능 (spec 00).
-- 서비스 계약은 직결과 동일하고, URL이 filegate 바이트 엔드포인트를 가리킬 뿐이다.

-- storage 종류와 접근 모드 (ADR 001: capability는 선언식 — 운영자가 선언하고
-- 틀리면 사용 시 실패한다).
--   s3: 기존 접속 필드 필수. force_relay로 직결 대신 중계를 강제할 수 있다
--       (CORS 없는 벤더, 사설망 뒤의 MinIO 직접 제어).
--   fs: root_path 하나가 접근 계약의 전부다 — 시크릿이 없는 storage.
--       presigned 개념이 없으므로 항상 중계다.
ALTER TABLE storages
    ADD COLUMN kind text NOT NULL DEFAULT 's3' CHECK (kind IN ('s3', 'fs')),
    ADD COLUMN force_relay boolean NOT NULL DEFAULT false,
    ADD COLUMN root_path text;

-- s3 접속 필드를 fs가 비울 수 있게 푼다. 종류별 필수는 아래 CHECK가 집행한다.
ALTER TABLE storages
    ALTER COLUMN endpoint DROP NOT NULL,
    ALTER COLUMN public_endpoint DROP NOT NULL,
    ALTER COLUMN region DROP NOT NULL,
    ALTER COLUMN bucket DROP NOT NULL,
    ALTER COLUMN access_key DROP NOT NULL,
    ALTER COLUMN secret_key_ciphertext DROP NOT NULL,
    ALTER COLUMN secret_key_nonce DROP NOT NULL,
    ALTER COLUMN enc_key_id DROP NOT NULL;

ALTER TABLE storages
    ADD CONSTRAINT storages_s3_fields CHECK (
        kind <> 's3' OR (
            endpoint IS NOT NULL AND public_endpoint IS NOT NULL
            AND region IS NOT NULL AND bucket IS NOT NULL
            AND access_key IS NOT NULL AND secret_key_ciphertext IS NOT NULL
            AND secret_key_nonce IS NOT NULL AND enc_key_id IS NOT NULL
            AND root_path IS NULL
        )
    ),
    ADD CONSTRAINT storages_fs_fields CHECK (
        kind <> 'fs' OR (
            root_path IS NOT NULL
            AND endpoint IS NULL AND public_endpoint IS NULL
            AND region IS NULL AND bucket IS NULL
            AND access_key IS NULL AND secret_key_ciphertext IS NULL
            AND secret_key_nonce IS NULL AND enc_key_id IS NULL
            AND force_relay = false -- fs는 선언 없이도 항상 중계
        )
    );

-- 중계 lease의 인증·검증 재료 (ADR 003: 중계 바이트 엔드포인트는 lease별 secret).
--   secret_hash: 바이트 엔드포인트 인증 — raw는 URL에만 산다 (클라이언트 키와
--                같은 원칙: 서버는 해시만).
--   uploaded_size/md5: 중계 쓰기가 스트림 중 직접 계산해 기록 — commit의
--                      사후 검증이 head_object 대신 이것을 대조한다.
--   read_filename: 중계 읽기의 Content-Disposition 표현 (직결의 서명 파라미터 등가).
ALTER TABLE leases
    ADD COLUMN secret_hash text CHECK (secret_hash ~ '^sha256:[0-9a-f]{64}$'),
    ADD COLUMN uploaded_size bigint CHECK (uploaded_size >= 0),
    ADD COLUMN uploaded_md5 text,
    ADD COLUMN read_filename text;
