use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, Socket, Type};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use super::discovery::{connect_by_url, discover, probe_location};
use super::renderer::{Controller, Renderer, RendererInfo};

const DEFAULT_SWEEP_INTERVAL: Duration = Duration::from_secs(60);
const DEFAULT_MISS_THRESHOLD: usize = 3;
const SSDP_MULTICAST_ADDR: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
const SSDP_MULTICAST_PORT: u16 = 1900;
const NOTIFY_BURST_COOLDOWN: Duration = Duration::from_secs(15);

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

        let notify_this = Arc::clone(self);
        tokio::spawn(async move {
            notify_this.run_notify_listener().await;
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

    async fn run_notify_listener(self: Arc<Self>) {
        let sock = match bind_ssdp_multicast_socket() {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(error = %e, "SSDP NOTIFY multicast listener unavailable");
                return;
            }
        };
        let connect_sem = Arc::new(Semaphore::new(4));
        let mut recent_alive: HashMap<String, Instant> = HashMap::new();
        let mut buf = [0u8; 4096];

        loop {
            let n = tokio::select! {
                _ = self.cancel.cancelled() => break,
                res = sock.recv_from(&mut buf) => match res {
                    Ok((n, _)) => n,
                    Err(_) => {
                        tokio::select! {
                            _ = self.cancel.cancelled() => break,
                            _ = tokio::time::sleep(Duration::from_millis(250)) => continue,
                        }
                    }
                }
            };
            let text = String::from_utf8_lossy(&buf[..n]);
            let Some(msg) = parse_ssdp_notify(&text) else {
                continue;
            };
            match msg {
                SsdpNotify::Alive { location } => {
                    self.handle_notify_alive(location, &mut recent_alive, &connect_sem);
                }
                SsdpNotify::Byebye { usn, location } => {
                    if let Ok(permit) = Arc::clone(&connect_sem).try_acquire_owned() {
                        let this = Arc::clone(&self);
                        tokio::spawn(async move {
                            let _permit = permit;
                            this.handle_notify_byebye(&usn, &location).await;
                        });
                    }
                }
            }
        }
    }

    fn handle_notify_alive(
        self: &Arc<Self>,
        location: String,
        recent_alive: &mut HashMap<String, Instant>,
        connect_sem: &Arc<Semaphore>,
    ) {
        let now = Instant::now();
        recent_alive.retain(|_, ts| now.duration_since(*ts) < NOTIFY_BURST_COOLDOWN);
        if recent_alive.contains_key(&location) {
            return;
        }
        if (self.cb.list_upnp)()
            .iter()
            .any(|info| info.location == location)
        {
            recent_alive.insert(location, now);
            return;
        }
        recent_alive.insert(location.clone(), now);

        let Ok(permit) = Arc::clone(connect_sem).try_acquire_owned() else {
            return;
        };
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let _permit = permit;
            match connect_by_url(&location).await {
                Ok(r) => {
                    let info = r.info();
                    tracing::info!(
                        name = %info.name,
                        model = %info.model,
                        host = %info.host,
                        "discovered UPnP renderer via SSDP NOTIFY"
                    );
                    (this.cb.on_found)(r);
                }
                Err(e) => {
                    tracing::debug!(location = %location, error = %e, "SSDP NOTIFY renderer connect failed");
                }
            }
        });
    }

    async fn handle_notify_byebye(&self, usn: &str, location: &str) {
        let usn_udn = usn.split("::").next().unwrap_or(usn).trim();
        let candidates: Vec<RendererInfo> = (self.cb.list_upnp)()
            .into_iter()
            .filter(|info| {
                (!usn_udn.is_empty() && usn_udn.eq_ignore_ascii_case(&info.id))
                    || (!location.is_empty() && info.location.eq_ignore_ascii_case(location))
            })
            .collect();
        for info in candidates {
            if (self.cb.is_active)(&info.id) {
                continue;
            }
            if !probe_location(&info.location).await {
                (self.cb.on_removed)(&info.id, &info.name);
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SsdpNotify {
    Alive { location: String },
    Byebye { usn: String, location: String },
}

pub(crate) fn parse_ssdp_notify(raw: &str) -> Option<SsdpNotify> {
    let mut lines = raw.lines();
    let first = lines.next()?.trim();
    if !first.to_ascii_uppercase().starts_with("NOTIFY ") {
        return None;
    }

    let mut nt = "";
    let mut nts = "";
    let mut location = "";
    let mut usn = "";
    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            break;
        }
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let v = v.trim();
        if k.eq_ignore_ascii_case("nt") {
            nt = v;
        } else if k.eq_ignore_ascii_case("nts") {
            nts = v;
        } else if k.eq_ignore_ascii_case("location") {
            location = v;
        } else if k.eq_ignore_ascii_case("usn") {
            usn = v;
        }
    }

    let nt_low = nt.to_ascii_lowercase();
    let usn_low = usn.to_ascii_lowercase();
    let is_renderer_nt = nt_low.contains("avtransport")
        || nt_low.contains("mediarenderer")
        || usn_low.contains("avtransport")
        || usn_low.contains("mediarenderer");
    let is_root_byebye =
        nt_low.contains("upnp:rootdevice") && nts.eq_ignore_ascii_case("ssdp:byebye");
    if !is_renderer_nt && !is_root_byebye {
        return None;
    }

    if nts.eq_ignore_ascii_case("ssdp:alive") {
        if location.is_empty() {
            return None;
        }
        Some(SsdpNotify::Alive {
            location: location.to_string(),
        })
    } else if nts.eq_ignore_ascii_case("ssdp:byebye") {
        Some(SsdpNotify::Byebye {
            usn: usn.to_string(),
            location: location.to_string(),
        })
    } else {
        None
    }
}

fn bind_ssdp_multicast_socket() -> std::io::Result<tokio::net::UdpSocket> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_reuse_address(true)?;
    #[cfg(unix)]
    {
        let _ = sock.set_reuse_port(true);
    }
    let bind_addr = SocketAddr::V4(SocketAddrV4::new(
        Ipv4Addr::UNSPECIFIED,
        SSDP_MULTICAST_PORT,
    ));
    sock.bind(&bind_addr.into())?;
    let mut joined = false;
    for iface_ip in super::discovery::lan_ipv4_interfaces() {
        if sock
            .join_multicast_v4(&SSDP_MULTICAST_ADDR, &iface_ip)
            .is_ok()
        {
            joined = true;
        }
    }
    if !joined {
        sock.join_multicast_v4(&SSDP_MULTICAST_ADDR, &Ipv4Addr::UNSPECIFIED)?;
    }
    sock.set_nonblocking(true)?;
    tokio::net::UdpSocket::from_std(sock.into())
}
