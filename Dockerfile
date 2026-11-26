# syntax=docker/dockerfile:1

# Stage 1: Build the embedded React/Vite web client natively on the host platform.
FROM --platform=$BUILDPLATFORM node:24-slim AS web-builder

WORKDIR /src/web
COPY web/package.json web/package-lock.json ./
RUN npm ci

COPY web/ ./
RUN npm run build

# Stage 2: Base Rust + cargo-chef toolchain for the target architecture (Debian 13 Trixie).
FROM lukemathwalker/cargo-chef:latest-rust-slim-trixie AS chef

RUN apt-get update \
    && apt-get install -y --no-install-recommends build-essential ca-certificates pkg-config \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src

# Stage 3: Compute the dependency recipe (stable across src/ and web/ edits).
FROM chef AS planner

COPY Cargo.toml Cargo.lock ./
COPY src/ ./src/
RUN cargo chef prepare --recipe-path recipe.json

# Stage 4: Cook external dependencies in a cacheable layer, then build tacet.
FROM chef AS rust-builder

COPY --from=planner /src/recipe.json recipe.json
RUN cargo chef cook --release --locked --recipe-path recipe.json

COPY Cargo.toml Cargo.lock ./
COPY src/ ./src/
COPY --from=web-builder /src/web/dist ./web/dist

RUN cargo build --release --locked \
    && strip target/release/tacet \
    && cp target/release/tacet /out-tacet

# Stage 5: Minimal Debian Trixie runtime image with CA certs, timezone data, ffmpeg, and non-root user.
FROM debian:trixie-slim

ARG OCI_SOURCE="https://github.com/nchapman/tacet"
LABEL org.opencontainers.image.title="tacet" \
      org.opencontainers.image.description="Headless UPnP/DLNA control point and multi-zone bit-perfect hi-res audio server" \
      org.opencontainers.image.source="${OCI_SOURCE}"

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl ffmpeg gosu tzdata \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd -g 1000 tacet \
    && useradd -u 1000 -g tacet -d /data -s /usr/sbin/nologin tacet \
    && mkdir -p /data /music \
    && chown -R tacet:tacet /data /music

COPY --from=rust-builder /out-tacet /usr/local/bin/tacet
COPY docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
RUN chmod +x /usr/local/bin/docker-entrypoint.sh

# Pre-bake Essentia Discogs-EffNet ONNX models (~21 MB total; CC BY-NC-SA 4.0,
# Music Technology Group, Universitat Pompeu Fabra) outside /data so mounting /data
# never hides them and sonic analysis works offline out of the box.
RUN mkdir -p /usr/share/tacet/models/sonic \
    && curl -fsSL -o /usr/share/tacet/models/sonic/discogs-effnet-bsdynamic-1.onnx \
        "https://essentia.upf.edu/models/feature-extractors/discogs-effnet/discogs-effnet-bsdynamic-1.onnx" \
    && curl -fsSL -o /usr/share/tacet/models/sonic/discogs-effnet-bsdynamic-1.json \
        "https://essentia.upf.edu/models/feature-extractors/discogs-effnet/discogs-effnet-bsdynamic-1.json" \
    && curl -fsSL -o /usr/share/tacet/models/sonic/engagement_regression-discogs-effnet-1.onnx \
        "https://essentia.upf.edu/models/classification-heads/engagement/engagement_regression-discogs-effnet-1.onnx" \
    && curl -fsSL -o /usr/share/tacet/models/sonic/approachability_regression-discogs-effnet-1.onnx \
        "https://essentia.upf.edu/models/classification-heads/approachability/approachability_regression-discogs-effnet-1.onnx" \
    && curl -fsSL -o /usr/share/tacet/models/sonic/voice_instrumental-discogs-effnet-1.onnx \
        "https://essentia.upf.edu/models/classification-heads/voice_instrumental/voice_instrumental-discogs-effnet-1.onnx" \
    && curl -fsSL -o /usr/share/tacet/models/sonic/danceability-discogs-effnet-1.onnx \
        "https://essentia.upf.edu/models/classification-heads/danceability/danceability-discogs-effnet-1.onnx" \
    && curl -fsSL -o /usr/share/tacet/models/sonic/mood_relaxed-discogs-effnet-1.onnx \
        "https://essentia.upf.edu/models/classification-heads/mood_relaxed/mood_relaxed-discogs-effnet-1.onnx" \
    && curl -fsSL -o /usr/share/tacet/models/sonic/mood_acoustic-discogs-effnet-1.onnx \
        "https://essentia.upf.edu/models/classification-heads/mood_acoustic/mood_acoustic-discogs-effnet-1.onnx" \
    && curl -fsSL -o /usr/share/tacet/models/sonic/mood_aggressive-discogs-effnet-1.onnx \
        "https://essentia.upf.edu/models/classification-heads/mood_aggressive/mood_aggressive-discogs-effnet-1.onnx" \
    && curl -fsSL -o /usr/share/tacet/models/sonic/mood_electronic-discogs-effnet-1.onnx \
        "https://essentia.upf.edu/models/classification-heads/mood_electronic/mood_electronic-discogs-effnet-1.onnx"

ENV TACET_PORT=4320 \
    TACET_DATA_DIR=/data \
    TACET_MUSIC_DIR=/music \
    TACET_SONIC_MODELS_DIR=/usr/share/tacet/models/sonic

VOLUME ["/data", "/music"]
EXPOSE 4320

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD curl -fsS "http://127.0.0.1:${TACET_PORT:-4320}/api/health" >/dev/null || exit 1

ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]
