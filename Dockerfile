# syntax=docker/dockerfile:1

# Stage 1: Build the embedded React/Vite web client natively on the host platform.
FROM --platform=$BUILDPLATFORM node:24-alpine AS web-builder

WORKDIR /src/web
COPY web/package.json web/package-lock.json ./
RUN npm ci

COPY web/ ./
RUN npm run build

# Stage 2: Cross-compile the pure-Go static binary (CGO_ENABLED=0).
FROM --platform=$BUILDPLATFORM golang:1.27-alpine AS go-builder

ARG TARGETOS
ARG TARGETARCH

WORKDIR /src
COPY go.mod go.sum ./
RUN go mod download

COPY cmd/ ./cmd/
COPY internal/ ./internal/
COPY --from=web-builder /src/internal/api/web/dist ./internal/api/web/dist

RUN CGO_ENABLED=0 GOOS=${TARGETOS} GOARCH=${TARGETARCH} \
    go build -trimpath -ldflags="-s -w" -o /out/tacet ./cmd/tacet

# Stage 3: Minimal runtime image with CA certs, timezone data, and non-root user.
FROM alpine:3.21

ARG OCI_SOURCE="https://github.com/nchapman/tacet"
LABEL org.opencontainers.image.title="tacet" \
      org.opencontainers.image.description="Headless UPnP/DLNA control point and multi-zone bit-perfect hi-res audio server" \
      org.opencontainers.image.source="${OCI_SOURCE}"

RUN apk add --no-cache ca-certificates ffmpeg su-exec tzdata \
    && addgroup -g 1000 tacet \
    && adduser -D -u 1000 -G tacet -h /data tacet \
    && mkdir -p /data /music \
    && chown -R tacet:tacet /data /music

COPY --from=go-builder /out/tacet /usr/local/bin/tacet
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
