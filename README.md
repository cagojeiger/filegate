# filegate

정책 기반 파일 게이트웨이. 서비스는 intent와 file_id만 알고, 물리(벤더·버킷·수명)는 filegate가 소유한다.

- 방향·원칙: [docs/adr/](docs/adr/README.md)
- 오퍼레이션 계약: [docs/spec/](docs/spec/00-operations.md)
- 기술 선택: [docs/stack/](docs/stack/README.md) · 벤더 사실: [docs/vendors/](docs/vendors/README.md)

## 개발 환경

설정은 `configs/filegate.yaml`에 있다 (로컬 개발 기본값 커밋됨). 프로덕션은 자기 설정을 별도로 두고 `FILEGATE_CONFIG`로 가리킨다. 설정이 없으면 부팅이 명확한 에러로 실패한다.

```sh
docker compose up -d          # MinIO(9000/9001) + PostgreSQL(55432) + 버킷 프로비저닝
cargo run --bin filegate      # configs/filegate.yaml 로드, http://127.0.0.1:8080
```

컨테이너로 띄울 때는 설정을 마운트한다:

```sh
docker run -v ./configs/filegate.yaml:/etc/filegate/filegate.yaml filegate:dev
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
