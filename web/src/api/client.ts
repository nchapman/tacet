import type {
  Album,
  ConnectRendererResponse,
  DiscoverResponse,
  LocalAlbumsResponse,
  LocalTracksResponse,
  QobuzLoginRequest,
  QobuzSearchResult,
  QobuzStatus,
  QueueTrackRequest,
  RendererInfo,
  RenderersResponse,
  RescanResponse,
  StreamMode,
  TransportAction,
  ZoneSnapshot,
} from "./types";

export class TacetAPIError extends Error {
  readonly status: number;

  constructor(message: string, status: number) {
    super(message);
    this.name = "TacetAPIError";
    this.status = status;
  }
}

export interface EventHandlers {
  onState?: (snap: ZoneSnapshot) => void;
  onRenderers?: (renderers: RendererInfo[]) => void;
  onOpen?: () => void;
  onError?: () => void;
}

export interface ClientOptions {
  baseUrl?: string;
  fetchImpl?: typeof fetch;
}

export class TacetClient {
  private readonly baseUrl: string;
  private readonly fetchImpl: typeof fetch;

  constructor(opts: ClientOptions = {}) {
    this.baseUrl = (opts.baseUrl ?? "").replace(/\/+$/, "");
    this.fetchImpl = opts.fetchImpl ?? ((input, init) => globalThis.fetch(input, init));
  }

  private url(path: string, query?: Record<string, string | undefined>): string {
    const base = `${this.baseUrl}${path}`;
    if (!query) return base;
    const params = new URLSearchParams();
    for (const [key, val] of Object.entries(query)) {
      if (val !== undefined && val !== "") {
        params.set(key, val);
      }
    }
    const qs = params.toString();
    return qs ? `${base}?${qs}` : base;
  }

  private async request<T>(path: string, init: RequestInit = {}, query?: Record<string, string | undefined>): Promise<T> {
    const headers = new Headers(init.headers);
    if (init.body !== undefined && !headers.has("Content-Type")) {
      headers.set("Content-Type", "application/json");
    }
    const res = await this.fetchImpl(this.url(path, query), { ...init, headers });
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
      const msg =
        typeof payload === "object" && payload !== null && "error" in payload && typeof (payload as { error: unknown }).error === "string"
          ? (payload as { error: string }).error
          : text.trim() || `HTTP ${res.status}`;
      throw new TacetAPIError(msg, res.status);
    }
    if (!text) {
      throw new TacetAPIError(`Empty response from ${path}`, res.status);
    }
    if (parseErr !== undefined) {
      throw new TacetAPIError(`Invalid JSON response from ${path}`, res.status);
    }
    return payload as T;
  }

  // Renderers & settings
  listRenderers(): Promise<RenderersResponse> {
    return this.request("/api/renderers");
  }

  discoverRenderers(): Promise<DiscoverResponse> {
    return this.request("/api/renderers/discover", { method: "POST" });
  }

  connectRenderer(url: string): Promise<ConnectRendererResponse> {
    return this.request("/api/renderers/connect", {
      method: "POST",
      body: JSON.stringify({ url }),
    });
  }

  selectRenderer(id: string): Promise<ZoneSnapshot> {
    return this.request("/api/renderers/select", {
      method: "POST",
      body: JSON.stringify({ id }),
    });
  }

  setStreamMode(mode: StreamMode): Promise<{ streamMode: StreamMode }> {
    return this.request("/api/settings/stream-mode", {
      method: "POST",
      body: JSON.stringify({ mode }),
    });
  }

  // State & live SSE stream
  getState(rendererId?: string): Promise<ZoneSnapshot> {
    return this.request("/api/state", {}, { renderer: rendererId });
  }

  subscribeEvents(handlers: EventHandlers): () => void {
    const es = new EventSource(this.url("/api/events"));
    es.onopen = () => handlers.onOpen?.();
    es.onerror = () => handlers.onError?.();
    es.addEventListener("state", (ev) => {
      try {
        handlers.onState?.(JSON.parse((ev as MessageEvent<string>).data) as ZoneSnapshot);
      } catch {
        // Ignore malformed frames
      }
    });
    es.addEventListener("renderers", (ev) => {
      try {
        handlers.onRenderers?.(JSON.parse((ev as MessageEvent<string>).data) as RendererInfo[]);
      } catch {
        // Ignore malformed frames
      }
    });
    return () => es.close();
  }

  // Queue & transport
  playQueue(req: QueueTrackRequest): Promise<ZoneSnapshot> {
    return this.request("/api/queue/play", {
      method: "POST",
      body: JSON.stringify(req),
    });
  }

  enqueue(req: QueueTrackRequest): Promise<ZoneSnapshot> {
    return this.request("/api/queue/enqueue", {
      method: "POST",
      body: JSON.stringify(req),
    });
  }

  jumpQueue(index: number, rendererId?: string): Promise<ZoneSnapshot> {
    return this.request("/api/queue/jump", {
      method: "POST",
      body: JSON.stringify({ rendererId, index }),
    });
  }

  removeFromQueue(index: number, rendererId?: string): Promise<ZoneSnapshot> {
    return this.request("/api/queue/remove", {
      method: "POST",
      body: JSON.stringify({ rendererId, index }),
    });
  }

  clearQueue(rendererId?: string): Promise<ZoneSnapshot> {
    return this.request("/api/queue/clear", { method: "POST" }, { renderer: rendererId });
  }

  transport(action: TransportAction, rendererId?: string): Promise<ZoneSnapshot> {
    return this.request(`/api/transport/${action}`, { method: "POST" }, { renderer: rendererId });
  }

  seek(positionSec: number, rendererId?: string): Promise<ZoneSnapshot> {
    return this.request("/api/transport/seek", {
      method: "POST",
      body: JSON.stringify({ rendererId, positionSec: Math.max(0, Math.round(positionSec)) }),
    });
  }

  setVolume(volume: number, rendererId?: string): Promise<ZoneSnapshot> {
    return this.request("/api/transport/volume", {
      method: "POST",
      body: JSON.stringify({ rendererId, volume: Math.max(0, Math.min(100, Math.round(volume))) }),
    });
  }

  setMute(muted: boolean, rendererId?: string): Promise<ZoneSnapshot> {
    return this.request("/api/transport/mute", {
      method: "POST",
      body: JSON.stringify({ rendererId, muted }),
    });
  }

  // Local NAS library
  listLocalAlbums(): Promise<LocalAlbumsResponse> {
    return this.request("/api/library/albums");
  }

  getLocalAlbum(id: string): Promise<Album> {
    return this.request(`/api/library/albums/${encodeURIComponent(id)}`);
  }

  searchLocalTracks(query?: string): Promise<LocalTracksResponse> {
    return this.request("/api/library/tracks", {}, { q: query });
  }

  rescanLibrary(): Promise<RescanResponse> {
    return this.request("/api/library/rescan", { method: "POST" });
  }

  // Qobuz
  qobuzOAuthStartURL(): string {
    return this.url("/api/qobuz/oauth/start");
  }

  qobuzStatus(): Promise<QobuzStatus> {
    return this.request("/api/qobuz/status");
  }

  qobuzLogin(req: QobuzLoginRequest): Promise<QobuzStatus> {
    return this.request("/api/qobuz/login", {
      method: "POST",
      body: JSON.stringify(req),
    });
  }

  qobuzSearch(query: string): Promise<QobuzSearchResult> {
    return this.request("/api/qobuz/search", {}, { q: query });
  }

  qobuzAlbum(id: string): Promise<Album> {
    return this.request(`/api/qobuz/albums/${encodeURIComponent(id)}`);
  }
}

export const tacetClient = new TacetClient();
