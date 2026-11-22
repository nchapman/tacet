# tacet

An open-source, headless UPnP/DLNA control point server written in Go for lossless hi-res playback (up to 24-bit/192kHz and DSD) from a local NAS library and Qobuz to compatible network renderers (such as a WiiM Ultra).

`tacet` delivers original, untouched audio files—no software decoding, resampling, or DSP in the signal path—while keeping playback queues alive on the server so remote apps can disconnect without interrupting playback.

## Features

- **UPnP/DLNA Control Point (`huin/goupnp`):**
  - Automatic SSDP discovery of `MediaRenderer` devices on the LAN, plus direct URL connect (`http://<ip>:49152/description.xml` for WiiM).
  - Negotiates MIME types against `ConnectionManager.GetProtocolInfo` (uses standard `audio/flac` for WiiM/LinkPlay renderers) and inspects `AVTransport` SCPD to cache per-device capabilities (`SetNextAVTransportURI`, `Seek`).
  - UPnP GENA event subscriptions (`SUBSCRIBE`/`NOTIFY` `LastChange`) for instant transport, gapless track-transition, and hardware volume/mute sync, backed by lightweight polling.
  - Server-side playback queue per renderer with gapless playback via `SetNextAVTransportURI` (with automatic `STOPPED` fallback).
  - Full transport (`Play`, `Pause`, `Stop`, `Next`, `Prev`, `Seek`) and `RenderingControl` (`Volume`, `Mute`).
- **Two Qobuz Delivery Modes (up to 24-bit/192kHz FLAC):**
  - **Direct (`direct`):** Hands the signed Qobuz CDN URL directly to your renderer—zero audio traffic passes through the server.
  - **Quiet Relay (`relay`):** Terminates TLS on the server and forwards raw, untouched FLAC bytes over plain HTTP with `Range` support.
- **Direct Local NAS Library:**
  - Recursively scans local music directories (`.flac`, `.wav`, `.aiff`, `.m4a`/ALAC, `.dsf`, `.mp3`, `.ogg`, `.opus`), reading tags, embedded/folder cover art, and exact sample rate/bit depth/duration from audio headers.
  - Serves untouched files over plain HTTP with `Range` and DLNA streaming headers.
- **Remote Control API + Embedded React Web Client:**
  - Versioned `/api/v1` multi-zone JSON REST API with OpenAPI 3.1 spec (`GET /api/v1/openapi.yaml`, `internal/api/openapi.yaml`), typed error envelopes, and split Server-Sent Events (`/api/v1/events`).
  - Typed TypeScript client SDK (`web/src/api/`) and React + Vite web remote compiled and embedded directly into the single Go binary at `/`.

## Quick Start

### Binary

```bash
make build
./tacet --music-dir /path/to/music
```

### Docker (Linux / NAS)

Multi-architecture images (`linux/amd64`, `linux/arm64`) are published to GitHub Container Registry:

- `:latest` (and semver tags like `:1.0.0`, `:1.0`, `:1`) — latest stable Git release tag (`v*`)
- `:dev` — latest commit on `main`

```bash
docker run -d \
  --name tacet \
  --restart unless-stopped \
  --network host \
  -v /path/to/music:/music:ro \
  -v tacet-data:/data \
  ghcr.io/nchapman/tacet:latest
```

Or use the included `docker-compose.yml` with `MUSIC_DIR` (via shell or `.env`):

```bash
MUSIC_DIR=/path/to/music docker compose up -d
```

Inside the container, `TACET_MUSIC_DIR` defaults to `/music` and accepts comma-separated paths if you mount multiple directories (e.g. `TACET_MUSIC_DIR=/music/flac,/music/dsd`).

> **Note:** `--network host` is recommended on Linux/NAS hosts so UPnP/DLNA SSDP discovery (`239.255.255.250:1900`) and GENA event callbacks work out of the box. On bridge networks (such as Docker Desktop on macOS), publish `-p 4320:4320` and set `TACET_PUBLIC_URL` + `TACET_RENDERER_URL`.

For frontend development with hot reload against a running Go server on `:4320`:

```bash
cd web && npm install && npm run dev
```

Open `http://localhost:4320` (or `http://<your-lan-ip>:4320` from your phone/tablet).

### Flags & Environment Variables

| Flag | Env Var | Default | Description |
|---|---|---|---|
| `--port` | `TACET_PORT` | `4320` | HTTP server port (media, API, web remote) |
| `--music-dir` | `TACET_MUSIC_DIR` | *(empty)* | Comma-separated local music directories |
| `--data-dir` | `TACET_DATA_DIR` | OS user config dir | Persistent data directory for `library.db` (SQLite catalog, prefs, saved Qobuz session) |
| `--missing-retention-days` | `TACET_MISSING_RETENTION_DAYS` | `30` | Days to retain soft-deleted missing music before purging on scan (`0` = never purge) |
| `--public-url` | `TACET_PUBLIC_URL` | *(auto)* | Override base URL sent to renderers (useful behind NAT/bridge Docker) |
| `--renderer-url` | `TACET_RENDERER_URL` | *(empty)* | Connect directly to a renderer XML URL (e.g. `http://192.168.1.50:49152/description.xml`) |
| `--stream-mode` | `TACET_STREAM_MODE` | `direct` | Default Qobuz mode: `direct` or `relay` |
| `--cors-origin` | `TACET_CORS_ORIGIN` | *(empty)* | Optional `Access-Control-Allow-Origin` header for external web/hybrid clients |

### Qobuz Authentication

Open the **Qobuz** tab in the web remote and click **Sign In with Qobuz (Browser OAuth)**, or paste a `user_auth_token` from a logged-in `play.qobuz.com` session. You can also configure credentials via environment variables:

```bash
export QOBUZ_AUTH_TOKEN="..."
# Optional explicit app credentials (otherwise extracted from play.qobuz.com automatically):
# export QOBUZ_APP_ID="..."
# export QOBUZ_APP_SECRET="..."
```

## Build & Test

```bash
make web-install  # install React/Vite dependencies in web/
make check        # web typecheck + vitest + vite build, fmt-check, go vet, golangci-lint, go test -race
make build        # compiles React UI via Vite and embeds it into ./tacet
```
