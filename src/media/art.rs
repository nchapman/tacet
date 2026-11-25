use std::collections::HashMap;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use fast_image_resize::{FilterType, ResizeAlg, ResizeOptions, Resizer, images::Image};
use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, ImageFormat, Rgba};
use sha2::{Digest, Sha256};

pub const ART_SIZE_SMALL: u32 = 400;
pub const ART_SIZE_LARGE: u32 = 1000;
const ART_JPEG_QUALITY: u8 = 80;
const MAX_ART_DIMENSION: u32 = 8192;
const MAX_ART_PIXELS: u64 = 40_000_000;
const MAX_MEM_CACHE_ENTRIES: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArtFormat {
    pub ext: &'static str,
    pub mime: &'static str,
}

pub const ART_FORMAT_JPEG: ArtFormat = ArtFormat {
    ext: "jpg",
    mime: "image/jpeg",
};
pub const ART_FORMAT_WEBP: ArtFormat = ArtFormat {
    ext: "webp",
    mime: "image/webp",
};

#[derive(Debug, Clone, Default)]
pub struct ArtEncoders {
    pub cwebp_path: Option<PathBuf>,
    pub cjpeg_path: Option<PathBuf>,
}

static ART_ENC: OnceLock<ArtEncoders> = OnceLock::new();

pub fn detect_art_encoders() -> &'static ArtEncoders {
    ART_ENC.get_or_init(|| {
        let cwebp_path = which_bin("cwebp");
        let mut cjpeg_path = None;
        for cand in [
            "/opt/homebrew/opt/mozjpeg/bin/cjpeg",
            "/usr/local/opt/mozjpeg/bin/cjpeg",
        ] {
            let p = PathBuf::from(cand);
            if p.is_file() {
                cjpeg_path = Some(p);
                break;
            }
        }
        if cjpeg_path.is_none() {
            cjpeg_path = which_bin("cjpeg");
        }
        ArtEncoders {
            cwebp_path,
            cjpeg_path,
        }
    })
}

fn which_bin(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let full = dir.join(name);
        if full.is_file() {
            return Some(full);
        }
    }
    None
}

/// Reports whether an HTTP `Accept` header explicitly advertises `image/webp`
/// (never matching bare `*/*` so UPnP/DLNA renderers always receive JPEG).
pub fn accepts_webp(accept: &str) -> bool {
    for part in accept.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (media_type, params) = part
            .split_once(';')
            .map(|(m, p)| (m.trim(), p))
            .unwrap_or((part, ""));
        if !media_type.eq_ignore_ascii_case("image/webp") {
            continue;
        }
        let zero_q = params.split(';').any(|p| {
            p.trim().split_once('=').is_some_and(|(k, v)| {
                k.trim().eq_ignore_ascii_case("q")
                    && v.trim().parse::<f64>().is_ok_and(|q| q <= 0.0)
            })
        });
        return !zero_q;
    }
    false
}

pub fn parse_art_bucket(raw: Option<&str>) -> u32 {
    let Some(s) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return ART_SIZE_LARGE;
    };
    match s.parse::<u32>() {
        Ok(w) if w > 0 && w <= 500 => ART_SIZE_SMALL,
        _ => ART_SIZE_LARGE,
    }
}

pub fn is_art_hash(id: &str) -> bool {
    id.len() == 16 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

pub fn hash_art_key(s: &str) -> String {
    let sum = Sha256::digest(s.as_bytes());
    hex::encode(&sum[..8])
}

pub struct ArtCache {
    dir: Option<PathBuf>,
    mem: Mutex<HashMap<String, (Vec<u8>, &'static str)>>,
}

impl ArtCache {
    pub fn new(dir: Option<PathBuf>) -> Arc<Self> {
        if let Some(ref d) = dir {
            let _ = std::fs::create_dir_all(d);
        }
        Arc::new(Self {
            dir,
            mem: Mutex::new(HashMap::new()),
        })
    }

    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    pub fn preferred_format(&self) -> ArtFormat {
        if detect_art_encoders().cwebp_path.is_some() {
            ART_FORMAT_WEBP
        } else {
            ART_FORMAT_JPEG
        }
    }

    fn disk_path(&self, key: &str, size: u32, fmt: ArtFormat) -> Option<PathBuf> {
        let dir = self.dir.as_ref()?;
        let safe_key: String = key
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
            .collect();
        if safe_key.is_empty() {
            return None;
        }
        Some(dir.join(format!("{safe_key}_{size}.{}", fmt.ext)))
    }

    pub fn exists_disk(&self, key: &str, size: u32, fmt: ArtFormat) -> bool {
        self.disk_path(key, size, fmt).is_some_and(|p| p.is_file())
    }

    pub fn get_or_build<F>(
        &self,
        key: &str,
        size: u32,
        fmt: ArtFormat,
        loader: F,
    ) -> Option<(Vec<u8>, &'static str)>
    where
        F: FnOnce() -> Option<(Vec<u8>, String)>,
    {
        let cache_key = format!("{key}_{size}.{}", fmt.ext);
        if let Some(hit) = self
            .mem
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&cache_key)
            .cloned()
        {
            return Some(hit);
        }

        if let Some(path) = self.disk_path(key, size, fmt)
            && let Ok(bytes) = std::fs::read(&path)
            && !bytes.is_empty()
        {
            self.put_mem(cache_key, bytes.clone(), fmt.mime);
            return Some((bytes, fmt.mime));
        }

        let (raw, _orig_mime) = loader()?;
        // Never serve unvalidated raw bytes on decode failure (prevents arbitrary file / SSRF oracle).
        let (encoded, out_mime) = resize_and_encode(&raw, size, fmt)?;

        if let Some(path) = self.disk_path(key, size, fmt) {
            let suffix: u64 = rand::random();
            let tmp = path.with_file_name(format!(
                "{}.{suffix:016x}.tmp",
                path.file_name().and_then(|n| n.to_str()).unwrap_or("art")
            ));
            if std::fs::write(&tmp, &encoded).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
        self.put_mem(cache_key, encoded.clone(), out_mime);
        Some((encoded, out_mime))
    }

    fn put_mem(&self, key: String, bytes: Vec<u8>, mime: &'static str) {
        let mut g = self.mem.lock().unwrap_or_else(|e| e.into_inner());
        if g.len() >= MAX_MEM_CACHE_ENTRIES {
            g.clear();
        }
        g.insert(key, (bytes, mime));
    }
}

pub fn resize_and_encode(
    raw: &[u8],
    max_dim: u32,
    fmt: ArtFormat,
) -> Option<(Vec<u8>, &'static str)> {
    let reader = image::ImageReader::new(Cursor::new(raw))
        .with_guessed_format()
        .ok()?;
    let (w, h) = reader.into_dimensions().ok()?;
    if w == 0 || h == 0 || w > MAX_ART_DIMENSION || h > MAX_ART_DIMENSION {
        return None;
    }
    if (w as u64) * (h as u64) > MAX_ART_PIXELS {
        return None;
    }

    let img = image::load_from_memory(raw).ok()?;
    let (dst_w, dst_h) = fit_dimensions(w, h, max_dim);

    let resized_rgba = if dst_w != w || dst_h != h {
        let src_rgba = img.to_rgba8();
        let mut dst = Image::new(dst_w, dst_h, fast_image_resize::PixelType::U8x4);
        let mut resizer = Resizer::new();
        let opts = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Lanczos3));
        resizer.resize(&src_rgba, &mut dst, &opts).ok()?;
        image::RgbaImage::from_raw(dst_w, dst_h, dst.into_vec())?
    } else {
        img.to_rgba8()
    };

    // Flatten alpha onto dark background (#09090b)
    let mut rgb = image::RgbImage::new(dst_w, dst_h);
    for (x, y, Rgba([r, g, b, a])) in resized_rgba.enumerate_pixels() {
        let alpha = (*a as u32) * 255 / 255;
        let inv = 255 - alpha;
        let out_r = ((*r as u32 * alpha + 9 * inv) / 255) as u8;
        let out_g = ((*g as u32 * alpha + 9 * inv) / 255) as u8;
        let out_b = ((*b as u32 * alpha + 11 * inv) / 255) as u8;
        rgb.put_pixel(x, y, image::Rgb([out_r, out_g, out_b]));
    }

    let dyn_img = DynamicImage::ImageRgb8(rgb);
    if fmt == ART_FORMAT_WEBP {
        let mut out = Vec::new();
        if dyn_img
            .write_to(&mut Cursor::new(&mut out), ImageFormat::WebP)
            .is_ok()
            && !out.is_empty()
        {
            return Some((out, ART_FORMAT_WEBP.mime));
        }
    }

    let mut out = Vec::new();
    let mut enc = JpegEncoder::new_with_quality(&mut out, ART_JPEG_QUALITY);
    enc.encode_image(&dyn_img).ok()?;
    Some((out, ART_FORMAT_JPEG.mime))
}

fn fit_dimensions(w: u32, h: u32, max_dim: u32) -> (u32, u32) {
    let max_side = w.max(h);
    if max_side <= max_dim {
        return (w, h);
    }
    let scale = max_dim as f64 / max_side as f64;
    let nw = ((w as f64 * scale).round() as u32).max(1);
    let nh = ((h as f64 * scale).round() as u32).max(1);
    (nw, nh)
}
