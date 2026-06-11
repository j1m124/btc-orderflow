# syntax=docker/dockerfile:1.7
#
# Multi-stage build → single runtime image.
#
#   Stage 1 (chef)             pinned rust + cargo-chef base; installs the
#                              rust-toolchain.toml nightly once.
#   Stage 2 (planner)          `cargo chef prepare` — distills the workspace
#                              to recipe.json (manifests + lockfile only).
#   Stage 3 (rust-builder)     `cargo chef cook` builds ALL dependencies in a
#                              layer keyed on recipe.json, so app-only commits
#                              reuse it via the GHA layer cache. Then the real
#                              `cargo build` compiles just the workspace
#                              crates + runs wasm-bindgen.
#   Stage 4 (frontend-builder) runs Vite + Bun against the wasm-bindgen
#                              output to produce dist/.
#   Stage 5 (runtime)          debian-slim with the server binary + dist/,
#                              ca-certificates, tini, curl-for-healthcheck.
#
# Layer-caching rationale (this is what makes CI fast — don't break it):
#   - BuildKit `RUN --mount=type=cache` does NOT persist on GitHub Actions
#     (mounts live in runner-local BuildKit state, wiped per job). Everything
#     cacheable must live in image layers, which `cache-to: type=gha` exports.
#   - The cook layer holds CARGO_HOME (crate registry + the multi-GB zed git
#     checkout) AND target/ with every dependency compiled for both the
#     native and wasm targets. It only rebuilds when recipe.json changes,
#     i.e. on Cargo.toml/Cargo.lock edits — not on source edits.
#   - BUILD_SHA/BUILD_REF change every commit, so they are declared AFTER
#     the cook layer; declaring them earlier would invalidate it every push.
#   - The base image is digest-pinned: an upstream `rust:bookworm` rebuild
#     would otherwise re-key every layer mid-week. Bump deliberately.
#
# GHA passes BUILD_SHA / BUILD_REF as build args; build.rs in crates/client
# threads them into the bottom-bar version string. STATIC_DIR + LISTEN_ADDR
# are baked into the runtime env so Dokploy only has to set DATABASE_URL
# and ALLOWED_ORIGINS.

# ============================================================================
# Stage 1: chef base (pinned rust:bookworm + cargo-chef preinstalled)
# ============================================================================
# The digest pins BOTH the cargo-chef version (0.1.77) and the rust:bookworm
# base it's built on. `rust-toolchain.toml` (channel = nightly, targets =
# wasm32-unknown-unknown) makes rustup install the real toolchain on the
# first cargo/rustc invocation — the image's stable rust is irrelevant.
FROM lukemathwalker/cargo-chef:0.1.77-rust-bookworm@sha256:fa7281503a177bd5af6261f4041ca6b36d9f0de8d3090886c33cbd8e65b88ca9 AS chef

# Use the system `git` binary for cargo's git-dep fetches instead of the
# bundled libgit2. libgit2 emits no progress during fetch, which makes
# large repos (zed-industries/zed is multi-GB) look hung for 10+ min on
# first build. The CLI path is generally faster and prints progress.
ENV CARGO_NET_GIT_FETCH_WITH_CLI=true

WORKDIR /build

# Bring the toolchain pin in first so the rustup install layer caches
# independently of source changes.
COPY rust-toolchain.toml ./
RUN rustc --version

# ============================================================================
# Stage 2: planner — produce the dependency recipe
# ============================================================================
# Re-runs on every commit (COPY . . invalidates), but takes ~2s. When no
# manifest changed, the emitted recipe.json is byte-identical, so the
# builder's `COPY --from=planner` layer — and the expensive cook layer
# behind it — stay cache hits.
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ============================================================================
# Stage 3: Rust + WASM build
# ============================================================================
FROM chef AS rust-builder

COPY --from=planner /build/recipe.json recipe.json

# The vendored gpui-component fork is path-patched in the workspace
# Cargo.toml, but chef's recipe only captures workspace-member manifests —
# without the real vendor/ tree, cook can't resolve the [patch] and fails.
# Copying it in ALSO means the fork compiles during cook, so this layer
# holds every dependency (gpui, the fork, registry crates) for both
# targets; it re-keys only when recipe.json or vendor/ change.
COPY vendor/ vendor/

RUN cargo chef cook --release --recipe-path recipe.json --package server \
 && cargo chef cook --release --recipe-path recipe.json --package client \
        --target wasm32-unknown-unknown

# Prebuilt wasm-bindgen instead of `cargo install` (~3s vs ~90s compile).
# Must match the wasm-bindgen crate version in Cargo.lock — drift = the JS
# shim references symbols the WASM blob doesn't export. The Makefile pins
# the same value; bump together.
ARG WASM_BINDGEN_VERSION=0.2.120
RUN curl -fsSL "https://github.com/wasm-bindgen/wasm-bindgen/releases/download/${WASM_BINDGEN_VERSION}/wasm-bindgen-${WASM_BINDGEN_VERSION}-x86_64-unknown-linux-musl.tar.gz" \
    | tar -xz -C /usr/local/bin --strip-components=1 --wildcards '*/wasm-bindgen' \
 && wasm-bindgen --version

# Bring the real source. `.dockerignore` filters target/, www/node_modules/,
# www/dist/, www/src/wasm/, .env, etc.
COPY . .

# Build-time version tags surfaced through crates/client/build.rs into the
# bottom-bar version string ("v0.1.0-abc1234"). Empty on local builds.
# Declared HERE (not at the top) because they change every commit — see the
# layer-caching rationale above.
ARG BUILD_SHA=""
ARG BUILD_REF=""

# Only the workspace crates + the vendored gpui-component compile here; all
# other deps come precompiled from the cook layer.
RUN cargo build -p server --release \
 && cargo build -p client --lib --target wasm32-unknown-unknown --release \
 && wasm-bindgen \
        /build/target/wasm32-unknown-unknown/release/client.wasm \
        --out-dir /build/www/src/wasm \
        --target web \
        --no-typescript \
 && mkdir -p /artifacts \
 && cp /build/target/release/server /artifacts/server

# ============================================================================
# Stage 4: Frontend build (Vite + Bun)
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
# Stage 5: Runtime
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
