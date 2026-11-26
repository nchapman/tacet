# Recommendations, Radio & Autoplay Architecture

This document captures our research, live API verification, and architecture for adding **Track Radio**, **Artist Radio**, **Album Radio**, and **Continue Playing (Autoplay)** to `tacet` — designed to work **local-first without Qobuz lock-in**, while seamlessly unlocking Qobuz's catalog when connected.

---

## 1. Guiding Principles

1. **Local-only feature parity**: Every radio/autoplay feature available with Qobuz must have a first-class local-only equivalent so users without a streaming subscription (or in local-only mode) get genuinely smart recommendations over their own library.
2. **One provider-agnostic interface**: The queue manager, API, and UI speak one `Recommender` abstraction; backends degrade cleanly from Qobuz → ListenBrainz (local-filtered) → pure-offline local heuristics (and future local sonic analysis).
3. **Prefer local playback**: When Qobuz returns recommendations, `prefer_local_tracks()` swaps in owned local copies automatically — upgraded from fuzzy `artist|title` matching to **exact ISRC matching first**.
4. **Thin, non-blocking enrichment**: Metadata resolution and analysis run asynchronously in the background after scans, never blocking library ingestion or playback.

---

## 2. Core User-Facing Features

All four features behave identically whether Qobuz is connected or not:

| Feature | Seed | Behavior |
|---|---|---|
| **Track Radio** | Track | Replaces/starts a radio queue seeded from one track. |
| **Artist Radio** | Artist | Builds a radio queue from an artist and similar artists. |
| **Album Radio** | Album | Builds a radio queue matching the vibe/era/neighbors of an album. |
| **Continue Playing (Autoplay)** | Active queue tail + history | Per-zone toggle; when 1–2 tracks remain, appends fresh tracks while excluding already-played tracks. |

---

## 3. Identity & Metadata Resolution (`tacet-resolver` + Local Tags)

To connect local files cleanly to **both** ListenBrainz (which uses MusicBrainz IDs) and Qobuz (which uses Qobuz numeric IDs, searchable by ISRC), local tracks need canonical identifiers.

### 3.1 Tag Extraction During Local Scan (`src/library/scanner/local.rs`)
During normal `lofty` scans, capture tags that may already exist in files (e.g. from MusicBrainz Picard / beets):
- `ISRC` / `TSRC`
- `MUSICBRAINZ_TRACKID` (recording MBID), `MUSICBRAINZ_ARTISTID`, `MUSICBRAINZ_ALBUMID`, `MUSICBRAINZ_RELEASEGROUPID`
- `UPC` / `BARCODE`
- ReplayGain (`REPLAYGAIN_TRACK_GAIN`, `REPLAYGAIN_TRACK_PEAK`)

### 3.2 `tacet-resolver` Background Enrichment
For albums/tracks missing canonical IDs or clean genres, `tacet-resolver` resolves full albums in a single request:

- **Base URL**: `https://tacet-resolver.68a117ca.tryvamos.dev` (configurable via env/prefs)
- **Auth**: `X-API-Key: <TACET_RESOLVER_API_KEY>` (or `Authorization: Bearer <key>`)
- **Health**: `GET /v1/health`, `GET /v1/readyz` (no auth)
- **Endpoints**:
  - `POST /v1/resolve/album` — up to 250 tracks/request, **300 req/min** limit. Preferred for library scans: aligns the whole tracklist in one call and skips fingerprinting when metadata/UPC/MBID matches.
  - `POST /v1/resolve/tracks` — up to 50 tracks/request, **120 req/min** limit. Best for loose/singles files or unmatched stragglers.
  - Overall per-IP limit: **600 req/min** (respect `HTTP 429` + `Retry-After` with exponential backoff).
- **Optional fingerprinting**: Raw `fpcalc` (Chromaprint, 60s+, default algorithm 2) + `durationSec` can be sent for tracks that fail metadata matching.

**Verified live results:**
- `POST /v1/resolve/album` on a real 12-track local album (*Adele — 19*, metadata only — no UPC/ISRC/fingerprints):
  - **171 ms**, **12/12 matched** (`confidence: 1.0`, `unmatchedIndices: []`)
  - Returned `releaseMbid`, `releaseGroupMbid`, `artistMbid`, `upc`, normalized `genres` (`["pop", "blue-eyed soul", "pop soul", "blues", "neo soul"]`), and per-track `recordingMbid` + `isrc`.
- `POST /v1/resolve/tracks` on 2 loose tracks:
  - **136 ms**; *Miles Davis — So What* matched at `confidence: 1.0` with `isrc`, `recordingMbid`, `artistMbid`, and 5 jazz sub-genres.
  - Note: Loose-track matching without an album context can occasionally match a different version (e.g. a live/extended cut at `confidence: 0.8333`), so always prefer `/v1/resolve/album` and enforce a confidence threshold + duration sanity check for loose tracks.

### 3.3 Why Canonical IDs Unlock Both Worlds
- **`recordingMbid` / `artistMbid`** → Direct indexed SQLite joins against ListenBrainz collaborative-filtering recommendations (`WHERE mb_recording_id IN (...)` / `WHERE mb_artist_id IN (...)`).
- **`isrc`** → Exact lookup on Qobuz (`GET /track/search?query=<ISRC>&limit=5`) and exact O(1) local preference in `prefer_local_tracks()` (eliminating *"Remastered"* / *"Deluxe Edition"* mismatches).
- **`genres`** → Clean, multi-label MusicBrainz genres (dramatically better than raw ID3 `GENRE` tags for offline radio).

---

## 4. Recommendation Backends (Verified Live)

### 4.1 Qobuz Native (`https://www.qobuz.com/api.json/0.2`)
Requires existing `X-App-Id` + `X-User-Auth-Token`. All four endpoints were verified live (`200 OK`):

1. **Autoplay / "Continue Playing": `POST /dynamic/suggest`**
   - Request body:
     ```json
     {
       "limit": 20,
       "listened_tracks_ids": [292859800],
       "track_to_analyse": [
         {
           "track_id": 292859800,
           "artist_id": 934333,
           "genre_id": 113,
           "label_id": 7247898
         }
       ]
     }
     ```
   - Response: `{ "algorithm": "reco", "tracks": { "items": [...] } }` (v1 track objects with full album, `isrc`, `audio_info.replaygain_track_gain`, `audio_info.replaygain_track_peak`, `label`, `genre`, hi-res specs).
   - **Critical quirk verified in testing**: You **must** pass all 4 fields (`track_id`, `artist_id`, `genre_id`, `label_id`) in `track_to_analyse`. Omitting `artist_id`/`genre_id`/`label_id` causes Qobuz to ignore the seed and return cached/generic recommendations.
   - Supports `listened_tracks_ids` to prevent repeats during endless autoplay.

2. **Dedicated Radio Endpoints**:
   - `GET /radio/track?track_id=<id>` (~25–30 tracks, `type: "radio-track"`)
   - `GET /radio/artist?artist_id=<id>` (~25 tracks, `type: "radio-artist"`)
   - `GET /radio/album?album_id=<id>` (~30 tracks, `type: "radio-album"`)
   - **Schema note**: `/radio/*` items use Qobuz's newer v2/discover track shape (`artists: [{ id, name, roles }]`, `audio_info: { maximum_bit_depth, maximum_sampling_rate, maximum_channel_count }`, `physical_support`, `rights`, `album: { id, title, image, label, genre }`), matching `src/qobuz/client/catalog.rs`.

3. **Seeding Qobuz from a Local Track**:
   - `GET /track/get?isrc=...` is **not** supported (`HTTP 400`).
   - **`GET /track/search?query=<ISRC>&limit=5` works directly** and returns exact Qobuz editions for that ISRC with zero noise.
   - Fallback when ISRC is absent: `GET /track/search?query="{artist} {title}"&limit=10` scored by normalized artist/title/album/duration.
   - Cache the resolved `(local_track_id → qobuz_track_id, artist_id, genre_id, label_id)` in SQLite so each local track is looked up at most once.

4. **What Qobuz Returns About Songs (and What It Doesn't)**:
   - **Included**: `audio_info.replaygain_track_gain` & `replaygain_track_peak`, `isrc`, full liner-note `performers` credits (Producer, Engineer, Session Musicians), `composer`, `work`, `label`, hierarchical `genre`, `release_date_original`, `popularity` (on albums), hi-res specs.
   - **Not included**: No BPM/tempo, musical key, energy, valence, mood tags, or sonic embeddings.
   - **No preview clips for subscribed accounts**: Verified live on `GET /track/getFileUrl` — `intent` only accepts `stream`, `download`, `import` (and requires user auth); `sample`/`preview` return `400 Invalid argument`. Bulk-fetching full Qobuz streams solely for audio analysis is impractical, so sonic embeddings are focused on the local library (matching how Plexamp's Sonic Analysis worked alongside Tidal).

### 4.2 ListenBrainz (Open Collaborative Filtering — Local-First)
Free, open, **no login required** for radio/similarity endpoints (requires descriptive `User-Agent: tacet/<version> (<url>)` and ≤1 req/sec respecting `X-RateLimit-Remaining` / `X-RateLimit-Reset-In`):

- **Session-based Collaborative Filtering (`https://labs.api.listenbrainz.org`)**:
  - `POST /similar-recordings/json` — recordings frequently played in the same listening sessions as a seed `recording_mbid`.
  - `POST /similar-artists/json` — similar artists for a seed `artist_mbid`.
- **LB Radio (`https://api.listenbrainz.org/1/...`)**:
  - `GET /1/lb-radio/artist/{artist_mbid}?mode=easy|medium|hard&pop_begin=0&pop_end=100` — built-in **Deep Cuts / Popularity dial** (`mode` + popularity percentile range).
  - `GET /1/lb-radio/tags?tag=...&operator=AND|OR&pop_begin=...&pop_end=...` — **Tag / Mood / Genre Radio**.
  - `GET /1/explore/lb-radio?prompt=...&mode=...` — prompt-based playlist generation.
- **How it powers Local-Only Radio**:
  - Seed with the local track's `recording_mbid` or `artist_mbid` (from `tacet-resolver` or file tags).
  - Intersect returned `recording_mbid` / `similar_artist_mbid` lists against local SQLite tracks — yielding real collaborative-filtering recommendations strictly over tracks the user owns.
  - Optional future tie-in: user-supplied ListenBrainz token enables scrobbling + personal CF recommendations (`GET /1/cf/recommendation/user/{user}/recording`).

### 4.3 Pure-Offline Local Fallback
When offline, when `tacet-resolver` / ListenBrainz aren't used, or when a library has niche/unmatched tracks:
- Rank local library tracks by shared `artist_mbid`/artist, overlapping `genres` (from `tacet-resolver` or file tags), era proximity (`year`), and freshness (`last_played_at` / `play_count`) while avoiding recently played tracks and back-to-back artist repetition.

---

## 5. Backend Comparison Summary

| Capability | Qobuz Native | ListenBrainz → Local | Pure-Offline Local | Future Local Sonic Analysis |
|---|---|---|---|---|
| Works without Qobuz | No | **Yes** | **Yes** | **Yes** |
| Works completely offline | No | No | **Yes** | **Yes** |
| Directly playable tracks | **Yes** (1 call + `prefer_local_tracks`) | Local MBID SQL join | **Yes** | **Yes** |
| Collaborative taste ("played next") | **Yes** (`/dynamic/suggest`, `/radio/*`) | **Yes** (`similar-recordings/artists`) | No | No |
| Deep Cuts / Popularity dial | No | **Yes** (`easy/medium/hard`, `pop_begin..pop_end`) | Play-count weighting | No |
| Credits / Label Radio | **Yes** (`performers`, `label`) | Partial | File tags | No |
| BPM / Key / Timbre / Smooth Transitions | No | No | No | **Yes** (next phase) |

---

## 6. Planned Schema & Module Outline

### SQLite Additions (`src/library/migrations.rs`)
- On `albums`: `mb_release_id`, `mb_release_group_id`, `mb_artist_id`, `upc`, `genres_json`, `resolved_at`, `resolve_confidence`.
- On `tracks`: `isrc`, `mb_recording_id`, `mb_artist_id`, `replaygain_track_gain`, `replaygain_track_peak`, `genres_json`.
- Indexes on `tracks(isrc)`, `tracks(mb_recording_id)`, `tracks(mb_artist_id)` for O(1) matching.
- Small cache table `qobuz_seed_cache(track_id PRIMARY KEY, qobuz_track_id, qobuz_artist_id, qobuz_genre_id, qobuz_label_id, updated_at)` for lazy local→Qobuz seed lookups.

### Proposed Modules
- `src/resolver/` — `tacet-resolver` HTTP client + background album/track enrichment worker (rate-limit & `429` aware).
- `src/recommend/` — `Recommender` trait + `QobuzRecommender`, `ListenBrainzRecommender`, and `OfflineLocalRecommender`, plus a unified router that respects local-only mode and applies `prefer_local_tracks()`.
- `src/queue/` — per-zone `autoplay: bool` flag + automatic top-up hook when the queue approaches its end.

---

## 7. Local Sonic Analysis (`Discogs-EffNet` + Classification Heads)

Neither Qobuz nor ListenBrainz provides per-track sonic embeddings, moods, or texture scores. Running **Essentia's `discogs-effnet-bsdynamic-1.onnx`** locally via ONNX Runtime (`ort` in Rust) gives `tacet` the complete Plexamp Sonic Analysis feature set completely offline — with zero Python/TensorFlow runtime.

### 7.1 Base Model (`discogs-effnet-bsdynamic-1.onnx`) — Verified Live
- **Download**: `https://essentia.upf.edu/models/feature-extractors/discogs-effnet/discogs-effnet-bsdynamic-1.onnx` (**17 MB**)
- **Input**: `melspectrogram` (`[batch, 128, 96]` — 16 kHz mono, 512-pt FFT, 256-pt hop, 96 Slaney mel bands `0..8 kHz`, `log10(1 + 10000 * mel)`, 128-frame patches with 64-frame hop)
- **Outputs** (in a single forward pass):
  - `embeddings` (`[batch, 1280]`) → 1280-d sonic embedding
  - `activations` (`[batch, 400]`) → 400 scored Discogs music styles (e.g. `Hip Hop---Boom Bap`, `Funk / Soul---Soul`, `Rock---Shoegaze`, `Folk, World, & Country---Folk`)
- **Benchmarked CPU speed** (10 real local tracks from `tacet` library):
  - **~350 ms per full track** (~120 ms ffmpeg/decode + ~25 ms mel + ~190 ms ONNX). Across 4–8 cores, a 5,000-track library completes in under 5 minutes.
- **Verified clustering quality** (zero tags used):
  - Correctly separated groove-driven tracks vs. sparse acoustic ballads on the *same* album (*Alanis Morissette — All I Really Want* ↔ *You Learn* at `0.776`, while acoustic *Perfect* clustered closer to *Alexi Murdoch* `0.710`).
  - Tightly clustered acoustic singer-songwriters (*Alexi Murdoch* ↔ *Amos Lee* `0.810`, *Andy Shauf* `0.704`) and separated Boom Bap / Soul / R&B cleanly.

### 7.2 Plexamp-Equivalent Features Powered Purely by the Base Model
None of the core Plex Sonic Analysis features require downstream heads:
- **Sonically Similar Tracks / Albums / Artists**: Cosine similarity over L2-normalized 1280-d embeddings (album embedding = mean of its tracks; artist embedding = mean of its albums/tracks). Brute-force scan over 10,000 tracks in Rust takes **<2 ms** in RAM (~25 MB as `f16` / ~50 MB as `f32`, no vector DB needed).
- **Track Radio & Album Radio**: Nearest sonic neighbors with artist/album spacing (Album Radio queues full sonically similar albums back-to-back).
- **Mixes For You**: Cluster recently and historically played albums in 1280-d space and blend in sonically similar neighbors.
- **Sonic Adventure (Track A → Track B)**: Shortest smooth path through the 1280-d cosine neighbor graph.
- **Style / Genre Radio**: Directly from the base model's 400 Discogs style probabilities.

### 7.3 Optional Downstream Classification Heads (All Official `.onnx`, ~0.5% Overhead)
All 10 heads below take the exact same `1280`-d embedding, have official `.onnx` exports on `essentia.upf.edu/models/classification-heads/`, and were benchmarked live on CPU:

| Head | Size | 1 track (pooled) | 200 patches (exact) | DJ / Filter Use Case |
|---|---:|---:|---:|---|
| `engagement_regression-discogs-effnet-1` | 502 KB | 0.005 ms | 0.14 ms | Lean-back background ↔ lean-forward active listening |
| `approachability_regression-discogs-effnet-1` | 502 KB | 0.011 ms | 0.21 ms | Mainstream/accessible ↔ niche/experimental |
| `voice_instrumental-discogs-effnet-1` | 502 KB | 0.008 ms | 0.17 ms | Vocals vs. instrumental (Focus / Study / Ambient DJ) |
| `danceability-discogs-effnet-1` | 502 KB | 0.007 ms | 0.16 ms | Groove / rhythm energy (97% CV accuracy) |
| `mood_relaxed-discogs-effnet-1` | 502 KB | 0.007 ms | 0.15 ms | Chill / Late-Night / Wind-Down (91% CV accuracy) |
| `mood_acoustic-discogs-effnet-1` | 502 KB | 0.014 ms | 0.16 ms | Acoustic / unplugged recordings |
| `mood_aggressive-discogs-effnet-1` | 502 KB | 0.008 ms | 0.18 ms | Intensity guardrail (keep heavy tracks out of mellow mixes) |
| `mood_electronic-discogs-effnet-1` | 502 KB | 0.008 ms | 0.14 ms | Electronic / synth texture axis |
| `mtg_jamendo_moodtheme-discogs-effnet-1` | 2.6 MB | 0.013 ms | 0.33 ms | 56 mood/theme tags (`calm`, `dark`, `dream`, `groovy`, `melancholic`, …) |
| `mtg_jamendo_instrument-discogs-effnet-1` | 2.6 MB | 0.032 ms | 0.33 ms | 40 instruments (`piano`, `cello`, `brass`, `acousticguitar`, …) |
| **ALL 10 COMBINED** | **9.11 MB** | **0.11 ms** | **2.0 ms** | **0.5% overhead on top of the 350 ms base encoder** |

### 7.4 Implementation & Blending Notes
- **Store raw (unnormalized) mean-pooled `1280`-d `f32` embeddings** (~5 KB/track) in SQLite, and L2-normalize in memory for cosine search. This preserves the exact input scale expected by downstream heads so any new head can be run over 10,000 stored tracks in ~1 second later with zero audio re-decoding.
- **Exact mel-spectrogram validation**: Validate the pure-Rust (`rustfft`) Slaney mel frontend against Essentia's reference output to `< 1e-4` error.
- **Licensing & distribution**: Essentia pretrained weights are **CC BY-NC-SA 4.0** (non-commercial). Download weights on demand into `tacet`'s data directory when the user enables Sonic Analysis rather than bundling them in the binary.
- **Blended Local + Qobuz UX (Plexamp pattern)**: Keep one unified UI (*Track Radio*, *Artist Radio*, *Album Radio*, *Continue Playing*, *Guest DJs*). Local libraries get full Sonic Analysis + ListenBrainz + offline fallback; Qobuz tracks seamlessly blend Qobuz `/radio/*`, `/dynamic/suggest`, ListenBrainz, and metadata/credits DJ rules — with `prefer_local_tracks()` preferring local copies throughout. Optional future enhancement: passively embed played Qobuz tracks via `tacet`'s audio proxy so frequently played streaming favorites join the same local embedding space over time.
