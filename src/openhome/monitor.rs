use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::{OpenHomeRenderer, connect_locations, discover_openhome_locations};
use crate::upnp::{Controller, RendererInfo, probe_location};

type OnRemovedCallback = Arc<dyn Fn(&str, &str) + Send + Sync>;

pub struct OpenHomeMonitorCallbacks {
    pub list_openhome: Arc<dyn Fn() -> Vec<RendererInfo> + Send + Sync>,
    pub is_active: Arc<dyn Fn(&str) -> bool + Send + Sync>,
    pub on_found: Arc<dyn Fn(Arc<OpenHomeRenderer>) + Send + Sync>,
    pub on_removed: OnRemovedCallback,
}

/// Periodic OpenHome SSDP discovery & health monitor.
/// Mirrors `upnp::Monitor` and `CastMonitor` so OpenHome renderers appear
/// automatically on startup and disappear cleanly when powered off.
pub struct OpenHomeMonitor {
    interval: Duration,
    cb: OpenHomeMonitorCallbacks,
    cancel: CancellationToken,
    misses: Mutex<HashMap<String, u32>>,
    logged_loc: Mutex<HashMap<String, String>>,
}

impl OpenHomeMonitor {
    pub fn new(interval: Duration, cb: OpenHomeMonitorCallbacks) -> Arc<Self> {
        Arc::new(Self {
            interval: if interval.is_zero() {
                Duration::from_secs(60)
            } else {
                interval
            },
            cb,
            cancel: CancellationToken::new(),
            misses: Mutex::new(HashMap::new()),
            logged_loc: Mutex::new(HashMap::new()),
        })
    }

    pub fn start(self: &Arc<Self>) {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            this.run_loop().await;
        });
    }

    pub fn stop(&self) {
        self.cancel.cancel();
    }

    async fn run_loop(&self) {
        self.sweep().await;
        let mut ticker = tokio::time::interval(self.interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await;
        loop {
            tokio::select! {
                () = self.cancel.cancelled() => return,
                _ = ticker.tick() => self.sweep().await,
            }
        }
    }

    async fn sweep(&self) {
        let known = (self.cb.list_openhome)();
        let known_locs: HashSet<String> = known.iter().map(|r| r.location.clone()).collect();

        // Step 1: targeted SSDP M-SEARCH (no HTTP fetches yet).
        let raw_locs = discover_openhome_locations(Duration::from_secs(2)).await;
        let seen_locs: HashSet<String> = raw_locs.iter().cloned().collect();

        // Step 2: only connect/probe genuinely new locations (skip already-registered ones).
        let new_locs: Vec<String> = raw_locs
            .into_iter()
            .filter(|loc| !known_locs.contains(loc))
            .collect();
        let newly_connected = connect_locations(new_locs).await;

        let mut seen_ids = HashSet::new();
        for r in &known {
            if seen_locs.contains(&r.location) {
                seen_ids.insert(r.id.clone());
            }
        }

        for renderer in newly_connected {
            let info = renderer.info();
            seen_ids.insert(info.id.clone());
            let should_log = {
                let mut map = self.logged_loc.lock().unwrap_or_else(|e| e.into_inner());
                let changed = map.get(&info.id).map(String::as_str) != Some(info.location.as_str());
                if changed {
                    map.insert(info.id.clone(), info.location.clone());
                }
                changed
            };
            if should_log {
                tracing::info!(
                    id = %info.id,
                    name = %info.name,
                    location = %info.location,
                    "discovered OpenHome renderer"
                );
            }
            (self.cb.on_found)(renderer);
        }

        // Step 3: track misses on idle renderers and confirm with HTTP probe before evicting.
        let mut candidates = Vec::new();
        {
            let mut misses = self.misses.lock().unwrap_or_else(|e| e.into_inner());
            for id in &seen_ids {
                misses.remove(id);
            }
            for r in &known {
                if seen_ids.contains(&r.id) || (self.cb.is_active)(&r.id) {
                    misses.remove(&r.id);
                    continue;
                }
                let count = misses.entry(r.id.clone()).or_insert(0);
                *count += 1;
                if *count >= 3 {
                    candidates.push(r.clone());
                }
            }
        }

        for r in candidates {
            if probe_location(&r.location).await {
                self.misses
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&r.id);
            } else {
                self.misses
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&r.id);
                self.logged_loc
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&r.id);
                (self.cb.on_removed)(&r.id, &r.name);
            }
        }
    }
}
