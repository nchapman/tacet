export type TrackSource = "local" | "qobuz";
export type StreamMode = "direct" | "relay";

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
  qobuzTrackId?: number;
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

export interface ZoneSnapshot {
  renderer: RendererInfo | null;
  state: string;
  positionSec: number;
  durationSec: number;
  volume: number;
  muted: boolean;
  hasVolume: boolean;
  queue: Track[];
  currentIndex: number;
  currentTrack?: Track;
  gaplessArmed: boolean;
  eventsActive: boolean;
  streamMode: StreamMode;
  activeUrl?: string;
  lastError?: string;
}

export interface RenderersResponse {
  renderers: RendererInfo[];
  activeId: string;
  streamMode: StreamMode;
}

export interface DiscoverResponse {
  discovered: number;
  renderers: RendererInfo[];
  activeId: string;
}

export interface ConnectRendererResponse {
  renderer: RendererInfo;
  renderers: RendererInfo[];
  activeId: string;
}

export interface LocalAlbumsResponse {
  roots: string[];
  albums: Album[];
}

export interface LocalTracksResponse {
  tracks: Track[];
}

export interface RescanResponse {
  tracks: number;
  albums: number;
}

export interface QobuzStatus {
  configured: boolean;
  authenticated: boolean;
  userName?: string;
  subscription?: string;
  maxFormatId: number;
}

export interface QobuzSearchResult {
  tracks: Track[];
  albums: Album[];
}

export interface QueueTrackRequest {
  rendererId?: string;
  tracks?: Track[];
  localTrackId?: string;
  localAlbumId?: string;
  qobuzAlbumId?: string;
  startIndex?: number;
}

export interface QobuzLoginRequest {
  authToken?: string;
  email?: string;
  password?: string;
  appId?: string;
  appSecret?: string;
}

export type TransportAction = "play" | "pause" | "stop" | "next" | "prev";
