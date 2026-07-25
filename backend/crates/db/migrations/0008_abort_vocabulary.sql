-- 어휘 교정: 파일 상태 'reclaimed' → 'aborted'.
--
-- reclaim은 업계에서 공간 회수(GC)를 뜻한다. 파일의 상태 이름으로 쓰면 범주가
-- 어긋난다 — 그 상태가 말하는 것은 "확정되지 못하고 버려진 업로드"고, 그건
-- S3의 AbortMultipartUpload와 같은 뜻이다.
--
-- 클라이언트 계약은 바뀌지 않는다. 이 상태는 밖으로 나간 적이 없다 — stat이
-- 404로 번역한다 (spec 00: 확정된 적 없는 파일은 파일이 된 적이 없다).
ALTER TABLE files DROP CONSTRAINT files_state_check;

UPDATE files SET state = 'aborted' WHERE state = 'reclaimed';

ALTER TABLE files ADD CONSTRAINT files_state_check
    CHECK (state IN ('pending', 'active', 'deleted', 'aborted'));
