pub mod autoscan;
pub mod migrations;
pub mod probe;
pub mod scanner;
pub mod store;
pub mod types;

pub use autoscan::{AutoScanConfig, AutoScanner};
pub use probe::{AudioProps, probe_audio};
pub use scanner::Catalog;
pub use store::{AlbumEnrichmentUpdate, EnrichmentStats, EntityTag, TrackEnrichmentPatch};
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
        assert_eq!(cat.schema_version().unwrap(), 3);

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

    #[test]
    fn soft_delete_fts_track_search_and_partial_in_library_parity() {
        let store = super::store::LibraryStore::open(":memory:").unwrap();
        let alb = Album {
            id: "qobuz:a1".into(),
            source: SOURCE_QOBUZ.into(),
            source_type: SOURCE_TYPE_REMOTE.into(),
            provider: SOURCE_QOBUZ.into(),
            title: "Kind of Blue".into(),
            artist: "Miles Davis".into(),
            genre: "Jazz".into(),
            year: 1959,
            available: true,
            is_partial: false,
            ..Default::default()
        };
        let tr = Track {
            id: "qobuz:t1".into(),
            album_id: "qobuz:a1".into(),
            source: SOURCE_QOBUZ.into(),
            source_type: SOURCE_TYPE_REMOTE.into(),
            provider: SOURCE_QOBUZ.into(),
            title: "Flamenco Sketches".into(),
            artist: "Miles Davis".into(),
            album: "Kind of Blue".into(),
            available: true,
            ..Default::default()
        };
        store
            .replace_provider(
                SOURCE_QOBUZ,
                SOURCE_TYPE_REMOTE,
                std::slice::from_ref(&alb),
                &[tr],
                true,
            )
            .unwrap();

        // Searching albums by track title must surface the album via tracks_fts.
        let (found, total) = store
            .query_albums(&AlbumFilter {
                query: "Flamenco".into(),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(total, 1);
        assert_eq!(found[0].id, "qobuz:a1");
        assert!(found[0].in_library);

        // Partial album must report in_library = false and preserve genre/year when incoming blank.
        let partial = Album {
            id: "qobuz:a1".into(),
            source: SOURCE_QOBUZ.into(),
            source_type: SOURCE_TYPE_REMOTE.into(),
            provider: SOURCE_QOBUZ.into(),
            title: "Kind of Blue".into(),
            artist: "Miles Davis".into(),
            is_partial: true,
            available: true,
            ..Default::default()
        };
        store.upsert_album_with_tracks(&partial, &[]).unwrap();
        let got = store.get_album("qobuz:a1").unwrap().unwrap();
        assert!(!got.in_library);
        assert_eq!(got.genre, "Jazz");
        assert_eq!(got.year, 1959);

        // Soft-deleting provider removes rows from FTS and track lists.
        store
            .replace_provider(SOURCE_QOBUZ, SOURCE_TYPE_REMOTE, &[], &[], true)
            .unwrap();
        let (after, total_after) = store
            .query_albums(&AlbumFilter {
                query: "Flamenco".into(),
                include_unavailable: true,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(total_after, 0);
        assert!(after.is_empty());
        let got_after = store.get_album("qobuz:a1").unwrap().unwrap();
        assert!(!got_after.in_library);
        assert!(got_after.tracks.is_empty());
    }

    #[test]
    fn normalize_isrc_handles_formatting_and_rejects_invalid() {
        assert_eq!(normalize_isrc("us-at2-15-01950"), "USAT21501950");
        assert_eq!(normalize_isrc("  GBZN81300014 "), "GBZN81300014");
        assert_eq!(normalize_isrc("short"), "");
        assert_eq!(normalize_isrc("TOO-LONG-ISRC-12345"), "");
    }

    fn sample_enriched_album_and_track() -> (Album, Track) {
        let alb = Album {
            id: "local:a1".into(),
            source: SOURCE_LOCAL.into(),
            source_type: SOURCE_TYPE_LOCAL.into(),
            provider: SOURCE_LOCAL.into(),
            title: "Kind of Blue".into(),
            artist: "Miles Davis".into(),
            genre: "Jazz".into(),
            label: "Columbia".into(),
            upc: "886973355220".into(),
            release_date: "1959-08-17".into(),
            year: 1959,
            qobuz_artist_id: 100,
            qobuz_genre_id: 80,
            qobuz_label_id: 42,
            mb_release_id: "rel-mbid-1".into(),
            mb_release_group_id: "rg-mbid-1".into(),
            mb_artist_id: "art-mbid-1".into(),
            available: true,
            ..Default::default()
        };
        let tr = Track {
            id: "local:t1".into(),
            album_id: "local:a1".into(),
            source: SOURCE_LOCAL.into(),
            source_type: SOURCE_TYPE_LOCAL.into(),
            provider: SOURCE_LOCAL.into(),
            title: "So What".into(),
            artist: "Miles Davis".into(),
            album: "Kind of Blue".into(),
            isrc: "USBJN0920070".into(),
            mb_recording_id: "rec-mbid-1".into(),
            mb_artist_id: "art-mbid-1".into(),
            qobuz_track_id: 555,
            qobuz_artist_id: 100,
            qobuz_genre_id: 80,
            qobuz_label_id: 42,
            replaygain_gain: Some(0.0),
            replaygain_peak: Some(0.95),
            composer: "Miles Davis".into(),
            performers: "Miles Davis, Trumpet".into(),
            available: true,
            ..Default::default()
        };
        (alb, tr)
    }

    #[test]
    fn migration_3_metadata_roundtrip_preserves_enrichment() {
        let store = store::LibraryStore::open(":memory:").unwrap();
        let (alb, tr) = sample_enriched_album_and_track();
        store.upsert_album_with_tracks(&alb, &[tr]).unwrap();

        let got_alb = store.get_album("local:a1").unwrap().unwrap();
        assert_eq!(got_alb.upc, "886973355220");
        assert_eq!(got_alb.label, "Columbia");
        assert_eq!(got_alb.release_date, "1959-08-17");
        assert_eq!(
            (
                got_alb.qobuz_artist_id,
                got_alb.qobuz_genre_id,
                got_alb.qobuz_label_id
            ),
            (100, 80, 42)
        );
        assert_eq!(
            (
                got_alb.mb_release_id.as_str(),
                got_alb.mb_release_group_id.as_str(),
                got_alb.mb_artist_id.as_str()
            ),
            ("rel-mbid-1", "rg-mbid-1", "art-mbid-1")
        );

        let got_tr = store.get_track("local:t1").unwrap().unwrap();
        assert_eq!(got_tr.isrc, "USBJN0920070");
        assert_eq!(got_tr.mb_recording_id, "rec-mbid-1");
        assert_eq!(got_tr.mb_artist_id, "art-mbid-1");
        assert_eq!(
            (
                got_tr.qobuz_artist_id,
                got_tr.qobuz_genre_id,
                got_tr.qobuz_label_id
            ),
            (100, 80, 42)
        );
        assert_eq!(
            (got_tr.replaygain_gain, got_tr.replaygain_peak),
            (Some(0.0), Some(0.95))
        );
        assert_eq!(
            (got_tr.composer.as_str(), got_tr.performers.as_str()),
            ("Miles Davis", "Miles Davis, Trumpet")
        );

        // Re-scanning an untagged copy must preserve resolver/enrichment IDs and ReplayGain.
        let bare_alb = Album {
            id: "local:a1".into(),
            source: SOURCE_LOCAL.into(),
            source_type: SOURCE_TYPE_LOCAL.into(),
            provider: SOURCE_LOCAL.into(),
            title: "Kind of Blue".into(),
            artist: "Miles Davis".into(),
            available: true,
            ..Default::default()
        };
        let bare_tr = Track {
            id: "local:t1".into(),
            album_id: "local:a1".into(),
            source: SOURCE_LOCAL.into(),
            source_type: SOURCE_TYPE_LOCAL.into(),
            provider: SOURCE_LOCAL.into(),
            title: "So What".into(),
            artist: "Miles Davis".into(),
            album: "Kind of Blue".into(),
            available: true,
            ..Default::default()
        };
        store
            .upsert_album_with_tracks(&bare_alb, &[bare_tr])
            .unwrap();
        let after_alb = store.get_album("local:a1").unwrap().unwrap();
        assert_eq!(
            (after_alb.upc.as_str(), after_alb.mb_release_id.as_str()),
            ("886973355220", "rel-mbid-1")
        );
        let after_tr = store.get_track("local:t1").unwrap().unwrap();
        assert_eq!(
            (
                after_tr.isrc.as_str(),
                after_tr.mb_recording_id.as_str(),
                after_tr.replaygain_gain
            ),
            ("USBJN0920070", "rec-mbid-1", Some(0.0))
        );
    }

    #[test]
    fn migration_3_satellite_tables_and_entity_tags_triggers() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        migrations::run_migrations(&mut conn).unwrap();
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             INSERT INTO albums(id, source_type, provider, external_id, title, sort_title, artist, sort_artist, added_at, updated_at)
             VALUES ('a1', 'local', 'local', 'a1', 'T', 't', 'A', 'a', 1, 1);
             INSERT INTO tracks(id, album_id, source_type, provider, external_id, title, sort_title, artist, sort_artist, album, sort_album, added_at, updated_at)
             VALUES ('t1', 'a1', 'local', 'local', 't1', 'S', 's', 'A', 'a', 'T', 't', 1, 1);
             INSERT INTO album_enrichment(album_id, status, confidence, resolved_at) VALUES ('a1', 'matched', 1.0, 1);
             INSERT INTO track_qobuz_match(track_id, qobuz_track_id, match_method, matched_at) VALUES ('t1', 99, 'isrc', 1);
             INSERT INTO track_sonic(track_id, model_version, emb_uint8, emb_min, emb_scale, analyzed_at)
             VALUES ('t1', 'discogs-effnet-bsdynamic-1', zeroblob(1280), -0.1, 0.004, 1);
             INSERT INTO track_sonic_heads(track_id, engagement, instrumental, computed_at) VALUES ('t1', 0.8, 0.9, 1);
             INSERT INTO entity_tags(entity_type, entity_id, kind, tag, score, source) VALUES ('album', 'a1', 'genre', 'jazz', 1.0, 'resolver');
             INSERT INTO entity_tags(entity_type, entity_id, kind, tag, score, source) VALUES ('track', 't1', 'mood', 'relaxed', 0.85, 'head');",
        )
        .unwrap();
        conn.execute("DELETE FROM albums WHERE id = 'a1'", [])
            .unwrap();
        let remaining_tags: i64 = conn
            .query_row("SELECT COUNT(*) FROM entity_tags", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining_tags, 0);
        let remaining_sonic: i64 = conn
            .query_row("SELECT COUNT(*) FROM track_sonic", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining_sonic, 0);
    }

    #[test]
    fn prefer_local_tracks_matches_by_isrc_before_title() {
        let cat = Catalog::new_in_memory(vec![]);
        let alb = Album {
            id: "local:a1".into(),
            source: SOURCE_LOCAL.into(),
            source_type: SOURCE_TYPE_LOCAL.into(),
            provider: SOURCE_LOCAL.into(),
            title: "Kind of Blue".into(),
            artist: "Miles Davis".into(),
            available: true,
            ..Default::default()
        };
        let local_tr = Track {
            id: "local:t1".into(),
            album_id: "local:a1".into(),
            source: SOURCE_LOCAL.into(),
            source_type: SOURCE_TYPE_LOCAL.into(),
            provider: SOURCE_LOCAL.into(),
            title: "So What".into(),
            artist: "Miles Davis".into(),
            album: "Kind of Blue".into(),
            isrc: "USBJN0920070".into(),
            available: true,
            ..Default::default()
        };
        cat.store()
            .upsert_album_with_tracks(&alb, &[local_tr])
            .unwrap();

        let remote_tr = Track {
            id: "qobuz:999".into(),
            source: SOURCE_QOBUZ.into(),
            source_type: SOURCE_TYPE_REMOTE.into(),
            provider: SOURCE_QOBUZ.into(),
            title: "So What (Legacy Edition 2009 Remaster - Studio Version)".into(),
            artist: "Miles Davis".into(),
            album: "Kind of Blue (Legacy Edition)".into(),
            isrc: "US-BJN-09-20070".into(),
            artwork_url: "https://static.qobuz.com/cover.jpg".into(),
            available: true,
            ..Default::default()
        };
        let matched = cat.prefer_local_tracks(vec![remote_tr]);
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].id, "local:t1");
        assert!(matched[0].in_library);
        assert_eq!(matched[0].artwork_url, "https://static.qobuz.com/cover.jpg");
    }
}
