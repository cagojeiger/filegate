-- storage의 내부 접근 주소와 외부 서명/전송 주소를 분리한다 (ADR 001).
-- 기존 등록은 지금 쓰던 endpoint를 공개 주소로 승격한다.
ALTER TABLE storages
    ADD COLUMN public_endpoint text;

UPDATE storages
SET public_endpoint = endpoint
WHERE public_endpoint IS NULL;

ALTER TABLE storages
    ALTER COLUMN public_endpoint SET NOT NULL;
