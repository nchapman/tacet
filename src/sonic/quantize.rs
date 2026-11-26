pub const EMBEDDING_DIM: usize = 1280;

/// Raw (unnormalized) 1280-d embedding quantized to asymmetric `uint8` for compact SQLite storage
/// (1,288 bytes/track: 1,280 bytes + min + scale). Preserves raw vector magnitude so downstream
/// classification heads can be run or re-run later without re-decoding audio.
#[derive(Debug, Clone)]
pub struct QuantizedEmbedding {
    pub bytes: Vec<u8>,
    pub min: f32,
    pub scale: f32,
}

pub fn quantize_uint8(raw: &[f32]) -> QuantizedEmbedding {
    if raw.is_empty() {
        return QuantizedEmbedding {
            bytes: Vec::new(),
            min: 0.0,
            scale: 0.0,
        };
    }
    let mut min_v = f32::INFINITY;
    let mut max_v = f32::NEG_INFINITY;
    for &x in raw {
        if x.is_finite() {
            if x < min_v {
                min_v = x;
            }
            if x > max_v {
                max_v = x;
            }
        }
    }
    if !min_v.is_finite() || !max_v.is_finite() {
        return QuantizedEmbedding {
            bytes: vec![0u8; raw.len()],
            min: 0.0,
            scale: 0.0,
        };
    }
    let span = max_v - min_v;
    if span <= 1e-12 {
        return QuantizedEmbedding {
            bytes: vec![0u8; raw.len()],
            min: min_v,
            scale: 0.0,
        };
    }
    let scale = span / 255.0;
    let inv_scale = 1.0 / scale;
    let bytes: Vec<u8> = raw
        .iter()
        .map(|&x| {
            let clamped = if x.is_finite() { x } else { min_v };
            ((clamped - min_v) * inv_scale).round().clamp(0.0, 255.0) as u8
        })
        .collect();
    QuantizedEmbedding {
        bytes,
        min: min_v,
        scale,
    }
}

pub fn dequantize_uint8(bytes: &[u8], min: f32, scale: f32) -> Option<Vec<f32>> {
    if bytes.len() != EMBEDDING_DIM || !min.is_finite() || !scale.is_finite() {
        return None;
    }
    let out = bytes.iter().map(|&q| f32::from(q) * scale + min).collect();
    Some(out)
}

/// L2-normalized 1280-d embedding quantized to symmetric `int8` for RAM-resident integer SIMD
/// cosine search (1,284 bytes/track: ~12.8 MB per 10,000 tracks).
#[derive(Debug, Clone)]
pub struct Int8UnitVec {
    pub data: [i8; EMBEDDING_DIM],
    pub scale: f32,
}

impl Int8UnitVec {
    pub fn from_raw_f32(raw: &[f32]) -> Option<Self> {
        if raw.len() != EMBEDDING_DIM {
            return None;
        }
        let norm_sq: f64 = raw
            .iter()
            .map(|&x| {
                let v = if x.is_finite() { f64::from(x) } else { 0.0 };
                v * v
            })
            .sum();
        let norm = norm_sq.sqrt();
        if norm <= 1e-12 {
            return None;
        }
        let inv_norm = (1.0 / norm) as f32;

        let mut unit = [0.0f32; EMBEDDING_DIM];
        let mut max_abs = 0.0f32;
        for (dst, &x) in unit.iter_mut().zip(raw.iter()) {
            let v = if x.is_finite() { x * inv_norm } else { 0.0 };
            *dst = v;
            let a = v.abs();
            if a > max_abs {
                max_abs = a;
            }
        }
        if max_abs <= 1e-12 {
            return None;
        }
        let scale = max_abs / 127.0;
        let inv_scale = 1.0 / scale;
        let mut data = [0i8; EMBEDDING_DIM];
        for (dst, &v) in data.iter_mut().zip(unit.iter()) {
            *dst = (v * inv_scale).round().clamp(-127.0, 127.0) as i8;
        }
        Some(Self { data, scale })
    }

    pub fn from_uint8_blob(bytes: &[u8], min: f32, scale: f32) -> Option<Self> {
        let raw = dequantize_uint8(bytes, min, scale)?;
        Self::from_raw_f32(&raw)
    }

    #[inline]
    pub fn cosine_similarity(&self, other: &Self) -> f32 {
        let mut acc: i32 = 0;
        for i in 0..EMBEDDING_DIM {
            acc += i32::from(self.data[i]) * i32::from(other.data[i]);
        }
        (acc as f32) * (self.scale * other.scale)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uint8_and_int8_quantization_preserve_cosine_similarity() {
        let mut a = vec![0.0f32; EMBEDDING_DIM];
        let mut b = vec![0.0f32; EMBEDDING_DIM];
        for i in 0..EMBEDDING_DIM {
            let x = (i as f32 * 0.037).sin();
            let y = (i as f32 * 0.037 + 0.35).sin();
            a[i] = x * 0.8 + 0.1;
            b[i] = y * 0.6 - 0.05;
        }
        let exact_cos = {
            let dot: f64 = a
                .iter()
                .zip(b.iter())
                .map(|(&x, &y)| f64::from(x) * f64::from(y))
                .sum();
            let na: f64 = a
                .iter()
                .map(|&x| f64::from(x) * f64::from(x))
                .sum::<f64>()
                .sqrt();
            let nb: f64 = b
                .iter()
                .map(|&y| f64::from(y) * f64::from(y))
                .sum::<f64>()
                .sqrt();
            (dot / (na * nb)) as f32
        };

        let qa = quantize_uint8(&a);
        let qb = quantize_uint8(&b);
        let va = Int8UnitVec::from_uint8_blob(&qa.bytes, qa.min, qa.scale).unwrap();
        let vb = Int8UnitVec::from_uint8_blob(&qb.bytes, qb.min, qb.scale).unwrap();

        let approx_cos = va.cosine_similarity(&vb);
        assert!(
            (exact_cos - approx_cos).abs() < 0.005,
            "exact={exact_cos} approx={approx_cos}"
        );
        assert!((va.cosine_similarity(&va) - 1.0).abs() < 0.005);
    }
}
