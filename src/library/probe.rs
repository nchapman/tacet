use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::Duration;

use lofty::file::AudioFile;
use lofty::prelude::TaggedFileExt;
use lofty::probe::Probe;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AudioProps {
    pub sample_rate: i32,
    pub bit_depth: i32,
    pub channels: i32,
    pub total_samples: u64,
    pub duration: Duration,
}

/// Read stream-level audio properties (sample rate, bit depth, channels, duration).
/// Uses fast header parsers for FLAC/WAV/AIFF/DSF and falls back to `lofty` for
/// MP3/M4A/ALAC/OGG/Opus (and any format where header probing returns empty).
pub fn probe_audio(path: &Path) -> AudioProps {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| format!(".{}", s.to_ascii_lowercase()))
        .unwrap_or_default();

    if let Ok(mut f) = File::open(path) {
        let header_props = match ext.as_str() {
            ".flac" => probe_flac(&mut f),
            ".wav" => probe_wav(&mut f),
            ".aiff" | ".aif" => probe_aiff(&mut f),
            ".dsf" => probe_dsf(&mut f),
            _ => AudioProps::default(),
        };
        if header_props.sample_rate > 0 {
            return header_props;
        }
    }

    if let Ok(probe) = Probe::open(path)
        && let Ok(tagged) = probe.read()
    {
        let p = tagged.properties();
        return AudioProps {
            sample_rate: p.sample_rate().unwrap_or(0) as i32,
            bit_depth: p.bit_depth().unwrap_or(0) as i32,
            channels: p.channels().unwrap_or(0) as i32,
            total_samples: 0,
            duration: p.duration(),
        };
    }

    AudioProps::default()
}

/// Extract embedded picture bytes and MIME type using `lofty`.
pub fn read_embedded_art(path: &Path, max_bytes: usize) -> Option<(Vec<u8>, String)> {
    let tagged = Probe::open(path).ok()?.read().ok()?;
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag())?;
    let pic = tag.pictures().first()?;
    let data = pic.data();
    if data.is_empty() || data.len() > max_bytes {
        return None;
    }
    let mime = pic
        .mime_type()
        .map(|m| m.as_str().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "image/jpeg".to_string());
    Some((data.to_vec(), mime))
}

fn probe_flac<R: Read + Seek>(r: &mut R) -> AudioProps {
    let mut hdr = [0u8; 42];
    if r.read_exact(&mut hdr).is_err() {
        return AudioProps::default();
    }
    if &hdr[0..4] != b"fLaC" {
        return AudioProps::default();
    }
    let block_type = hdr[4] & 0x7F;
    let block_len = ((hdr[5] as usize) << 16) | ((hdr[6] as usize) << 8) | (hdr[7] as usize);
    if block_type != 0 || block_len < 34 {
        return AudioProps::default();
    }
    let si = &hdr[8..42];
    let sample_rate = ((si[10] as i32) << 12) | ((si[11] as i32) << 4) | ((si[12] as i32) >> 4);
    let channels = (((si[12] >> 1) & 0x07) as i32) + 1;
    let bit_depth = ((((si[12] & 0x01) << 4) | (si[13] >> 4)) as i32) + 1;
    let total_samples = ((u64::from(si[13] & 0x0F)) << 32)
        | (u64::from(si[14]) << 24)
        | (u64::from(si[15]) << 16)
        | (u64::from(si[16]) << 8)
        | u64::from(si[17]);

    let duration = if sample_rate > 0 && total_samples > 0 {
        Duration::from_secs_f64(total_samples as f64 / sample_rate as f64)
    } else {
        Duration::ZERO
    };
    AudioProps {
        sample_rate,
        bit_depth,
        channels,
        total_samples,
        duration,
    }
}

fn probe_wav<R: Read + Seek>(r: &mut R) -> AudioProps {
    let mut riff = [0u8; 12];
    if r.read_exact(&mut riff).is_err() {
        return AudioProps::default();
    }
    if &riff[0..4] != b"RIFF" || &riff[8..12] != b"WAVE" {
        return AudioProps::default();
    }
    let mut props = AudioProps::default();
    let mut byte_rate = 0u32;
    let mut data_size = 0u32;

    let mut chdr = [0u8; 8];
    for _ in 0..64 {
        if r.read_exact(&mut chdr).is_err() {
            break;
        }
        let size = u32::from_le_bytes([chdr[4], chdr[5], chdr[6], chdr[7]]);
        let mut next_offset = i64::from(size);
        if next_offset % 2 == 1 {
            next_offset += 1;
        }
        match &chdr[0..4] {
            b"fmt " => {
                if size >= 16 {
                    let mut fmt_buf = [0u8; 16];
                    if r.read_exact(&mut fmt_buf).is_err() {
                        return props;
                    }
                    props.channels = i32::from(u16::from_le_bytes([fmt_buf[2], fmt_buf[3]]));
                    props.sample_rate =
                        u32::from_le_bytes([fmt_buf[4], fmt_buf[5], fmt_buf[6], fmt_buf[7]]) as i32;
                    byte_rate =
                        u32::from_le_bytes([fmt_buf[8], fmt_buf[9], fmt_buf[10], fmt_buf[11]]);
                    props.bit_depth = i32::from(u16::from_le_bytes([fmt_buf[14], fmt_buf[15]]));
                    if next_offset > 16 {
                        let _ = r.seek(SeekFrom::Current(next_offset - 16));
                    }
                } else {
                    let _ = r.seek(SeekFrom::Current(next_offset));
                }
            }
            b"data" => {
                data_size = size;
                if props.sample_rate > 0 {
                    break;
                }
                let _ = r.seek(SeekFrom::Current(next_offset));
            }
            _ => {
                if r.seek(SeekFrom::Current(next_offset)).is_err() {
                    break;
                }
            }
        }
    }

    if byte_rate > 0 && data_size > 0 && data_size != 0xFFFF_FFFF {
        props.duration = Duration::from_secs_f64(f64::from(data_size) / f64::from(byte_rate));
        if props.channels > 0 && props.bit_depth > 0 {
            let bytes_per_frame = (props.channels as u64) * (((props.bit_depth + 7) / 8) as u64);
            if let Some(samples) = u64::from(data_size).checked_div(bytes_per_frame) {
                props.total_samples = samples;
            }
        }
    }
    props
}

fn probe_aiff<R: Read + Seek>(r: &mut R) -> AudioProps {
    let mut form = [0u8; 12];
    if r.read_exact(&mut form).is_err() {
        return AudioProps::default();
    }
    if &form[0..4] != b"FORM" || (&form[8..12] != b"AIFF" && &form[8..12] != b"AIFC") {
        return AudioProps::default();
    }

    let mut chdr = [0u8; 8];
    for _ in 0..64 {
        if r.read_exact(&mut chdr).is_err() {
            break;
        }
        let size = u32::from_be_bytes([chdr[4], chdr[5], chdr[6], chdr[7]]);
        let mut next_offset = i64::from(size);
        if next_offset % 2 == 1 {
            next_offset += 1;
        }
        if &chdr[0..4] == b"COMM" && size >= 18 {
            let mut comm = [0u8; 18];
            if r.read_exact(&mut comm).is_err() {
                return AudioProps::default();
            }
            let channels = i32::from(u16::from_be_bytes([comm[0], comm[1]]));
            let num_frames = u32::from_be_bytes([comm[2], comm[3], comm[4], comm[5]]);
            let bit_depth = i32::from(u16::from_be_bytes([comm[6], comm[7]]));
            let sample_rate = decode_ieee80(&comm[8..18]).round() as i32;
            let duration = if sample_rate > 0 && num_frames > 0 {
                Duration::from_secs_f64(f64::from(num_frames) / f64::from(sample_rate))
            } else {
                Duration::ZERO
            };
            return AudioProps {
                sample_rate,
                bit_depth,
                channels,
                total_samples: u64::from(num_frames),
                duration,
            };
        }
        if r.seek(SeekFrom::Current(next_offset)).is_err() {
            break;
        }
    }
    AudioProps::default()
}

fn probe_dsf<R: Read + Seek>(r: &mut R) -> AudioProps {
    let mut hdr = [0u8; 80];
    if r.read_exact(&mut hdr).is_err() {
        return AudioProps::default();
    }
    if &hdr[0..4] != b"DSD " || &hdr[28..32] != b"fmt " {
        return AudioProps::default();
    }
    let f = &hdr[28..];
    let channels = u32::from_le_bytes([f[24], f[25], f[26], f[27]]) as i32;
    let sample_rate = u32::from_le_bytes([f[28], f[29], f[30], f[31]]) as i32;
    let bit_depth = u32::from_le_bytes([f[32], f[33], f[34], f[35]]) as i32;
    let sample_count = u64::from_le_bytes([f[36], f[37], f[38], f[39], f[40], f[41], f[42], f[43]]);
    let duration = if sample_rate > 0 && sample_count > 0 {
        let secs = sample_count as f64 / f64::from(sample_rate);
        if secs.is_finite() && (0.0..1e12).contains(&secs) {
            Duration::from_secs_f64(secs)
        } else {
            Duration::ZERO
        }
    } else {
        Duration::ZERO
    };
    AudioProps {
        sample_rate,
        bit_depth,
        channels,
        total_samples: sample_count,
        duration,
    }
}

fn decode_ieee80(b: &[u8]) -> f64 {
    if b.len() != 10 {
        return 0.0;
    }
    let mut exp = i32::from(u16::from_be_bytes([b[0], b[1]]));
    let sign = if exp & 0x8000 != 0 {
        exp &= 0x7FFF;
        -1.0
    } else {
        1.0
    };
    let mant = u64::from_be_bytes([b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9]]);
    if (exp == 0 && mant == 0) || exp == 0x7FFF {
        return 0.0;
    }
    sign * (mant as f64) * 2.0_f64.powi(exp - 16383 - 63)
}
