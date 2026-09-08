# ADR 004: 등록부의 정본을 PostgreSQL에 둔다

- 상태: Accepted
- 최초 결정: 2026-07-03
- 근거: [000](000-identity.md); 중앙 관리 방향은 [007](007-grove-storage-foundation.md)

## 결정

| 정보 | 정본·변경 경계 |
|---|---|
| storage·client·자격증명 | PostgreSQL·운영자 API |
| 프로세스 설정·마스터 키·운영자 토큰 | env |
| client 배치 | 생성 시 `storage_id`로 지정 |
| client bearer 키 | sha256 해시 저장 |
| storage·S3 secret | AES-GCM 암호화 저장 |
| 사용량 | 파일·위치 행에서 조회 시 집계 |

운영자 API의 변경을 모든 프로세스가 같은 DB에서 관찰한다.
storage 등록 시 접근 검증을 수행하며, 소유·참조 관계는 FK로 유지한다.

## 결과

재배포 없이 등록을 관리한다. stable id·단건 조회·멱등 삭제를 제공하므로
API 클라이언트가 같은 계약으로 자동화할 수 있다. 현재 Terraform 예제도 이 API를 사용한다.

현재 client 배치는 생성 시 고정한다. storage의 capacity는 사용량 비교 기준선이다.
배치 변경·용량 집행·자동 이동은 각각 추가 계약으로 정한다.

키 회전과 복구는 [등록부 spec](../spec/01-registry.md)을 따른다.
DB 백업과 마스터 키가 등록부·논리키 복구의 기준이다.
