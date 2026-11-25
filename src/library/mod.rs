pub mod autoscan;
pub mod migrations;
pub mod probe;
pub mod scanner;
pub mod store;
pub mod types;

pub use autoscan::{AutoScanConfig, AutoScanner};
pub use probe::{AudioProps, probe_audio};
pub use scanner::Catalog;
pub use types::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sort_and_version_keys() {
        assert_eq!(sort_key("The Beatles"), "beatles");
        assert_eq!(sort_key("A Tribe Called Quest"), "tribe called quest");
        assert_eq!(
            version_key("Pink Floyd", "Dark Side of the Moon (2011 Remaster)"),
            version_key("Pink Floyd", "Dark Side of the Moon")
        );
        assert!(
            quality_rank("FLAC", 24, 192000, SOURCE_TYPE_LOCAL)
                > quality_rank("FLAC", 16, 44100, SOURCE_TYPE_REMOTE)
        );
    }

    #[test]
    fn store_prefs_credentials_and_search() {
        let cat = Catalog::new_in_memory(vec![]);
        assert_eq!(cat.schema_version().unwrap(), 2);

        cat.set_pref(PREF_STREAM_MODE, "direct").unwrap();
        assert_eq!(
            cat.get_pref(PREF_STREAM_MODE).unwrap().as_deref(),
            Some("direct")
        );
        cat.delete_pref(PREF_STREAM_MODE).unwrap();
        assert_eq!(cat.get_pref(PREF_STREAM_MODE).unwrap(), None);

        #[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
        struct Cred {
            token: String,
        }
        cat.set_provider_credentials(
            "qobuz",
            &Cred {
                token: "abc".into(),
            },
        )
        .unwrap();
        let got: Option<Cred> = cat.get_provider_credentials("qobuz").unwrap();
        assert_eq!(
            got,
            Some(Cred {
                token: "abc".into()
            })
        );
    }

    #[tokio::test]
    async fn scan_local_wav_and_fts_search() {
        let dir = tempfile::tempdir().unwrap();
        let alb_dir = dir.path().join("Kind of Blue");
        std::fs::create_dir_all(&alb_dir).unwrap();
        let wav_path = alb_dir.join("So What.wav");

        // Write a minimal valid 16-bit 44.1kHz stereo WAV header + 1 sec of silence.
        let mut wav = Vec::new();
        let data_size: u32 = 44100 * 4;
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_size).to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&2u16.to_le_bytes()); // stereo
        wav.extend_from_slice(&44100u32.to_le_bytes());
        wav.extend_from_slice(&(44100u32 * 4).to_le_bytes());
        wav.extend_from_slice(&4u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_size.to_le_bytes());
        wav.resize(44 + data_size as usize, 0);
        std::fs::write(&wav_path, &wav).unwrap();

        let cat = Catalog::new_in_memory(vec![dir.path().to_path_buf()]);
        let n = cat.scan().await.unwrap();
        assert_eq!(n, 1);

        let (albums, total) = cat.query_albums(&AlbumFilter::default()).unwrap();
        assert_eq!(total, 1);
        assert_eq!(albums[0].title, "Kind of Blue");
        assert_eq!(albums[0].max_sample_rate, 44100);
        assert_eq!(albums[0].max_bit_depth, 16);

        let search = cat.unified_search("So What", 10).unwrap();
        assert_eq!(search.tracks.len(), 1);
        assert_eq!(search.tracks[0].title, "So What");

        // Record play & check heavy rotation.
        cat.record_play(&search.tracks[0]);
        let hr = cat.heavy_rotation(30, 10).unwrap();
        assert_eq!(hr.len(), 1);
        assert_eq!(hr[0].recent_plays, 1);
    }

    #[tokio::test]
    async fn unmounted_root_preserves_library_and_prefer_local_uses_etag_cache() {
        let dir = tempfile::tempdir().unwrap();
        let cat = Catalog::new_in_memory(vec![dir.path().to_path_buf()]);
        let alb = Album {
            id: "local:a1".into(),
            source: SOURCE_LOCAL.into(),
            source_type: SOURCE_TYPE_LOCAL.into(),
            title: "A Love Supreme".into(),
            artist: "John Coltrane".into(),
            available: true,
            in_library: true,
            ..Default::default()
        };
        let tr = Track {
            id: "local:t1".into(),
            album_id: alb.id.clone(),
            source: SOURCE_LOCAL.into(),
            source_type: SOURCE_TYPE_LOCAL.into(),
            title: "Acknowledgement".into(),
            artist: "John Coltrane".into(),
            album: "A Love Supreme".into(),
            available: true,
            in_library: true,
            ..Default::default()
        };
        cat.store()
            .upsert_album_with_tracks(&alb, std::slice::from_ref(&tr))
            .unwrap();

        // prefer_local_albums swaps a remote duplicate for the local album and caches by ETag
        let remote_dup = Album {
            id: "qobuz:999".into(),
            source: "qobuz".into(),
            source_type: SOURCE_TYPE_REMOTE.into(),
            provider: "qobuz".into(),
            title: "A Love Supreme".into(),
            artist: "John Coltrane".into(),
            ..Default::default()
        };
        let preferred = cat.prefer_local_albums(vec![remote_dup.clone()]);
        assert_eq!(preferred[0].id, "local:a1");
        let preferred2 = cat.prefer_local_albums(vec![remote_dup]);
        assert_eq!(preferred2[0].id, "local:a1");

        // Adding a new local album bumps the ETag and invalidates the cached lookup map
        let alb2 = Album {
            id: "local:a2".into(),
            source: SOURCE_LOCAL.into(),
            source_type: SOURCE_TYPE_LOCAL.into(),
            title: "Giant Steps".into(),
            artist: "John Coltrane".into(),
            available: true,
            in_library: true,
            ..Default::default()
        };
        cat.store().upsert_album_with_tracks(&alb2, &[]).unwrap();
        let remote_dup2 = Album {
            id: "qobuz:1000".into(),
            source: "qobuz".into(),
            source_type: SOURCE_TYPE_REMOTE.into(),
            provider: "qobuz".into(),
            title: "Giant Steps".into(),
            artist: "John Coltrane".into(),
            ..Default::default()
        };
        let preferred3 = cat.prefer_local_albums(vec![remote_dup2]);
        assert_eq!(preferred3[0].id, "local:a2");

        // Remove the configured root directory to simulate an unmounted NAS
        std::fs::remove_dir_all(dir.path()).unwrap();
        let scanned = cat.scan().await.unwrap();
        assert_eq!(scanned, 0);
        // Existing local album must NOT be wiped
        assert!(cat.has_album("local:a1"));
    }

    #[test]
    fn probe_dsf_bounds_absurd_sample_count_without_panic() {
        let dir = tempfile::tempdir().unwrap();
        let dsf_path = dir.path().join("crafted.dsf");
        let mut hdr = [0u8; 80];
        hdr[0..4].copy_from_slice(b"DSD ");
        hdr[28..32].copy_from_slice(b"fmt ");
        hdr[28 + 24..28 + 28].copy_from_slice(&2u32.to_le_bytes());
        hdr[28 + 28..28 + 32].copy_from_slice(&1u32.to_le_bytes());
        hdr[28 + 32..28 + 36].copy_from_slice(&1u32.to_le_bytes());
        hdr[28 + 36..28 + 44].copy_from_slice(&u64::MAX.to_le_bytes());
        std::fs::write(&dsf_path, hdr).unwrap();

        let props = probe::probe_audio(&dsf_path);
        assert_eq!(props.duration, std::time::Duration::ZERO);
    }
}
