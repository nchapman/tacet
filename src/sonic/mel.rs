use std::f32::consts::PI;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use rustfft::FftPlanner;
use rustfft::num_complex::Complex;

pub const SAMPLE_RATE: u32 = 16_000;
pub const FRAME_SIZE: usize = 512;
pub const HOP_SIZE: usize = 256;
pub const N_BINS: usize = FRAME_SIZE / 2 + 1;
pub const N_MELS: usize = 96;
pub const PATCH_FRAMES: usize = 128;
pub const PATCH_HOP: usize = 64;
/// Cap patches per track (evenly strided across the full song) so 20-minute tracks
/// stay fast and bounded in RAM while normal 2–5 minute tracks use all patches.
pub const MAX_PATCHES: usize = 48;

struct MelContext {
    window: [f32; FRAME_SIZE],
    /// Sparse triangular filterbank: for each mel band, `(start_bin, weights)`.
    filters: Vec<(usize, Vec<f32>)>,
}

static MEL_CTX: OnceLock<MelContext> = OnceLock::new();

fn mel_context() -> &'static MelContext {
    MEL_CTX.get_or_init(|| {
        let mut window = [0.0f32; FRAME_SIZE];
        for (n, w) in window.iter_mut().enumerate() {
            *w = 0.5 * (1.0 - (2.0 * PI * n as f32 / FRAME_SIZE as f32).cos());
        }
        let filters = build_slaney_mel_filterbank();
        MelContext { window, filters }
    })
}

fn hz_to_slaney_mel(hz: f64) -> f64 {
    const F_SP: f64 = 200.0 / 3.0;
    const MIN_LOG_HZ: f64 = 1000.0;
    const MIN_LOG_MEL: f64 = MIN_LOG_HZ / F_SP;
    let logstep = (6.4f64).ln() / 27.0;
    if hz >= MIN_LOG_HZ {
        MIN_LOG_MEL + (hz / MIN_LOG_HZ).ln() / logstep
    } else {
        hz / F_SP
    }
}

fn slaney_mel_to_hz(mel: f64) -> f64 {
    const F_SP: f64 = 200.0 / 3.0;
    const MIN_LOG_HZ: f64 = 1000.0;
    const MIN_LOG_MEL: f64 = MIN_LOG_HZ / F_SP;
    let logstep = (6.4f64).ln() / 27.0;
    if mel >= MIN_LOG_MEL {
        MIN_LOG_HZ * (logstep * (mel - MIN_LOG_MEL)).exp()
    } else {
        mel * F_SP
    }
}

fn build_slaney_mel_filterbank() -> Vec<(usize, Vec<f32>)> {
    let f_min = 0.0f64;
    let f_max = f64::from(SAMPLE_RATE) / 2.0;
    let mel_min = hz_to_slaney_mel(f_min);
    let mel_max = hz_to_slaney_mel(f_max);

    let n_pts = N_MELS + 2;
    let mel_pts: Vec<f64> = (0..n_pts)
        .map(|i| mel_min + (mel_max - mel_min) * (i as f64) / ((n_pts - 1) as f64))
        .collect();
    let hz_pts: Vec<f64> = mel_pts.iter().map(|&m| slaney_mel_to_hz(m)).collect();
    let fft_freqs: Vec<f64> = (0..N_BINS)
        .map(|k| f_max * (k as f64) / ((N_BINS - 1) as f64))
        .collect();

    let mut out = Vec::with_capacity(N_MELS);
    for m in 0..N_MELS {
        let f_left = hz_pts[m];
        let f_center = hz_pts[m + 1];
        let f_right = hz_pts[m + 2];
        let enorm = 2.0 / (f_right - f_left);

        let mut start_bin = None;
        let mut weights = Vec::new();
        for (k, &f) in fft_freqs.iter().enumerate() {
            let lower = (f - f_left) / (f_center - f_left);
            let upper = (f_right - f) / (f_right - f_center);
            let w = lower.min(upper).max(0.0) * enorm;
            if w > 0.0 {
                if start_bin.is_none() {
                    start_bin = Some(k);
                }
                weights.push(w as f32);
            } else if start_bin.is_some() {
                break;
            }
        }
        out.push((start_bin.unwrap_or(0), weights));
    }
    out
}

/// Computes `[num_patches, 128, 96]` row-major `f32` patches matching Essentia's
/// `TensorflowPredictEffnetDiscogs` frontend (`log10(1 + 10000 * mel)`).
pub fn compute_mel_patches(samples: &[f32]) -> (Vec<f32>, usize) {
    if samples.is_empty() {
        return (Vec::new(), 0);
    }
    let ctx = mel_context();
    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(FRAME_SIZE);

    let n_frames = if samples.len() <= FRAME_SIZE {
        1
    } else {
        (samples.len() - FRAME_SIZE) / HOP_SIZE + 1
    };

    let mut mel_frames = vec![0.0f32; n_frames * N_MELS];
    let mut buf = vec![Complex::new(0.0f32, 0.0f32); FRAME_SIZE];
    let mut power = [0.0f32; N_BINS];

    for frame_idx in 0..n_frames {
        let offset = frame_idx * HOP_SIZE;
        for (i, slot) in buf.iter_mut().enumerate() {
            let s = samples.get(offset + i).copied().unwrap_or(0.0);
            *slot = Complex::new(s * ctx.window[i], 0.0);
        }
        fft.process(&mut buf);
        for (k, p) in power.iter_mut().enumerate() {
            let c = buf[k];
            *p = c.re * c.re + c.im * c.im;
        }
        let dst = &mut mel_frames[frame_idx * N_MELS..(frame_idx + 1) * N_MELS];
        for (m, (start_bin, weights)) in ctx.filters.iter().enumerate() {
            let mut sum = 0.0f32;
            for (j, &w) in weights.iter().enumerate() {
                sum += power[*start_bin + j] * w;
            }
            dst[m] = (1.0 + 10_000.0 * sum.max(0.0)).log10();
        }
    }

    let raw_starts: Vec<usize> = if n_frames <= PATCH_FRAMES {
        vec![0]
    } else {
        (0..=n_frames - PATCH_FRAMES).step_by(PATCH_HOP).collect()
    };

    // Evenly subsample patch indices if the track exceeds MAX_PATCHES.
    let starts: Vec<usize> = if raw_starts.len() <= MAX_PATCHES {
        raw_starts
    } else {
        let last = raw_starts.len() - 1;
        (0..MAX_PATCHES)
            .map(|i| raw_starts[i * last / (MAX_PATCHES - 1)])
            .collect()
    };

    let num_patches = starts.len();
    let patch_elems = PATCH_FRAMES * N_MELS;
    let mut patches = vec![0.0f32; num_patches * patch_elems];

    for (p_idx, &start_frame) in starts.iter().enumerate() {
        let copy_frames = (n_frames.saturating_sub(start_frame)).min(PATCH_FRAMES);
        let src_start = start_frame * N_MELS;
        let src_end = src_start + copy_frames * N_MELS;
        let dst_start = p_idx * patch_elems;
        patches[dst_start..dst_start + copy_frames * N_MELS]
            .copy_from_slice(&mel_frames[src_start..src_end]);
    }

    (patches, num_patches)
}

const MAX_DECODE_SECS: &str = "600";
const MAX_DECODE_BYTES: u64 = (SAMPLE_RATE as u64) * 600 * 4;
const DECODE_TIMEOUT: Duration = Duration::from_secs(30);

/// Decodes up to 10 minutes of an audio file to mono 16 kHz `f32` PCM via `ffmpeg`
/// with bounded RAM and a hard 30s timeout.
pub fn decode_audio_16k_mono(ffmpeg_path: &Path, audio_path: &Path) -> Result<Vec<f32>, String> {
    let mut child = Command::new(ffmpeg_path)
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(audio_path)
        .args([
            "-t",
            MAX_DECODE_SECS,
            "-vn",
            "-ac",
            "1",
            "-ar",
            "16000",
            "-f",
            "f32le",
            "pipe:1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn ffmpeg failed: {e}"))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "missing ffmpeg stdout".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "missing ffmpeg stderr".to_string())?;

    let out_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.take(MAX_DECODE_BYTES).read_to_end(&mut buf);
        buf
    });
    let err_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.take(4096).read_to_end(&mut buf);
        buf
    });

    let deadline = Instant::now() + DECODE_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("ffmpeg decode timed out after 30s".into());
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                let _ = child.kill();
                return Err(format!("wait ffmpeg failed: {e}"));
            }
        }
    };

    let bytes = out_handle.join().unwrap_or_default();
    let err_bytes = err_handle.join().unwrap_or_default();

    if !status.success() {
        let err = String::from_utf8_lossy(&err_bytes);
        return Err(format!("ffmpeg decode failed: {}", err.trim()));
    }

    let (chunks, _) = bytes.as_chunks::<4>();
    let mut samples = Vec::with_capacity(chunks.len());
    for &chunk in chunks {
        samples.push(f32::from_le_bytes(chunk));
    }
    Ok(samples)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mel_patches_produce_expected_shape_and_finite_values() {
        // 3 seconds of 440 Hz sine wave at 16 kHz
        let n = SAMPLE_RATE as usize * 3;
        let samples: Vec<f32> = (0..n)
            .map(|i| (2.0 * PI * 440.0 * (i as f32) / (SAMPLE_RATE as f32)).sin() * 0.5)
            .collect();
        let (patches, num_patches) = compute_mel_patches(&samples);
        assert!(num_patches >= 1);
        assert_eq!(patches.len(), num_patches * PATCH_FRAMES * N_MELS);
        assert!(patches.iter().all(|v| v.is_finite() && *v >= 0.0));
        assert!(patches.iter().copied().fold(0.0f32, f32::max) > 1.0);
    }

    #[test]
    fn mel_patches_cap_long_tracks_at_max_patches() {
        // 20 minutes of silence/tone -> should cap cleanly at MAX_PATCHES
        let n = SAMPLE_RATE as usize * 60 * 10;
        let samples = vec![0.01f32; n];
        let (patches, num_patches) = compute_mel_patches(&samples);
        assert_eq!(num_patches, MAX_PATCHES);
        assert_eq!(patches.len(), MAX_PATCHES * PATCH_FRAMES * N_MELS);
    }
}
