import { useMemo, useState } from "react";
import { truncateAtSentence } from "@/api/format";
import { Dialog, DialogContent, DialogDescription, DialogTitle } from "@/components/ui/dialog";
import { cn } from "@/lib/utils";

interface EditorialNoteProps {
  text: string;
  title: string;
  subtitle?: string;
  targetWords?: number;
  className?: string;
}

/**
 * Displays a clean sentence-boundary editorial preview inline and opens the
 * full formatted review/biography in a frosted reading modal on "Read more",
 * keeping the page layout anchored without pushing tracks or discography off-screen.
 */
export function EditorialNote({ text, title, subtitle, targetWords = 35, className }: EditorialNoteProps) {
  const [open, setOpen] = useState(false);

  const { preview, truncated } = useMemo(() => truncateAtSentence(text, targetWords), [text, targetWords]);
  const paragraphs = useMemo(
    () =>
      text
        .split(/\n+/)
        .map((p) => p.trim())
        .filter(Boolean),
    [text],
  );

  if (!preview) return null;

  return (
    <div className={cn("text-left", className)}>
      <p className="text-sm leading-relaxed text-foreground/75">{preview}</p>
      {truncated && (
        <>
          <button
            type="button"
            onClick={() => setOpen(true)}
            className="mt-1.5 text-xs font-medium text-muted-foreground hover:text-white focus:outline-none focus-visible:underline transition-colors cursor-pointer"
          >
            Read more
          </button>

          <Dialog open={open} onOpenChange={setOpen}>
            <DialogContent className="max-w-xl p-6 sm:p-7">
              <div className="pr-8">
                <DialogTitle className="text-xl font-bold tracking-tight text-white">{title}</DialogTitle>
                {subtitle && <DialogDescription className="mt-1 text-xs">{subtitle}</DialogDescription>}
              </div>
              <div className="mt-4 max-h-[65vh] overflow-y-auto pr-2 space-y-3.5 text-sm leading-relaxed text-foreground/85">
                {paragraphs.map((para, i) => {
                  const isByline = i === paragraphs.length - 1 && para.startsWith("©");
                  return (
                    <p key={i} className={isByline ? "pt-1 text-xs text-muted-foreground/70" : undefined}>
                      {para}
                    </p>
                  );
                })}
              </div>
            </DialogContent>
          </Dialog>
        </>
      )}
    </div>
  );
}
