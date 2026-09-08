# ADR 001: 저장소 접근 계약과 파일 위치를 등록부에 기록한다

- 상태: Accepted
- 최초 결정: 2026-07-03
- 근거: [000](000-identity.md), 단계별 storage 확장은 [007](007-grove-storage-foundation.md)

## 결정

| 항목 | 현재 계약 |
|---|---|
| storage | fs 경로 또는 외부 S3 endpoint·계정·공간·자격증명 |
| S3 backend | S3 호환 API를 공통 adapter로 사용 |
| fs backend | 준비된 로컬·NFS 경로를 사용 |
| 내부·공개 주소 | 서버 I/O와 presigned URL 주소를 각각 등록 |
| 전송 모드 | fs는 중계, S3는 `force_relay` 선언으로 선택 |
| 파일·위치 | `file`과 `location`을 분리 |
| 신규 배치 | client의 `storage_id` 하나로 결정 |
| 물리 이름 | FileGate가 발급한 고유 키 |

등록부가 접근 정보를 소유하면 같은 backend의 계정·주소 추가를 데이터 변경으로
처리할 수 있다. 파일 참조는 저장 위치 변경과 독립적이다.

## 결과

참조 무결성은 FK가 지킨다. 접근 검증은 등록·부팅 시 수행하고, 이후 I/O 실패는
해당 요청·복구 경로에서 처리한다. 자동 배치·이동은 후속 구현 범위다.
