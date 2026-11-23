# tacet

[![CI](https://github.com/nchapman/tacet/actions/workflows/ci.yml/badge.svg)](https://github.com/nchapman/tacet/actions/workflows/ci.yml)

A headless, self-hosted music server and control point for lossless hi-res playback from your local library and Qobuz to network renderers (UPnP/DLNA, WiiM/Linkplay, Google Cast) and web browsers.

`tacet` streams original, untouched audio files by default—up to 24-bit/192 kHz FLAC and DSD—while keeping playback queues running on the server so phones and browsers can disconnect without interrupting music. Everything ships in a single Go binary with an embedded web remote.

## Features

- **Multi-zone playback** — discovers and controls UPnP/DLNA renderers (SSDP discovery, GENA events, gapless `SetNextAVTransportURI`), native Linkplay device queues (WiiM, Audio Pro, Arylic), Google Cast devices and speaker groups (mDNS), and browser or custom HTTP/SSE players.
- **Bit-perfect by default, transcode when needed** — serves original `.flac`, `.wav`, `.aiff`, `.m4a`/ALAC, `.dsf`, `.mp3`, `.ogg`, and `.opus` files over HTTP with `Range` support, plus optional per-zone `ffmpeg` transcoding to WAV or Opus (96–256 kbps).
- **Unified local + Qobuz library** — indexes local folders into SQLite (`library.db`) with live filesystem watching and scheduled incremental rescans, merged alongside synced Qobuz favorites, search, and artist discographies.
- **Two Qobuz delivery modes** — `direct` hands signed HTTPS CDN URLs straight to your renderer (zero audio traffic through the server), or `relay` terminates TLS on the server and forwards untouched FLAC over plain LAN HTTP.
- **Embedded web UI & REST/SSE API** — responsive React web remote served at `/`, backed by a versioned `/api/v1` API, live Server-Sent Events (`/api/v1/events`), and an OpenAPI 3.1 spec (`/api/v1/openapi.yaml`).

## Quick Start (Docker)

Multi-architecture images (`linux/amd64`, `linux/arm64`) with `ffmpeg` preinstalled are published to GitHub Container Registry:

- `ghcr.io/nchapman/tacet:latest` (and semver tags such as `:0.1.3`, `:0.1`) — latest Git release
- `ghcr.io/nchapman/tacet:dev` — latest commit on `main`

### Docker Compose

Save this as `compose.yaml` and replace `/path/to/music` with your music folder:

```yaml
services:
  tacet:
    image: ghcr.io/nchapman/tacet:latest
    container_name: tacet
    restart: unless-stopped
    network_mode: host
    volumes:
      - /path/to/music:/music:ro
      - tacet-data:/data

volumes:
  tacet-data:
```

Start the container:

```bash
docker compose up -d
```

Open `http://<host-ip>:4320` (or `http://localhost:4320` locally) in a browser.

> If you cloned this repository, you can also run the included [`docker-compose.yml`](docker-compose.yml) directly:
> ```bash
> MUSIC_DIR=/path/to/music docker compose up -d
> ```

### Docker CLI

```bash
docker run -d \
  --name tacet \
  --restart unless-stopped \
  --network host \
  -v /path/to/music:/music:ro \
  -v tacet-data:/data \
  ghcr.io/nchapman/tacet:latest
```

### Common Docker Options

- **Bridge networking (macOS / Docker Desktop):** `network_mode: host` is recommended on Linux/NAS hosts so UPnP SSDP (`239.255.255.250:1900`), Cast mDNS, and GENA callbacks work automatically. On bridge networks, publish port `4320` and point renderers at your host IP:

  ```yaml
  services:
    tacet:
      image: ghcr.io/nchapman/tacet:latest
      container_name: tacet
      restart: unless-stopped
      ports:
        - "4320:4320"
      environment:
        TACET_PUBLIC_URL: http://192.168.1.10:4320
        TACET_RENDERER_URL: http://192.168.1.50:49152/description.xml
      volumes:
        - /path/to/music:/music:ro
        - tacet-data:/data

  volumes:
    tacet-data:
  ```

- **Host permissions (`PUID` / `PGID`):** the container defaults to UID/GID `1000` and adjusts ownership of `/data` on startup. Set `PUID` and `PGID` in `environment` to match your NAS user if needed.
- **Multiple music folders:** mount each folder under `/music` and pass comma-separated paths in `TACET_MUSIC_DIR` (for example `TACET_MUSIC_DIR=/music/flac,/music/dsd`).
- **Health check:** the image includes a built-in `HEALTHCHECK` against `GET /api/health`.

## Qobuz Setup

1. Open the web UI and go to **Settings → Connect Qobuz**.
2. Click **Sign in with Qobuz** to authenticate via browser OAuth, or expand **Or sign in with an auth token** to paste a `user_auth_token`.

You can also pass credentials via environment variables (useful for headless setups):

```bash
QOBUZ_AUTH_TOKEN="..."
# Or email/password login:
# QOBUZ_EMAIL="you@example.com"
# QOBUZ_PASSWORD="..."
# Optional explicit app credentials (auto-discovered from play.qobuz.com if omitted):
# QOBUZ_APP_ID="..."
# QOBUZ_APP_SECRET="..."
```

## Configuration

Every CLI flag can also be set via an environment variable. Explicit CLI flags and environment variables take precedence over defaults and saved SQLite preferences (such as `--stream-mode`).

| Flag | Environment Variable | Default | Description |
|---|---|---|---|
| `--port` | `TACET_PORT` | `4320` | HTTP port for media serving, API, and web UI |
| `--music-dir` | `TACET_MUSIC_DIR` | *(empty; `/music` in Docker)* | Comma-separated local music directories to index |
| `--data-dir` | `TACET_DATA_DIR` | OS config dir (`/data` in Docker) | Persistent directory for `library.db` and caches |
| `--missing-retention-days` | `TACET_MISSING_RETENTION_DAYS` | `30` | Days to keep soft-deleted missing tracks before purging on scan (`0` = never purge) |
| `--public-url` | `TACET_PUBLIC_URL` | *(auto)* | Base URL sent to renderers (e.g. `http://192.168.1.10:4320`) |
| `--renderer-url` | `TACET_RENDERER_URL` | *(empty)* | UPnP description XML URL to connect directly (e.g. `http://192.168.1.50:49152/description.xml`) |
| `--stream-mode` | `TACET_STREAM_MODE` | `direct` | Default Qobuz delivery mode: `direct` or `relay` |
| `--cors-origin` | `TACET_CORS_ORIGIN` | *(empty)* | Optional `Access-Control-Allow-Origin` value for external web clients |
| `--event-log` | `TACET_EVENT_LOG` | *(empty)* | Path (or `-` for stderr) to write raw JSONL playback/GENA/poll/SSE events |
| *(Docker only)* | `PUID` / `PGID` | `1000` | UID/GID used by the container entrypoint when dropping root privileges |

## Building from Source & Development

### Prerequisites

- **Go 1.27+** (see [`go.mod`](go.mod))
- **Node.js 24+** (for building the embedded React/Vite frontend)
- **ffmpeg** *(optional)* — required only for per-zone Opus/WAV transcoding (included automatically in the Docker image)

### Build & Run

```bash
make web-install
make build
./tacet --music-dir /path/to/music
```

Open `http://localhost:4320` (or `http://<your-lan-ip>:4320` from another device on your network).

### Development & Testing

```bash
make check        # web typecheck + vitest + vite build, gofmt check, go vet, golangci-lint, go test -race
```

To run the Vite dev server with hot reload against a running Go server on `:4320`:

```bash
cd web && npm run dev
```

The OpenAPI 3.1 specification is stored at [`internal/api/openapi.yaml`](internal/api/openapi.yaml) and served at `GET /api/v1/openapi.yaml`.

## Scope & Limitations

- **Trusted LAN deployment:** `tacet` is designed for home networks and does not include user accounts or authentication (mutating browser requests are protected by same-origin CSRF checks). Avoid exposing port `4320` directly to the public internet without a reverse proxy and auth layer.
- **Discovery across subnets:** Automatic UPnP SSDP and Google Cast mDNS discovery require multicast on the same LAN segment. Across VLANs or in bridged containers, add UPnP renderers via `TACET_RENDERER_URL` or the web UI's manual connect dialog.
