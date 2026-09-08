# ADR 000: 업무 의미와 파일 물리 관리를 분리한다

- 상태: Accepted (현재 FileGate의 배경 결정)
- 최초 결정: 2026-07-03
- 새 제품 전제: [ADR 007](007-grove-storage-foundation.md)

## 전제

여러 서비스가 하나의 FileGate를 공유한다. 저장소 접근·인증·파일 정리를
한곳에 모아 서비스별 구현 중복을 줄인다.

## 결정

| 경계 | 소유자·계약 |
|---|---|
| 업무 의미·사용자 권한 | 서비스 |
| 파일 위치·접근 발급·물리 정리 | FileGate |
| 파일 참조 | 네이티브 `file_id`, S3 `(client, key)` |
| 네이티브 바이트 | storage presigned 직결, fs·force_relay는 FileGate 경유 |
| S3 바이트 | FileGate 경유 |
| 인증 | 자체 client 키·SigV4·운영자 토큰 |
| 기반 공간 | 운영자가 준비하고 FileGate가 접근 검증 |
| 삭제 | 서비스가 detach 요청, reconciler가 purge |

FileGate가 관리하는 데이터 영역은 FileGate가 쓰기를 소유한다.
등록 시 접근을 검증하고 부팅 시 다시 확인한다. 메타데이터와 물리 위치는
[등록부](../spec/01-registry.md)와 [파일 계약](../spec/00-operations.md)으로 관리한다.

## 결과

서비스의 안정 참조는 물리 위치와 분리된다. 직결로 발급된 URL은 FileGate 장애
중에도 저장소와 TTL 조건에 따라 동작하며, 중계 전송은 FileGate 가용성을 따른다.
