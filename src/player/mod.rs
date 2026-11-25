mod remote;

pub use self::remote::RemotePlayer;

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use rand::RngCore;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};

use self::remote::PlayerMetadata;
use crate::upnp::{Controller, DeviceEvent, RendererInfo};

const DEFAULT_GRACE_PERIOD: Duration = Duration::from_secs(20);
const MIN_GRACE_PERIOD: Duration = Duration::from_secs(5);
const MAX_GRACE_PERIOD: Duration = Duration::from_secs(60 * 60);
const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_PLAYERS: usize = 64;
const MAX_PLAYER_LABEL_RUNES: usize = 80;
const MAX_PLAYER_ID_LEN: usize = 64;

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

type OnRegisterFn = Arc<dyn Fn(Arc<dyn Controller>) + Send + Sync>;
type OnUnregisterFn = Arc<dyn Fn(&str) + Send + Sync>;
pub(super) type OnEventFn = Arc<dyn Fn(DeviceEvent) + Send + Sync>;

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
        let zone_id = normalize_zone_id(&req.player_id)?;
        let grace = req
            .grace_period_sec
            .map(|s| clamp_grace_period(Duration::from_secs(s.max(0) as u64)))
            .unwrap_or(self.grace_period);
        let meta = PlayerMetadata {
            name: clamp_string(&req.name, "Remote Player"),
            model: clamp_string(&req.model, "Remote Endpoint"),
            manufacturer: clamp_string(&req.manufacturer, "tacet"),
            host: if req.host.trim().is_empty() {
                "remote".into()
            } else {
                req.host.trim().into()
            },
            codecs: normalize_codecs(&req.codecs),
            has_volume: req.has_volume.unwrap_or(true),
            supports_set_next: req.supports_set_next.unwrap_or(true),
            supports_seek: req.supports_seek.unwrap_or(true),
            prefer_relative: req.prefer_relative_media.unwrap_or(true),
            qobuz_delivery: parse_qobuz_delivery(&req.qobuz_delivery)?,
            grace_period: grace,
        };

        let (rp, tok, info) = {
            let mut g = self.inner.write().unwrap_or_else(|e| e.into_inner());
            if g.closed {
                return Err(PlayerError::Disconnected);
            }
            if let Some(existing) = g.players.get(&zone_id).cloned() {
                if !existing.verify_token(existing_token) {
                    return Err(PlayerError::Unauthorized);
                }
                existing.update_metadata(meta);
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
                let rp = Arc::new(RemotePlayer::new(
                    zone_id.clone(),
                    tok.clone(),
                    self.command_timeout,
                    self.on_event.clone(),
                    meta,
                ));
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
        rp.apply_update(req, qd);
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

pub(super) fn clamp_string(raw: &str, fallback: &str) -> String {
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

pub(super) fn normalize_codecs(raw: &[String]) -> Vec<String> {
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

pub(super) fn clamp_grace_period(d: Duration) -> Duration {
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

    #[tokio::test]
    async fn attach_stream_signals_replaced_before_channel_eof() {
        let reg = Registry::new(None, None, None);
        let r = reg
            .register(
                RegisterRequest {
                    player_id: "tab_test".into(),
                    name: "Tab".into(),
                    ..Default::default()
                },
                "",
            )
            .unwrap();
        let rp = reg.get(&r.zone_id, &r.token).unwrap();
        let mut first = rp.attach_stream();
        let _second = rp.attach_stream();

        let got_replaced = tokio::select! {
            biased;
            res = &mut first.replaced => res.is_ok(),
            _ = first.commands.recv() => false,
        };
        assert!(got_replaced, "replaced signal must win over command EOF");
    }
}
