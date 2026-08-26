-- S3 요청 경로의 pending 세션 (spec 03).
--
-- s3_keys는 성공적으로 확정된 논리 이름공간만 나타낸다. 진행 중 업로드를
-- 거기에 먼저 넣으면 overwrite 실패가 기존 객체를 가리므로, 별도 세션이
-- create의 논리키와 모드를 file_id에 묶는다. 외부 저장소와 DB는 한
-- 트랜잭션으로 묶을 수 없으므로 state가 먼저 Complete/Abort의 승자를 정하고,
-- expected_*와 기존 location/lease가 실패 뒤 reconciler 재시도 재료를 남긴다.
-- file 삭제에는 CASCADE라 종착 행 정리 뒤 매달린 세션이 남지 않는다.
-- 기존 pending S3 multipart는 원 logical key를 저장하지 않았으므로 backfill할
-- 수 없다. 배포는 구버전 writer를 먼저 중단하고 진행 세션을 drain/만료한다
-- (spec 03 전환 조건).
CREATE TABLE s3_uploads (
    file_id       uuid PRIMARY KEY REFERENCES files (id) ON DELETE CASCADE,
    key           text NOT NULL,
    multipart     boolean NOT NULL,
    state         text NOT NULL DEFAULT 'open'
                  CHECK (state IN ('open', 'completing', 'aborting')),
    expected_size bigint CHECK (expected_size >= 0),
    expected_etag text,
    created_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT s3_upload_completion_fields CHECK (
        (state = 'completing' AND expected_size IS NOT NULL AND expected_etag IS NOT NULL)
        OR
        (state <> 'completing' AND expected_size IS NULL AND expected_etag IS NULL)
    )
);

CREATE INDEX s3_uploads_state_idx ON s3_uploads (state, updated_at);
