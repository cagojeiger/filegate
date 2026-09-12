# 기술·운영

## 실행 구조

| 역할 | 구현 |
|---|---|
| 서버 프로세스 | Rust filegate 바이너리, axum, tokio |
| 원격 관리 CLI | 독립 gscli 바이너리, clap·reqwest, 관리자 HTTP API |
| 메타데이터 | PostgreSQL + sqlx, 부팅 시 마이그레이션 |
| 바이트 | tokio filesystem I/O, aws-sdk-s3 |
| 스트림 | axum body, 임시 스풀, 크기·MD5·SHA256 계측 |
| 비밀 | AES-256-GCM, HKDF, secrecy, 상수시간 비교 |
| 관측 | tracing 구조화 로그 |
| 이미지 | `debian:bookworm-slim`, release 바이너리·CA 인증서, 비root 사용자 |

의존성 버전은 [Cargo.toml](../../Cargo.toml), 소스 책임은
[소스 구조](../development/source-layout.md), 키 계약은 [등록부](../spec/01-registry.md)가 정본이다.

## 설정

| 설정 | 공급·관리 |
|---|---|
| bind·로그·DB URL·pool 크기·multipart·CORS | env, 로컬 예시는 [.env.example](../../.env.example) |
| 마스터 키·key id·이전 키 쌍 | env, [키 회전](../spec/01-registry.md#키와-비밀) |
| 운영자 토큰 | env의 쉼표 목록 |
| storage·client·키·S3 자격증명 | PostgreSQL, 운영자 API |

프로세스는 환경 변수를 읽는다. 배포 도구가 env 또는 Secret을 공급한다.
`deploy/local/main.tf`와 E2E의 Terraform 등록 절차는 현재 유지한다.
Terraform 제거와 대체 등록 절차는 후속 변경에서 함께 적용한다.

## 현재 CLI

| 명령 | 현재 동작 |
|---|---|
| `filegate`, `filegate serve` | 서버 기동·migration·등록 저장소 검증 |
| `filegate status` | 로컬 설정으로 DB·저장소 접근 검사, usage·client 수 출력 |
| `filegate --help` | 명령 도움말 |
| `gscli status` | 원격 HTTP 상태·등록부 요약, 물리 접근은 not_checked |
| `gscli storage/client ...`, `credential/client-key list`, `usage ...` | 관리자 API 읽기 명령 |
| `gscli update [--check]` | 서버 연결 없이 최신 CLI 확인·설치, 공식 설치 기록 검증 |

`filegate status`는 HTTP 서버 없이 동작하고 DB URL·마스터 키를 포함한 서버 설정을 읽는다.
DB migration은 수행하지 않으며, fs 접근 검사는 probe 파일 쓰기·삭제를 포함한다.
검사 성공은 exit 0, storage 실패는 exit 1이다. 테스트는 바이트·용량 표현 2개다.
`gscli`은 DB·마스터 키 없이 `GROVE_ENDPOINT`·운영자 토큰으로 연결한다.
`cargo install --path backend/crates/cli --locked`로 소스에서 설치한다.
변경 명령·로컬 doctor 개편·Terraform 이관은 [CLI 스펙](../spec/04-cli.md)의 후속 작업이다.

## 컨테이너 연결

```sh
docker build -f deploy/docker/Dockerfile -t filegate:dev .
docker run --rm -p 8080:8080 --env-file .env \
  -e FILEGATE_BIND=0.0.0.0:8080 \
  -e FILEGATE_DATABASE_URL=postgres://filegate:filegate@host.docker.internal:55432/filegate \
  filegate:dev
```

위 실행 예시는 Docker Desktop 기준이다. storage의 내부 endpoint도 컨테이너에서 접근 가능한 주소로 등록한다.

| 실행 위치 | DB 주소 | MinIO 내부 endpoint |
|---|---|---|
| 호스트 | `127.0.0.1:55432` | `http://127.0.0.1:9000` |
| Docker Desktop 컨테이너 | `host.docker.internal:55432` | `http://host.docker.internal:9000` |
| 같은 Compose 네트워크 | `postgres:5432` | `http://minio:9000` |
| Linux Docker Engine host network | `127.0.0.1:55432` | `http://127.0.0.1:9000` |

Linux의 기존 Compose loopback 공개 주소는
`docker run --rm --network host --env-file .env filegate:dev`로 접근한다.
fs storage에는 컨테이너에서 보이는 데이터 마운트를 제공한다.

## 워커

```mermaid
flowchart LR
    Boot["설정 / DB 연결 / migration / storage 검증"] --> Run["HTTP + reconciler"]
    Run --> Tick["tick"]
    Tick --> Lock["PostgreSQL advisory transaction lock"]
    Lock --> Jobs["관찰 / 복구 / 정리 / 사용량 스냅샷"]
```

| 조건 | 동작 |
|---|---|
| 여러 API 프로세스 | DB 트랜잭션으로 전이 직렬화 |
| 워커 tick | 락을 얻은 프로세스가 공유 저장소 작업 수행 |
| 연결·프로세스 종료 | 트랜잭션 락 자동 해제 |
| 로컬 임시 스풀 | 각 프로세스가 자기 임시 영역 정리 |
| fs를 여러 프로세스가 사용 | 같은 데이터 마운트 공유, 임시→최종 rename은 같은 filesystem |
| 종료 신호 | HTTP 종료 후 워커 종료 |

후보 스캔은 작업별 배치로 처리하며, 일별 스냅샷과 일부 filesystem 스캔은 전체 집합을 읽는다.
작업 순서는 [reconciler.rs](../../backend/crates/api/src/reconciler.rs)에 있다.

## 검증

```sh
export DATABASE_URL=postgres://filegate:filegate@127.0.0.1:55432/filegate
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

| 검사 | 전제·범위 |
|---|---|
| API·core·infra 단위 테스트 | DB 연결 없는 테스트 |
| `db/tests` | PostgreSQL과 `DATABASE_URL`; sqlx가 테스트 DB 생성 |
| `migrations.rs` | URL이 있으면 migration 검증, 없으면 조기 반환 |
| `scripts/e2e-*.sh` | 로컬 전용 DB·MinIO·서버·스크립트별 등록 전제 |
| `scripts/e2e-registry.sh` | 시작·종료 시 등록부 초기화, 전용 개발 DB에서 실행 |
| `scripts/s3-capture.py` | S3 endpoint·자격증명·bucket으로 실제 객체·multipart 검증 |

## 로그

| 레벨 | 대상 |
|---|---|
| info | 부팅·종료·실제 요청 |
| debug | 반복 tick·락 획득 실패 |
| warn | 재시도 가능한 이상 |
| error | 요청·워커·준비 상태 실패 |

성공한 health/readiness 요청은 로그에서 제외하고 실패를 기록한다.
