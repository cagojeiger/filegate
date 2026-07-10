# filegate

정책 기반 파일 게이트웨이. 서비스는 intent와 file_id만 알고, 물리(벤더·버킷·수명)는 filegate가 소유한다.

- 방향·원칙: [docs/adr/](docs/adr/README.md)
- 오퍼레이션 계약: [docs/spec/](docs/spec/00-operations.md)
- 기술 선택: [docs/stack/](docs/stack/README.md) · 벤더 사실: [docs/vendors/](docs/vendors/README.md)

## 개발 환경

설정은 전부 **환경 변수**다 (로컬 `.env`, 배포는 Terraform이 만든 k8s Secret): 서버 설정 + 마스터 키 + 운영자 토큰. 등록부(storages·clients·bindings)는 DB에 살고 운영자 API로 관리하며, storage 시크릿은 암호화되어 등록부에 보관된다 ([spec 01](docs/spec/01-registry.md)).

```sh
docker compose up -d          # MinIO(9000/9001) + PostgreSQL(55432) + 버킷 프로비저닝
cp .env.example .env          # 로컬 자격증명
cargo run --bin filegate      # http://127.0.0.1:8080
```

컨테이너로 띄울 때는 env만 준다:

```sh
docker run --env-file .env filegate:dev
```

확인:

```sh
curl http://127.0.0.1:8080/          # {"name":"filegate","version":...}
curl http://127.0.0.1:8080/health    # {"status":"ok"}   — liveness (무의존)
curl http://127.0.0.1:8080/ready     # {"status":"ready"} — readiness (DB 체크)
curl http://127.0.0.1:8080/metrics   # Prometheus 스크레이프
```

검사와 이미지 빌드:

```sh
cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
docker build -f deploy/docker/Dockerfile -t filegate:dev .
```

릴리스는 `VERSION` 파일을 올려 main에 머지하면 GitHub Actions가 ghcr 이미지와 태그를 발행한다.
