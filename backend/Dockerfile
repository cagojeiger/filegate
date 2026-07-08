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
# sqlx 매크로는 커밋된 .sqlx 메타데이터로 검사한다 (라이브 DB 불필요).
ENV SQLX_OFFLINE=true
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

ENV FILEGATE_BIND=0.0.0.0:8080
EXPOSE 8080

USER appuser
ENTRYPOINT ["/usr/local/bin/filegate"]
