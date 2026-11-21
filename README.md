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
- **Remote Control API + Embedded Web UI:**
  - Clean JSON REST API + live Server-Sent Events (`/api/events`) for future native mobile/desktop remote apps.
  - Built-in responsive web remote served at `/`.

## Quick Start

```bash
go run ./cmd/tacet --music-dir /path/to/music
```

Open `http://localhost:8080` (or `http://<your-lan-ip>:8080` from your phone/tablet).

### Flags & Environment Variables

| Flag | Env Var | Default | Description |
|---|---|---|---|
| `--port` | `TACET_PORT` | `8080` | HTTP server port (media, API, web remote) |
| `--music-dir` | `TACET_MUSIC_DIR` | *(empty)* | Comma-separated local music directories |
| `--public-url` | `TACET_PUBLIC_URL` | *(auto)* | Override base URL sent to renderers (useful in Docker) |
| `--renderer-url` | `TACET_RENDERER_URL` | *(empty)* | Connect directly to a renderer XML URL (e.g. `http://192.168.1.50:49152/description.xml`) |
| `--stream-mode` | `TACET_STREAM_MODE` | `direct` | Default Qobuz mode: `direct` or `relay` |

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
go test -race ./...
go build -o tacet ./cmd/tacet
```
