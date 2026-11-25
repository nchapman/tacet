use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, OnceLock};

use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::Semaphore;

use crate::library::Track;

#[derive(Debug, Clone, Default)]
pub struct TranscoderInfo {
    pub ffmpeg_path: Option<PathBuf>,
    pub has_libopus: bool,
    pub has_libfdk_aac: bool,
    pub has_libsoxr: bool,
}

impl TranscoderInfo {
    pub fn available(&self) -> bool {
        self.ffmpeg_path.is_some() && self.has_libopus
    }
}

static TRANSCODER: OnceLock<TranscoderInfo> = OnceLock::new();
static TRANSCODE_SEM: OnceLock<Arc<Semaphore>> = OnceLock::new();

pub fn transcode_semaphore() -> Arc<Semaphore> {
    Arc::clone(TRANSCODE_SEM.get_or_init(|| {
        let slots = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .max(4);
        Arc::new(Semaphore::new(slots))
    }))
}

pub fn detect_transcoder() -> &'static TranscoderInfo {
    TRANSCODER.get_or_init(probe_transcoder)
}

fn probe_transcoder() -> TranscoderInfo {
    let Some(path) = which_ffmpeg() else {
        return TranscoderInfo::default();
    };
    let Ok(out) = std::process::Command::new(&path)
        .args(["-hide_banner", "-encoders"])
        .output()
    else {
        return TranscoderInfo {
            ffmpeg_path: Some(path),
            ..Default::default()
        };
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut has_libopus = false;
    let mut has_libfdk_aac = false;
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        if let (Some(flags), Some(enc)) = (fields.next(), fields.next())
            && flags.starts_with('A')
        {
            match enc {
                "libopus" => has_libopus = true,
                "libfdk_aac" => has_libfdk_aac = true,
                _ => {}
            }
        }
    }
    let has_libsoxr = std::process::Command::new(&path)
        .args(["-hide_banner", "-buildconf"])
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("--enable-libsoxr"));

    TranscoderInfo {
        ffmpeg_path: Some(path),
        has_libopus,
        has_libfdk_aac,
        has_libsoxr,
    }
}

fn which_ffmpeg() -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let full = dir.join("ffmpeg");
        if full.is_file() {
            return Some(full);
        }
    }
    None
}

#[derive(Debug, Clone)]
pub struct TranscodeSpec {
    pub quality: String,
    pub mime: &'static str,
    pub filter_args: Vec<String>,
    pub codec_args: Vec<String>,
    pub fmt_args: Vec<String>,
    pub wav_header: Option<Vec<u8>>,
    pub exact_length: Option<u64>,
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
}

pub fn parse_transcode_spec(
    quality: &str,
    container: &str,
    track: &Track,
    info: &TranscoderInfo,
) -> Option<TranscodeSpec> {
    let q = quality.trim().to_ascii_lowercase();
    if q.is_empty() || q == "original" {
        return None;
    }

    if q == "wav" {
        return Some(build_wav_spec(track, info.has_libsoxr));
    }

    let bitrate = match q.as_str() {
        "opus_96" => "96k",
        "opus_128" => "128k",
        "opus_160" => "160k",
        "opus_256" => "256k",
        _ => return None,
    };

    let c = container.trim().to_ascii_lowercase();
    if c == "adts" || c == "aac" {
        return Some(build_aac_spec(
            &q,
            bitrate,
            info.has_libsoxr,
            info.has_libfdk_aac,
        ));
    }

    let af = if info.has_libsoxr {
        "aresample=48000:resampler=soxr:precision=28:osf=flt:rematrix_maxval=1.0,aformat=channel_layouts=mono|stereo"
    } else {
        "aresample=48000:resampler=swr:filter_size=128:phase_shift=10:kaiser_beta=12:cutoff=0.96:tsf=fltp:osf=flt:rematrix_maxval=1.0,aformat=channel_layouts=mono|stereo"
    };

    let (mime, fmt_args) = if c == "webm" {
        (
            "audio/webm; codecs=\"opus\"",
            vec!["-f".into(), "webm".into()],
        )
    } else {
        (
            "audio/ogg; codecs=\"opus\"",
            vec!["-f".into(), "ogg".into()],
        )
    };

    Some(TranscodeSpec {
        quality: q,
        mime,
        filter_args: vec!["-af".into(), af.into()],
        codec_args: vec![
            "-c:a".into(),
            "libopus".into(),
            "-b:a".into(),
            bitrate.into(),
            "-vbr".into(),
            "on".into(),
            "-compression_level".into(),
            "10".into(),
            "-application".into(),
            "audio".into(),
        ],
        fmt_args,
        wav_header: None,
        exact_length: None,
        sample_rate: 48000,
        channels: 2,
        bits_per_sample: 16,
    })
}

fn build_aac_spec(quality: &str, bitrate: &str, has_soxr: bool, has_fdk: bool) -> TranscodeSpec {
    let af = if has_soxr {
        "aresample=48000:resampler=soxr:precision=28:osf=flt:rematrix_maxval=1.0,aformat=channel_layouts=mono|stereo"
    } else {
        "aresample=48000:resampler=swr:osf=flt:rematrix_maxval=1.0,aformat=channel_layouts=mono|stereo"
    };
    let codec = if has_fdk { "libfdk_aac" } else { "aac" };
    TranscodeSpec {
        quality: quality.into(),
        mime: "audio/aac",
        filter_args: vec!["-af".into(), af.into()],
        codec_args: vec!["-c:a".into(), codec.into(), "-b:a".into(), bitrate.into()],
        fmt_args: vec!["-f".into(), "adts".into()],
        wav_header: None,
        exact_length: None,
        sample_rate: 48000,
        channels: 2,
        bits_per_sample: 16,
    }
}

fn build_wav_spec(track: &Track, has_soxr: bool) -> TranscodeSpec {
    let is_dsd = track.ext.eq_ignore_ascii_case(".dsf") || track.mime.contains("dsf");
    let sr: u32 = if is_dsd {
        88_200
    } else if track.sample_rate > 0 {
        (track.sample_rate as u32).clamp(8000, 192_000)
    } else {
        44_100
    };
    let ch: u16 = if track.channels > 0 {
        (track.channels as u16).clamp(1, 2)
    } else {
        2
    };
    let bps: u16 = if is_dsd || track.bit_depth > 16 {
        24
    } else {
        16
    };
    let (pcm_codec, pcm_fmt) = if bps == 24 {
        ("pcm_s24le", "s24le")
    } else {
        ("pcm_s16le", "s16le")
    };

    let mut filter_args = Vec::new();
    if is_dsd {
        let af = if has_soxr {
            format!("aresample={sr}:resampler=soxr:precision=28:dither_method=triangular")
        } else {
            format!("aresample={sr}")
        };
        filter_args = vec!["-af".into(), af];
    }

    let wav_header = build_wav_header(sr, ch, bps, 0xFFFF_FFFF);

    TranscodeSpec {
        quality: "wav".into(),
        mime: "audio/wav",
        filter_args,
        codec_args: vec![
            "-c:a".into(),
            pcm_codec.into(),
            "-ar".into(),
            sr.to_string(),
            "-ac".into(),
            ch.to_string(),
        ],
        fmt_args: vec!["-f".into(), pcm_fmt.into()],
        wav_header: Some(wav_header),
        exact_length: None,
        sample_rate: sr,
        channels: ch,
        bits_per_sample: bps,
    }
}

pub fn build_wav_header(
    sample_rate: u32,
    channels: u16,
    bits_per_sample: u16,
    data_size: u32,
) -> Vec<u8> {
    let block_align = channels * (bits_per_sample / 8);
    let byte_rate = sample_rate * block_align as u32;
    let riff_size = data_size.saturating_add(36);

    let mut hdr = Vec::with_capacity(44);
    hdr.extend_from_slice(b"RIFF");
    hdr.extend_from_slice(&riff_size.to_le_bytes());
    hdr.extend_from_slice(b"WAVE");
    hdr.extend_from_slice(b"fmt ");
    hdr.extend_from_slice(&16u32.to_le_bytes());
    hdr.extend_from_slice(&1u16.to_le_bytes()); // PCM
    hdr.extend_from_slice(&channels.to_le_bytes());
    hdr.extend_from_slice(&sample_rate.to_le_bytes());
    hdr.extend_from_slice(&byte_rate.to_le_bytes());
    hdr.extend_from_slice(&block_align.to_le_bytes());
    hdr.extend_from_slice(&bits_per_sample.to_le_bytes());
    hdr.extend_from_slice(b"data");
    hdr.extend_from_slice(&data_size.to_le_bytes());
    hdr
}

/// Spawns `ffmpeg` to transcode `input` (file path or HTTPS URL) according to `spec`
/// and streams stdout chunks into an `axum::body::Body`.
///
/// Acquires a concurrency permit before spawning and kills/reaps `ffmpeg` immediately
/// if the HTTP client disconnects (`tx.closed()`).
pub async fn spawn_ffmpeg_stream(
    ffmpeg_path: &Path,
    input: &str,
    spec: &TranscodeSpec,
    seek_sec: Option<f64>,
) -> std::io::Result<axum::body::Body> {
    let permit = transcode_semaphore()
        .acquire_owned()
        .await
        .map_err(|_| std::io::Error::other("transcode semaphore closed"))?;

    let mut cmd = Command::new(ffmpeg_path);
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);
    if input.starts_with("http://") || input.starts_with("https://") {
        cmd.args([
            "-protocol_whitelist",
            "http,https,tls,tcp",
            "-rw_timeout",
            "15000000",
        ]);
    }
    if let Some(ss) = seek_sec
        && ss.is_finite()
        && ss > 0.0
        && ss < 86_400.0
    {
        cmd.args(["-ss", &format!("{ss:.3}")]);
    }
    cmd.args(["-i", input, "-vn", "-sn", "-dn"]);
    cmd.args(&spec.filter_args);
    cmd.args(&spec.codec_args);
    cmd.args(&spec.fmt_args);
    cmd.arg("pipe:1");
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::null());
    cmd.kill_on_drop(true);

    let mut child = cmd.spawn()?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("ffmpeg stdout unavailable"))?;
    let header_prefix = spec.wav_header.clone();
    let (tx, rx) = tokio::sync::mpsc::channel::<std::io::Result<bytes::Bytes>>(16);

    tokio::spawn(async move {
        let _permit = permit;
        if let Some(hdr) = header_prefix
            && tx.send(Ok(bytes::Bytes::from(hdr))).await.is_err()
        {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return;
        }
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let read_res = tokio::select! {
                biased;
                _ = tx.closed() => {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    return;
                }
                res = stdout.read(&mut buf) => res,
            };
            match read_res {
                Ok(0) => break,
                Ok(n) => {
                    if tx
                        .send(Ok(bytes::Bytes::copy_from_slice(&buf[..n])))
                        .await
                        .is_err()
                    {
                        let _ = child.kill().await;
                        let _ = child.wait().await;
                        return;
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(e)).await;
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    return;
                }
            }
        }
        let _ = child.wait().await;
    });

    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    });
    Ok(axum::body::Body::from_stream(stream))
}
