import { fmtDuration, fmtQuality } from "../api/format";
import type { QueueItem } from "../api/types";

interface QueuePanelProps {
  items: QueueItem[];
  currentItemId?: string;
  onJump: (itemId: string) => void;
  onRemove: (itemId: string) => void;
  onClear: () => void;
}

export function QueuePanel({ items, currentItemId, onJump, onRemove, onClear }: QueuePanelProps) {
  return (
    <section className="card">
      <div className="card-header">
        <h2>Queue ({items.length})</h2>
        {items.length > 0 && (
          <button type="button" className="small" onClick={onClear}>
            Clear
          </button>
        )}
      </div>

      {items.length === 0 ? (
        <p className="muted empty-hint">Queue is empty</p>
      ) : (
        <ul className="track-list queue-list">
          {items.map((item, idx) => (
            <li key={item.itemId} className={`track-row ${item.itemId === currentItemId ? "playing" : ""}`}>
              <span className="track-num mono">{idx + 1}</span>
              <button type="button" className="track-info-btn" onClick={() => onJump(item.itemId)}>
                <div className="track-title">{item.track.title}</div>
                <div className="track-meta">
                  {item.track.artist}
                  {fmtQuality(item.track) ? ` · ${fmtQuality(item.track)}` : ""}
                </div>
              </button>
              <span className="track-dur mono">{fmtDuration(item.track.durationSec)}</span>
              <button
                type="button"
                className="small icon-btn"
                onClick={() => onRemove(item.itemId)}
                title="Remove from queue"
                aria-label={`Remove ${item.track.title} from queue`}
              >
                ✕
              </button>
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}
