# ADR 002: 접근과 완료 복구를 lease 원장으로 추적한다

- 상태: Accepted
- 최초 결정: 2026-07-03
- 근거: [000](000-identity.md), [005](005-presigned-byte-plane.md), [006](006-s3-compat-surface.md)

## 결정

lease는 단일 목적의 접근·만료·진행 기록이다. 네이티브 API는 lease URL을 발급하고,
S3 API는 내부적으로 같은 원장을 사용한다.

| 단계 | 책임 |
|---|---|
| 쓰기 시작 | pending 파일·위치·write lease 기록 |
| 전송 | 저장소 직결 또는 FileGate 중계 |
| 확정 | 실측·선언 검증 뒤 active 전이 |
| 읽기 | 현재 location 해석·접근 기록 |
| 만료·중단 | reconciler의 정리·완료 복구 |

| 성질 | 저장소 직결 | FileGate 중계 |
|---|---|---|
| 검증 주체 | 저장소 서명 검증 | FileGate 원장·토큰 검증 |
| 발급 후 URL 취소 | TTL 만료로 종료 | 원장 상태로 제어 |
| 사용 관찰 | 발급 기록 | 스트림 계측 |
| 크기 검증 | 확정 시 실물 대조 | 전송 중 계측·상한 검사 |

## 결과

완료 전 바이트는 pending 상태이며, 실패한 작업의 재료는 복구가 소유한다.
DB 완료 소유권은 새 part와 generic 회수를 직렬화한다. 직결 UploadPart의
외부 직렬화는 검증된 part 목록을 사용하는 vendor Complete가 담당한다.

TTL·재발급·복구 전이는 [파일](../spec/00-operations.md),
[네이티브 multipart](../spec/02-multipart.md), [S3](../spec/03-s3-surface.md)에 둔다.
