# filegate

정책 기반 파일 게이트웨이. 서비스는 intent와 file_id만 알고, 물리(벤더·버킷·수명)는 filegate가 소유한다.

- 방향·원칙: [docs/adr/](docs/adr/README.md)
- 오퍼레이션 계약: [docs/spec/](docs/spec/00-operations.md)
- 기술 선택: [docs/stack/](docs/stack/README.md) · 벤더 사실: [docs/vendors/](docs/vendors/README.md)

## 개발 환경

```sh
docker compose up -d          # MinIO(9000/9001) + PostgreSQL(55432)
cp .env.example .env          # 필요 시 값 조정
cargo run --bin filegate      # http://127.0.0.1:8080
```

확인:

```sh
curl http://127.0.0.1:8080/          # {"name":"filegate","version":...}
curl http://127.0.0.1:8080/healthz   # ok
```

검사와 이미지 빌드:

```sh
cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
docker build -f backend/Dockerfile -t filegate:dev .
```

릴리스는 `VERSION` 파일을 올려 main에 머지하면 GitHub Actions가 ghcr 이미지와 태그를 발행한다.
