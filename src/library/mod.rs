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
}
