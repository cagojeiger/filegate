# syntax=docker/dockerfile:1.7

# opsgate의 하우스 Dockerfile 패턴: cargo-chef + sccache로 의존성 레이어를
# 캐시하고, 런타임은 debian slim + 비루트 사용자 + 바이너리만 담는다.

FROM rust:1.96.1-bookworm AS chef

WORKDIR /app
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        cmake \
        g++ \
        make \
        perl \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*
RUN --mount=type=cache,id=filegate-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=filegate-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    cargo install cargo-chef --locked \
    && cargo install sccache --locked --no-default-features

ENV RUSTC_WRAPPER=sccache \
    SCCACHE_DIR=/sccache \
    CARGO_INCREMENTAL=0

FROM chef AS planner

COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY backend ./backend
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder

COPY --from=planner /app/recipe.json recipe.json
# sqlx query! 매크로를 쓰기 시작하면 SQLX_OFFLINE=true + .sqlx 메타데이터 COPY가 여기 들어온다.
RUN --mount=type=cache,id=filegate-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=filegate-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=filegate-sccache,target=/sccache,sharing=locked \
    --mount=type=cache,id=filegate-target-release,target=/app/target,sharing=locked \
    cargo chef cook --release --locked --recipe-path recipe.json
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY backend ./backend
RUN --mount=type=cache,id=filegate-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=filegate-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=filegate-sccache,target=/sccache,sharing=locked \
    --mount=type=cache,id=filegate-target-release,target=/app/target,sharing=locked \
    cargo build --release --locked --bin filegate \
    && cp /app/target/release/filegate /usr/local/bin/filegate

FROM debian:bookworm-slim AS runtime

COPY --from=chef /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
RUN groupadd --gid 10001 app \
    && useradd --uid 10001 --gid app --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin appuser
WORKDIR /app
COPY --from=builder /usr/local/bin/filegate /usr/local/bin/filegate

# 설정은 전부 env다 (ADR 004): 서버 설정과 자격증명은 배포 env/ESO가 주고,
# 등록부(storages·clients·bindings)는 DB에 산다 — 이미지에 설정 파일이 없다.
ENV FILEGATE_BIND=0.0.0.0:8080
EXPOSE 8080

USER appuser
ENTRYPOINT ["/usr/local/bin/filegate"]
