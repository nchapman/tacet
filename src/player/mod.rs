use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::upnp::{
    Controller, DeviceEvent, MediaItem, RendererInfo, Result as UpnpResult, SERVICE_AV_TRANSPORT,
    TransportStatus, UpnpError, local_ip_for_host,
};

const DEFAULT_GRACE_PERIOD: Duration = Duration::from_secs(20);
const MIN_GRACE_PERIOD: Duration = Duration::from_secs(5);
const MAX_GRACE_PERIOD: Duration = Duration::from_secs(60 * 60);
const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_PLAYERS: usize = 64;
const MAX_PLAYER_LABEL_RUNES: usize = 80;
const MAX_PLAYER_ID_LEN: usize = 64;
const MAX_REPORTED_SECONDS: f64 = 24.0 * 60.0 * 60.0;

#[derive(Debug, Error)]
pub enum PlayerError {
    #[error("player endpoint not found: {0}")]
    NotFound(String),
    #[error("invalid player credentials")]
    Unauthorized,
    #[error("player endpoint is not connected")]
    Disconnected,
    #[error("player endpoint timed out acknowledging command")]
    CommandTimeout,
    #[error("player endpoint blocked playback — user interaction required on the device")]
    PlaybackBlocked,
    #[error("invalid player registration: {0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, PlayerError>;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RegisterRequest {
    #[serde(default)]
    pub player_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub manufacturer: String,
    #[serde(default)]
    pub codecs: Vec<String>,
    pub has_volume: Option<bool>,
    pub supports_set_next: Option<bool>,
    pub supports_seek: Option<bool>,
    pub prefer_relative_media: Option<bool>,
    #[serde(default)]
    pub qobuz_delivery: String,
    pub grace_period_sec: Option<i32>,
    #[serde(skip)]
    pub host: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdateRequest {
    pub name: Option<String>,
    pub model: Option<String>,
    pub manufacturer: Option<String>,
    pub codecs: Option<Vec<String>>,
    pub has_volume: Option<bool>,
    pub supports_set_next: Option<bool>,
    pub supports_seek: Option<bool>,
    pub prefer_relative_media: Option<bool>,
    pub qobuz_delivery: Option<String>,
    pub grace_period_sec: Option<i32>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Registration {
    pub zone_id: String,
    pub token: String,
    pub renderer: RendererInfo,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaPayload {
    pub id: String,
    pub url: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub artwork_url: String,
    pub mime: String,
    pub duration_sec: i32,
    #[serde(skip_serializing_if = "is_zero_i32")]
    pub sample_rate: i32,
    #[serde(skip_serializing_if = "is_zero_i32")]
    pub bit_depth: i32,
    pub seekable: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub transcoded: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Command {
    pub id: String,
    #[serde(rename = "type")]
    pub cmd_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media: Option<MediaPayload>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position_sec: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub muted: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateReport {
    #[serde(default)]
    pub cmd_id: String,
    pub ok: Option<bool>,
    #[serde(default)]
    pub error_code: String,
    #[serde(default)]
    pub error_message: String,
    #[serde(default)]
    pub state: String,
    pub position_sec: Option<f64>,
    pub duration_sec: Option<f64>,
    #[serde(default)]
    pub track_uri: String,
    pub volume: Option<i32>,
    pub muted: Option<bool>,
}

fn is_zero_i32(v: &i32) -> bool {
    *v == 0
}

fn is_false(v: &bool) -> bool {
    !*v
}

pub struct CommandStream {
    pub id: u64,
    pub commands: mpsc::Receiver<Command>,
    pub replaced: oneshot::Receiver<()>,
}

#[derive(Debug)]
struct CmdResult {
    ok: bool,
    error_code: String,
    error_message: String,
}

type OnRegisterFn = Arc<dyn Fn(Arc<dyn Controller>) + Send + Sync>;
type OnUnregisterFn = Arc<dyn Fn(&str) + Send + Sync>;
type OnEventFn = Arc<dyn Fn(DeviceEvent) + Send + Sync>;

struct RegistryInner {
    players: HashMap<String, Arc<RemotePlayer>>,
    closed: bool,
}

pub struct Registry {
    inner: RwLock<RegistryInner>,
    grace_period: Duration,
    command_timeout: Duration,
    on_register: Option<OnRegisterFn>,
    on_unregister: Option<OnUnregisterFn>,
    on_event: Option<OnEventFn>,
}

impl Registry {
    pub fn new(
        on_register: Option<OnRegisterFn>,
        on_unregister: Option<OnUnregisterFn>,
        on_event: Option<OnEventFn>,
    ) -> Arc<Self> {
        Self::with_options(
            on_register,
            on_unregister,
            on_event,
            DEFAULT_GRACE_PERIOD,
            DEFAULT_COMMAND_TIMEOUT,
        )
    }

    pub fn with_options(
        on_register: Option<OnRegisterFn>,
        on_unregister: Option<OnUnregisterFn>,
        on_event: Option<OnEventFn>,
        grace_period: Duration,
        command_timeout: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            inner: RwLock::new(RegistryInner {
                players: HashMap::new(),
                closed: false,
            }),
            grace_period: if grace_period.is_zero() {
                DEFAULT_GRACE_PERIOD
            } else {
                grace_period
            },
            command_timeout: if command_timeout.is_zero() {
                DEFAULT_COMMAND_TIMEOUT
            } else {
                command_timeout
            },
            on_register,
            on_unregister,
            on_event,
        })
    }

    pub fn register(
        self: &Arc<Self>,
        req: RegisterRequest,
        existing_token: &str,
    ) -> Result<Registration> {
        let name = clamp_string(&req.name, "Remote Player");
        let model = clamp_string(&req.model, "Remote Endpoint");
        let manufacturer = clamp_string(&req.manufacturer, "tacet");
        let zone_id = normalize_zone_id(&req.player_id)?;
        let codecs = normalize_codecs(&req.codecs);
        let host = if req.host.trim().is_empty() {
            "remote".to_string()
        } else {
            req.host.trim().to_string()
        };

        let has_volume = req.has_volume.unwrap_or(true);
        let supports_set_next = req.supports_set_next.unwrap_or(true);
        let supports_seek = req.supports_seek.unwrap_or(true);
        let prefer_relative = req.prefer_relative_media.unwrap_or(true);
        let qobuz_delivery = parse_qobuz_delivery(&req.qobuz_delivery)?;
        let grace = req
            .grace_period_sec
            .map(|s| clamp_grace_period(Duration::from_secs(s.max(0) as u64)))
            .unwrap_or(self.grace_period);

        let (rp, tok, info) = {
            let mut g = self.inner.write().unwrap_or_else(|e| e.into_inner());
            if g.closed {
                return Err(PlayerError::Disconnected);
            }
            if let Some(existing) = g.players.get(&zone_id).cloned() {
                if !existing.verify_token(existing_token) {
                    return Err(PlayerError::Unauthorized);
                }
                existing.update_metadata(
                    name,
                    model,
                    manufacturer,
                    host,
                    codecs,
                    has_volume,
                    supports_set_next,
                    supports_seek,
                    prefer_relative,
                    qobuz_delivery,
                    grace,
                );
                let info = existing.info();
                let tok = existing.token.clone();
                (existing, tok, info)
            } else {
                if g.players.len() >= MAX_PLAYERS {
                    return Err(PlayerError::Invalid(format!(
                        "maximum active player endpoints ({MAX_PLAYERS}) reached"
                    )));
                }
                let tok = random_hex(32);
                let local_ip = if host == "remote" || host.is_empty() {
                    "127.0.0.1".into()
                } else {
                    local_ip_for_host(&host)
                };
                let rp = Arc::new(RemotePlayer {
                    token: tok.clone(),
                    command_timeout: self.command_timeout,
                    on_event: self.on_event.clone(),
                    cmd_seq: AtomicU64::new(0),
                    event_seq: AtomicU32::new(0),
                    state: Mutex::new(RemotePlayerState {
                        prefer_relative,
                        qobuz_delivery,
                        grace_period: grace,
                        local_ip,
                        info: RendererInfo {
                            id: zone_id.clone(),
                            kind: "player".into(),
                            name,
                            manufacturer,
                            model,
                            location: format!("tacet://{zone_id}"),
                            host,
                            has_volume,
                            supports_set_next,
                            supports_seek,
                            supports_events: true,
                            supports_queue: false,
                            sink_protocols: codecs,
                        },
                        status: TransportStatus {
                            state: "STOPPED".into(),
                            volume: 100,
                            has_vol_value: has_volume,
                            has_mute_val: has_volume,
                            has_volume,
                            ..Default::default()
                        },
                        stream_id: 0,
                        cmd_tx: None,
                        replaced_tx: None,
                        pending: HashMap::new(),
                        grace_task: None,
                    }),
                });
                g.players.insert(zone_id.clone(), Arc::clone(&rp));
                let info = rp.info();
                (rp, tok, info)
            }
        };

        self.schedule_grace(&zone_id, &rp, grace);
        if let Some(cb) = &self.on_register {
            cb(rp);
        }
        Ok(Registration {
            zone_id,
            token: tok,
            renderer: info,
        })
    }

    pub fn update(&self, zone_id: &str, token: &str, req: UpdateRequest) -> Result<RendererInfo> {
        let rp = self.get(zone_id, token)?;
        let qd = match req.qobuz_delivery {
            Some(ref s) => Some(parse_qobuz_delivery(s)?),
            None => None,
        };
        {
            let mut st = rp.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(n) = req.name {
                st.info.name = clamp_string(&n, &st.info.name);
            }
            if let Some(m) = req.model {
                st.info.model = clamp_string(&m, &st.info.model);
            }
            if let Some(m) = req.manufacturer {
                st.info.manufacturer = clamp_string(&m, &st.info.manufacturer);
            }
            if let Some(c) = req.codecs {
                st.info.sink_protocols = normalize_codecs(&c);
            }
            if let Some(hv) = req.has_volume {
                st.info.has_volume = hv;
                st.status.has_volume = hv;
            }
            if let Some(sn) = req.supports_set_next {
                st.info.supports_set_next = sn;
            }
            if let Some(sk) = req.supports_seek {
                st.info.supports_seek = sk;
            }
            if let Some(pr) = req.prefer_relative_media {
                st.prefer_relative = pr;
            }
            if let Some(q) = qd {
                st.qobuz_delivery = q;
            }
            if let Some(gs) = req.grace_period_sec {
                st.grace_period = clamp_grace_period(Duration::from_secs(gs.max(0) as u64));
            }
        }
        if let Some(cb) = &self.on_register {
            cb(Arc::clone(&rp) as Arc<dyn Controller>);
        }
        Ok(rp.info())
    }

    pub fn get(&self, zone_id: &str, token: &str) -> Result<Arc<RemotePlayer>> {
        let rp = {
            let g = self.inner.read().unwrap_or_else(|e| e.into_inner());
            g.players
                .get(zone_id)
                .cloned()
                .ok_or_else(|| PlayerError::NotFound(zone_id.into()))?
        };
        if !rp.verify_token(token) {
            return Err(PlayerError::Unauthorized);
        }
        Ok(rp)
    }

    pub fn unregister(&self, zone_id: &str, token: &str) -> Result<()> {
        let rp = {
            let mut g = self.inner.write().unwrap_or_else(|e| e.into_inner());
            let rp = g
                .players
                .get(zone_id)
                .cloned()
                .ok_or_else(|| PlayerError::NotFound(zone_id.into()))?;
            if !rp.verify_token(token) {
                return Err(PlayerError::Unauthorized);
            }
            g.players.remove(zone_id);
            rp
        };
        rp.shutdown();
        if let Some(cb) = &self.on_unregister {
            cb(zone_id);
        }
        Ok(())
    }

    pub fn detach_stream(self: &Arc<Self>, rp: &Arc<RemotePlayer>, stream_id: u64) -> bool {
        let zone_id = rp.info().id;
        let closed = self.inner.read().unwrap_or_else(|e| e.into_inner()).closed;
        let (detached, grace) = rp.detach_stream(stream_id);
        if !detached {
            return false;
        }
        if !closed {
            self.schedule_grace(&zone_id, rp, grace);
        }
        true
    }

    pub fn close(&self) {
        let list = {
            let mut g = self.inner.write().unwrap_or_else(|e| e.into_inner());
            g.closed = true;
            g.players.drain().map(|(_, v)| v).collect::<Vec<_>>()
        };
        for rp in list {
            rp.shutdown();
        }
    }

    fn schedule_grace(self: &Arc<Self>, zone_id: &str, rp: &Arc<RemotePlayer>, grace: Duration) {
        let reg = Arc::clone(self);
        let zone_id = zone_id.to_string();
        let target = Arc::clone(rp);
        let mut st = rp.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(prev) = st.grace_task.take() {
            prev.abort();
        }
        if st.cmd_tx.is_some() {
            return;
        }
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            st.grace_task = Some(handle.spawn(async move {
                tokio::time::sleep(grace).await;
                reg.reap_if_disconnected(&zone_id, &target);
            }));
        }
    }

    fn reap_if_disconnected(&self, zone_id: &str, target: &Arc<RemotePlayer>) {
        let removed = {
            let mut g = self.inner.write().unwrap_or_else(|e| e.into_inner());
            match g.players.get(zone_id) {
                Some(cur) if Arc::ptr_eq(cur, target) && !cur.push_connected() => {
                    g.players.remove(zone_id)
                }
                _ => None,
            }
        };
        if let Some(cur) = removed {
            cur.shutdown();
            if let Some(cb) = &self.on_unregister {
                cb(zone_id);
            }
        }
    }
}

struct RemotePlayerState {
    prefer_relative: bool,
    qobuz_delivery: String,
    grace_period: Duration,
    local_ip: String,
    info: RendererInfo,
    status: TransportStatus,
    stream_id: u64,
    cmd_tx: Option<mpsc::Sender<Command>>,
    replaced_tx: Option<oneshot::Sender<()>>,
    pending: HashMap<String, oneshot::Sender<CmdResult>>,
    grace_task: Option<JoinHandle<()>>,
}

pub struct RemotePlayer {
    token: String,
    command_timeout: Duration,
    on_event: Option<OnEventFn>,
    cmd_seq: AtomicU64,
    event_seq: AtomicU32,
    state: Mutex<RemotePlayerState>,
}

impl RemotePlayer {
    fn verify_token(&self, candidate: &str) -> bool {
        let a = self.token.as_bytes();
        let b = candidate.trim().as_bytes();
        if a.len() != b.len() {
            return false;
        }
        let mut diff = 0u8;
        for (&x, &y) in a.iter().zip(b.iter()) {
            diff |= x ^ y;
        }
        diff == 0
    }

    #[allow(clippy::too_many_arguments)]
    fn update_metadata(
        &self,
        name: String,
        model: String,
        manufacturer: String,
        host: String,
        codecs: Vec<String>,
        has_volume: bool,
        supports_set_next: bool,
        supports_seek: bool,
        prefer_relative: bool,
        qobuz_delivery: String,
        grace_period: Duration,
    ) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.info.name = name;
        st.info.model = model;
        st.info.manufacturer = manufacturer;
        st.info.host = host;
        st.info.sink_protocols = codecs;
        st.info.has_volume = has_volume;
        st.info.supports_set_next = supports_set_next;
        st.info.supports_seek = supports_seek;
        st.status.has_volume = has_volume;
        st.prefer_relative = prefer_relative;
        st.qobuz_delivery = qobuz_delivery;
        st.grace_period = grace_period;
    }

    pub fn attach_stream(&self) -> CommandStream {
        let (cmd_tx, cmd_rx) = mpsc::channel::<Command>(32);
        let (rep_tx, rep_rx) = oneshot::channel::<()>();
        let (stream_id, prev_rep) = {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(t) = st.grace_task.take() {
                t.abort();
            }
            st.stream_id += 1;
            let id = st.stream_id;
            let prev = st.replaced_tx.replace(rep_tx);
            st.cmd_tx = Some(cmd_tx);
            (id, prev)
        };
        if let Some(prev) = prev_rep {
            let _ = prev.send(());
        }
        CommandStream {
            id: stream_id,
            commands: cmd_rx,
            replaced: rep_rx,
        }
    }

    fn detach_stream(&self, stream_id: u64) -> (bool, Duration) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.stream_id != stream_id {
            return (false, st.grace_period);
        }
        st.cmd_tx = None;
        st.replaced_tx = None;
        (true, st.grace_period)
    }

    fn shutdown(&self) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(t) = st.grace_task.take() {
            t.abort();
        }
        st.cmd_tx = None;
        if let Some(rep) = st.replaced_tx.take() {
            let _ = rep.send(());
        }
        for (_, tx) in st.pending.drain() {
            let _ = tx.send(CmdResult {
                ok: false,
                error_code: "DISCONNECTED".into(),
                error_message: "player disconnected".into(),
            });
        }
    }

    pub fn report_state(&self, rep: StateReport) {
        let (ev, ack_tx) = {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let mut ev = DeviceEvent {
                renderer_id: st.info.id.clone(),
                service: SERVICE_AV_TRANSPORT.into(),
                seq: self.event_seq.fetch_add(1, Ordering::Relaxed) + 1,
                ..Default::default()
            };

            let state_up = rep.state.trim().to_ascii_uppercase();
            if matches!(
                state_up.as_str(),
                "PLAYING" | "PAUSED_PLAYBACK" | "STOPPED" | "TRANSITIONING"
            ) {
                st.status.state = state_up.clone();
                ev.has_state = true;
                ev.state = state_up;
            }
            if let Some(pos) = rep.position_sec
                && pos.is_finite()
                && pos >= 0.0
            {
                let d = Duration::from_secs_f64(pos.min(MAX_REPORTED_SECONDS));
                st.status.position = d;
                st.status.has_position = true;
                ev.has_position = true;
                ev.position = d;
            }
            if let Some(dur) = rep.duration_sec
                && dur.is_finite()
                && dur >= 0.0
            {
                let d = Duration::from_secs_f64(dur.min(MAX_REPORTED_SECONDS));
                st.status.duration = d;
                ev.has_duration = true;
                ev.duration = d;
            }
            if !rep.track_uri.trim().is_empty() {
                let uri = rep.track_uri.trim().to_string();
                st.status.track_uri = uri.clone();
                ev.has_track_uri = true;
                ev.track_uri = uri;
            }
            if let Some(vol) = rep.volume {
                let v = vol.clamp(0, 100);
                st.status.volume = v;
                st.status.has_vol_value = true;
                ev.has_volume = true;
                ev.volume = v;
            }
            if let Some(muted) = rep.muted {
                st.status.muted = muted;
                st.status.has_mute_val = true;
                ev.has_mute = true;
                ev.muted = muted;
            }

            let ack_tx = if !rep.cmd_id.trim().is_empty() {
                st.pending.remove(rep.cmd_id.trim())
            } else {
                None
            };
            (ev, ack_tx)
        };

        if let Some(tx) = ack_tx {
            let _ = tx.send(CmdResult {
                ok: rep.ok.unwrap_or(true),
                error_code: rep.error_code,
                error_message: rep.error_message,
            });
        }
        if ev.has_any_update()
            && let Some(cb) = &self.on_event
        {
            cb(ev);
        }
    }

    async fn send_command(&self, mut cmd: Command) -> UpnpResult<()> {
        let id = format!("cmd-{}", self.cmd_seq.fetch_add(1, Ordering::Relaxed) + 1);
        cmd.id = id.clone();

        let (tx, ack_rx) = {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let Some(tx) = st.cmd_tx.clone() else {
                return Err(UpnpError::Other(PlayerError::Disconnected.to_string()));
            };
            let (ack_tx, ack_rx) = oneshot::channel();
            st.pending.insert(id.clone(), ack_tx);
            (tx, ack_rx)
        };

        if tx.send(cmd).await.is_err() {
            self.state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pending
                .remove(&id);
            return Err(UpnpError::Other(PlayerError::Disconnected.to_string()));
        }

        match tokio::time::timeout(self.command_timeout, ack_rx).await {
            Ok(Ok(res)) if res.ok => Ok(()),
            Ok(Ok(res)) => {
                if res.error_code.eq_ignore_ascii_case("PLAYBACK_BLOCKED") {
                    Err(UpnpError::Other(PlayerError::PlaybackBlocked.to_string()))
                } else {
                    let msg = if res.error_message.is_empty() {
                        res.error_code
                    } else {
                        res.error_message
                    };
                    Err(UpnpError::Other(msg))
                }
            }
            Ok(Err(_)) => Err(UpnpError::Other(PlayerError::Disconnected.to_string())),
            Err(_) => {
                self.state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .pending
                    .remove(&id);
                Err(UpnpError::Other(PlayerError::CommandTimeout.to_string()))
            }
        }
    }
}

#[async_trait]
impl Controller for RemotePlayer {
    fn info(&self) -> RendererInfo {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .info
            .clone()
    }

    fn local_ip(&self) -> String {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .local_ip
            .clone()
    }

    async fn set_uri(&self, item: &MediaItem) -> UpnpResult<()> {
        self.send_command(Command {
            id: String::new(),
            cmd_type: "set_uri".into(),
            media: Some(to_media_payload(item)),
            position_sec: None,
            volume: None,
            muted: None,
        })
        .await
    }

    async fn set_next_uri(&self, item: &MediaItem) -> UpnpResult<()> {
        self.send_command(Command {
            id: String::new(),
            cmd_type: "set_next_uri".into(),
            media: Some(to_media_payload(item)),
            position_sec: None,
            volume: None,
            muted: None,
        })
        .await
    }

    async fn play(&self) -> UpnpResult<()> {
        self.send_command(Command {
            id: String::new(),
            cmd_type: "play".into(),
            media: None,
            position_sec: None,
            volume: None,
            muted: None,
        })
        .await
    }

    async fn pause(&self) -> UpnpResult<()> {
        self.send_command(Command {
            id: String::new(),
            cmd_type: "pause".into(),
            media: None,
            position_sec: None,
            volume: None,
            muted: None,
        })
        .await
    }

    async fn stop(&self) -> UpnpResult<()> {
        self.send_command(Command {
            id: String::new(),
            cmd_type: "stop".into(),
            media: None,
            position_sec: None,
            volume: None,
            muted: None,
        })
        .await
    }

    async fn seek(&self, pos: Duration) -> UpnpResult<()> {
        let secs = pos.as_secs_f64().round().max(0.0) as i32;
        self.send_command(Command {
            id: String::new(),
            cmd_type: "seek".into(),
            media: None,
            position_sec: Some(secs),
            volume: None,
            muted: None,
        })
        .await
    }

    async fn status(&self) -> UpnpResult<TransportStatus> {
        Ok(self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .status
            .clone())
    }

    async fn set_volume(&self, vol: i32) -> UpnpResult<()> {
        self.send_command(Command {
            id: String::new(),
            cmd_type: "set_volume".into(),
            media: None,
            position_sec: None,
            volume: Some(vol.clamp(0, 100)),
            muted: None,
        })
        .await
    }

    async fn set_mute(&self, muted: bool) -> UpnpResult<()> {
        self.send_command(Command {
            id: String::new(),
            cmd_type: "set_mute".into(),
            media: None,
            position_sec: None,
            volume: None,
            muted: Some(muted),
        })
        .await
    }

    fn prefer_relative_media(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .prefer_relative
    }

    fn qobuz_delivery(&self) -> &str {
        // Static-str safe lookup for the 3 valid values
        match self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .qobuz_delivery
            .as_str()
        {
            "force" => "force",
            "direct" => "direct",
            _ => "auto",
        }
    }

    fn push_connected(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .cmd_tx
            .is_some()
    }
}

fn to_media_payload(item: &MediaItem) -> MediaPayload {
    MediaPayload {
        id: item.id.clone(),
        url: item.url.clone(),
        title: item.title.clone(),
        artist: item.artist.clone(),
        album: item.album.clone(),
        artwork_url: item.album_art_url.clone(),
        mime: item.mime.clone(),
        duration_sec: item.duration.as_secs_f64().round() as i32,
        sample_rate: item.sample_rate,
        bit_depth: item.bit_depth,
        seekable: item.seekable,
        transcoded: item.transcoded,
    }
}

fn clamp_string(raw: &str, fallback: &str) -> String {
    let s = raw.trim();
    if s.is_empty() {
        return fallback.into();
    }
    s.chars().take(MAX_PLAYER_LABEL_RUNES).collect()
}

fn normalize_zone_id(raw: &str) -> Result<String> {
    let s = raw.trim();
    if s.is_empty() {
        return Ok(format!("player:{}", random_hex(8)));
    }
    let clean = s.strip_prefix("player:").unwrap_or(s);
    if clean.is_empty()
        || clean.len() > MAX_PLAYER_ID_LEN
        || !clean
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        return Err(PlayerError::Invalid(format!("invalid playerId {raw:?}")));
    }
    Ok(format!("player:{clean}"))
}

fn normalize_codecs(raw: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for c in raw {
        let s = c.trim().to_ascii_lowercase();
        if s.is_empty() {
            continue;
        }
        let proto = if s.starts_with("http-get:") {
            s
        } else {
            format!("http-get:*:{s}:*")
        };
        if !out.contains(&proto) {
            out.push(proto);
        }
    }
    out
}

fn parse_qobuz_delivery(raw: &str) -> Result<String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "" | "auto" => Ok("auto".into()),
        "force" => Ok("force".into()),
        "direct" => Ok("direct".into()),
        other => Err(PlayerError::Invalid(format!(
            "invalid qobuzDelivery {other:?}"
        ))),
    }
}

fn clamp_grace_period(d: Duration) -> Duration {
    d.clamp(MIN_GRACE_PERIOD, MAX_GRACE_PERIOD)
}

fn random_hex(n_bytes: usize) -> String {
    let mut buf = vec![0u8; n_bytes];
    rand::rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn register_command_ack_and_unregister() {
        let reg = Registry::new(None, None, None);
        let r = reg
            .register(
                RegisterRequest {
                    player_id: "macbook".into(),
                    name: "MacBook Pro".into(),
                    codecs: vec!["audio/flac".into()],
                    ..Default::default()
                },
                "",
            )
            .unwrap();
        assert_eq!(r.zone_id, "player:macbook");
        assert!(!r.token.is_empty());

        let rp = reg.get(&r.zone_id, &r.token).unwrap();
        let mut stream = rp.attach_stream();
        assert!(rp.push_connected());

        let rp_ack = Arc::clone(&rp);
        let ack_task = tokio::spawn(async move {
            let cmd = stream.commands.recv().await.unwrap();
            assert_eq!(cmd.cmd_type, "play");
            rp_ack.report_state(StateReport {
                cmd_id: cmd.id,
                ok: Some(true),
                state: "PLAYING".into(),
                ..Default::default()
            });
        });

        rp.play().await.unwrap();
        ack_task.await.unwrap();
        assert_eq!(rp.status().await.unwrap().state, "PLAYING");

        reg.unregister(&r.zone_id, &r.token).unwrap();
        assert!(reg.get(&r.zone_id, &r.token).is_err());
    }
}
