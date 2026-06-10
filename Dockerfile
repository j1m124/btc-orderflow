# syntax=docker/dockerfile:1.7
#
# Multi-stage build → single runtime image.
#
#   Stage 1 (rust-builder)     compiles the server binary + WASM client
#                              and runs wasm-bindgen.
#   Stage 2 (frontend-builder) runs Vite + Bun against the wasm-bindgen
#                              output to produce dist/.
#   Stage 3 (runtime)          debian-slim with the server binary + dist/,
#                              ca-certificates, tini, curl-for-healthcheck.
#
# GHA passes BUILD_SHA / BUILD_REF as build args; build.rs in crates/client
# threads them into the bottom-bar version string. STATIC_DIR + LISTEN_ADDR
# are baked into the runtime env so Dokploy only has to set DATABASE_URL
# and ALLOWED_ORIGINS.

# ============================================================================
# Stage 1: Rust + WASM build
# ============================================================================
# The official rust image ships rustup. `rust-toolchain.toml` in the source
# (channel = nightly-2026-05-29, targets = wasm32-unknown-unknown, components
# = rustfmt + clippy) triggers rustup to install the pinned nightly on the
# first cargo invocation inside the workspace.
FROM rust:bookworm AS rust-builder

# Match the Makefile's pinned wasm-bindgen-cli. Drift between this and the
# wasm-bindgen crate in Cargo.lock = JS shim references symbols the WASM
# blob didn't export. Bump both together.
ARG WASM_BINDGEN_VERSION=0.2.120

# Build-time version tags surfaced through crates/client/build.rs into the
# bottom-bar version string ("v0.1.0-abc1234"). Empty on local builds.
ARG BUILD_SHA=""
ARG BUILD_REF=""
ENV BUILD_SHA=${BUILD_SHA} \
    BUILD_REF=${BUILD_REF} \
    # Use the system `git` binary for cargo's git-dep fetches instead of the
    # bundled libgit2. libgit2 emits no progress during fetch, which makes
    # large repos (zed-industries/zed is multi-GB) look hung for 10+ min on
    # first build. The CLI path is generally faster and prints progress.
    CARGO_NET_GIT_FETCH_WITH_CLI=true

WORKDIR /build

# Bring the toolchain pin in first so the rustup install layer caches
# independently of source changes.
COPY rust-toolchain.toml ./
RUN rustc --version

# Install wasm-bindgen-cli once. The cargo registry / git caches persist
# across image builds via BuildKit cache mounts.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo install --locked wasm-bindgen-cli --version ${WASM_BINDGEN_VERSION}

# Bring the rest of the workspace. `.dockerignore` filters target/,
# www/node_modules/, www/dist/, www/src/wasm/, .env, etc.
COPY . .

# Build server (host = linux/amd64) and WASM client in release mode, run
# wasm-bindgen, and copy the binary OUT of the cache-mounted target/ within
# the same RUN so it survives the mount teardown.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/build/target \
    cargo build -p server --release \
 && cargo build -p client --lib --target wasm32-unknown-unknown --release \
 && wasm-bindgen \
        /build/target/wasm32-unknown-unknown/release/client.wasm \
        --out-dir /build/www/src/wasm \
        --target web \
        --no-typescript \
 && mkdir -p /artifacts \
 && cp /build/target/release/server /artifacts/server

# ============================================================================
# Stage 2: Frontend build (Vite + Bun)
# ============================================================================
# `bun --bun vite build` matches the dev workflow in www/package.json — the
# `--bun` flag forces Bun's runtime instead of shelling out to Node (Bun's
# default for unknown CLIs). www/bun.lock is gitignored, so no
# --frozen-lockfile.
FROM oven/bun:1 AS frontend-builder

WORKDIR /build/www

COPY --from=rust-builder /build/www ./

RUN bun install \
 && bun --bun vite build

# ============================================================================
# Stage 3: Runtime
# ============================================================================
FROM debian:bookworm-slim AS runtime

# ca-certificates: rustls trust store for outbound TLS (Binance REST + WS).
# tini:            PID-1 zombie reaper + SIGTERM forwarder (Docker stop sends
#                  SIGTERM to PID 1; without tini, the kernel drops it unless
#                  the process explicitly handles it — tokio::signal::ctrl_c
#                  only catches SIGINT).
# curl:            used by the Docker HEALTHCHECK below.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        ca-certificates \
        tini \
        curl \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=rust-builder /artifacts/server /app/server
COPY --from=frontend-builder /build/www/dist /app/dist

# Sensible defaults; Dokploy overrides DATABASE_URL / ALLOWED_ORIGINS via
# its env-var UI. LISTEN_ADDR binds the in-container interface (Traefik
# routes from there).
ENV LISTEN_ADDR=0.0.0.0:8787 \
    STATIC_DIR=/app/dist \
    RUST_LOG=server=info,sqlx=warn

EXPOSE 8787

# Dokploy honours Docker HEALTHCHECK. start_period covers the boot-time
# Binance cold-start (REST gap-heal + first WS connect — usually <10s but
# 30s leaves slack for slow links).
HEALTHCHECK --interval=30s --timeout=5s --start-period=30s --retries=3 \
    CMD curl -fsS http://127.0.0.1:8787/healthz || exit 1

ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["/app/server"]
