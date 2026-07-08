# 기술 스택

구현 기술 결정을 모은다. ADR과의 역할 구분: ADR은 잘 변하지 않는 방향·구조·원칙을, 이 디렉토리는 **생태계와 함께 바뀌는 구현 선택**(언어, 프레임워크, 크레이트 버전)을 담는다. 크레이트 버전은 형제 프로젝트(`~/project/*gate`)와 맞춰 두고, 갱신 시 함께 올린다.

조사일: 2026-07-08. 기준: notegate·opsgate의 워크스페이스(둘이 거의 동일).

## 결정

- **언어: Rust.** 컨트롤 플레인(발급·확정·회계)과 중계 데이터 플레인(바이트 스트리밍 패스스루)을 한 프로세스에 담는다. 정적 링크 단일 바이너리가 배포 산출물이다 — 런타임·인터프리터 의존 0 (공리 3).
- **메타데이터 저장소: PostgreSQL (sqlx).** 회계의 예약·정산·해제를 단일 트랜잭션으로 원자화한다(ADR 004). lease 원장이 곧 감사 기록이고(ADR 002), reconciler도 같은 DB를 본다 — 별도 큐 없음. 바이트는 DB에 넣지 않는다.
- **저장소 접근: aws-sdk-s3.** provider adapter(ADR 001)의 1차 계약이 S3 호환이고, presigned URL 발급이 직결 모드의 핵심 요구다. 같은 SDK로 MinIO·R2·OCI를 endpoint 교체만으로 다룬다. `object_store`(fs+s3 단일 trait)는 fs adapter와의 통합 관점에서 검토 후보.
- **fs adapter·중계 스트리밍: tokio::fs + axum body.** presigned 개념이 없는 로컬/NFS는 항상 중계이며, 선언 크기에서 스트림을 끊는 요구(ADR 002)를 상수 메모리로 처리한다.

## 크레이트 (형제 프로젝트 기준)

| 역할 | 크레이트 | 비고 |
|---|---|---|
| async 런타임 | `tokio` (full), `tokio-util` | |
| HTTP | `axum` 0.8, `tower`, `tower-http` | cors·limit·timeout·request-id·trace |
| DB | `sqlx` 0.8 | `runtime-tokio, tls-rustls, postgres, macros, migrate, uuid, chrono, json` |
| 저장소 | `aws-sdk-s3`, `aws-config` | **filegate 신규** — 형제엔 없음 |
| 설정 | `config` 0.15 (toml), `dotenvy` | |
| 에러 | `thiserror` 2, `anyhow` 1 | |
| 관측 | `tracing`, `tracing-subscriber` (env-filter, json) | |
| 직렬화·타입 | `serde`, `serde_json`, `uuid` v4, `chrono`/`time`, `validator`, `schemars` | |
| 비밀·암호 | `secrecy`, `zeroize`, `subtle`, `sha2`, `hmac`, `aes-gcm`, `base64`, `rand` | 중계 lease secret·서명에 쓴다 |
| HTTP 클라이언트 | `reqwest` 0.12 (rustls-tls) | |

MCP 표면이 필요해지면 형제와 같은 `rmcp`를 쓴다.

## 워크스페이스 레이아웃

형제 표준을 따른다: `backend/crates/{core, model, db, service, api}`.

- **core** 도메인 로직·불변식, **model** 타입, **db** sqlx 접근, **service** 오케스트레이션, **api** axum 핸들러.
- provider adapter(s3, fs)는 저장소 경계이므로 `infra` 크레이트 또는 `service` 하위에 둔다 — 구현 시 결정.

## 멀티 파드와 단일 워커 (notegate 검증 패턴)

여러 파드로 떠도 reconciler는 DB당 하나만 돌아야 한다. notegate의 purge worker 패턴을 그대로 쓴다 (`purge_worker.rs` + `purge_repo.rs`).

- **API 경로는 무상태 수평 확장.** 회계 원자성과 상태 전이 경합은 PG 트랜잭션·조건부 갱신이 담당하므로 파드 수와 무관하다 (ADR 004).
- **워커는 모든 파드가 spawn하고, 실행은 락이 고른다.** 매 tick마다 트랜잭션을 열고 `pg_try_advisory_xact_lock(고정 i64 키)`을 시도한다. 못 잡으면 다른 파드가 돌고 있다는 뜻 — 조용히 skip. 리더 선출·전용 워커 배포가 따로 없다.
- **잠금은 자가 회복이다.** xact 락은 트랜잭션 종료(커밋·롤백·커넥션 사망) 시 자동 해제라, 워커 파드가 죽어도 갱신·정리 절차 없이 다음 tick에 다른 파드가 이어받는다.
- **루프 형태**: `tokio::time::interval` + `MissedTickBehavior::Delay` + `CancellationToken` graceful shutdown.
- **배치는 유계**: CTE + `LIMIT`으로 한 run에 조금씩. run 결과는 tracing 구조화 로그로.
- **부팅 배선** (notegate main.rs 순서): config 로드 → PG 연결·마이그레이션 → 상태 구성 → HTTP listen + worker spawn → `tokio::select`로 종료 신호 대기 → HTTP부터 순차 shutdown.

filegate의 reconciler 잡: pending 만료 회수(capacity 해제), deleted purge(물리 삭제 + 해제), 이후 tiering. 주의: fs/NFS provider를 멀티 파드로 쓰려면 모든 파드가 같은 마운트를 공유해야 한다 — 중계 요청이 어느 파드로 와도 같은 파일에 닿아야 하고, 임시 경로 + rename 원자성은 같은 마운트 안에서만 성립한다.

## 비밀 저장

**모든 설정은 배포 config다 — DB에 비밀을 저장하지 않는다** (ADR 004: 세 계층 모두 배포 설정 기준, admin 표면 없음).

- provider 자격증명(벤더 access/secret 키)은 config 파일·환경 변수에 산다. DB에 안 들어가므로 at-rest 암호화 계층이 필요 없다.
- 클라이언트 등록도 config다 — llmgate처럼 운영자가 외부에서 만들어 등록한다. filegate 안에 등록 테이블·등록 API가 없다.
- 그래서 KeyPolicy(HKDF 파생·HMAC 해시·AES-GCM) 같은 crypto-at-rest 계층은 두지 않는다. DB에 저장되는 비밀이 하나도 없어 보호할 대상이 없기 때문이다.
- 런타임에 메모리로 들고 있는 비밀은 `secrecy::SecretString`으로 감싸 Debug 유출을 막는다.
- 클라이언트 키 검증은 요청이 제시한 키를 config 값과 상수 시간 비교한다 (인증 미들웨어와 함께 구현). 저장된 해시가 아니라 config 원본과 대조하므로 회전 = config 갱신 + 재배포다.

구현 단계에서 형제(notegate/opsgate)에서 가져올 것: `core/error.rs`(thiserror 에러 체계), `validator` 기반 config 검증, 필요 시 `moka` 캐시.

## 빌드 규율 (형제 공통)

- clippy `warn`: `unwrap_used`, `expect_used`, `panic`, `todo`, `unreachable`, `indexing_slicing`, `unwrap_in_result`, `await_holding_lock`.
- release: `lto = true`, `codegen-units = 1`, `strip = true`, `debug = 1`.
- 로컬 개발은 docker-compose(MinIO + PostgreSQL).
