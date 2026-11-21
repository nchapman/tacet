import type {
  APIErrorBody,
  Album,
  ArtistSummary,
  ConnectZoneResponse,
  DiscoverZonesResponse,
  GenreSummary,
  HeavyRotationAlbum,
  LibraryAlbumsQuery,
  LibraryArtistsQuery,
  LibraryStatus,
  LibraryTracksQuery,
  PagedResponse,
  PlaybackEvent,
  PlayerCommand,
  PlayerRegistration,
  PlayerStateReport,
  QobuzLoginRequest,
  QobuzQuality,
  QobuzSearchResult,
  QobuzStatus,
  QueueEvent,
  QueuePosition,
  QueueSource,
  RegisterPlayerRequest,
  ServerInfo,
  Settings,
  SnapshotEvent,
  StreamMode,
  Track,
  TransportAction,
  UpdatePlayerRequest,
  VolumeEvent,
  ZoneErrorEvent,
  ZoneSnapshot,
  ZonesResponse,
  ZonesUpdatedEvent,
} from "./types";

export class TacetAPIError extends Error {
  readonly status: number;
  readonly code: string;
  readonly retryable: boolean;
  readonly details?: Record<string, unknown>;

  constructor(status: number, body: APIErrorBody) {
    super(body.message);
    this.name = "TacetAPIError";
    this.status = status;
    this.code = body.code;
    this.retryable = body.retryable;
    this.details = body.details;
  }
}

export interface EventHandlers {
  onSnapshot?: (ev: SnapshotEvent) => void;
  onPlayback?: (ev: PlaybackEvent) => void;
  onVolume?: (ev: VolumeEvent) => void;
  onQueue?: (ev: QueueEvent) => void;
  onZoneError?: (ev: ZoneErrorEvent) => void;
  onZonesUpdated?: (ev: ZonesUpdatedEvent) => void;
  onSettingsUpdated?: (ev: Settings) => void;
  onQobuzStatus?: (ev: QobuzStatus) => void;
  onLibraryUpdated?: (ev: LibraryStatus) => void;
  onResync?: () => void;
  onOpen?: () => void;
  onError?: () => void;
}

export interface RequestOptions {
  signal?: AbortSignal;
  timeoutMs?: number;
}

export interface ClientOptions {
  baseUrl?: string;
  fetchImpl?: typeof fetch;
  eventSourceImpl?: typeof EventSource;
  defaultTimeoutMs?: number;
}

export interface PageQuery {
  q?: string;
  sort?: "artist" | "title" | "year";
  limit?: number;
  offset?: number;
}

export class ZoneClient {
  private readonly client: TacetClient;
  readonly zoneId: string;

  constructor(client: TacetClient, zoneId: string) {
    this.client = client;
    this.zoneId = zoneId;
  }

  private zonePath(suffix = ""): string {
    return `/api/v1/zones/${encodeURIComponent(this.zoneId)}${suffix}`;
  }

  get(opts?: RequestOptions): Promise<ZoneSnapshot> {
    return this.client.request(this.zonePath(), {}, undefined, opts);
  }

  play(source: QueueSource, startIndex = 0, opts?: RequestOptions): Promise<ZoneSnapshot> {
    return this.client.request(
      this.zonePath("/queue"),
      { method: "PUT", body: JSON.stringify({ source, startIndex }) },
      undefined,
      opts,
    );
  }

  addItems(
    source: QueueSource,
    params: { position?: QueuePosition; autoplay?: boolean } = {},
    opts?: RequestOptions,
  ): Promise<ZoneSnapshot> {
    return this.client.request(
      this.zonePath("/queue/items"),
      {
        method: "POST",
        body: JSON.stringify({
          source,
          position: params.position ?? "end",
          autoplay: params.autoplay ?? true,
        }),
      },
      undefined,
      opts,
    );
  }

  jump(itemId: string, opts?: RequestOptions): Promise<ZoneSnapshot> {
    return this.client.request(
      this.zonePath("/queue/jump"),
      { method: "POST", body: JSON.stringify({ itemId }) },
      undefined,
      opts,
    );
  }

  removeItem(itemId: string, opts?: RequestOptions): Promise<ZoneSnapshot> {
    return this.client.request(
      this.zonePath(`/queue/items/${encodeURIComponent(itemId)}`),
      { method: "DELETE" },
      undefined,
      opts,
    );
  }

  moveItem(itemId: string, toIndex: number, opts?: RequestOptions): Promise<ZoneSnapshot> {
    return this.client.request(
      this.zonePath(`/queue/items/${encodeURIComponent(itemId)}/move`),
      { method: "POST", body: JSON.stringify({ toIndex }) },
      undefined,
      opts,
    );
  }

  clearQueue(opts?: RequestOptions): Promise<ZoneSnapshot> {
    return this.client.request(this.zonePath("/queue"), { method: "DELETE" }, undefined, opts);
  }

  transport(action: TransportAction, opts?: RequestOptions): Promise<ZoneSnapshot> {
    return this.client.request(this.zonePath(`/transport/${action}`), { method: "POST" }, undefined, opts);
  }

  seek(positionSec: number, opts?: RequestOptions): Promise<ZoneSnapshot> {
    return this.client.request(
      this.zonePath("/transport/seek"),
      { method: "POST", body: JSON.stringify({ positionSec: Math.max(0, Math.round(positionSec)) }) },
      undefined,
      opts,
    );
  }

  setVolume(volume: number, opts?: RequestOptions): Promise<ZoneSnapshot> {
    const clamped = Math.max(0, Math.min(100, Math.round(volume)));
    return this.client.request(
      this.zonePath("/volume"),
      { method: "PUT", body: JSON.stringify({ volume: clamped }) },
      undefined,
      opts,
    );
  }

  setMute(muted: boolean, opts?: RequestOptions): Promise<ZoneSnapshot> {
    return this.client.request(
      this.zonePath("/mute"),
      { method: "PUT", body: JSON.stringify({ muted }) },
      undefined,
      opts,
    );
  }
}

export class TacetClient {
  private readonly baseUrl: string;
  private readonly fetchImpl: typeof fetch;
  private readonly eventSourceImpl?: typeof EventSource;
  private readonly defaultTimeoutMs?: number;

  constructor(opts: ClientOptions = {}) {
    this.baseUrl = (opts.baseUrl ?? "").replace(/\/+$/, "");
    this.fetchImpl = opts.fetchImpl ?? ((input, init) => globalThis.fetch(input, init));
    this.eventSourceImpl = opts.eventSourceImpl;
    this.defaultTimeoutMs = opts.defaultTimeoutMs;
  }

  /**
   * Resolves a server-relative media/artwork path (e.g. "/media/art/local:...")
   * against baseUrl while leaving absolute http(s) URLs untouched.
   */
  resolveUrl(pathOrUrl?: string): string {
    if (!pathOrUrl) return "";
    if (/^https?:\/\//i.test(pathOrUrl)) return pathOrUrl;
    if (!this.baseUrl) return pathOrUrl;
    return `${this.baseUrl}${pathOrUrl.startsWith("/") ? "" : "/"}${pathOrUrl}`;
  }

  /** Returns a scoped facade for controlling a specific zone. */
  zone(zoneId: string): ZoneClient {
    return new ZoneClient(this, zoneId);
  }

  private url(path: string, query?: Record<string, string | string[] | number | undefined>): string {
    const base = `${this.baseUrl}${path}`;
    if (!query) return base;
    const params = new URLSearchParams();
    for (const [key, val] of Object.entries(query)) {
      if (val === undefined || val === "") continue;
      if (Array.isArray(val)) {
        for (const item of val) {
          if (item !== "") params.append(key, item);
        }
      } else {
        params.set(key, String(val));
      }
    }
    const qs = params.toString();
    return qs ? `${base}?${qs}` : base;
  }

  async request<T>(
    path: string,
    init: RequestInit = {},
    query?: Record<string, string | string[] | number | undefined>,
    opts?: RequestOptions,
  ): Promise<T> {
    const headers = new Headers(init.headers);
    if (init.body !== undefined && !headers.has("Content-Type")) {
      headers.set("Content-Type", "application/json");
    }

    const timeoutMs = opts?.timeoutMs ?? this.defaultTimeoutMs;
    let timeoutCtrl: AbortController | undefined;
    let timer: ReturnType<typeof setTimeout> | undefined;
    let signal = opts?.signal;

    if (timeoutMs && timeoutMs > 0) {
      timeoutCtrl = new AbortController();
      timer = setTimeout(() => timeoutCtrl?.abort(new Error(`Request timed out after ${timeoutMs}ms`)), timeoutMs);
      signal = signal ? AbortSignal.any([signal, timeoutCtrl.signal]) : timeoutCtrl.signal;
    }

    try {
      const res = await this.fetchImpl(this.url(path, query), { ...init, headers, signal });
      const text = await res.text();
      let payload: unknown = undefined;
      let parseErr: unknown = undefined;
      if (text) {
        try {
          payload = JSON.parse(text);
        } catch (err) {
          parseErr = err;
        }
      }
      if (!res.ok) {
        throw parseAPIError(res.status, payload, text);
      }
      if (res.status === 204) {
        return undefined as T;
      }
      if (!text) {
        throw new TacetAPIError(res.status, {
          code: "INVALID_RESPONSE",
          message: `Empty response from ${path}`,
          retryable: true,
        });
      }
      if (parseErr !== undefined) {
        throw new TacetAPIError(res.status, {
          code: "INVALID_RESPONSE",
          message: `Invalid JSON response from ${path}`,
          retryable: true,
        });
      }
      return payload as T;
    } finally {
      if (timer !== undefined) clearTimeout(timer);
    }
  }

  // System & Settings
  info(opts?: RequestOptions): Promise<ServerInfo> {
    return this.request("/api/v1/info", {}, undefined, opts);
  }

  getSettings(opts?: RequestOptions): Promise<Settings> {
    return this.request("/api/v1/settings", {}, undefined, opts);
  }

  patchSettings(
    patch: { streamMode?: StreamMode; qobuzMaxQuality?: QobuzQuality },
    opts?: RequestOptions,
  ): Promise<Settings> {
    return this.request("/api/v1/settings", { method: "PATCH", body: JSON.stringify(patch) }, undefined, opts);
  }

  // Zones & discovery
  listZones(opts?: RequestOptions): Promise<ZonesResponse> {
    return this.request("/api/v1/zones", {}, undefined, opts);
  }

  discoverZones(opts?: RequestOptions): Promise<DiscoverZonesResponse> {
    return this.request("/api/v1/zones/discover", { method: "POST" }, undefined, opts);
  }

  connectZone(url: string, opts?: RequestOptions): Promise<ConnectZoneResponse> {
    return this.request("/api/v1/zones/connect", { method: "POST", body: JSON.stringify({ url }) }, undefined, opts);
  }

  removeZone(zoneId: string, opts?: RequestOptions): Promise<{ removed: boolean; zoneId: string }> {
    return this.request(`/api/v1/zones/${encodeURIComponent(zoneId)}`, { method: "DELETE" }, undefined, opts);
  }

  // Live SSE stream
  subscribeEvents(handlers: EventHandlers, zoneIds?: string[]): () => void {
    const ES = this.eventSourceImpl ?? globalThis.EventSource;
    const es = new ES(this.url("/api/v1/events", zoneIds?.length ? { zone: zoneIds } : undefined));
    es.onopen = () => handlers.onOpen?.();
    es.onerror = () => handlers.onError?.();

    const bind = <T>(eventName: string, cb?: (data: T) => void) => {
      if (!cb) return;
      es.addEventListener(eventName, (ev) => {
        try {
          cb(JSON.parse((ev as MessageEvent<string>).data) as T);
        } catch {
          // Malformed SSE frame; request resync
          handlers.onResync?.();
        }
      });
    };

    bind<SnapshotEvent>("snapshot", handlers.onSnapshot);
    bind<PlaybackEvent>("zone.playback", handlers.onPlayback);
    bind<VolumeEvent>("zone.volume", handlers.onVolume);
    bind<QueueEvent>("zone.queue", handlers.onQueue);
    bind<ZoneErrorEvent>("zone.error", handlers.onZoneError);
    bind<ZonesUpdatedEvent>("zones.updated", handlers.onZonesUpdated);
    bind<Settings>("settings.updated", handlers.onSettingsUpdated);
    bind<QobuzStatus>("qobuz.status", handlers.onQobuzStatus);
    bind<LibraryStatus>("library.updated", handlers.onLibraryUpdated);
    es.addEventListener("resync", () => handlers.onResync?.());

    return () => es.close();
  }

  // Remote HTTP/SSE audio player endpoints

  registerPlayer(
    body: RegisterPlayerRequest,
    token?: string,
    opts?: RequestOptions,
  ): Promise<PlayerRegistration> {
    const headers: Record<string, string> = {};
    if (token) headers.Authorization = `Bearer ${token}`;
    return this.request<PlayerRegistration>(
      "/api/v1/players",
      {
        method: "POST",
        headers,
        body: JSON.stringify(body),
      },
      undefined,
      opts,
    );
  }

  updatePlayer(
    zoneId: string,
    body: UpdatePlayerRequest,
    token?: string,
    opts?: RequestOptions,
  ): Promise<{ zoneId: string; renderer: PlayerRegistration["renderer"] }> {
    const headers: Record<string, string> = {};
    if (token) headers.Authorization = `Bearer ${token}`;
    return this.request<{ zoneId: string; renderer: PlayerRegistration["renderer"] }>(
      `/api/v1/players/${encodeURIComponent(zoneId)}`,
      {
        method: "PATCH",
        headers,
        body: JSON.stringify(body),
      },
      undefined,
      opts,
    );
  }

  reportPlayerState(
    zoneId: string,
    report: PlayerStateReport,
    token?: string,
    opts?: RequestOptions,
  ): Promise<void> {
    const headers: Record<string, string> = {};
    if (token) headers.Authorization = `Bearer ${token}`;
    return this.request<void>(
      `/api/v1/players/${encodeURIComponent(zoneId)}/state`,
      {
        method: "POST",
        headers,
        body: JSON.stringify(report),
      },
      undefined,
      opts,
    );
  }

  unregisterPlayer(
    zoneId: string,
    token?: string,
    opts?: RequestOptions,
  ): Promise<{ unregistered: boolean; zoneId: string }> {
    const headers: Record<string, string> = {};
    if (token) headers.Authorization = `Bearer ${token}`;
    return this.request<{ unregistered: boolean; zoneId: string }>(
      `/api/v1/players/${encodeURIComponent(zoneId)}`,
      { method: "DELETE", headers },
      undefined,
      opts,
    );
  }

  subscribePlayerCommands(
    zoneId: string,
    handlers: {
      onConnected?: (info: { zoneId: string; streamId: number }) => void;
      onCommand?: (cmd: PlayerCommand) => void;
      onReplaced?: (reason: string) => void;
      onError?: () => void;
    },
  ): () => void {
    const Ctor = this.eventSourceImpl ?? globalThis.EventSource;
    const url = this.url(`/api/v1/players/${encodeURIComponent(zoneId)}/commands`);
    const es = new Ctor(url, { withCredentials: true });
    const parse = <T,>(ev: Event): T | null => {
      try {
        return JSON.parse((ev as MessageEvent<string>).data) as T;
      } catch {
        return null;
      }
    };
    es.addEventListener("connected", (ev: Event) => {
      const data = parse<{ zoneId: string; streamId: number }>(ev);
      if (data) handlers.onConnected?.(data);
    });
    es.addEventListener("command", (ev: Event) => {
      const cmd = parse<PlayerCommand>(ev);
      if (cmd) handlers.onCommand?.(cmd);
    });
    es.addEventListener("replaced", (ev: Event) => {
      const data = parse<{ reason?: string }>(ev);
      handlers.onReplaced?.(data?.reason ?? "replaced");
      es.close();
    });
    es.onerror = () => handlers.onError?.();
    return () => es.close();
  }

  // Unified multi-source library (local NAS + remote providers)
  libraryStatus(opts?: RequestOptions): Promise<LibraryStatus> {
    return this.request("/api/v1/library/status", {}, undefined, opts);
  }

  listLibraryAlbums(query: LibraryAlbumsQuery = {}, opts?: RequestOptions): Promise<PagedResponse<Album>> {
    const params: Record<string, string | number | undefined> = {
      q: query.q,
      source: query.source && query.source !== "all" ? query.source : undefined,
      provider: query.provider,
      artist: query.artist,
      genre: query.genre,
      sort: query.sort,
      groupVersions: query.groupVersions ? "true" : undefined,
      limit: query.limit,
      offset: query.offset,
    };
    return this.request("/api/v1/library/albums", {}, params, opts);
  }

  listLocalAlbums(query: PageQuery = {}, opts?: RequestOptions): Promise<PagedResponse<Album>> {
    return this.request("/api/v1/library/albums", {}, { ...query }, opts);
  }

  getLibraryAlbum(id: string, opts?: RequestOptions): Promise<Album> {
    return this.request(`/api/v1/library/albums/${encodeURIComponent(id)}`, {}, undefined, opts);
  }

  getLocalAlbum(id: string, opts?: RequestOptions): Promise<Album> {
    return this.getLibraryAlbum(id, opts);
  }

  listLibraryTracks(query: LibraryTracksQuery = {}, opts?: RequestOptions): Promise<PagedResponse<Track>> {
    const params: Record<string, string | number | undefined> = {
      q: query.q,
      source: query.source && query.source !== "all" ? query.source : undefined,
      provider: query.provider,
      artist: query.artist,
      albumId: query.albumId,
      album: query.album,
      genre: query.genre,
      sort: query.sort,
      dir: query.dir,
      limit: query.limit,
      offset: query.offset,
    };
    return this.request("/api/v1/library/tracks", {}, params, opts);
  }

  listLocalTracks(query: Pick<PageQuery, "q" | "limit" | "offset"> = {}, opts?: RequestOptions): Promise<PagedResponse<Track>> {
    return this.request("/api/v1/library/tracks", {}, { ...query }, opts);
  }

  listLibraryArtists(query: LibraryArtistsQuery = {}, opts?: RequestOptions): Promise<PagedResponse<ArtistSummary>> {
    const params: Record<string, string | number | undefined> = {
      q: query.q,
      source: query.source && query.source !== "all" ? query.source : undefined,
      provider: query.provider,
      genre: query.genre,
      limit: query.limit,
      offset: query.offset,
    };
    return this.request("/api/v1/library/artists", {}, params, opts);
  }

  listLibraryGenres(
    query: Pick<LibraryArtistsQuery, "source" | "provider"> = {},
    opts?: RequestOptions,
  ): Promise<{ items: GenreSummary[] }> {
    const params: Record<string, string | number | undefined> = {
      source: query.source && query.source !== "all" ? query.source : undefined,
      provider: query.provider,
    };
    return this.request("/api/v1/library/genres", {}, params, opts);
  }

  getHeavyRotation(
    days = 30,
    limit = 20,
    opts?: RequestOptions,
  ): Promise<{ days: number; items: HeavyRotationAlbum[] }> {
    return this.request("/api/v1/library/heavy-rotation", {}, { days, limit }, opts);
  }

  rescanLibrary(wait = false, opts?: RequestOptions): Promise<LibraryStatus> {
    return this.request("/api/v1/library/rescan", { method: "POST" }, wait ? { wait: "true" } : undefined, opts);
  }

  syncLibrary(provider = "all", wait = false, opts?: RequestOptions): Promise<LibraryStatus> {
    return this.request(
      "/api/v1/library/sync",
      { method: "POST" },
      { provider, ...(wait ? { wait: "true" } : {}) },
      opts,
    );
  }

  addRemoteAlbum(provider: string, externalId: string, opts?: RequestOptions): Promise<Album> {
    return this.request(
      "/api/v1/library/remote/albums",
      { method: "POST", body: JSON.stringify({ provider, externalId }) },
      undefined,
      opts,
    );
  }

  removeRemoteAlbum(id: string, opts?: RequestOptions): Promise<{ removed: boolean; id: string }> {
    return this.request(`/api/v1/library/remote/albums/${encodeURIComponent(id)}`, { method: "DELETE" }, undefined, opts);
  }

  addRemoteTrack(provider: string, externalId: string, opts?: RequestOptions): Promise<Track> {
    return this.request(
      "/api/v1/library/remote/tracks",
      { method: "POST", body: JSON.stringify({ provider, externalId }) },
      undefined,
      opts,
    );
  }

  removeRemoteTrack(id: string, opts?: RequestOptions): Promise<{ removed: boolean; id: string }> {
    return this.request(`/api/v1/library/remote/tracks/${encodeURIComponent(id)}`, { method: "DELETE" }, undefined, opts);
  }

  setPrimaryVersion(albumId: string, opts?: RequestOptions): Promise<Album> {
    return this.request(`/api/v1/library/albums/${encodeURIComponent(albumId)}/primary`, { method: "POST" }, undefined, opts);
  }

  groupVersions(albumIds: string[], opts?: RequestOptions): Promise<{ versionGroupId: string }> {
    return this.request(
      "/api/v1/library/versions/group",
      { method: "POST", body: JSON.stringify({ albumIds }) },
      undefined,
      opts,
    );
  }

  ungroupVersion(albumId: string, opts?: RequestOptions): Promise<Album> {
    return this.request(
      `/api/v1/library/albums/${encodeURIComponent(albumId)}/version-group`,
      { method: "DELETE" },
      undefined,
      opts,
    );
  }

  // Qobuz
  qobuzOAuthStartURL(): string {
    return this.url("/api/v1/qobuz/oauth/start");
  }

  qobuzStatus(opts?: RequestOptions): Promise<QobuzStatus> {
    return this.request("/api/v1/qobuz/status", {}, undefined, opts);
  }

  qobuzLogin(req: QobuzLoginRequest, opts?: RequestOptions): Promise<QobuzStatus> {
    return this.request("/api/v1/qobuz/login", { method: "POST", body: JSON.stringify(req) }, undefined, opts);
  }

  qobuzLogout(opts?: RequestOptions): Promise<QobuzStatus> {
    return this.request("/api/v1/qobuz/session", { method: "DELETE" }, undefined, opts);
  }

  qobuzSearch(
    query: string,
    paging: { limit?: number; offset?: number } = {},
    opts?: RequestOptions,
  ): Promise<QobuzSearchResult> {
    return this.request("/api/v1/qobuz/search", {}, { q: query, ...paging }, opts);
  }

  qobuzAlbum(id: string, opts?: RequestOptions): Promise<Album> {
    return this.request(`/api/v1/qobuz/albums/${encodeURIComponent(id)}`, {}, undefined, opts);
  }
}

function parseAPIError(status: number, payload: unknown, rawText: string): TacetAPIError {
  if (typeof payload === "object" && payload !== null && "error" in payload) {
    const errVal = (payload as { error: unknown }).error;
    if (typeof errVal === "object" && errVal !== null && "message" in errVal) {
      const e = errVal as Partial<APIErrorBody>;
      return new TacetAPIError(status, {
        code: typeof e.code === "string" ? e.code : `HTTP_${status}`,
        message: typeof e.message === "string" ? e.message : `HTTP ${status}`,
        retryable: Boolean(e.retryable),
        details: e.details,
      });
    }
    if (typeof errVal === "string") {
      return new TacetAPIError(status, { code: `HTTP_${status}`, message: errVal, retryable: status >= 500 });
    }
  }
  return new TacetAPIError(status, {
    code: `HTTP_${status}`,
    message: rawText.trim() || `HTTP ${status}`,
    retryable: status >= 500,
  });
}

export const tacetClient = new TacetClient();
