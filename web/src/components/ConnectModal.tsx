import { type FormEvent, useState } from "react";

interface ConnectModalProps {
  open: boolean;
  onClose: () => void;
  onConnect: (url: string) => Promise<boolean>;
}

export function ConnectModal({ open, onClose, onConnect }: ConnectModalProps) {
  const [url, setUrl] = useState("");
  const [submitting, setSubmitting] = useState(false);

  if (!open) return null;

  const handleSubmit = async (e: FormEvent) => {
    e.preventDefault();
    const trimmed = url.trim();
    if (!trimmed) return;
    setSubmitting(true);
    try {
      const ok = await onConnect(trimmed);
      if (ok) {
        setUrl("");
        onClose();
      }
    } finally {
      setSubmitting(false);
    }
  };

  return (
    <div className="modal-backdrop" role="dialog" aria-modal="true" aria-labelledby="connect-modal-title" onClick={onClose}>
      <div className="modal-card" onClick={(e) => e.stopPropagation()}>
        <h3 id="connect-modal-title">Connect UPnP Renderer by URL</h3>
        <p className="muted">
          Enter the device description XML URL on your LAN. For WiiM / LinkPlay renderers, this is typically{" "}
          <code>http://&lt;ip&gt;:49152/description.xml</code>.
        </p>
        <form onSubmit={handleSubmit} className="form-stack">
          <input
            type="url"
            autoFocus
            required
            placeholder="http://192.168.1.50:49152/description.xml"
            value={url}
            onChange={(e) => setUrl(e.target.value)}
          />
          <div className="modal-actions">
            <button type="button" onClick={onClose} disabled={submitting}>
              Cancel
            </button>
            <button type="submit" className="primary" disabled={submitting || !url.trim()}>
              {submitting ? "Connecting…" : "Connect"}
            </button>
          </div>
        </form>
      </div>
    </div>
  );
}
