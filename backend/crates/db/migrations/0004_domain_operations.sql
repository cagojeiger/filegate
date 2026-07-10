-- 도메인 오퍼레이션(spec 00 완결)의 스키마 보강. 적용된 마이그레이션은
-- 수정하지 않는다 — sqlx 체크섬이 부팅을 막는다.

-- pending → reclaimed: lease 만료 회수의 종착 상태. 행이 남는 이유는
-- lease 원장(FK)이 파일 행을 참조하기 때문이고, 회수와 늦은 commit의
-- 경합을 조건부 전이 하나로 끊는 장치이기도 하다. 두 정합 CHECK
-- (active/deleted의 시각 요구)는 reclaimed에 공허하게 참이라 그대로다.
ALTER TABLE files DROP CONSTRAINT files_state_check;
ALTER TABLE files ADD CONSTRAINT files_state_check
    CHECK (state IN ('pending', 'active', 'deleted', 'reclaimed'));

-- 회계 0행 백필 — 등록부 도입기(0002)에 만들어진 storage는 usage 행이
-- 없어서 예약(조건부 UPDATE)이 항상 0행 = 507이 된다. 이후 등록은
-- insert_storage가 같은 트랜잭션에서 시드한다.
INSERT INTO storage_usage (storage_id)
SELECT id FROM storages
ON CONFLICT (storage_id) DO NOTHING;
