# 소스 구조

```text
backend/crates/
├── cli/                   gscli: 원격 관리자 API 읽기 명령
│   ├── src/               인자·설정·HTTP·응답 모델·출력
│   │   └── update/        업데이트 흐름·다운로드·설치 기록·파일 교체, 분리된 tests/
│   └── tests/             설정·조회·오류·status HTTP·update 진입점 테스트
├── api/src/
│   ├── admin/              등록부·운영자 인증·usage
│   ├── s3/                 SigV4·라우팅·객체·multipart
│   │   ├── object_response.rs Range·응답 헤더 정책
│   │   └── object_response/ Range·응답 헤더 테스트
│   ├── v1/                 네이티브 파일·multipart·relay
│   ├── blobs.rs            lease URL 바이트 전송
│   ├── spool.rs            스트림 계측·임시 파일
│   ├── storage_access.rs   등록부에서 backend 구성·물리 작업
│   ├── status.rs           현재 로컬 DB·저장소 진단 CLI
│   └── reconciler/         완료 복구
├── db/
│   ├── src/files/          파일·lease 상태 전이
│   ├── src/s3_registry/    자격증명·논리키·업로드 세션
│   ├── migrations/         PostgreSQL 스키마
│   └── tests/              DB 통합 테스트
├── infra/src/              fs·외부 S3 I/O
└── core/src/               설정·암호·해시·multipart 계산
```

## 책임

| 모듈 | 입력 → 결과 | 정합성 경계 |
|---|---|---|
| `api/routes`, `api/admin` | HTTP → 인증된 요청 | 표면별 인증·예약 경로 |
| `api/s3/auth` | 원본 URI·헤더 → client | SigV4 검증 |
| `api/s3/object_response` | Range·쿼리 → 응답 정책 | 인코딩·헤더 검증 |
| `api/s3/handlers`, `multipart` | 인증된 요청 → 저장·확정 | DB 소유권 → 물리 I/O → DB 확정 |
| `db/files`, `db/s3_registry/uploads` | 전이 요청 → 조건부 결과 | 행 락·트랜잭션 |
| `db/s3_registry/keys` | 논리키 교체 → 옛 파일 detach | 호출자의 확정 트랜잭션에 참여 |
| `infra/fs`, `infra/s3` | 물리 주소 → 바이트 I/O | filesystem·vendor 계약 |
| `api/reconciler` | DB 후보·실물 관찰 → 복구 | 보존된 소유권·재시도 |
| `api/status` | 로컬 Config → DB·저장소 접근·요약 | HTTP 독립, 부팅과 같은 storage 검사 |
| `cli` | 운영자 인자 → 관리자 HTTP API → table·JSON | DB 의존성 없음, 기존 서버·로컬 status와 분리 |
| `cli/update` | 공식 Release → 검증된 실행 파일 | 서버 인증 독립, 설치·업데이트의 동일 잠금·교체 |
| `core` | 값 → 검증·계산 | 프로토콜·DB에서 독립된 계산 |

`uploads`의 크기보다 상태 전이의 원자성을 우선한다. 논리키 교체와 옛 파일
detach는 같은 트랜잭션을 공유한다.

## 검증 위치

| 범위 | 테스트 |
|---|---|
| 조합 라우팅·인증·CORS | `api/src/routes/tests.rs` |
| S3 서명·쿼리 | `api/src/s3/auth.rs`, `s3/mod.rs` |
| Range·응답 헤더 | `api/src/s3/object_response/tests.rs` |
| 파일 상태·동시성·GC | `db/tests/file_*`, `native_multipart_completion.rs` |
| S3 원자적 교체·완료·회수 | `db/tests/s3_*` |
| filesystem 조립·임시 보호 | `infra/src/fs.rs` |
| 현재 CLI 표현 | `api/src/status.rs` (바이트·용량 2개) |
| 원격 CLI | `cli/tests/{config,reads,failures,status}.rs`, `cli/src/output_tests.rs` |
| CLI·서버 응답 계약 | `scripts/e2e-cli.py` (CI, 격리 DB·실제 서버) |
| CLI 설치·릴리스 계약 | `deploy/tests/test_{installer,manifest,version}.py` |
| 실제 바이트 경로 | `scripts/e2e-*.sh`, `scripts/s3-capture.py` |

실행 명령은 [기술·운영](../stack/README.md#검증)을 따른다.
