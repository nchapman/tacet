import type { RendererInfo, StreamMode } from "../api/types";
import type { ConnectionStatus } from "../hooks/useTacet";

interface HeaderProps {
  renderers: RendererInfo[];
  activeId: string;
  streamMode: StreamMode;
  connection: ConnectionStatus;
  discovering: boolean;
  onSelectRenderer: (id: string) => void;
  onDiscover: () => void;
  onOpenConnect: () => void;
  onStreamModeChange: (mode: StreamMode) => void;
}

export function Header({
  renderers,
  activeId,
  streamMode,
  connection,
  discovering,
  onSelectRenderer,
  onDiscover,
  onOpenConnect,
  onStreamModeChange,
}: HeaderProps) {
  return (
    <header className="app-header">
      <div className="brand-group">
        <div className="brand">
          tacet <span className="brand-sub">UPnP / DLNA Control Point</span>
        </div>
        <span className={`conn-pill conn-${connection}`} title={`Event stream: ${connection}`}>
          <span className="conn-dot" />
          {connection === "live" ? "Live" : connection === "reconnecting" ? "Reconnecting" : "Connecting"}
        </span>
      </div>

      <div className="toolbar">
        <select
          aria-label="Active UPnP Renderer"
          value={activeId}
          onChange={(e) => onSelectRenderer(e.target.value)}
          disabled={renderers.length === 0}
        >
          {renderers.length === 0 ? (
            <option value="">No renderers found</option>
          ) : (
            renderers.map((r) => (
              <option key={r.id} value={r.id}>
                {r.name}
                {r.host ? ` (${r.host})` : ""}
              </option>
            ))
          )}
        </select>

        <button type="button" onClick={onDiscover} disabled={discovering}>
          {discovering ? "Scanning…" : "Discover"}
        </button>

        <button type="button" onClick={onOpenConnect}>
          + URL
        </button>

        <select
          aria-label="Qobuz Delivery Mode"
          value={streamMode}
          onChange={(e) => onStreamModeChange(e.target.value as StreamMode)}
        >
          <option value="direct">Qobuz: Direct (CDN → Renderer)</option>
          <option value="relay">Qobuz: Quiet Relay (Plain HTTP)</option>
        </select>
      </div>
    </header>
  );
}
