use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::scanner::Catalog;

const DEFAULT_WATCH_DEBOUNCE: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy)]
pub struct AutoScanConfig {
    pub watch_enabled: bool,
    pub interval: Duration,
    pub debounce: Duration,
}

impl Default for AutoScanConfig {
    fn default() -> Self {
        Self {
            watch_enabled: true,
            interval: Duration::from_secs(60 * 60),
            debounce: DEFAULT_WATCH_DEBOUNCE,
        }
    }
}

pub type OnScanDoneFn = Arc<dyn Fn(usize) + Send + Sync>;

pub struct AutoScanner {
    cat: Arc<Catalog>,
    on_scan_done: Option<OnScanDoneFn>,
    cfg: RwLock<AutoScanConfig>,
    cancel: CancellationToken,
    reconf_tx: Mutex<Option<mpsc::Sender<AutoScanConfig>>>,
}

impl AutoScanner {
    pub fn new(
        cat: Arc<Catalog>,
        mut cfg: AutoScanConfig,
        on_scan_done: Option<OnScanDoneFn>,
    ) -> Arc<Self> {
        if cfg.debounce.is_zero() {
            cfg.debounce = DEFAULT_WATCH_DEBOUNCE;
        }
        Arc::new(Self {
            cat,
            on_scan_done,
            cfg: RwLock::new(cfg),
            cancel: CancellationToken::new(),
            reconf_tx: Mutex::new(None),
        })
    }

    pub fn config(&self) -> AutoScanConfig {
        *self.cfg.read().unwrap_or_else(|e| e.into_inner())
    }

    pub fn configure(&self, mut next: AutoScanConfig) {
        if next.debounce.is_zero() {
            next.debounce = self.config().debounce;
            if next.debounce.is_zero() {
                next.debounce = DEFAULT_WATCH_DEBOUNCE;
            }
        }
        *self.cfg.write().unwrap_or_else(|e| e.into_inner()) = next;
        let tx = self
            .reconf_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(tx) = tx {
            let _ = tx.try_send(next);
        }
    }

    pub fn start(self: &Arc<Self>) {
        let mut tx_guard = self.reconf_tx.lock().unwrap_or_else(|e| e.into_inner());
        if tx_guard.is_some() {
            return;
        }
        let (reconf_tx, reconf_rx) = mpsc::channel::<AutoScanConfig>(4);
        *tx_guard = Some(reconf_tx);
        drop(tx_guard);

        let this = Arc::clone(self);
        tokio::spawn(async move {
            this.run_loop(reconf_rx).await;
        });
    }

    async fn run_loop(&self, mut reconf_rx: mpsc::Receiver<AutoScanConfig>) {
        let mut cfg = self.config();
        let (ev_tx, mut ev_rx) = mpsc::channel::<Event>(256);
        let mut watcher = if cfg.watch_enabled {
            start_watcher(&self.cat, ev_tx.clone())
        } else {
            None
        };

        let mut debounce_deadline: Option<tokio::time::Instant> = None;
        let mut next_periodic = next_periodic_deadline(cfg.interval);

        loop {
            let next_timer = match (debounce_deadline, next_periodic) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (Some(a), None) => Some(a),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            };

            tokio::select! {
                _ = self.cancel.cancelled() => break,
                Some(new_cfg) = reconf_rx.recv() => {
                    if new_cfg.watch_enabled && watcher.is_none() {
                        watcher = start_watcher(&self.cat, ev_tx.clone());
                    } else if !new_cfg.watch_enabled {
                        watcher = None;
                        debounce_deadline = None;
                    }
                    next_periodic = next_periodic_deadline(new_cfg.interval);
                    cfg = new_cfg;
                }
                Some(ev) = ev_rx.recv() => {
                    if is_relevant_event(&ev) {
                        debounce_deadline = Some(tokio::time::Instant::now() + cfg.debounce);
                    }
                }
                _ = sleep_until_opt(next_timer) => {
                    let now = tokio::time::Instant::now();
                    let mut reason = None;
                    if let Some(dl) = debounce_deadline && now >= dl {
                        debounce_deadline = None;
                        reason = Some("file-change");
                    }
                    if let Some(per) = next_periodic && now >= per {
                        next_periodic = Some(now + cfg.interval);
                        if reason.is_none() {
                            reason = Some("scheduled");
                        }
                    }
                    if let Some(r) = reason && !self.cat.roots().is_empty() {
                        self.execute_scan(r).await;
                    }
                }
            }
        }
        drop(watcher);
    }

    async fn execute_scan(&self, reason: &str) {
        let started = Instant::now();
        match self.cat.scan().await {
            Ok(n) => {
                let st = self.cat.status();
                tracing::info!(
                    reason,
                    tracks = n,
                    albums = st.local_albums,
                    elapsed_ms = started.elapsed().as_millis(),
                    "auto library scan complete"
                );
                if let Some(cb) = &self.on_scan_done {
                    cb(n);
                }
            }
            Err(e) => {
                tracing::warn!(reason, error = %e, "auto library scan failed");
            }
        }
    }

    pub fn stop(&self) {
        self.cancel.cancel();
    }
}

fn next_periodic_deadline(interval: Duration) -> Option<tokio::time::Instant> {
    if interval.is_zero() {
        None
    } else {
        Some(tokio::time::Instant::now() + interval)
    }
}

async fn sleep_until_opt(deadline: Option<tokio::time::Instant>) {
    if let Some(dl) = deadline {
        tokio::time::sleep_until(dl).await;
    } else {
        std::future::pending::<()>().await;
    }
}

fn start_watcher(cat: &Catalog, tx: mpsc::Sender<Event>) -> Option<RecommendedWatcher> {
    let roots = cat.roots();
    if roots.is_empty() {
        return None;
    }
    let mut w = match notify::recommended_watcher(move |res: notify::Result<Event>| {
        if let Ok(ev) = res {
            let _ = tx.try_send(ev);
        }
    }) {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(error = %e, "library file watcher unavailable");
            return None;
        }
    };
    for root in &roots {
        if root.exists()
            && let Err(e) = w.watch(root, RecursiveMode::Recursive)
        {
            tracing::warn!(root = %root.display(), error = %e, "library watcher failed on root");
        }
    }
    tracing::info!(roots = roots.len(), "library file watcher active");
    Some(w)
}

fn is_relevant_event(ev: &Event) -> bool {
    match ev.kind {
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) => {}
        _ => return false,
    }
    ev.paths.iter().any(|p| is_relevant_path(p, &ev.kind))
}

fn is_relevant_path(p: &Path, kind: &EventKind) -> bool {
    let Some(base) = p.file_name().and_then(|s| s.to_str()) else {
        return false;
    };
    if base.is_empty() || base.starts_with('.') {
        return false;
    }
    let lower = base.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "@eadir" | "#recycle" | "$recycle.bin" | "system volume information"
    ) {
        return false;
    }
    if lower.ends_with(".tmp") || lower.ends_with(".part") || lower.ends_with(".crdownload") {
        return false;
    }
    if matches!(kind, EventKind::Remove(_)) {
        return true;
    }
    if matches!(kind, EventKind::Create(_)) && p.is_dir() {
        return true;
    }
    matches!(
        p.extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_ascii_lowercase())
            .as_deref(),
        None | Some(
            "flac"
                | "wav"
                | "aiff"
                | "aif"
                | "m4a"
                | "mp3"
                | "ogg"
                | "opus"
                | "dsf"
                | "jpg"
                | "jpeg"
                | "png"
                | "webp"
        )
    )
}
