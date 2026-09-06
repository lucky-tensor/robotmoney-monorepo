# Rust devnet service images: ONE shared builder, TWO runtime targets.
#
# Canonical: Plan tracking issue #109 §11 — Phase 5; split per issue #1354
# (mirrors robotmoney/devops#1).
# Used by: testing/ethereum-testnet/config/docker-compose.dapp.yaml
#
# `indexer` (services/explorer-indexer) and `explorer-api`
# (clients/explorer-api) are both members of the root cargo workspace and share
# a large dependency closure (alloy, tokio, reqwest, rmpc-logging, …). An even
# earlier arrangement gave each service its OWN Dockerfile, so each spun up its
# own `rust` builder and recompiled that closure independently — paying the cost
# twice and rebuilding on every devnet boot. That is why the `builder` stage
# below is shared: a single `cargo build --release --bin indexer --bin
# explorer-api` against the committed Cargo.lock compiles the workspace exactly
# once per source change.
#
# The two services no longer share a restart domain, though: instead of one
# image carrying both binaries and being selected by a compose `entrypoint:`
# override, this file exposes two named targets — `indexer` and `explorer-api`
# — that each `COPY --from=builder` only their own binary and bake their own
# ENTRYPOINT. Compose builds both targets from the same Dockerfile and build
# context, so BuildKit resolves the `builder` stage to the same cache key for
# both and executes the cargo compile once; the second target's build is a
# cache hit on every builder-stage layer. (Concurrent solves of an identical
# vertex are deduplicated in-flight by BuildKit, so this holds whether compose
# builds the two targets sequentially or in parallel.) The result is
# independent images — independent redeploys — at single-compile cost.
#
# Build context is the repo root (see .dockerignore for what is excluded) so the
# whole workspace resolves; `--bin indexer --bin explorer-api` restricts the
# compile to just those two binaries and their dependency closure.
#
# There is deliberately no default target: `docker build` without `--target`
# would stop at the last stage and silently produce the explorer-api image.
# Every consumer names its target explicitly (see docker-compose.dapp.yaml).

FROM rust:1-bookworm AS builder
WORKDIR /build

RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config libssl-dev ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY . .
RUN cargo build --release --bin indexer --bin explorer-api

# Runtime deps shared by both binaries.
# ca-certificates + libssl3: TLS for both binaries' RPC/DB clients.
FROM debian:bookworm-slim AS runtime-base
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/*

# --- indexer image -----------------------------------------------------------
# Chain-polling write workload. No HTTP listener, and its compose service
# declares no healthcheck, so `curl` is intentionally NOT installed here — it
# only ever existed for the explorer-api container healthcheck. The indexer's
# own outbound fetches (payload_uri digest verification, issue #1294) go through
# reqwest in-process, not a shelled-out curl.
FROM runtime-base AS indexer
COPY --from=builder /build/target/release/indexer /usr/local/bin/indexer
ENTRYPOINT ["/usr/local/bin/indexer"]

# --- explorer-api image ------------------------------------------------------
# HTTP read workload. curl is required by the explorer-api container healthcheck
# in docker-compose.dapp.yaml (`curl -sf http://localhost:8080/health`).
# EXPLORER_API_BIND / EXPOSE 8080 belong to this image only.
FROM runtime-base AS explorer-api
RUN apt-get update \
    && apt-get install -y --no-install-recommends curl \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/target/release/explorer-api /usr/local/bin/explorer-api
ENV EXPLORER_API_BIND=0.0.0.0:8080
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/explorer-api"]
