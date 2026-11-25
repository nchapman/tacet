# syntax=docker/dockerfile:1

# Stage 1: Build the embedded React/Vite web client natively on the host platform.
FROM --platform=$BUILDPLATFORM node:24-alpine AS web-builder

WORKDIR /src/web
COPY web/package.json web/package-lock.json ./
RUN npm ci

COPY web/ ./
RUN npm run build

# Stage 2: Base Rust + cargo-chef toolchain for the target architecture.
FROM lukemathwalker/cargo-chef:latest-rust-1-alpine AS chef

RUN apk add --no-cache build-base musl-dev pkgconfig
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

# Stage 3: Minimal runtime image with CA certs, timezone data, ffmpeg, and non-root user.
FROM alpine:3.21

ARG OCI_SOURCE="https://github.com/nchapman/tacet"
LABEL org.opencontainers.image.title="tacet" \
      org.opencontainers.image.description="Headless UPnP/DLNA control point and multi-zone bit-perfect hi-res audio server" \
      org.opencontainers.image.source="${OCI_SOURCE}"

RUN apk add --no-cache ca-certificates ffmpeg libjpeg-turbo-utils libwebp-tools su-exec tzdata \
    && addgroup -g 1000 tacet \
    && adduser -D -u 1000 -G tacet -h /data tacet \
    && mkdir -p /data /music \
    && chown -R tacet:tacet /data /music

COPY --from=rust-builder /out-tacet /usr/local/bin/tacet
COPY docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
RUN chmod +x /usr/local/bin/docker-entrypoint.sh

ENV TACET_PORT=4320 \
    TACET_DATA_DIR=/data \
    TACET_MUSIC_DIR=/music

VOLUME ["/data", "/music"]
EXPOSE 4320

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD wget --spider -q "http://127.0.0.1:${TACET_PORT:-4320}/api/health" || exit 1

ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]
