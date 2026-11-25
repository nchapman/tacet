pub mod art;
pub mod server;
pub mod transcode;

pub use art::{
    ArtCache, ArtEncoders, accepts_webp, detect_art_encoders, is_art_hash, parse_art_bucket,
};
pub use server::{MediaHandler, is_allowed_remote_art_host, parse_byte_range};
pub use transcode::{
    TranscodeSpec, TranscoderInfo, build_wav_header, detect_transcoder, parse_transcode_spec,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_range_and_webp_accept() {
        assert_eq!(parse_byte_range("bytes=0-99", 1000), Some((0, 99)));
        assert_eq!(parse_byte_range("bytes=500-", 1000), Some((500, 999)));
        assert_eq!(parse_byte_range("bytes=-100", 1000), Some((900, 999)));
        assert_eq!(parse_byte_range("bytes=1000-1005", 1000), None);

        assert!(accepts_webp("image/avif,image/webp,image/png,*/*;q=0.8"));
        assert!(!accepts_webp("*/*"));
        assert!(!accepts_webp("image/webp;q=0, image/jpeg"));
        assert_eq!(parse_art_bucket(Some("300")), art::ART_SIZE_SMALL);
        assert_eq!(parse_art_bucket(Some("800")), art::ART_SIZE_LARGE);
        assert!(is_allowed_remote_art_host("static.qobuz.com"));
        assert!(!is_allowed_remote_art_host("evil.example.com"));
    }

    #[test]
    fn wav_header_structure() {
        let hdr = build_wav_header(44100, 2, 16, 44100 * 4);
        assert_eq!(hdr.len(), 44);
        assert_eq!(&hdr[0..4], b"RIFF");
        assert_eq!(&hdr[8..12], b"WAVE");
        assert_eq!(&hdr[36..40], b"data");
    }

    #[test]
    fn art_cache_rejects_non_image_raw_bytes() {
        let cache = ArtCache::new(None);
        let res = cache.get_or_build("secret", art::ART_SIZE_SMALL, art::ART_FORMAT_JPEG, || {
            Some((
                b"not an image: ssh-rsa AAAAB3...".to_vec(),
                "image/jpeg".into(),
            ))
        });
        assert!(res.is_none());
    }
}
