# filegate

정책 기반 파일 게이트웨이. 네이티브 표면은 `file_id`, S3 호환 표면은 서비스가 정한 논리키로 파일을 참조하고, 물리(벤더·버킷·수명)는 filegate가 소유한다.

- 방향·원칙: [docs/adr/](docs/adr/README.md)
- 오퍼레이션 계약: [docs/spec/](docs/spec/00-operations.md)
- 서비스 연동: [docs/guide/](docs/guide/service-integration.md)
- 기술 선택: [docs/stack/](docs/stack/README.md) · 벤더 사실: [docs/vendors/](docs/vendors/README.md)

## 개발 환경

설정은 전부 **환경 변수**다 (로컬 `.env`, 배포는 Terraform이 만든 k8s Secret): 서버 설정 + 마스터 키 + 운영자 토큰. 등록부(storages·clients·credentials)는 DB에 살고 운영자 API로 관리하며, storage와 S3 자격증명 시크릿은 암호화되어 등록부에 보관된다 ([spec 01](docs/spec/01-registry.md)).

```sh
docker compose up -d          # MinIO(9000/9001) + PostgreSQL(55432) + 버킷 프로비저닝
cp .env.example .env          # 로컬 자격증명
cargo run --bin filegate      # http://127.0.0.1:8080
```

컨테이너도 설정 파일 없이 env로 설정한다. Docker Desktop에서는
`.env.example`의 호스트용 DB 주소를 `host.docker.internal`로 덮어쓴다:

```sh
docker run --rm -p 8080:8080 --env-file .env \
  -e FILEGATE_BIND=0.0.0.0:8080 \
  -e FILEGATE_DATABASE_URL=postgres://filegate:filegate@host.docker.internal:55432/filegate \
  filegate:dev
```

이 구성에서 storage를 등록할 때의 내부 endpoint도 컨테이너에서 도달 가능한
주소여야 한다. `deploy/local/main.tf`의 `127.0.0.1` endpoint는 filegate를
호스트에서 실행하는 위 개발 절차를 기준으로 한다. Linux Docker Engine에서 현재
Compose의 loopback 공개 주소를 그대로 쓰려면 host network로 실행한다:

```sh
docker run --rm --network host --env-file .env filegate:dev
```

같은 Compose 네트워크에서 실행한다면 DB와 storage의 내부 endpoint에는 각각
`postgres:5432`, `minio:9000`처럼 서비스 이름과 컨테이너 포트를 사용한다.

확인:

```sh
curl http://127.0.0.1:8080/          # {"name":"filegate","version":...}
curl http://127.0.0.1:8080/healthz    # {"status":"ok"}   — liveness (무의존)
curl http://127.0.0.1:8080/readyz     # {"status":"ready"} — readiness (DB 체크)
```

검사와 이미지 빌드:

```sh
cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
docker build -f deploy/docker/Dockerfile -t filegate:dev .
```

`db` 통합 테스트(`#[sqlx::test]`)는 `DATABASE_URL`이 있어야 돈다 — `docker compose up` 후
`export DATABASE_URL=postgres://filegate:filegate@127.0.0.1:55432/filegate`. 없으면 그 테스트는
실패한다 (CI는 PG 서비스로 자동 공급). `migrations.rs`만 예외로 없으면 조용히 스킵한다.

릴리스는 `VERSION` 파일을 올려 main에 머지하면 GitHub Actions가 ghcr 이미지와 태그를 발행한다.
