-- S3 호환 표면의 등록부 (spec 03, ADR 006).
--
-- s3_credentials: access key id → client. secret은 저장하지 않는다 —
-- SigV4는 서버가 secret으로 HMAC을 재계산해야 해서 해시 보관이 불가능하고,
-- 마스터 키 + access key id에서 파생하면(core::Crypto::s3_secret) 저장
-- 자체가 없다. 발급과 검증이 같은 값을 재계산한다.
CREATE TABLE s3_credentials (
    access_key_id text PRIMARY KEY CHECK (access_key_id ~ '^[a-z0-9]{8,64}$'),
    client_id     text NOT NULL REFERENCES clients (id) ON DELETE CASCADE,
    created_at    timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX s3_credentials_client_idx ON s3_credentials (client_id);

-- s3_keys: S3 표면의 논리 이름공간 — (client, bucket=intent, key) → file.
-- 서비스가 정한 이름(논리키)은 서비스 소유다 (ADR 003). 물리 배치와
-- 무관하다 (물리는 locations 소유 — tiering이 위치를 옮겨도 매핑 불변).
-- overwrite(같은 키 재PUT)는 매핑을 새 file로 갈아끼우고 옛 file은 delete
-- 결정으로 넘긴다 — S3의 덮어쓰기 시맨틱을 상태 기계로 번역한 것.
-- file FK는 CASCADE: 종착 행 보존 정리(spec 00)가 file 행을 지울 때
-- 매핑도 함께 사라진다 — 매달린 매핑이 남지 않는다.
CREATE TABLE s3_keys (
    client_id  text NOT NULL REFERENCES clients (id) ON DELETE CASCADE,
    bucket     text NOT NULL,
    key        text NOT NULL,
    file_id    uuid NOT NULL REFERENCES files (id) ON DELETE CASCADE,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (client_id, bucket, key)
);
CREATE INDEX s3_keys_file_idx ON s3_keys (file_id);
