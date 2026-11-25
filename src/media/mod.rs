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

    #[tokio::test]
    async fn media_handler_serves_range_wav_cover_art_and_rejects_symlink_and_ssrf() {
        use crate::library::Catalog;
        use axum::body::to_bytes;
        use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let alb1 = dir.path().join("alb1");
        let alb2 = dir.path().join("alb2");
        std::fs::create_dir_all(&alb1).unwrap();
        std::fs::create_dir_all(&alb2).unwrap();

        // Write valid 44-byte WAV header + 1764 bytes of silence
        let mut wav = build_wav_header(44100, 2, 16, 1764);
        wav.extend(std::iter::repeat_n(0u8, 1764));
        std::fs::write(alb1.join("01 - Tone.wav"), &wav).unwrap();
        std::fs::write(alb2.join("01 - Sym.wav"), &wav).unwrap();

        // Write a real 4x4 PNG cover in alb1
        let mut png_bytes = Vec::new();
        let img = image::RgbaImage::from_pixel(4, 4, image::Rgba([200, 100, 50, 255]));
        image::DynamicImage::ImageRgba8(img)
            .write_to(
                &mut std::io::Cursor::new(&mut png_bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        std::fs::write(alb1.join("cover.png"), &png_bytes).unwrap();

        // Put a symlink cover.jpg in alb2 (must be ignored by find_folder_art)
        #[cfg(unix)]
        std::os::unix::fs::symlink(alb1.join("cover.png"), alb2.join("cover.jpg")).unwrap();

        let cat = Catalog::new_in_memory(vec![dir.path().to_path_buf()]);
        cat.scan().await.unwrap();
        let tracks = cat.query_tracks(&Default::default()).unwrap().0;
        assert_eq!(tracks.len(), 2);
        let t1 = tracks
            .iter()
            .find(|t| t.file_path.contains("alb1"))
            .unwrap();
        let t2 = tracks
            .iter()
            .find(|t| t.file_path.contains("alb2"))
            .unwrap();

        let mh = MediaHandler::new(Arc::clone(&cat), None);
        let empty_hdrs = HeaderMap::new();
        let empty_q = std::collections::HashMap::new();
        let w300_q = std::collections::HashMap::from([("w".to_string(), "300".to_string())]);

        // Full GET 200
        let r_full = mh
            .serve_local(
                Method::GET,
                &format!("{}.wav", t1.id),
                &empty_q,
                &empty_hdrs,
            )
            .await;
        assert_eq!(r_full.status(), StatusCode::OK);
        let body = to_bytes(r_full.into_body(), 1024 * 1024).await.unwrap();
        assert_eq!(body.len(), wav.len());

        // Range GET 206
        let mut range_hdrs = HeaderMap::new();
        range_hdrs.insert(header::RANGE, HeaderValue::from_static("bytes=0-43"));
        let r_part = mh
            .serve_local(Method::GET, &t1.id, &empty_q, &range_hdrs)
            .await;
        assert_eq!(r_part.status(), StatusCode::PARTIAL_CONTENT);
        let part_body = to_bytes(r_part.into_body(), 4096).await.unwrap();
        assert_eq!(part_body.as_ref(), &wav[..44]);

        // Unsatisfiable range -> 416
        let mut bad_range = HeaderMap::new();
        bad_range.insert(header::RANGE, HeaderValue::from_static("bytes=999999-"));
        let r_416 = mh
            .serve_local(Method::GET, &t1.id, &empty_q, &bad_range)
            .await;
        assert_eq!(r_416.status(), StatusCode::RANGE_NOT_SATISFIABLE);

        // Missing track -> 404
        let r_404 = mh
            .serve_local(Method::GET, "local:missing", &empty_q, &empty_hdrs)
            .await;
        assert_eq!(r_404.status(), StatusCode::NOT_FOUND);

        // Real cover.png served & transcoded to JPEG/WebP
        let r_art = mh.serve_art(&t1.id, &w300_q, &empty_hdrs).await;
        assert_eq!(r_art.status(), StatusCode::OK);
        assert_eq!(
            r_art.headers().get(header::CONTENT_TYPE).unwrap(),
            "image/jpeg"
        );

        let mut webp_hdrs = HeaderMap::new();
        webp_hdrs.insert(header::ACCEPT, HeaderValue::from_static("image/webp"));
        let r_webp = mh.serve_art(&t1.id, &w300_q, &webp_hdrs).await;
        assert_eq!(r_webp.status(), StatusCode::OK);

        // Symlink cover.jpg in alb2 must NOT be served
        #[cfg(unix)]
        {
            let r_sym = mh.serve_art(&t2.id, &w300_q, &empty_hdrs).await;
            assert_eq!(r_sym.status(), StatusCode::NOT_FOUND);
        }

        // Remote art SSRF allowlist rejects non-Qobuz hosts & non-HTTP schemes
        let q_evil = std::collections::HashMap::from([(
            "url".to_string(),
            "https://evil.example.com/a.jpg".to_string(),
        )]);
        let r_evil = mh.serve_remote_art(&q_evil, &empty_hdrs).await;
        assert_eq!(r_evil.status(), StatusCode::FORBIDDEN);
        let q_scheme = std::collections::HashMap::from([(
            "url".to_string(),
            "file:///etc/passwd".to_string(),
        )]);
        let r_scheme = mh.serve_remote_art(&q_scheme, &empty_hdrs).await;
        assert_eq!(r_scheme.status(), StatusCode::FORBIDDEN);

        assert_seekable_wav_ranges(&mh, &t1.id).await;
    }

    async fn assert_seekable_wav_ranges(mh: &MediaHandler, track_id: &str) {
        use axum::body::to_bytes;
        use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};

        // Verify exact_length calculation unconditionally (does not require ffmpeg).
        let spec = parse_transcode_spec(
            "wav",
            "",
            &crate::library::Track {
                sample_rate: 44_100,
                bit_depth: 16,
                channels: 2,
                ..Default::default()
            },
            441,
            &TranscoderInfo::default(),
        )
        .expect("wav spec");
        assert_eq!(spec.exact_length, Some(44 + 441 * 4));

        if detect_transcoder().ffmpeg_path.is_none() {
            return;
        }
        let empty_hdrs = HeaderMap::new();
        let wav_q = std::collections::HashMap::from([("q".to_string(), "wav".to_string())]);
        let r_head = mh
            .serve_local(Method::HEAD, track_id, &wav_q, &empty_hdrs)
            .await;
        assert_eq!(r_head.status(), StatusCode::OK);
        assert_eq!(
            r_head.headers().get(header::ACCEPT_RANGES).unwrap(),
            "bytes"
        );
        let exact_len: u64 = r_head
            .headers()
            .get(header::CONTENT_LENGTH)
            .unwrap()
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(exact_len, 44 + 441 * 4);

        let mut hdr_range = HeaderMap::new();
        hdr_range.insert(header::RANGE, HeaderValue::from_static("bytes=0-43"));
        let r_hdr = mh
            .serve_local(Method::GET, track_id, &wav_q, &hdr_range)
            .await;
        assert_eq!(r_hdr.status(), StatusCode::PARTIAL_CONTENT);
        let hdr_bytes = to_bytes(r_hdr.into_body(), 1024).await.unwrap();
        assert_eq!(hdr_bytes.len(), 44);
        assert_eq!(&hdr_bytes[0..4], b"RIFF");
        assert_eq!(&hdr_bytes[8..12], b"WAVE");

        let mut mid_range = HeaderMap::new();
        mid_range.insert(header::RANGE, HeaderValue::from_static("bytes=444-643"));
        let r_mid = mh
            .serve_local(Method::GET, track_id, &wav_q, &mid_range)
            .await;
        assert_eq!(r_mid.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            r_mid.headers().get(header::CONTENT_RANGE).unwrap(),
            &format!("bytes 444-643/{exact_len}")
        );
        let mid_bytes = to_bytes(r_mid.into_body(), 4096).await.unwrap();
        assert_eq!(mid_bytes.len(), 200);

        let mut bad_range = HeaderMap::new();
        bad_range.insert(header::RANGE, HeaderValue::from_static("bytes=99999999-"));
        let r_oob = mh
            .serve_local(Method::GET, track_id, &wav_q, &bad_range)
            .await;
        assert_eq!(r_oob.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    }
}
