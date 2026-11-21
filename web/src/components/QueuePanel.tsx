import { fmtDuration, fmtQuality } from "../api/format";
import type { Track } from "../api/types";

interface QueuePanelProps {
  queue: Track[];
  currentIndex: number;
  onJump: (index: number) => void;
  onRemove: (index: number) => void;
  onClear: () => void;
}

export function QueuePanel({ queue, currentIndex, onJump, onRemove, onClear }: QueuePanelProps) {
  return (
    <section className="card">
      <div className="card-header">
        <h2>Queue ({queue.length})</h2>
        {queue.length > 0 && (
          <button type="button" className="small" onClick={onClear}>
            Clear
          </button>
        )}
      </div>

      {queue.length === 0 ? (
        <p className="muted empty-hint">Queue is empty</p>
      ) : (
        <ul className="track-list queue-list">
          {queue.map((item, idx) => (
            <li key={`${item.id}-${idx}`} className={`track-row ${idx === currentIndex ? "playing" : ""}`}>
              <span className="track-num mono">{idx + 1}</span>
              <button type="button" className="track-info-btn" onClick={() => onJump(idx)}>
                <div className="track-title">{item.title}</div>
                <div className="track-meta">
                  {item.artist}
                  {fmtQuality(item) ? ` · ${fmtQuality(item)}` : ""}
                </div>
              </button>
              <span className="track-dur mono">{fmtDuration(item.durationSec)}</span>
              <button
                type="button"
                className="small icon-btn"
                onClick={() => onRemove(idx)}
                title="Remove from queue"
                aria-label={`Remove ${item.title} from queue`}
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
