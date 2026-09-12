# 문서

| 읽을 내용 | 정본 |
|---|---|
| 단계별 제품 방향과 구현 경계 | [ADR 007](adr/007-grove-storage-foundation.md) |
| 결정과 근거 | [ADR 목차](adr/README.md) |
| 현재 네이티브 파일 API | [파일](spec/00-operations.md) · [multipart](spec/02-multipart.md) |
| 현재 S3 API | [S3 계약](spec/03-s3-surface.md) |
| 등록·인증·키 회전 | [등록부](spec/01-registry.md) |
| 로컬 진단 / 원격 관리 CLI | [status](stack/README.md#현재-cli) · [CLI 구현·후속 계약](spec/04-cli.md) |
| 버전 / CLI 설치·배포 | [릴리스 계약](development/releases.md) |
| 서비스 연결 | [네이티브](guide/service-integration.md) · [S3](guide/s3-onboarding.md) |
| 등록부 운영·Terraform 이관 | [CLI 등록 절차](guide/registry-management.md) |
| 코드 책임·테스트 위치 | [소스 구조](development/source-layout.md) |
| 실행·설정·검증 | [기술·운영](stack/README.md) |
| 외부 저장소 조사 기록 | [벤더 노트](vendors/README.md) |

## 현재와 방향

1차·2차 열은 미구현 개선 계획이다. 현재 제공 기능은 현재 구현 열과 각 spec의 상태 표시를 따른다.

| 축 | 현재 구현 | 1차 개선 계획 | 2차 확장 계획 |
|---|---|---|---|
| 메타데이터 | PostgreSQL 정본 | 중앙 관리 유지 | Node·Root·작업 상태 추가 |
| 바이트 | fs + 외부 S3 backend | 외부 S3 presigned 전송 지원 유지 | Agent가 mounted filesystem I/O 수행 |
| 클라이언트 | 네이티브 + S3 중계 API | 현행 계약 유지, 소비자 전환은 별도 결정 | 독자 스토리지의 S3 호환 계약 구체화 |
| 배치 | client에 storage 하나 고정 | 현행 배치 유지 | 조인·배치·이동 모델 설계 |
| 복구 | 업로드 완료·삭제·만료 복구 | 현행 복구 유지 | Node 장애·작업 복구 설계 |
| 관리 | 운영자 API, gscli 조회·변경, Terraform 비교 예제 | 등록부 Terraform 운영 이관 | Storage Server·Agent 조인 |
| 실행 이름 | 서버 `filegate`·`FILEGATE_*`, CLI `gscli`·`GROVE_*` | 기존 서버 계약 유지 | 서버 이름 변경은 별도 릴리스 |

Grove Storage는 후속 제품명이다. 현재 fs adapter는 서버 로컬 구현이며, 2차의 독립
Agent가 아니다. Terraform 운영 이관·Node 조인은 계획이며 현재 계약은 spec을 따른다.

## 문서 규칙

| 표현 | 용도 |
|---|---|
| Mermaid | 책임 관계, 요청 흐름, 상태 전이 |
| ASCII 트리 | 디렉터리와 물리 파일 배치 |
| Markdown 표 | 입력·출력·실패, 책임, 지원 상태 |
| 짧은 문장 | 결정 이유, 원자성·보존 조건 |

ADR은 결정, spec은 계약, guide는 사용 절차, 코드는 구현 이유를 소유한다.
주석은 락 순서·인코딩·복구 재료처럼 코드만으로 드러나지 않는 조건을 설명한다.
