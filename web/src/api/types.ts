export type TrackSource = "local" | "qobuz" | (string & {});
export type SourceType = "local" | "remote";
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

export type AudioQuality = "opus_96" | "opus_128" | "opus_160" | "opus_256" | "original" | "wav";

export interface Settings {
  streamMode: StreamMode;
  qobuzMaxQuality: QobuzQuality;
  transcodeAvailable?: boolean;
  libraryWatchEnabled?: boolean;
  libraryScanIntervalMin?: number;
}

export interface ProviderSyncStatus {
  provider: string;
  sourceType: SourceType;
  connected: boolean;
  syncing: boolean;
  albums: number;
  tracks: number;
  lastSyncAt?: string;
  lastError?: string;
}

export interface LibraryStatus {
  configured: boolean;
  scanning: boolean;
  tracks: number;
  albums: number;
  localTracks?: number;
  localAlbums?: number;
  remoteTracks?: number;
  remoteAlbums?: number;
  missingTracks?: number;
  missingAlbums?: number;
  providers?: ProviderSyncStatus[];
  etag: string;
}

export interface Track {
  id: string;
  source: TrackSource;
  sourceType?: SourceType;
  provider?: string;
  externalId?: string;
  title: string;
  artist: string;
  albumArtist?: string;
  album: string;
  albumId?: string;
  genre?: string;
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
  playCount?: number;
  lastPlayedAt?: string;
  addedAt?: string;
  loved?: boolean;
  available?: boolean;
  inLibrary?: boolean;
  explicit?: boolean;
}

export interface AlbumVersion {
  id: string;
  source: TrackSource;
  sourceType: SourceType;
  provider: string;
  title: string;
  year?: number;
  trackCount: number;
  primaryFormat?: string;
  maxSampleRate?: number;
  maxBitDepth?: number;
  isPrimary: boolean;
  available: boolean;
}

export interface AlbumCredit {
  name: string;
  roles: string[];
}

export interface Album {
  id: string;
  source: TrackSource;
  sourceType?: SourceType;
  provider?: string;
  externalId?: string;
  title: string;
  artist: string;
  genre?: string;
  label?: string;
  description?: string;
  copyright?: string;
  releaseDate?: string;
  awards?: string[];
  bookletUrl?: string;
  credits?: AlbumCredit[];
  year?: number;
  trackCount: number;
  durationSec?: number;
  artworkUrl?: string;
  primaryFormat?: string;
  maxSampleRate?: number;
  maxBitDepth?: number;
  versionGroupId?: string;
  versionCount?: number;
  isPrimary?: boolean;
  isPartial?: boolean;
  playCount?: number;
  addedAt?: string;
  available?: boolean;
  inLibrary?: boolean;
  tracks?: Track[];
  versions?: AlbumVersion[];
}

export interface ArtistSummary {
  name: string;
  albumCount: number;
  trackCount: number;
  artworkUrl?: string;
  providers: string[];
}

export interface TopSearchResult {
  type: "artist" | "album" | "track";
  score: number;
  artist?: ArtistSummary;
  album?: Album;
  track?: Track;
}

export interface LibrarySearchResult {
  query: string;
  topResult?: TopSearchResult;
  artists: ArtistSummary[];
  albums: Album[];
  tracks: Track[];
}

export interface GenreSummary {
  name: string;
  albumCount: number;
  trackCount: number;
}

export interface HeavyRotationAlbum extends Album {
  recentPlays: number;
  lastPlayed?: string;
}

export interface LibraryAlbumsQuery {
  q?: string;
  source?: "all" | "local" | "remote";
  provider?: string;
  artist?: string;
  genre?: string;
  sort?: "artist" | "title" | "year" | "added" | "plays";
  groupVersions?: boolean;
  limit?: number;
  offset?: number;
}

export interface LibraryTracksQuery {
  q?: string;
  source?: "all" | "local" | "remote";
  provider?: string;
  artist?: string;
  albumId?: string;
  album?: string;
  genre?: string;
  sort?: "album" | "title" | "artist" | "year" | "added" | "plays" | "duration" | "quality";
  dir?: "asc" | "desc";
  limit?: number;
  offset?: number;
}

export interface LibraryArtistsQuery {
  q?: string;
  source?: "all" | "local" | "remote";
  provider?: string;
  genre?: string;
  limit?: number;
  offset?: number;
}

export interface RendererInfo {
  id: string;
  kind?: "upnp" | "player" | string;
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

export type RepeatMode = "off" | "all" | "one";

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
  audioQuality?: AudioQuality;
  activeQuality?: AudioQuality;
  shuffle?: boolean;
  repeat?: RepeatMode;
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

export interface QobuzArtist {
  id: number;
  name: string;
  artworkUrl?: string;
  albumsCount?: number;
  biography?: string;
}

export interface QobuzArtistDetail {
  artist: QobuzArtist;
  albums: Album[];
  tracks?: Track[];
  similarArtists?: QobuzArtist[];
}

export interface QobuzSearchResult {
  artists: QobuzArtist[];
  tracks: Track[];
  albums: Album[];
}

export interface RegisterPlayerRequest {
  playerId?: string;
  name: string;
  model?: string;
  manufacturer?: string;
  codecs?: string[];
  hasVolume?: boolean;
  supportsSetNext?: boolean;
  supportsSeek?: boolean;
  preferRelativeMedia?: boolean;
  qobuzDelivery?: QobuzDelivery;
  gracePeriodSec?: number;
}

export interface UpdatePlayerRequest {
  name?: string;
  model?: string;
  manufacturer?: string;
  codecs?: string[];
  hasVolume?: boolean;
  supportsSetNext?: boolean;
  supportsSeek?: boolean;
  preferRelativeMedia?: boolean;
  qobuzDelivery?: QobuzDelivery;
  gracePeriodSec?: number;
}

export interface PlayerRegistration {
  zoneId: string;
  token: string;
  renderer: RendererInfo;
}

export interface PlayerMediaPayload {
  id: string;
  url: string;
  title: string;
  artist: string;
  album: string;
  artworkUrl?: string;
  mime: string;
  durationSec: number;
  sampleRate?: number;
  bitDepth?: number;
  seekable?: boolean;
  transcoded?: boolean;
}

export interface PlayerCommand {
  id: string;
  type: "set_uri" | "set_next_uri" | "play" | "pause" | "stop" | "seek" | "set_volume" | "set_mute";
  media?: PlayerMediaPayload;
  positionSec?: number;
  volume?: number;
  muted?: boolean;
}

export interface PlayerStateReport {
  cmdId?: string;
  ok?: boolean;
  errorCode?: string;
  errorMessage?: string;
  state?: PlaybackState;
  positionSec?: number;
  durationSec?: number;
  trackUri?: string;
  volume?: number;
  muted?: boolean;
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
  serverTime?: string;
  zones: ZoneSnapshot[];
  settings: Settings;
  qobuz: QobuzStatus;
  library: LibraryStatus;
}

export type QobuzDelivery = "auto" | "force" | "direct";

export interface PlaybackEvent {
  zoneId: string;
  revision: number;
  state: PlaybackState;
  positionSec: number;
  positionUpdatedAt: string;
  serverTime?: string;
  durationSec: number;
  currentIndex: number;
  currentItemId?: string;
  currentTrack?: Track;
  audioQuality?: AudioQuality;
  activeQuality?: AudioQuality;
  shuffle?: boolean;
  repeat?: RepeatMode;
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

export interface DiscoverGenre {
  id: number;
  name: string;
}

export interface DiscoverShelf {
  slug: string;
  title: string;
  subtitle?: string;
  items: Album[];
  hasMore: boolean;
}

export interface DiscoverFeed {
  connected: boolean;
  genres: DiscoverGenre[];
  shelves: DiscoverShelf[];
}

export interface DiscoverShelfPage {
  slug: string;
  title: string;
  subtitle?: string;
  items: Album[];
  hasMore: boolean;
  total?: number;
  offset: number;
  nextOffset?: number;
  limit: number;
}

export type RadioSeedKind = "track" | "album" | "artist";
export type RadioScope = "all" | "local";

export interface StartRadioRequest {
  kind: RadioSeedKind;
  id: string;
  limit?: number;
  scope?: RadioScope;
}

export interface RadioStationSummary {
  seedKind: RadioSeedKind;
  seedLabel: string;
  seedTrackId?: string;
  trackCount: number;
  sourceCounts: Record<string, number>;
}

export interface StartRadioResponse {
  zone: ZoneSnapshot;
  station: RadioStationSummary;
}

export interface RadioPreviewResponse {
  seedKind: RadioSeedKind;
  seedLabel: string;
  seedTrackId?: string;
  tracks: Track[];
  sourceCounts: Record<string, number>;
}

export interface SonicSimilarTrackItem {
  track: Track;
  score: number;
}

export interface SonicSimilarAlbumItem {
  album: Album;
  score: number;
}

export interface SonicSimilarArtistItem {
  artist: ArtistSummary;
  score: number;
}
