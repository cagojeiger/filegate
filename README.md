# filegate

PostgreSQL에 파일 메타데이터를 기록하고, fs·외부 S3 저장소의 바이트를
네이티브 API와 S3 호환 API로 제공한다.

1차는 외부 S3 presigned 전송을 기반으로 관리 CLI와 등록부 Terraform 관리 이관을
진행한다. 2차에 사전 마운트된 파일시스템을 사용하는 Storage Server·Agent를 추가한다.
메타데이터는 PostgreSQL이 중앙 관리하며, FileGate 이름은 후속 이름 변경까지 유지한다.

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
기존 [Terraform 예제](deploy/local/main.tf)는 로컬 E2E의 등록 구성을 제공한다.

| 확인 | 결과 |
|---|---|
| `GET /` | 이름·버전 |
| `GET /healthz` | 프로세스 생존 |
| `GET /readyz` | DB 준비 상태 |

`VERSION` 갱신을 main에 머지하면 릴리스 워크플로가 태그와 GHCR 이미지를 발행한다.
실행 환경의 배포는 별도 운영 절차로 수행한다.
