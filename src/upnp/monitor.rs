use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::discovery::{discover, probe_location};
use super::renderer::{Controller, Renderer, RendererInfo};

const DEFAULT_SWEEP_INTERVAL: Duration = Duration::from_secs(60);
const DEFAULT_MISS_THRESHOLD: usize = 3;

pub struct MonitorConfig {
    pub sweep_interval: Duration,
    pub miss_threshold: usize,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            sweep_interval: DEFAULT_SWEEP_INTERVAL,
            miss_threshold: DEFAULT_MISS_THRESHOLD,
        }
    }
}

pub type RemovedCallback = Arc<dyn Fn(&str, &str) + Send + Sync>;

pub struct MonitorCallbacks {
    pub list_upnp: Arc<dyn Fn() -> Vec<RendererInfo> + Send + Sync>,
    pub is_active: Arc<dyn Fn(&str) -> bool + Send + Sync>,
    pub on_found: Arc<dyn Fn(Arc<Renderer>) + Send + Sync>,
    pub on_removed: RemovedCallback,
}

pub struct Monitor {
    cfg: MonitorConfig,
    cb: MonitorCallbacks,
    cancel: CancellationToken,
    started: Mutex<bool>,
}

impl Monitor {
    pub fn new(cfg: MonitorConfig, cb: MonitorCallbacks) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            cb,
            cancel: CancellationToken::new(),
            started: Mutex::new(false),
        })
    }

    pub fn start(self: &Arc<Self>) {
        {
            let mut g = self.started.lock().unwrap_or_else(|e| e.into_inner());
            if *g {
                return;
            }
            *g = true;
        }

        let this = Arc::clone(self);
        tokio::spawn(async move {
            let mut missed: HashMap<String, usize> = HashMap::new();
            let mut logged_loc: HashMap<String, String> = HashMap::new();

            loop {
                this.sweep(&mut missed, &mut logged_loc).await;
                tokio::select! {
                    _ = this.cancel.cancelled() => break,
                    _ = tokio::time::sleep(this.cfg.sweep_interval) => {}
                }
            }
        });
    }

    pub fn stop(&self) {
        self.cancel.cancel();
    }

    async fn sweep(
        &self,
        missed: &mut HashMap<String, usize>,
        logged_loc: &mut HashMap<String, String>,
    ) {
        let found = match discover().await {
            Ok(list) => list,
            Err(e) => {
                tracing::debug!(error = %e, "UPnP SSDP sweep failed");
                return;
            }
        };

        let mut seen_ids: HashSet<String> = HashSet::with_capacity(found.len());
        for r in found {
            let info = r.info();
            seen_ids.insert(info.id.clone());
            missed.remove(&info.id);
            if logged_loc.get(&info.id).map(String::as_str) != Some(&info.location) {
                tracing::info!(
                    name = %info.name,
                    model = %info.model,
                    host = %info.host,
                    "discovered UPnP renderer"
                );
                logged_loc.insert(info.id.clone(), info.location.clone());
            }
            (self.cb.on_found)(r);
        }

        let existing = (self.cb.list_upnp)();
        for info in existing {
            if seen_ids.contains(&info.id) {
                continue;
            }
            if (self.cb.is_active)(&info.id) {
                missed.remove(&info.id);
                continue;
            }
            let count = missed.entry(info.id.clone()).or_insert(0);
            *count += 1;
            if *count >= self.cfg.miss_threshold {
                if probe_location(&info.location).await {
                    *count = 0;
                } else {
                    missed.remove(&info.id);
                    logged_loc.remove(&info.id);
                    (self.cb.on_removed)(&info.id, &info.name);
                }
            }
        }
    }
}
