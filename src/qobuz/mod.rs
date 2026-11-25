pub mod auth;
pub mod client;
pub mod discover;
pub mod types;

pub use client::{Client, Config, sign_file_url};
pub use types::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_file_url_matches_qobuz_formula() {
        let sig = sign_file_url(123456, 27, "stream", 1700000000, "secret");
        let expected =
            md5_hex("trackgetFileUrlformat_id27intentstreamtrack_id1234561700000000secret");
        assert_eq!(sig, expected);
    }

    #[test]
    fn quality_round_trip() {
        assert_eq!(parse_quality("mp3_320").unwrap(), FORMAT_MP3_320);
        assert_eq!(parse_quality("flac_16_44").unwrap(), FORMAT_FLAC_16_44);
        assert_eq!(parse_quality("flac_24_96").unwrap(), FORMAT_FLAC_24_96);
        assert_eq!(parse_quality("flac_24_192").unwrap(), FORMAT_FLAC_192);
        assert!(parse_quality("dsd").is_err());
        assert_eq!(quality_label(FORMAT_FLAC_24_96), "flac_24_96");
    }

    #[test]
    fn credentials_export_restore_logout() {
        let c = Client::new(Config {
            app_id: "100".into(),
            app_secret: "sec".into(),
            auth_token: "tok".into(),
            ..Default::default()
        });
        assert!(c.is_connected());
        let creds = c.export_credentials();
        assert_eq!(creds.app_id, "100");
        assert_eq!(creds.auth_token, "tok");

        let c2 = Client::new(Config::default());
        assert!(!c2.is_connected());
        c2.restore_credentials(creds);
        assert!(c2.is_connected());

        let st = c2.logout();
        assert!(!st.authenticated);
        assert!(!c2.is_connected());
    }

    #[tokio::test]
    async fn mock_qobuz_api_login_album_cache_401_and_429() {
        use axum::{
            Router, extract::Query, http::StatusCode, response::IntoResponse, routing::get,
        };
        use std::collections::HashMap;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let album_hits = Arc::new(AtomicUsize::new(0));
        let hits_clone = Arc::clone(&album_hits);

        let app = Router::new()
            .route(
                "/user/login",
                get(|| async {
                    axum::Json(serde_json::json!({
                        "user_auth_token": "tok_live_123",
                        "user": {
                            "display_name": "Test User",
                            "credential": {"label": "Studio"}
                        }
                    }))
                }),
            )
            .route(
                "/album/get",
                get(move |Query(q): Query<HashMap<String, String>>| {
                    let hits = Arc::clone(&hits_clone);
                    async move {
                        match q.get("album_id").map(String::as_str) {
                            Some("unauth") => (StatusCode::UNAUTHORIZED, "bad token").into_response(),
                            Some("missing") => (StatusCode::NOT_FOUND, "not found").into_response(),
                            Some("rate") => (
                                StatusCode::TOO_MANY_REQUESTS,
                                [("retry-after", "1")],
                                "slow down",
                            )
                                .into_response(),
                            _ => {
                                hits.fetch_add(1, Ordering::SeqCst);
                                axum::Json(serde_json::json!({
                                    "id": "alb123",
                                    "title": "Blue Train",
                                    "release_date_original": "éé1957-09-15",
                                    "artist": {"id": 42, "name": "John Coltrane"},
                                    "genre": {"name": "Jazz"},
                                    "image": {"large": "https://static.qobuz.com/images/covers/1.jpg"},
                                    "tracks": {
                                        "items": [{
                                            "id": 9001,
                                            "title": "Blue Train",
                                            "track_number": 1,
                                            "media_number": 1,
                                            "duration": 643,
                                            "maximum_sampling_rate": 96.0,
                                            "maximum_bit_depth": 24,
                                            "streamable": true,
                                            "performer": {"id": 42, "name": "John Coltrane"}
                                        }]
                                    }
                                }))
                                .into_response()
                            }
                        }
                    }
                }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let c = Client::new(Config {
            base_url: format!("http://{addr}"),
            app_id: "100".into(),
            app_secret: "sec".into(),
            ..Default::default()
        });

        let st = c.login("user@example.com", "pw").await.unwrap();
        assert!(st.authenticated);
        assert_eq!(st.user_name, "Test User");

        // First album fetch hits mock server & parses non-ASCII release_date_original safely
        let alb = c.get_album("alb123").await.unwrap();
        assert_eq!(alb.title, "Blue Train");
        assert_eq!(alb.tracks.len(), 1);
        assert_eq!(album_hits.load(Ordering::SeqCst), 1);

        // Second fetch is served from cache
        let _ = c.get_album("alb123").await.unwrap();
        assert_eq!(album_hits.load(Ordering::SeqCst), 1);

        // 401 maps to Unauthenticated
        assert!(matches!(
            c.get_album("unauth").await,
            Err(QobuzError::Unauthenticated)
        ));

        // 404 maps to NotFound
        assert!(matches!(
            c.get_album("missing").await,
            Err(QobuzError::NotFound)
        ));

        // 429 maps to RateLimited
        assert!(matches!(
            c.get_album("rate").await,
            Err(QobuzError::RateLimited)
        ));

        srv.abort();
    }

    #[test]
    fn decode_qobuz_json_tolerates_null_fields_and_preserves_option_bool() {
        let raw = br#"{
            "id": "0884977129762",
            "title": "Tidal",
            "artist": {"id": 42, "name": "Fiona Apple", "picture": null, "image": null, "biography": null},
            "genre": {"name": null},
            "label": null,
            "release_date_original": null,
            "description": null,
            "release_type": null,
            "streamable": null,
            "tracks": {
                "items": [
                    null,
                    {
                        "id": 21984192,
                        "title": "Sleep to Dream",
                        "version": null,
                        "performers": null,
                        "performer": null,
                        "album": null
                    }
                ]
            }
        }"#;
        let album: types::QAlbum = types::decode_qobuz_json(raw).unwrap();
        assert_eq!(album.id, "0884977129762");
        assert_eq!(album.title, "Tidal");
        assert_eq!(album.streamable, None);
        assert_eq!(album.tracks.items.len(), 1);
        assert_eq!(album.tracks.items[0].title, "Sleep to Dream");

        let discover_raw = br#"{
            "id": "d1",
            "title": "Unstreamable",
            "rights": {"streamable": false},
            "ai_generated": null
        }"#;
        let disc: types::QDiscoverAlbum = types::decode_qobuz_json(discover_raw).unwrap();
        assert_eq!(disc.rights.streamable, Some(false));
        assert!(!disc.ai_generated);
    }
}
