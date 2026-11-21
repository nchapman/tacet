import type { StreamMode, ZoneSnapshot } from "../api/types";
import type { ConnectionStatus } from "../hooks/useTacet";

interface HeaderProps {
  zones: ZoneSnapshot[];
  activeZoneId: string;
  streamMode: StreamMode;
  connection: ConnectionStatus;
  discovering: boolean;
  onSelectZone: (zoneId: string) => void;
  onDiscover: () => void;
  onOpenConnect: () => void;
  onStreamModeChange: (mode: StreamMode) => void;
}

export function Header({
  zones,
  activeZoneId,
  streamMode,
  connection,
  discovering,
  onSelectZone,
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
          value={activeZoneId}
          onChange={(e) => onSelectZone(e.target.value)}
          disabled={zones.length === 0}
        >
          {zones.length === 0 ? (
            <option value="">No renderers found</option>
          ) : (
            zones.map((z) => (
              <option key={z.zoneId} value={z.zoneId}>
                {z.state === "playing" ? "▶ " : ""}
                {z.renderer.name}
                {z.renderer.host ? ` (${z.renderer.host})` : ""}
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
