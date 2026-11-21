export type TrackSource = "local" | "qobuz";
export type StreamMode = "direct" | "relay";
export type QobuzQuality = "mp3_320" | "flac_16_44" | "flac_24_96" | "flac_24_192";
export type PlaybackState = "idle" | "playing" | "paused" | "buffering" | "stopped";
export type QueuePosition = "end" | "next";
export type TransportAction = "play" | "pause" | "stop" | "next" | "prev";

export interface APIErrorBody {
  code: string;
  message: string;
  retryable: boolean;
  details?: Record<string, unknown>;
}

export interface ServerInfo {
  name: string;
  version: string;
  apiVersion: number;
  capabilities: {
    localLibrary: boolean;
    qobuz: boolean;
    gena: boolean;
  };
}

export interface Settings {
  streamMode: StreamMode;
  qobuzMaxQuality: QobuzQuality;
}

export interface LibraryStatus {
  configured: boolean;
  scanning: boolean;
  tracks: number;
  albums: number;
  etag: string;
}

export interface Track {
  id: string;
  source: TrackSource;
  title: string;
  artist: string;
  albumArtist?: string;
  album: string;
  albumId?: string;
  trackNumber?: number;
  discNumber?: number;
  year?: number;
  durationSec: number;
  sampleRate?: number;
  bitDepth?: number;
  channels?: number;
  mime: string;
  ext: string;
  size?: number;
  artworkUrl?: string;
}

export interface Album {
  id: string;
  source: TrackSource;
  title: string;
  artist: string;
  year?: number;
  trackCount: number;
  artworkUrl?: string;
  maxSampleRate?: number;
  maxBitDepth?: number;
  tracks?: Track[];
}

export interface RendererInfo {
  id: string;
  name: string;
  manufacturer?: string;
  model?: string;
  location: string;
  host: string;
  hasVolume: boolean;
  supportsSetNext: boolean;
  supportsSeek: boolean;
  supportsEvents: boolean;
  sinkProtocols?: string[];
}

export interface QueueItem {
  itemId: string;
  track: Track;
}

export interface ZoneSnapshot {
  zoneId: string;
  renderer: RendererInfo;
  revision: number;
  state: PlaybackState;
  positionSec: number;
  positionUpdatedAt: string;
  durationSec: number;
  volume: number;
  muted: boolean;
  queue: QueueItem[];
  currentIndex: number;
  currentItemId?: string;
  currentTrack?: Track;
  gaplessArmed: boolean;
  eventsActive: boolean;
}

export type QueueSource =
  | { type: "album"; albumId: string }
  | { type: "tracks"; trackIds: string[] };

export interface ZonesResponse {
  zones: ZoneSnapshot[];
}

export interface DiscoverZonesResponse {
  discovered: number;
  zones: ZoneSnapshot[];
}

export interface ConnectZoneResponse {
  zone: ZoneSnapshot;
  zones: ZoneSnapshot[];
}

export interface PagedResponse<T> {
  items: T[];
  total: number;
  limit: number;
  offset: number;
}

export interface QobuzStatus {
  configured: boolean;
  authenticated: boolean;
  userName?: string;
  subscription?: string;
  maxFormatId: number;
  maxQuality: QobuzQuality;
}

export interface QobuzSearchResult {
  tracks: Track[];
  albums: Album[];
}

export interface QobuzLoginRequest {
  authToken?: string;
  email?: string;
  password?: string;
  appId?: string;
  appSecret?: string;
}

// Split SSE event payloads
export interface SnapshotEvent {
  zones: ZoneSnapshot[];
  settings: Settings;
  qobuz: QobuzStatus;
  library: LibraryStatus;
}

export interface PlaybackEvent {
  zoneId: string;
  revision: number;
  state: PlaybackState;
  positionSec: number;
  positionUpdatedAt: string;
  durationSec: number;
  currentIndex: number;
  currentItemId?: string;
  currentTrack?: Track;
  gaplessArmed: boolean;
  eventsActive: boolean;
}

export interface VolumeEvent {
  zoneId: string;
  revision: number;
  volume: number;
  muted: boolean;
}

export interface QueueEvent {
  zoneId: string;
  revision: number;
  currentIndex: number;
  currentItemId?: string;
  currentTrack?: Track;
  items: QueueItem[];
}

export interface ZoneErrorEvent {
  id: string;
  zoneId: string;
  error: APIErrorBody;
  occurredAt: string;
}

export interface ZonesUpdatedEvent {
  zones: ZoneSnapshot[];
}
