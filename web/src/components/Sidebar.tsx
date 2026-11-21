import { Compass, Disc3, Home, ListMusic, Loader2, Settings, Users } from "lucide-react";
import type { LibraryStatus } from "@/api/types";
import { cn } from "@/lib/utils";

export type AppSection = "home" | "albums" | "artists" | "songs" | "qobuz" | "settings";

interface SidebarProps {
  activeSection: AppSection;
  libraryStatus: LibraryStatus | null;
  onNavigate: (section: AppSection) => void;
}

const LIBRARY_ITEMS: Array<{ id: AppSection; label: string; icon: typeof Home }> = [
  { id: "albums", label: "Albums", icon: Disc3 },
  { id: "artists", label: "Artists", icon: Users },
  { id: "songs", label: "Songs", icon: ListMusic },
];

export function Sidebar({ activeSection, libraryStatus, onNavigate }: SidebarProps) {
  return (
    <aside className="hidden lg:flex w-60 shrink-0 flex-col border-r border-white/[0.06] bg-[#0c0c0f] select-none">
      {/* Brand header — optical centers aligned with nav icons (30px) and text labels (52px) */}
      <div className="flex items-center justify-between pl-[18px] pr-3 pt-4 pb-3 shrink-0">
        <button
          type="button"
          onClick={() => onNavigate("home")}
          className="flex items-center gap-2.5 text-left cursor-pointer group"
        >
          <span className="flex h-6 w-6 shrink-0 items-center justify-center rounded-md bg-gradient-to-br from-white/15 to-white/[0.04] border border-white/10 text-white">
            <Disc3 className="h-3.5 w-3.5 transition-transform duration-500 group-hover:rotate-45" />
          </span>
          <span className="text-[15px] font-semibold tracking-tight text-foreground">tacet</span>
        </button>

        <button
          type="button"
          onClick={() => onNavigate("settings")}
          aria-label="Settings"
          aria-current={activeSection === "settings" ? "page" : undefined}
          title="Settings"
          className={cn(
            "flex h-7 w-7 items-center justify-center rounded-lg transition-colors cursor-pointer",
            activeSection === "settings"
              ? "bg-white/[0.12] text-foreground"
              : "text-muted-foreground hover:bg-white/[0.06] hover:text-foreground",
          )}
        >
          <Settings className="h-4 w-4" />
        </button>
      </div>

      {/* Scrollable nav groups */}
      <nav aria-label="Main navigation" className="flex-1 overflow-y-auto px-3 pt-1 space-y-5">
        <div className="space-y-0.5">
          <NavItem
            active={activeSection === "home"}
            icon={Home}
            label="Home"
            onClick={() => onNavigate("home")}
          />
        </div>

        <div className="space-y-1">
          <div className="px-2.5 pb-1 text-[11px] font-semibold uppercase tracking-wider text-muted-foreground/70">
            Library
          </div>
          {LIBRARY_ITEMS.map((item) => (
            <NavItem
              key={item.id}
              active={activeSection === item.id}
              icon={item.icon}
              label={item.label}
              onClick={() => onNavigate(item.id)}
            />
          ))}
        </div>

        <div className="space-y-1">
          <div className="px-2.5 pb-1 text-[11px] font-semibold uppercase tracking-wider text-muted-foreground/70">
            Discover
          </div>
          <NavItem
            active={activeSection === "qobuz"}
            icon={Compass}
            label="Qobuz"
            onClick={() => onNavigate("qobuz")}
          />
        </div>
      </nav>

      {/* Quiet library count — no heavy footer chrome competing with the PlayerBar */}
      {libraryStatus && (
        <div className="flex items-center justify-between px-[22px] py-3 text-[11px] text-muted-foreground/55 shrink-0">
          <span className="truncate">
            {libraryStatus.albums} albums · {libraryStatus.tracks} tracks
          </span>
          {libraryStatus.scanning && <Loader2 className="h-3 w-3 animate-spin text-emerald-400 shrink-0" />}
        </div>
      )}
    </aside>
  );
}

function NavItem({
  active,
  icon: Icon,
  label,
  onClick,
}: {
  active: boolean;
  icon: typeof Home;
  label: string;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      aria-current={active ? "page" : undefined}
      onClick={onClick}
      className={cn(
        "flex w-full items-center gap-3.5 rounded-lg px-2.5 py-2 text-[13.5px] font-medium transition-colors cursor-pointer",
        active
          ? "bg-white/[0.09] text-foreground"
          : "text-muted-foreground hover:bg-white/[0.04] hover:text-foreground",
      )}
    >
      <Icon className={cn("h-4 w-4 shrink-0", active ? "text-foreground" : "text-muted-foreground")} />
      <span className="truncate">{label}</span>
    </button>
  );
}

const MOBILE_ITEMS: Array<{ id: AppSection; label: string; icon: typeof Home }> = [
  { id: "home", label: "Home", icon: Home },
  { id: "albums", label: "Albums", icon: Disc3 },
  { id: "artists", label: "Artists", icon: Users },
  { id: "qobuz", label: "Qobuz", icon: Compass },
  { id: "settings", label: "Settings", icon: Settings },
];

export function MobileBottomNav({
  activeSection,
  onNavigate,
}: {
  activeSection: AppSection;
  onNavigate: (section: AppSection) => void;
}) {
  return (
    <nav
      aria-label="Mobile navigation"
      className="lg:hidden shrink-0 border-t border-white/[0.08] bg-[#0c0c0f]/95 backdrop-blur-2xl pb-[env(safe-area-inset-bottom)]"
    >
      <div className="grid grid-cols-5 h-15">
        {MOBILE_ITEMS.map((item) => {
          const active = activeSection === item.id || (item.id === "albums" && activeSection === "songs");
          const Icon = item.icon;
          return (
            <button
              key={item.id}
              type="button"
              aria-current={active ? "page" : undefined}
              onClick={() => onNavigate(item.id)}
              className={cn(
                "flex flex-col items-center justify-center gap-1 text-[10.5px] font-medium transition-colors cursor-pointer",
                active ? "text-foreground" : "text-muted-foreground",
              )}
            >
              <Icon className={cn("h-5 w-5", active && "text-white")} />
              <span>{item.label}</span>
            </button>
          );
        })}
      </div>
    </nav>
  );
}
