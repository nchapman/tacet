use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::{OpenHomeRenderer, discover_openhome};
use crate::upnp::{Controller, RendererInfo};

pub type RemovedCallback = Arc<dyn Fn(&str, &str) + Send + Sync>;

pub struct OpenHomeMonitorCallbacks {
    pub list_openhome: Arc<dyn Fn() -> Vec<RendererInfo> + Send + Sync>,
    pub is_active: Arc<dyn Fn(&str) -> bool + Send + Sync>,
    pub on_found: Arc<dyn Fn(Arc<OpenHomeRenderer>) + Send + Sync>,
    pub on_removed: RemovedCallback,
}

/// Background SSDP monitor for OpenHome Media renderers on the LAN.
pub struct OpenHomeMonitor {
    sweep_interval: Duration,
    miss_threshold: usize,
    cb: OpenHomeMonitorCallbacks,
    cancel: CancellationToken,
    started: Mutex<bool>,
}

impl OpenHomeMonitor {
    pub fn new(sweep_interval: Duration, cb: OpenHomeMonitorCallbacks) -> Arc<Self> {
        Arc::new(Self {
            sweep_interval: if sweep_interval.is_zero() {
                Duration::from_secs(30)
            } else {
                sweep_interval
            },
            miss_threshold: 3,
            cb,
            cancel: CancellationToken::new(),
            started: Mutex::new(false),
        })
    }

    pub fn start(self: &Arc<Self>) {
        let mut started = self.started.lock().unwrap_or_else(|e| e.into_inner());
        if *started {
            return;
        }
        *started = true;
        drop(started);

        let mon = Arc::clone(self);
        tokio::spawn(async move {
            let mut misses: HashMap<String, usize> = HashMap::new();
            mon.sweep(&mut misses).await;

            let mut ticker = tokio::time::interval(mon.sweep_interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            ticker.tick().await;

            loop {
                tokio::select! {
                    () = mon.cancel.cancelled() => break,
                    _ = ticker.tick() => {
                        mon.sweep(&mut misses).await;
                    }
                }
            }
        });
    }

    pub fn stop(&self) {
        self.cancel.cancel();
    }

    async fn sweep(&self, misses: &mut HashMap<String, usize>) {
        let found = discover_openhome(Duration::from_secs(3)).await;
        let mut seen_ids: HashSet<String> = HashSet::with_capacity(found.len());

        for renderer in found {
            let info = renderer.info();
            seen_ids.insert(info.id.clone());
            misses.remove(&info.id);
            (self.cb.on_found)(renderer);
        }

        let known = (self.cb.list_openhome)();
        for info in known {
            if seen_ids.contains(&info.id) {
                continue;
            }
            if (self.cb.is_active)(&info.id) {
                misses.remove(&info.id);
                continue;
            }
            let count = misses.entry(info.id.clone()).or_insert(0);
            *count += 1;
            if *count >= self.miss_threshold {
                misses.remove(&info.id);
                (self.cb.on_removed)(&info.id, &info.name);
            }
        }
    }
}
