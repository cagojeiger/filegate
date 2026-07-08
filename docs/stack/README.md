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

## 빌드 규율 (형제 공통)

- clippy `warn`: `unwrap_used`, `expect_used`, `panic`, `todo`, `unreachable`, `indexing_slicing`, `unwrap_in_result`, `await_holding_lock`.
- release: `lto = true`, `codegen-units = 1`, `strip = true`, `debug = 1`.
- 로컬 개발은 docker-compose(MinIO + PostgreSQL).
