use super::*;
use crate::library::{Album, SOURCE_LOCAL, SOURCE_TYPE_LOCAL, Track};
use crate::queue::AudioQuality;
use axum::body::to_bytes;
use tower::ServiceExt;

async fn send(app: &Router, method: Method, uri: &str, body: &str) -> Response {
    let mut req = Request::builder().method(method).uri(uri);
    if !body.is_empty() {
        req = req.header(header::CONTENT_TYPE, "application/json");
    }
    app.clone()
        .oneshot(req.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap()
}

async fn json_body(resp: Response) -> serde_json::Value {
    let bytes = to_bytes(resp.into_body(), 65536).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn seed_test_album(cat: &Catalog) {
    let album = Album {
        id: "local:alb1".into(),
        source: SOURCE_LOCAL.into(),
        source_type: SOURCE_TYPE_LOCAL.into(),
        title: "Kind of Blue".into(),
        artist: "Miles Davis".into(),
        genre: "Jazz".into(),
        year: 1959,
        available: true,
        in_library: true,
        ..Default::default()
    };
    let track = Track {
        id: "local:tr1".into(),
        album_id: album.id.clone(),
        source: SOURCE_LOCAL.into(),
        source_type: SOURCE_TYPE_LOCAL.into(),
        title: "So What".into(),
        artist: "Miles Davis".into(),
        album: "Kind of Blue".into(),
        album_artist: "Miles Davis".into(),
        genre: "Jazz".into(),
        track_number: 1,
        disc_number: 1,
        duration_sec: 562,
        available: true,
        in_library: true,
        ..Default::default()
    };
    cat.store()
        .upsert_album_with_tracks(&album, std::slice::from_ref(&track))
        .unwrap();
}

fn seed_two_track_album(cat: &Catalog) {
    let alb = Album {
        id: "local:a1".into(),
        source: SOURCE_LOCAL.into(),
        source_type: SOURCE_TYPE_LOCAL.into(),
        title: "Test Album".into(),
        artist: "Test Artist".into(),
        available: true,
        in_library: true,
        ..Default::default()
    };
    let tracks = vec![
        Track {
            id: "local:t1".into(),
            album_id: alb.id.clone(),
            source: SOURCE_LOCAL.into(),
            source_type: SOURCE_TYPE_LOCAL.into(),
            title: "One".into(),
            artist: "Test Artist".into(),
            album: "Test Album".into(),
            track_number: 1,
            duration_sec: 120,
            available: true,
            in_library: true,
            ..Default::default()
        },
        Track {
            id: "local:t2".into(),
            album_id: alb.id.clone(),
            source: SOURCE_LOCAL.into(),
            source_type: SOURCE_TYPE_LOCAL.into(),
            title: "Two".into(),
            artist: "Test Artist".into(),
            album: "Test Album".into(),
            track_number: 2,
            duration_sec: 140,
            available: true,
            in_library: true,
            ..Default::default()
        },
    ];
    cat.store().upsert_album_with_tracks(&alb, &tracks).unwrap();
}

async fn register_ack_player(
    app: &Router,
    players: &Arc<PlayerRegistry>,
    name: &str,
    player_id: &str,
) -> (String, String, tokio::task::JoinHandle<()>) {
    let reg_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/players")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(
                    r#"{{"name":"{name}","playerId":"{player_id}"}}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(reg_resp.status(), StatusCode::OK);
    let reg = json_body(reg_resp).await;
    let zone_id = reg["zoneId"].as_str().unwrap().to_string();
    let token = reg["token"].as_str().unwrap().to_string();
    let rp = players.get(&zone_id, &token).unwrap();
    let mut pstream = rp.attach_stream();
    let rp_ack = Arc::clone(&rp);
    let ack_handle = tokio::spawn(async move {
        while let Some(cmd) = pstream.commands.recv().await {
            rp_ack.report_state(crate::player::StateReport {
                cmd_id: cmd.id,
                ok: Some(true),
                state: "PLAYING".into(),
                ..Default::default()
            });
        }
    });
    (zone_id, token, ack_handle)
}

#[tokio::test]
async fn health_info_and_csrf_guard() {
    let cat = Catalog::new_in_memory(vec![]);
    let mgr = QueueManager::new(Arc::clone(&cat), None, 4320, StreamMode::Relay);
    let state = AppState::new(cat, None, mgr, None, String::new());
    let app = build_router(state);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Cross-origin POST should be rejected with 403 CSRF_REJECTED
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/library/rescan")
                .header(header::HOST, "localhost:4320")
                .header(header::ORIGIN, "https://evil.example.com")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // Tauri desktop origin should be allowed
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::OPTIONS)
                .uri("/api/v1/zones")
                .header(header::HOST, "localhost:4320")
                .header(header::ORIGIN, "tauri://localhost")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        resp.headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .unwrap(),
        "tauri://localhost"
    );
}

#[tokio::test]
async fn api_wire_contract_regression() {
    assert_eq!(
        serde_json::to_string(&AudioQuality::Opus96).unwrap(),
        "\"opus_96\""
    );
    assert_eq!(
        serde_json::to_string(&AudioQuality::Opus256).unwrap(),
        "\"opus_256\""
    );

    let cat = Catalog::new_in_memory(vec![]);
    let mgr = QueueManager::new(Arc::clone(&cat), None, 4320, StreamMode::Relay);
    let state = AppState::new(cat, None, mgr, None, String::new());
    let app = build_router(state);

    let resp = send(&app, Method::GET, "/api/v1/info", "").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let info_json = json_body(resp).await;
    assert_eq!(info_json["apiVersion"], 1);
    assert!(info_json["capabilities"].is_object());

    let reg_resp = send(
        &app,
        Method::POST,
        "/api/v1/players",
        r#"{"name":"Test Browser","playerId":"test_1"}"#,
    )
    .await;
    assert_eq!(reg_resp.status(), StatusCode::OK);
    let set_cookie = reg_resp
        .headers()
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(set_cookie.starts_with("tacet_player_session="));
    let reg_json = json_body(reg_resp).await;
    let zone_id = reg_json["zoneId"].as_str().unwrap().to_string();
    let token = reg_json["token"].as_str().unwrap().to_string();

    for (uri, body) in [
        (
            format!("/api/v1/zones/{zone_id}/shuffle"),
            r#"{"enabled":true}"#,
        ),
        (
            format!("/api/v1/zones/{zone_id}/repeat"),
            r#"{"mode":"all"}"#,
        ),
    ] {
        assert_eq!(
            send(&app, Method::PUT, &uri, body).await.status(),
            StatusCode::OK
        );
    }

    let cross_sse = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/players/{zone_id}/commands"))
                .header(header::COOKIE, format!("tacet_player_session={token}"))
                .header("sec-fetch-site", "cross-site")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(cross_sse.status(), StatusCode::FORBIDDEN);

    let del = app
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri(format!("/api/v1/players/{zone_id}"))
                .header(header::COOKIE, format!("tacet_player_session={token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(del.status(), StatusCode::OK);
    let del_json = json_body(del).await;
    assert_eq!(del_json["unregistered"], true);
    assert_eq!(del_json["zoneId"], zone_id);
}

#[tokio::test]
async fn library_settings_and_qobuz_oauth_handlers() {
    let dir = tempfile::tempdir().unwrap();
    let cat = Catalog::new_in_memory(vec![dir.path().to_path_buf()]);
    seed_test_album(&cat);

    let qb = crate::qobuz::Client::new(crate::qobuz::Config::default());
    let mgr = QueueManager::new(
        Arc::clone(&cat),
        Some(Arc::clone(&qb)),
        4320,
        StreamMode::Relay,
    );
    let state = AppState::new(cat, Some(qb), mgr, None, String::new());
    let app = build_router(state);

    for uri in [
        "/api/v1/openapi.yaml",
        "/api/v1/library/status",
        "/api/v1/library/albums",
        "/api/v1/library/albums/local:alb1",
        "/api/v1/library/tracks",
        "/api/v1/library/artists",
        "/api/v1/library/genres",
        "/api/v1/library/heavy-rotation",
        "/api/v1/library/search?q=Miles",
        "/api/v1/qobuz/status",
    ] {
        assert_eq!(
            send(&app, Method::GET, uri, "").await.status(),
            StatusCode::OK,
            "GET {uri}"
        );
    }

    assert_eq!(
        send(
            &app,
            Method::GET,
            "/api/v1/library/albums/local:missing",
            ""
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(&app, Method::POST, "/api/v1/library/rescan?wait=true", "")
            .await
            .status(),
        StatusCode::OK
    );

    let patch_ok = send(
        &app,
        Method::PATCH,
        "/api/v1/settings",
        r#"{"streamMode":"direct"}"#,
    )
    .await;
    assert_eq!(patch_ok.status(), StatusCode::OK);
    assert_eq!(json_body(patch_ok).await["streamMode"], "direct");

    assert_eq!(
        send(
            &app,
            Method::PATCH,
            "/api/v1/settings",
            r#"{"unknownField":123}"#
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );

    let bad_host = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/qobuz/oauth/start")
                .header(header::HOST, "evil.com/foo@bar")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bad_host.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        send(&app, Method::DELETE, "/api/v1/qobuz/session", "")
            .await
            .status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn zone_queue_by_id_transport_and_sse_shutdown() {
    let cat = Catalog::new_in_memory(vec![]);
    seed_two_track_album(&cat);

    let mgr = QueueManager::new(Arc::clone(&cat), None, 4320, StreamMode::Relay);
    let state = AppState::new(cat, None, mgr, None, String::new());
    let shutdown = state.shutdown.clone();
    let players = Arc::clone(&state.players);
    let app = build_router(state);

    let (zone_id, _token, ack_handle) = register_ack_player(&app, &players, "Desk", "desk_1").await;

    let rep = send(
        &app,
        Method::POST,
        &format!("/api/v1/zones/{zone_id}/queue/items"),
        r#"{"source":{"type":"tracks","trackIds":["local:t1","local:t2"]},"position":"end"}"#,
    )
    .await;
    assert_eq!(rep.status(), StatusCode::OK);
    let z_json = json_body(rep).await;
    let items = z_json["queue"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    let item0 = items[0]["itemId"].as_str().unwrap().to_string();
    let item1 = items[1]["itemId"].as_str().unwrap().to_string();

    let z_url = format!("/api/v1/zones/{zone_id}");
    assert_eq!(
        send(
            &app,
            Method::POST,
            &format!("{z_url}/queue/items/{item1}/move"),
            r#"{"toIndex":0}"#
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(
        send(
            &app,
            Method::DELETE,
            &format!("{z_url}/queue/items/{item0}"),
            ""
        )
        .await
        .status(),
        StatusCode::OK
    );
    let rm_missing = send(
        &app,
        Method::DELETE,
        &format!("{z_url}/queue/items/no_such_item"),
        "",
    )
    .await;
    assert_eq!(rm_missing.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        json_body(rm_missing).await["error"]["code"],
        "QUEUE_ITEM_NOT_FOUND"
    );
    assert_eq!(
        send(&app, Method::GET, "/api/v1/zones/zone:nonexistent", "")
            .await
            .status(),
        StatusCode::NOT_FOUND
    );

    for (method, path, body) in [
        (Method::PUT, "/quality", r#"{"quality":"opus_160"}"#),
        (Method::POST, "/transport/pause", ""),
        (Method::POST, "/transport/play", ""),
        (Method::POST, "/transport/seek", r#"{"positionSec":15}"#),
        (Method::PUT, "/volume", r#"{"volume":65}"#),
        (Method::PUT, "/mute", r#"{"muted":true}"#),
        (Method::POST, "/transport/stop", ""),
        (Method::DELETE, "/queue", ""),
    ] {
        let uri = format!("{z_url}{path}");
        assert_eq!(
            send(&app, method, &uri, body).await.status(),
            StatusCode::OK,
            "{uri}"
        );
    }

    let sse_resp = send(
        &app,
        Method::GET,
        &format!("/api/v1/events?zone={zone_id}"),
        "",
    )
    .await;
    assert_eq!(sse_resp.status(), StatusCode::OK);
    shutdown.cancel();
    let sse_bytes = to_bytes(sse_resp.into_body(), 256 * 1024).await.unwrap();
    let sse_text = String::from_utf8_lossy(&sse_bytes);
    assert!(sse_text.contains("event: snapshot"));
    assert!(sse_text.contains(&zone_id));
    ack_handle.abort();
}
