-- 중계 multipart write secret은 마스터 키 + lease id에서 결정적으로
-- 파생한다 (core::Crypto::relay_secret) — parts() 발급이 매번 같은 값을
-- 재파생하므로 원문을 저장할 이유가 없다. 인증은 secret_hash 대조
-- 그대로다. 파생 불가능한 값만 저장한다는 spec 02의 원칙이 secret에도
-- 성립한다. 진행 중 업로드의 원문도 함께 사라진다 — 배포 전환기의
-- multipart relay 업로드는 재시작이 계약이다.
ALTER TABLE leases DROP COLUMN write_secret;
