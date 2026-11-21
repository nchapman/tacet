import type { StreamMode, ZoneSnapshot } from "../api/types";
import type { BrowserPlayerController } from "../hooks/useBrowserPlayer";
import type { ConnectionStatus } from "../hooks/useTacet";

interface HeaderProps {
  zones: ZoneSnapshot[];
  activeZoneId: string;
  streamMode: StreamMode;
  connection: ConnectionStatus;
  discovering: boolean;
  browserPlayer: BrowserPlayerController;
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
  browserPlayer,
  onSelectZone,
  onDiscover,
  onOpenConnect,
  onStreamModeChange,
}: HeaderProps) {
  const handleToggleBrowserPlayer = () => {
    if (browserPlayer.enabled) {
      void browserPlayer.disable();
    } else {
      void browserPlayer.enable(true);
    }
  };

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
          aria-label="Active Audio Zone"
          value={activeZoneId}
          onChange={(e) => onSelectZone(e.target.value)}
          disabled={zones.length === 0}
        >
          {zones.length === 0 ? (
            <option value="">No renderers found</option>
          ) : (
            zones.map((z) => {
              const icon = z.renderer.kind === "player" ? "🎧 " : "";
              const isSelf = browserPlayer.zoneId === z.zoneId;
              return (
                <option key={z.zoneId} value={z.zoneId}>
                  {z.state === "playing" ? "▶ " : ""}
                  {icon}
                  {z.renderer.name}
                  {isSelf ? " (This Device)" : z.renderer.host ? ` (${z.renderer.host})` : ""}
                </option>
              );
            })
          )}
        </select>

        <button
          type="button"
          className={`player-toggle ${browserPlayer.enabled ? "active" : ""}`}
          onClick={handleToggleBrowserPlayer}
          title={
            browserPlayer.enabled
              ? `Active browser endpoint (${browserPlayer.name}) — click to disconnect`
              : "Register this browser as a controllable audio endpoint"
          }
        >
          {browserPlayer.enabled
            ? browserPlayer.status === "connected"
              ? "● Listening Here"
              : "… Connecting"
            : "Listen Here"}
        </button>

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
