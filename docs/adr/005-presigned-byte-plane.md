# ADR 005: 네이티브 전송은 서명 URL을 발급한다

- 상태: Accepted (네이티브 표면)
- 최초 결정: 2026-07-13
- 근거: [002](002-lease-model.md); S3 표면은 [006](006-s3-compat-surface.md)

## 결정

| 요청 | 발급 결과 | 전송 |
|---|---|---|
| create | PUT URL | 업로드 |
| read | GET URL | 다운로드 |
| parts | part별 PUT URL | multipart 전송 |
| storage가 S3 직결 | vendor presigned URL | 클라이언트 ↔ 저장소 |
| fs·force_relay | FileGate lease URL | 클라이언트 ↔ FileGate ↔ 저장소 |

네이티브 클라이언트는 발급된 URL로 전송하고 확정 결과를 확인한다.
URL 발급과 바이트 I/O를 분리해 직결 storage에서 FileGate의 전송 부하를 줄인다.

## 선택 근거

2026-07-13 aws-cli/botocore 스파이크에서 GetObject의 307 응답으로 다운로드를
오프로드하는 경로가 동작하지 않았다. 해당 실측에 따라 네이티브 URL 발급과
S3 응답 본문 전송을 별도 계약으로 채택했다.

## 결과

네이티브 계약은 [파일 spec](../spec/00-operations.md)과
[multipart spec](../spec/02-multipart.md)이 정의한다.
S3 SDK는 [S3 spec](../spec/03-s3-surface.md)의 바이트 응답을 사용한다.
