import { type FormEvent, useState } from "react";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogDescription, DialogTitle } from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";

interface ConnectModalProps {
  open: boolean;
  externalError?: string;
  onClose: () => void;
  onConnect: (descriptionUrl: string) => Promise<boolean>;
}

export function ConnectModal({ open, externalError, onClose, onConnect }: ConnectModalProps) {
  const [url, setUrl] = useState("");
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState("");

  const handleSubmit = async (e: FormEvent) => {
    e.preventDefault();
    const trimmed = url.trim();
    if (!trimmed) return;
    setSubmitting(true);
    setError("");
    try {
      const ok = await onConnect(trimmed);
      if (ok) {
        setUrl("");
        onClose();
      } else {
        setError(externalError || "Could not connect to UPnP renderer at that URL.");
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setSubmitting(false);
    }
  };

  const shownError = error ? externalError || error : "";

  return (
    <Dialog open={open} onOpenChange={(next) => !next && onClose()}>
      <DialogContent>
        <DialogTitle>Connect UPnP Renderer</DialogTitle>
        <DialogDescription className="mt-1">
          Enter the device description XML URL when SSDP multicast discovery cannot cross VLANs or Docker networks.
        </DialogDescription>

        <form onSubmit={handleSubmit} className="mt-4 space-y-4">
          <div className="space-y-1.5">
            <Input
              type="url"
              placeholder="http://192.168.1.50:49152/description.xml"
              value={url}
              onChange={(e) => setUrl(e.target.value)}
              autoFocus
              required
            />
            {shownError && <p className="text-xs text-red-400">{shownError}</p>}
          </div>

          <div className="flex justify-end gap-2">
            <Button type="button" variant="ghost" onClick={onClose}>
              Cancel
            </Button>
            <Button type="submit" disabled={submitting || !url.trim()}>
              {submitting ? "Connecting…" : "Connect"}
            </Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  );
}
