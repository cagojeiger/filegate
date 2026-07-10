# 기술 스택

구현 기술 결정을 모은다. ADR과의 역할 구분: ADR은 잘 변하지 않는 방향·구조·원칙을, 이 디렉토리는 **생태계와 함께 바뀌는 구현 선택**(언어, 프레임워크, 크레이트 버전)을 담는다. 크레이트 버전은 형제 프로젝트(`~/project/*gate`)와 맞춰 두고, 갱신 시 함께 올린다.

조사일: 2026-07-08. 기준: notegate·opsgate의 워크스페이스(둘이 거의 동일).

## 결정

- **언어: Rust.** 컨트롤 플레인(발급·확정·회계)과 중계 데이터 플레인(바이트 스트리밍 패스스루)을 한 프로세스에 담는다. 정적 링크 단일 바이너리가 배포 산출물이다 — 런타임·인터프리터 의존 0 (공리 3).
- **메타데이터 저장소: PostgreSQL (sqlx).** 회계의 예약·정산·해제를 단일 트랜잭션으로 원자화한다(ADR 004). lease 원장이 곧 감사 기록이고(ADR 002), reconciler도 같은 DB를 본다 — 별도 큐 없음. 바이트는 DB에 넣지 않는다.
- **저장소 접근: aws-sdk-s3.** storage adapter(ADR 001)의 1차 계약이 S3 호환이고, presigned URL 발급이 직결 모드의 핵심 요구다. 같은 SDK로 MinIO·R2·OCI를 endpoint 교체만으로 다룬다. `object_store`(fs+s3 단일 trait)는 fs adapter와의 통합 관점에서 검토 후보.
- **fs adapter·중계 스트리밍: tokio::fs + axum body.** presigned 개념이 없는 로컬/NFS는 항상 중계이며, 선언 크기에서 스트림을 끊는 요구(ADR 002)를 상수 메모리로 처리한다.

## 크레이트 (형제 프로젝트 기준)

| 역할 | 크레이트 | 비고 |
|---|---|---|
| async 런타임 | `tokio` (full), `tokio-util` | |
| HTTP | `axum` 0.8, `tower`, `tower-http` | cors·limit·timeout·request-id·trace |
| DB | `sqlx` 0.8 | `runtime-tokio, tls-rustls, postgres, macros, migrate, uuid, chrono, json` |
| 저장소 | `aws-sdk-s3`, `aws-config` | **filegate 신규** — 형제엔 없음 |
| 설정 | env + `dotenvy` | YAML 설정 파일 없음 — 등록부는 DB (ADR 004) |
| 에러 | `thiserror` 2, `anyhow` 1 | |
| 관측 | `tracing`, `tracing-subscriber` (env-filter, json), `metrics` + `metrics-exporter-prometheus` | `/metrics` 스크레이프. 프로브·스크레이프는 메트릭·로그에서 제외 |
| 직렬화·타입 | `serde`, `serde_json`, `uuid` v4, `chrono`/`time`, `validator`, `schemars` | |
| 비밀·암호 | `secrecy`, `aes-gcm`, `hkdf`, `sha2`, `subtle`, `rand` | storage 시크릿 암호화(opsgate 참조)·운영자 토큰 비교·클라이언트 키 해시 |
| HTTP 클라이언트 | `reqwest` 0.12 (rustls-tls) | |

## 워크스페이스 레이아웃

형제 표준을 따른다: `backend/crates/{core, model, db, service, api}`.

- **core** 도메인 로직·불변식, **model** 타입, **db** sqlx 접근, **service** 오케스트레이션, **api** axum 핸들러.
- storage adapter(s3, fs)는 저장소 경계이므로 `infra` 크레이트 또는 `service` 하위에 둔다 — 구현 시 결정.

## 멀티 파드와 단일 워커 (notegate 검증 패턴)

여러 파드로 떠도 reconciler는 DB당 하나만 돌아야 한다. notegate의 purge worker 패턴을 그대로 쓴다 (`purge_worker.rs` + `purge_repo.rs`).

- **API 경로는 무상태 수평 확장.** 회계 원자성과 상태 전이 경합은 PG 트랜잭션·조건부 갱신이 담당하므로 파드 수와 무관하다 (ADR 004).
- **워커는 모든 파드가 spawn하고, 실행은 락이 고른다.** 매 tick마다 트랜잭션을 열고 `pg_try_advisory_xact_lock(고정 i64 키)`을 시도한다. 못 잡으면 다른 파드가 돌고 있다는 뜻 — 조용히 skip. 리더 선출·전용 워커 배포가 따로 없다.
- **잠금은 자가 회복이다.** xact 락은 트랜잭션 종료(커밋·롤백·커넥션 사망) 시 자동 해제라, 워커 파드가 죽어도 갱신·정리 절차 없이 다음 tick에 다른 파드가 이어받는다.
- **루프 형태**: `tokio::time::interval` + `MissedTickBehavior::Delay` + `CancellationToken` graceful shutdown.
- **배치는 유계**: CTE + `LIMIT`으로 한 run에 조금씩. run 결과는 tracing 구조화 로그로.
- **부팅 배선** (notegate main.rs 순서): config 로드 → PG 연결·마이그레이션 → 상태 구성 → HTTP listen + worker spawn → `tokio::select`로 종료 신호 대기 → HTTP부터 순차 shutdown.

filegate의 reconciler 잡: pending 만료 회수(capacity 해제), deleted purge(물리 삭제 + 해제), 이후 tiering. 주의: fs/NFS storage를 멀티 파드로 쓰려면 모든 파드가 같은 마운트를 공유해야 한다 — 중계 요청이 어느 파드로 와도 같은 파일에 닿아야 하고, 임시 경로 + rename 원자성은 같은 마운트 안에서만 성립한다.

## 비밀과 설정

**비밀의 저장 방식은 성격이 정한다** (ADR 004, spec 01 "키와 비밀").

- 서버(프로세스) 설정은 전부 env다: bind, 로그 포맷, DB URL, 커넥션 수. YAML 설정 파일은 두지 않는다.
- env의 비밀은 셋뿐이다: 마스터 키(`FILEGATE_ENC_ROOT_SECRET`), 운영자 토큰(`FILEGATE_OPERATOR_TOKENS`, 쉼표 목록 — 메인/서브 로테이션), DB URL. Terraform이 k8s Secret으로 공급한다.
- 클라이언트 키(검증 전용)는 sha256 해시로만 DB에 저장한다. 인증 = 제시된 키를 해시해 조회. 회전 = 해시 행 추가·삭제.
- storage 시크릿(런타임 사용)은 AES-256-GCM으로 암호화해 DB에 저장한다 — AAD에 storage id 바인딩, 마스터 키는 env. opsgate의 credential 보관 방식을 참조한다.
- 메모리의 비밀은 `secrecy::SecretString`으로 Debug 유출을 막는다. 토큰 비교는 상수 시간(`subtle`).

구현 단계에서 형제(notegate/opsgate)에서 가져올 것: `core/error.rs`(thiserror 에러 체계), `validator` 기반 config 검증, 필요 시 `moka` 캐시.

## 로그 레벨 정책

기본 필터는 `info`. 평시 로그는 라이프사이클 이벤트만 보이고, 주기적 시스템 틱은 debug로 내려 노이즈를 없앤다.

| 레벨 | 대상 | 예 |
|---|---|---|
| **info** | 부팅·종료 마일스톤 (1회성, 운영자에게 의미) | `db.connected`, `storage.connected`, `server.listening`, `reconciler.started`/`stopped`, `server.shutting_down`, `shutdown.complete` |
| **info** | 실제 클라이언트 요청 (프로브 제외) | `request.end` |
| **debug** | 주기적 시스템 틱 (반복, 노이즈) | `reconciler.job`, `reconciler.skipped` |
| **warn** | 이상 징후 (치명적 아님) | `reconciler.join_failed` |
| **error** | 실패 | `ready.failed`, `reconciler.failed` |

프로브·스크레이프(/health, /ready, /metrics)의 성공 요청은 로그·메트릭 양쪽에서 제외한다. 실패한 프로브는 남긴다.

## 빌드 규율 (형제 공통)

- clippy `warn`: `unwrap_used`, `expect_used`, `panic`, `todo`, `unreachable`, `indexing_slicing`, `unwrap_in_result`, `await_holding_lock`.
- release: `lto = true`, `codegen-units = 1`, `strip = true`, `debug = 1`.
- 로컬 개발은 docker-compose(MinIO + PostgreSQL).
