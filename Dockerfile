# syntax=docker/dockerfile:1.7
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 The Ontele Authors
#
# Multi-arch (amd64/arm64), Debian trixie throughout. Three stages:
#   1. Rust build with a prebuilt cargo-chef image + BuildKit cache mounts, so
#      dependencies compile once and incremental rebuilds reuse the target dir
#   2. Comskip from source against trixie's ffmpeg 7 libraries
#   3. slim runtime: ffmpeg 7.1 (+ VA-API drivers), tini, non-root user
#
#   docker build -t ontele .
#   docker buildx build --platform linux/amd64,linux/arm64 -t ghcr.io/ontele/ontele .
#
# Rebuild cost after the first build: only the changed Rust crate. The apt,
# Comskip and dependency layers are cached, and cargo's registry + target dir
# live in cache mounts that survive `docker build` invocations.

ARG RUST_VERSION=1.98.0
ARG DEBIAN=trixie

# ---------- 1. build ----------
FROM lukemathwalker/cargo-chef:latest-rust-${RUST_VERSION}-${DEBIAN} AS chef
WORKDIR /src

FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY migrations ./migrations
COPY ui ./ui
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /src/recipe.json recipe.json
# dependencies only — reused until Cargo.toml/Cargo.lock change
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo chef cook --release --recipe-path recipe.json
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY migrations ./migrations
COPY ui ./ui
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --release --locked \
    && mkdir -p /out && cp target/release/ontele /out/ontele

# ---------- 2. comskip ----------
FROM debian:${DEBIAN}-slim AS comskip
RUN rm -f /etc/apt/apt.conf.d/docker-clean
RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt,sharing=locked \
    apt-get update && apt-get install -y --no-install-recommends \
      git ca-certificates build-essential autoconf automake libtool pkg-config \
      libargtable2-dev libavformat-dev libavcodec-dev libavutil-dev libswscale-dev
RUN git clone --depth 1 https://github.com/erikkaashoek/Comskip /Comskip \
    && cd /Comskip && ./autogen.sh && ./configure && make -j"$(nproc)"

# ---------- 3. runtime ----------
FROM debian:${DEBIAN}-slim
LABEL org.opencontainers.image.title="Ontele" \
      org.opencontainers.image.description="Media server: library, live TV, DVR, commercial skip" \
      org.opencontainers.image.source="https://github.com/ontele/ontele" \
      org.opencontainers.image.licenses="Apache-2.0"
RUN rm -f /etc/apt/apt.conf.d/docker-clean
RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt,sharing=locked \
    apt-get update && apt-get install -y --no-install-recommends \
      handbrake-cli \
      ffmpeg libargtable2-0 ca-certificates tzdata tini curl \
      va-driver-all libva-drm2 \
    # Intel iHD VA-API driver (QSV/VA-API on Gen8+) is amd64-only
    && if [ "$(dpkg --print-architecture)" = "amd64" ]; then \
         apt-get install -y --no-install-recommends intel-media-va-driver; fi
# uid/gid 1000 so bind-mounted libraries keep sane ownership; video/render
# groups give /dev/dri access when the device is passed through.
RUN groupadd -g 1000 ontele && useradd -u 1000 -g 1000 -M -s /usr/sbin/nologin ontele \
    && groupadd -f video && groupadd -f render && usermod -aG video,render ontele \
    && mkdir -p /data /media /music /recordings && chown -R ontele:ontele /data /recordings
COPY --from=builder /out/ontele /usr/local/bin/ontele
COPY --from=comskip /Comskip/comskip /usr/local/bin/comskip

ENV ONTELE_ADDR=0.0.0.0:7979 \
    ONTELE_DATA=/data \
    ONTELE_MEDIA=/media/movies,/media/tv \
    ONTELE_MUSIC=/music \
    ONTELE_RECORDINGS=/recordings \
    ONTELE_AUTH=proxy \
    ONTELE_LOG_FORMAT=json \
    RUST_LOG=info,sqlx=warn

VOLUME ["/data", "/media", "/music", "/recordings"]
EXPOSE 7979
USER ontele
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
  CMD curl -fsS http://127.0.0.1:7979/readyz || exit 1
# UDP tuner discovery needs host networking to see LAN broadcast;
# otherwise set ONTELE_HDHR (or Settings → HDHomeRun IP).
COPY tools/handbrake-postprocess.sh /usr/local/bin/handbrake-postprocess.sh

ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/ontele"]
