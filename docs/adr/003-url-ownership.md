# ADR 003: 서비스는 안정 이름을 소유한다

- 상태: Accepted
- 최초 결정: 2026-07-03
- 근거: [000](000-identity.md), [002](002-lease-model.md)

## 결정

| 이름·접근 | 소유자 |
|---|---|
| 사용자 URL·업무 권한 | 서비스 |
| 네이티브 파일 참조 | 서비스가 `file_id` 저장 |
| S3 파일 참조 | 서비스가 bucket·logical key 저장 |
| 물리 위치 | FileGate의 location |
| 만료 URL | 요청 시 발급, 유효기간 내 전달 |
| 파일명·표시 방식 | 서비스가 요청에서 지정 |

접근 URL의 수명과 위치가 바뀌어도 서비스의 안정 참조는 유지한다.
업무 권한을 확인한 서비스가 파일 접근을 위임한다.

## 인증 경계

| 표면 | 인증 |
|---|---|
| 네이티브 API | client bearer 키 |
| S3 API | SigV4 |
| 중계 URL | lease secret |
| 운영자 API | operator bearer 토큰 |
| 루트·health·readiness | 공개 상태 조회 |

상세 흐름은 [네이티브 가이드](../guide/service-integration.md)와
[S3 가이드](../guide/s3-onboarding.md)가 정본이다.
