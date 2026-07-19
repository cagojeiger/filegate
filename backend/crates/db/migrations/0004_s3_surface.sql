-- S3 호환 표면의 등록부 (spec 03, ADR 006).
--
-- s3_credentials: access key id → client + 암호화 secret. SigV4는 서버가
-- raw secret으로 HMAC을 재계산해야 하므로 해시 보관이 불가능하다. 이 secret은
-- 만료 없는 장수 신원이라, 마스터 키 + access key id에서 파생하면(찰나인
-- relay식) 루트가 전 자격증명의 스켈레톤 키가 되고 개별 회전이 막힌다.
-- 그래서 storage 벤더 시크릿과 같은 기계로 암호화 저장한다 (재현 필요 +
-- 장수 → 암호화 저장): AAD=access_key_id로 재배치를 막고, enc_key_id 라벨로
-- 복호 키를 고른다 (마스터 키 회전은 spec 01 런북이 storages와 함께 다스림).
CREATE TABLE s3_credentials (
    access_key_id         text PRIMARY KEY CHECK (access_key_id ~ '^[a-z0-9]{8,64}$'),
    client_id             text NOT NULL REFERENCES clients (id) ON DELETE CASCADE,
    secret_key_ciphertext bytea NOT NULL,
    secret_key_nonce      bytea NOT NULL CHECK (octet_length(secret_key_nonce) = 12),
    enc_key_id            text NOT NULL,
    created_at            timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX s3_credentials_client_idx ON s3_credentials (client_id);

-- s3_keys: S3 표면의 논리 이름공간 — (client, key) → file. 버킷은
-- client_id와 같으므로(0.3.0: client == bucket) 별도 컬럼이 없다.
-- 서비스가 정한 이름(논리키)은 서비스 소유다 (ADR 003). 물리 배치와
-- 무관하다 (물리는 locations 소유 — tiering이 위치를 옮겨도 매핑 불변).
-- overwrite(같은 키 재PUT)는 매핑을 새 file로 갈아끼우고 옛 file은 delete
-- 결정으로 넘긴다 — S3의 덮어쓰기 시맨틱을 상태 기계로 번역한 것.
-- file FK는 CASCADE: 종착 행 보존 정리(spec 00)가 file 행을 지울 때
-- 매핑도 함께 사라진다 — 매달린 매핑이 남지 않는다.
CREATE TABLE s3_keys (
    client_id  text NOT NULL REFERENCES clients (id) ON DELETE CASCADE,
    key        text NOT NULL,
    file_id    uuid NOT NULL REFERENCES files (id) ON DELETE CASCADE,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (client_id, key)
);
CREATE INDEX s3_keys_file_idx ON s3_keys (file_id);
