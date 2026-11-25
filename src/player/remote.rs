use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use super::{
    Command, CommandStream, MediaPayload, OnEventFn, PlayerError, StateReport, UpdateRequest,
    clamp_grace_period, clamp_string, normalize_codecs,
};
use crate::upnp::{
    Controller, DeviceEvent, MediaItem, RendererInfo, Result as UpnpResult, SERVICE_AV_TRANSPORT,
    TransportStatus, UpnpError, local_ip_for_host,
};

const MAX_REPORTED_SECONDS: f64 = 24.0 * 60.0 * 60.0;

#[derive(Debug)]
pub(super) struct CmdResult {
    pub(super) ok: bool,
    pub(super) error_code: String,
    pub(super) error_message: String,
}

pub(super) struct PlayerMetadata {
    pub(super) name: String,
    pub(super) model: String,
    pub(super) manufacturer: String,
    pub(super) host: String,
    pub(super) codecs: Vec<String>,
    pub(super) has_volume: bool,
    pub(super) supports_set_next: bool,
    pub(super) supports_seek: bool,
    pub(super) prefer_relative: bool,
    pub(super) qobuz_delivery: String,
    pub(super) grace_period: Duration,
}

pub(super) struct RemotePlayerState {
    pub(super) prefer_relative: bool,
    pub(super) qobuz_delivery: String,
    pub(super) grace_period: Duration,
    pub(super) local_ip: String,
    pub(super) info: RendererInfo,
    pub(super) status: TransportStatus,
    pub(super) stream_id: u64,
    pub(super) cmd_tx: Option<mpsc::Sender<Command>>,
    pub(super) replaced_tx: Option<oneshot::Sender<()>>,
    pub(super) pending: HashMap<String, oneshot::Sender<CmdResult>>,
    pub(super) grace_task: Option<JoinHandle<()>>,
}

pub struct RemotePlayer {
    pub(super) token: String,
    command_timeout: Duration,
    on_event: Option<OnEventFn>,
    cmd_seq: AtomicU64,
    event_seq: AtomicU32,
    pub(super) state: Mutex<RemotePlayerState>,
}

impl RemotePlayer {
    pub(super) fn new(
        zone_id: String,
        token: String,
        command_timeout: Duration,
        on_event: Option<OnEventFn>,
        meta: PlayerMetadata,
    ) -> Self {
        let local_ip = if meta.host == "remote" || meta.host.is_empty() {
            "127.0.0.1".into()
        } else {
            local_ip_for_host(&meta.host)
        };
        Self {
            token,
            command_timeout,
            on_event,
            cmd_seq: AtomicU64::new(0),
            event_seq: AtomicU32::new(0),
            state: Mutex::new(RemotePlayerState {
                prefer_relative: meta.prefer_relative,
                qobuz_delivery: meta.qobuz_delivery,
                grace_period: meta.grace_period,
                local_ip,
                info: RendererInfo {
                    id: zone_id.clone(),
                    kind: "player".into(),
                    name: meta.name,
                    manufacturer: meta.manufacturer,
                    model: meta.model,
                    location: format!("tacet://{zone_id}"),
                    host: meta.host,
                    has_volume: meta.has_volume,
                    supports_set_next: meta.supports_set_next,
                    supports_seek: meta.supports_seek,
                    supports_events: true,
                    supports_queue: false,
                    sink_protocols: meta.codecs,
                },
                status: TransportStatus {
                    state: "STOPPED".into(),
                    volume: 100,
                    has_vol_value: meta.has_volume,
                    has_mute_val: meta.has_volume,
                    has_volume: meta.has_volume,
                    ..Default::default()
                },
                stream_id: 0,
                cmd_tx: None,
                replaced_tx: None,
                pending: HashMap::new(),
                grace_task: None,
            }),
        }
    }

    pub(super) fn verify_token(&self, candidate: &str) -> bool {
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

    pub(super) fn update_metadata(&self, meta: PlayerMetadata) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.info.name = meta.name;
        st.info.model = meta.model;
        st.info.manufacturer = meta.manufacturer;
        st.info.host = meta.host;
        st.info.sink_protocols = meta.codecs;
        st.info.has_volume = meta.has_volume;
        st.info.supports_set_next = meta.supports_set_next;
        st.info.supports_seek = meta.supports_seek;
        st.status.has_volume = meta.has_volume;
        st.prefer_relative = meta.prefer_relative;
        st.qobuz_delivery = meta.qobuz_delivery;
        st.grace_period = meta.grace_period;
    }

    pub(super) fn apply_update(&self, req: UpdateRequest, qobuz_delivery: Option<String>) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
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
        if let Some(q) = qobuz_delivery {
            st.qobuz_delivery = q;
        }
        if let Some(gs) = req.grace_period_sec {
            st.grace_period = clamp_grace_period(Duration::from_secs(gs.max(0) as u64));
        }
    }

    pub fn attach_stream(&self) -> CommandStream {
        let (cmd_tx, cmd_rx) = mpsc::channel::<Command>(32);
        let (rep_tx, rep_rx) = oneshot::channel::<()>();
        let (stream_id, prev_rep, _old_tx) = {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(t) = st.grace_task.take() {
                t.abort();
            }
            st.stream_id += 1;
            let id = st.stream_id;
            let prev = st.replaced_tx.replace(rep_tx);
            let old_tx = st.cmd_tx.replace(cmd_tx);
            (id, prev, old_tx)
        };
        // Notify replaced stream BEFORE dropping `_old_tx` so it emits `event: replaced`
        // rather than observing channel EOF.
        if let Some(prev) = prev_rep {
            let _ = prev.send(());
        }
        drop(_old_tx);
        CommandStream {
            id: stream_id,
            commands: cmd_rx,
            replaced: rep_rx,
        }
    }

    pub(super) fn detach_stream(&self, stream_id: u64) -> (bool, Duration) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.stream_id != stream_id {
            return (false, st.grace_period);
        }
        st.cmd_tx = None;
        st.replaced_tx = None;
        for (_, tx) in st.pending.drain() {
            let _ = tx.send(CmdResult {
                ok: false,
                error_code: "DISCONNECTED".into(),
                error_message: "player stream disconnected".into(),
            });
        }
        (true, st.grace_period)
    }

    pub(super) fn shutdown(&self) {
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

            let norm_state = match rep.state.trim().to_ascii_uppercase().as_str() {
                "PLAYING" => Some("PLAYING"),
                "PAUSED" | "PAUSED_PLAYBACK" => Some("PAUSED_PLAYBACK"),
                "STOPPED" | "IDLE" | "NO_MEDIA_PRESENT" => Some("STOPPED"),
                "TRANSITIONING" | "BUFFERING" => Some("TRANSITIONING"),
                _ => None,
            };
            if let Some(state_up) = norm_state {
                st.status.state = state_up.into();
                ev.has_state = true;
                ev.state = state_up.into();
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
                if res.error_code.eq_ignore_ascii_case("PLAYBACK_BLOCKED")
                    || res.error_code.eq_ignore_ascii_case("AUTOPLAY_BLOCKED")
                {
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
        let media = if item.url.trim().is_empty() {
            None
        } else {
            Some(to_media_payload(item))
        };
        self.send_command(Command {
            id: String::new(),
            cmd_type: "set_next_uri".into(),
            media,
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
