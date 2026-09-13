# filegate

PostgreSQL에 파일 메타데이터를 기록하고, fs·외부 S3 저장소의 바이트를
네이티브 API와 S3 호환 API로 제공한다.

## 개선 계획

관리 CLI `gscli`가 등록부 조회·변경과 수동 업데이트를 제공한다.
운영 Terraform state 이관과 독립 Agent는 후속 작업이다.
아래 단계는 로드맵이며 현재 제공 기능은 각 spec의 상태 표시를 따른다.

1차는 외부 S3 presigned 전송을 기반으로 관리 CLI와 등록부 Terraform 관리 이관을
진행한다. 2차에 사전 마운트된 파일시스템을 사용하는 Storage Server·Agent를 추가한다.
메타데이터는 PostgreSQL이 중앙 관리하며, 기존 서버의 FileGate 이름과 계약은 유지한다.

| 문서 | 내용 |
|---|---|
| [문서 목차](docs/README.md) | 현재 구현과 제품 방향 |
| [단계별 제품 결정](docs/adr/007-grove-storage-foundation.md) | 전제·책임·전환 경계 |
| [코드 구조](docs/development/source-layout.md) | 모듈 책임과 테스트 위치 |
| [실행·운영](docs/stack/README.md) | 설정·컨테이너·검증 |
| [S3 연동](docs/guide/s3-onboarding.md) | endpoint·키·버킷·SDK |
| [네이티브 연동](docs/guide/service-integration.md) | 발급·전송·확정 |

## 로컬 실행

```sh
docker compose up -d
cp .env.example .env
cargo run --bin filegate
```

Compose는 PostgreSQL(`55432`), MinIO(`9000/9001`), 개발 버킷을 준비한다.
[운영자 API](docs/spec/01-registry.md)로 storage·client·자격증명을 등록한다.
기존 [Terraform 예제](deploy/local/main.tf)는 이관 전 비교용 구성을 제공한다.
[CLI 등록 절차](docs/guide/registry-management.md)는 Terraform 없는 등록 흐름을 제공한다.

| 확인 | 결과 |
|---|---|
| `GET /` | 이름·버전 |
| `GET /healthz` | 프로세스 생존 |
| `GET /readyz` | DB 준비 상태 |

버전 갱신을 main에 머지하고 CI가 성공하면 태그·GHCR 이미지·`gscli` 실행 파일을 발행한다.
실행 환경의 배포는 별도 운영 절차로 수행한다.

## 관리 CLI

```sh
cargo install --path backend/crates/cli --locked
gscli --endpoint https://filegate.example.com --token-file /path/to/operator-token status
gscli --endpoint https://filegate.example.com --token-file /path/to/operator-token client list --output json
gscli --endpoint https://filegate.example.com --token-file /path/to/operator-token \
  client create notegate --storage primary
```

`GROVE_ENDPOINT`·`GROVE_OPERATOR_TOKEN`으로 연결 설정을 공급할 수 있다.
CLI는 DB·마스터 키 없이 기존 관리자 API를 호출한다. 기존 `filegate status`는
서버 로컬 진단으로 유지한다. [명령·출력·후속 계약](docs/spec/04-cli.md).

배포 채널은 GitHub Release의 Linux/macOS 실행 파일이다.
`v0.4.0`은 첫 CLI 자산을 발행한다. 기존 `v0.3.10` 릴리스에는 CLI 자산이 없다.
[버전·설치·업데이트](docs/development/releases.md)를 따른다.
공식 설치본은 `gscli update --check`로 새 버전을 확인하고 `gscli update`로 설치한다.
일반 명령 실행 시 자동 업데이트는 수행하지 않는다.
