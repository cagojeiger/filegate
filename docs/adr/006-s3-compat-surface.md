# ADR 006: S3 요청을 공유 파일 원장으로 처리한다

- 상태: Accepted
- 최초 결정: 2026-07-14
- 근거: [002](002-lease-model.md), [003](003-url-ownership.md), [005](005-presigned-byte-plane.md)

## 결정

기존 S3 SDK가 endpoint·자격증명·bucket을 지정해 파일을 저장하도록
S3 호환 API를 제공한다. 업로드와 다운로드의 바이트는 FileGate를 지난다.

| 경계 | 계약 |
|---|---|
| 인증 | access key + secret, SigV4 header·query 서명 |
| 이름 | `bucket = client_id`, 서비스 소유 logical key |
| 원장 | 네이티브와 files·locations·leases 공유 |
| PUT | 스트림 계측 뒤 파일·논리키 확정 |
| multipart | Complete가 part 원장을 검증하고 확정 |
| 배치 | client가 참조하는 storage |
| 삭제 | 논리키 제거·detach 후 reconciler purge |

## 결과

S3 SDK의 바이트 응답 계약과 네이티브 URL 발급 계약이 같은 메타데이터를 사용한다.
1차는 외부 S3 presigned 전송 지원과 이 중계 계약을 유지한다.
[단계별 방향](007-grove-storage-foundation.md)의 독자 스토리지는 2차 확장이며,
기존 소비자의 전환과 중계 API 변경은 별도 결정한다.

현재 지원 오퍼레이션·실패·복구는 [S3 spec](../spec/03-s3-surface.md)이 정본이다.
