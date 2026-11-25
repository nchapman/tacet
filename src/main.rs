use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use tracing_subscriber::EnvFilter;

use tacet::api::{AppState, build_router};
use tacet::cache::{Cache, CacheOptions};
use tacet::cast::{CastMonitor, CastMonitorCallbacks};
use tacet::library::{
    AutoScanConfig, AutoScanner, Catalog, PREF_LIBRARY_SCAN_INTERVAL_MIN,
    PREF_LIBRARY_WATCH_ENABLED, PREF_MANUAL_RENDERERS, PREF_QOBUZ_MAX_QUALITY, PREF_STREAM_MODE,
    PREF_ZONE_QUALITIES, SOURCE_QOBUZ,
};
use tacet::linkplay;
use tacet::media::detect_transcoder;
use tacet::qobuz::{
    Client as QobuzClient, Config as QobuzConfig, FORMAT_FLAC_192, PersistedCredentials,
};
use tacet::queue::{AudioQuality, Manager as QueueManager, StreamMode};
use tacet::upnp::{self, GenaClient, Monitor, MonitorCallbacks, MonitorConfig};

#[derive(Parser, Debug)]
#[command(name = "tacet", version, about = "Headless multi-zone music server")]
struct Cli {
    /// HTTP port for media serving, API, and web remote
    #[arg(long, env = "TACET_PORT", default_value_t = 4320)]
    port: u16,

    /// Comma-separated local music directories to index
    #[arg(long, env = "TACET_MUSIC_DIR", default_value = "")]
    music_dir: String,

    /// Persistent data directory for SQLite databases and art cache
    #[arg(long, env = "TACET_DATA_DIR")]
    data_dir: Option<String>,

    /// Days to retain soft-deleted missing music before purging on scan (0 = never purge)
    #[arg(long, env = "TACET_MISSING_RETENTION_DAYS", default_value_t = 30)]
    missing_retention_days: u64,

    /// Base URL sent to renderers (e.g. http://192.168.1.10:4320); auto-detected if empty
    #[arg(long, env = "TACET_PUBLIC_URL", default_value = "")]
    public_url: String,

    /// Optional UPnP description XML URL to connect directly
    #[arg(long, env = "TACET_RENDERER_URL", default_value = "")]
    renderer_url: String,

    /// Default Qobuz delivery mode: relay or direct (overrides saved SQLite pref when set)
    #[arg(long, env = "TACET_STREAM_MODE")]
    stream_mode: Option<String>,

    /// Override packaged desktop CORS origins with one external origin (e.g. http://localhost:5173)
    #[arg(long, env = "TACET_CORS_ORIGIN", default_value = "")]
    cors_origin: String,

    /// Optional path (or '-' for stderr) for structured JSONL event logging
    #[arg(long, env = "TACET_EVENT_LOG", default_value = "")]
    event_log: String,
}

fn default_data_dir() -> PathBuf {
    if let Some(cfg) = dirs::config_dir() {
        return cfg.join("tacet");
    }
    PathBuf::from("./data")
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    if let Err(msg) = validate_cli(&cli) {
        return Err(msg.into());
    }
    let data_dir = cli
        .data_dir
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(default_data_dir);
    std::fs::create_dir_all(&data_dir)?;

    let roots: Vec<PathBuf> = cli
        .music_dir
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect();

    // 1. Unified SQLite library catalog
    let db_path = data_dir.join("library.db");
    let catalog = Catalog::open(&db_path.to_string_lossy(), roots)?;
    catalog.set_missing_retention(Duration::from_secs(cli.missing_retention_days * 86_400));
    let init_status = catalog.status();
    tracing::info!(
        db = %db_path.display(),
        tracks = init_status.tracks,
        albums = init_status.albums,
        "unified library database ready"
    );

    // 2. Shared SQLite KV cache & Qobuz client
    let cache_dir = data_dir.join("cache");
    let _ = std::fs::create_dir_all(&cache_dir);
    let cache_db = cache_dir.join("cache.db");
    let app_cache = Cache::open(&cache_db.to_string_lossy(), CacheOptions::default())
        .or_else(|_| Cache::open(":memory:", CacheOptions::default()))?;
    let qobuz = init_qobuz(&catalog, Arc::clone(&app_cache)).await;

    // 3. Queue / playback manager + GENA client
    let stream_mode = resolve_stream_mode(&cli, &catalog);
    let manager = QueueManager::new(
        Arc::clone(&catalog),
        Some(Arc::clone(&qobuz)),
        cli.port,
        stream_mode,
    );
    if !cli.public_url.trim().is_empty() {
        manager.set_public_url(cli.public_url.trim());
    }
    manager.load_persisted_zones();
    configure_event_log(&manager, &cli.event_log)?;

    let mgr_gena = Arc::clone(&manager);
    let gena = GenaClient::new(move |ev| {
        mgr_gena.apply_device_event(ev);
    });
    manager.set_gena(Arc::clone(&gena));

    let tc = detect_transcoder();
    if tc.available() {
        tracing::info!(
            ffmpeg = ?tc.ffmpeg_path,
            soxr = tc.has_libsoxr,
            "audio transcoder ready"
        );
    }

    // 4. HTTP API & Media server state
    let state = AppState::new(
        Arc::clone(&catalog),
        Some(Arc::clone(&qobuz)),
        Arc::clone(&manager),
        Some(Arc::clone(&gena)),
        cli.cors_origin,
    );
    if !cli.public_url.trim().is_empty() {
        state.set_public_url(cli.public_url.trim());
    }
    state.media.set_art_cache_dir(cache_dir.join("art"));

    // 5. AutoScanner, initial scan, manual renderers, UPnP & Cast monitors
    let auto_scanner = start_autoscan(&catalog, &state);
    spawn_manual_renderers(&catalog, &manager, &cli.renderer_url);
    let (upnp_monitor, cast_monitor) = start_device_monitors(&manager);

    // 6. Start Axum HTTP server with coordinated shutdown token so SSE/media streams close promptly
    let router = build_router(Arc::clone(&state));
    let addr = SocketAddr::from(([0, 0, 0, 0], cli.port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "tacet listening");

    serve_with_drain(listener, router, state.shutdown.clone()).await;

    tracing::info!("shutting down tacet...");
    upnp_monitor.stop();
    cast_monitor.stop();
    auto_scanner.stop();
    state.close();
    catalog.close();
    Ok(())
}

async fn init_qobuz(catalog: &Arc<Catalog>, app_cache: Arc<Cache>) -> Arc<QobuzClient> {
    let qobuz = QobuzClient::new(QobuzConfig {
        app_id: std::env::var("QOBUZ_APP_ID").unwrap_or_default(),
        app_secret: std::env::var("QOBUZ_APP_SECRET").unwrap_or_default(),
        auth_token: std::env::var("QOBUZ_AUTH_TOKEN").unwrap_or_default(),
        format_id: FORMAT_FLAC_192,
        cache: Some(app_cache),
        ..Default::default()
    });
    if let Ok(Some(saved)) = catalog.get_provider_credentials::<PersistedCredentials>(SOURCE_QOBUZ)
    {
        qobuz.restore_credentials(saved);
        if qobuz.is_connected() {
            tracing::info!(user = %qobuz.status().user_name, "restored Qobuz session from SQLite");
        }
    }
    if !qobuz.is_connected() {
        try_env_qobuz_login(catalog, &qobuz).await;
    }
    if let Ok(Some(max_q)) = catalog.get_pref(PREF_QOBUZ_MAX_QUALITY)
        && !max_q.is_empty()
    {
        let _ = qobuz.set_max_quality(&max_q);
    }
    catalog.register_remote_source(qobuz.as_remote_source());
    qobuz
}

async fn try_env_qobuz_login(catalog: &Catalog, qobuz: &QobuzClient) {
    let (Ok(email), Ok(password)) = (
        std::env::var("QOBUZ_EMAIL"),
        std::env::var("QOBUZ_PASSWORD"),
    ) else {
        return;
    };
    if email.trim().is_empty() || password.is_empty() {
        return;
    }
    match qobuz.login(&email, &password).await {
        Ok(st) => {
            let _ = catalog.set_provider_credentials(SOURCE_QOBUZ, &qobuz.export_credentials());
            tracing::info!(user = %st.user_name, "logged in to Qobuz via env credentials");
        }
        Err(e) => tracing::warn!(error = %e, "Qobuz env login failed"),
    }
}

fn configure_event_log(manager: &QueueManager, raw_path: &str) -> std::io::Result<()> {
    let lp = raw_path.trim();
    if lp.is_empty() {
        return Ok(());
    }
    if lp == "-" {
        manager.set_event_log(Some(Box::new(std::io::stderr())));
        tracing::info!("raw event stream logging enabled: stderr");
        return Ok(());
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let f = opts.open(lp)?;
    manager.set_event_log(Some(Box::new(f)));
    tracing::info!(path = lp, "raw event stream logging enabled");
    Ok(())
}

fn validate_cli(cli: &Cli) -> Result<(), String> {
    if cli.port == 0 {
        return Err("--port must be between 1 and 65535 (got 0)".into());
    }
    if let Some(explicit) = cli.stream_mode.as_deref().filter(|s| !s.trim().is_empty())
        && StreamMode::parse(explicit).is_none()
    {
        return Err(format!(
            "invalid --stream-mode {explicit:?}: expected \"relay\" or \"direct\""
        ));
    }
    Ok(())
}

fn resolve_stream_mode(cli: &Cli, catalog: &Catalog) -> StreamMode {
    // Explicit CLI flag / TACET_STREAM_MODE takes precedence over saved SQLite pref.
    if let Some(explicit) = cli.stream_mode.as_deref().filter(|s| !s.trim().is_empty()) {
        StreamMode::parse(explicit).unwrap_or(StreamMode::Relay)
    } else if let Ok(Some(saved_sm)) = catalog.get_pref(PREF_STREAM_MODE)
        && let Some(m) = StreamMode::parse(&saved_sm)
    {
        m
    } else {
        StreamMode::Relay
    }
}

fn parse_watch_enabled(raw: Option<&str>) -> bool {
    match raw {
        Some(v) => {
            let trimmed = v.trim();
            trimmed.eq_ignore_ascii_case("true") || trimmed == "1"
        }
        None => true,
    }
}

fn start_autoscan(catalog: &Arc<Catalog>, state: &Arc<AppState>) -> Arc<AutoScanner> {
    let watch_enabled = parse_watch_enabled(
        catalog
            .get_pref(PREF_LIBRARY_WATCH_ENABLED)
            .ok()
            .flatten()
            .as_deref(),
    );
    let interval_min: u64 = catalog
        .get_pref(PREF_LIBRARY_SCAN_INTERVAL_MIN)
        .ok()
        .flatten()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(|v| v.min(10_080))
        .unwrap_or(60);

    let state_for_scan = Arc::clone(state);
    let auto_scanner = AutoScanner::new(
        Arc::clone(catalog),
        AutoScanConfig {
            watch_enabled,
            interval: Duration::from_secs(interval_min * 60),
            debounce: Duration::from_secs(5),
        },
        Some(Arc::new(move |_| {
            state_for_scan.media.prewarm_art();
            state_for_scan.prewarm_artists();
        })),
    );
    state.set_auto_scanner(Arc::clone(&auto_scanner));
    auto_scanner.start();

    if !catalog.roots().is_empty() {
        let cat_bg = Arc::clone(catalog);
        let state_bg = Arc::clone(state);
        tokio::spawn(async move {
            if let Ok(n) = cat_bg.scan().await {
                tracing::info!(tracks = n, "initial local library scan complete");
                state_bg.media.prewarm_art();
                state_bg.prewarm_artists();
            }
        });
    } else {
        state.prewarm_artists();
    }
    auto_scanner
}

fn spawn_manual_renderers(catalog: &Catalog, manager: &Arc<QueueManager>, renderer_url: &str) {
    let saved_qualities: std::collections::HashMap<String, String> = catalog
        .get_pref(PREF_ZONE_QUALITIES)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let saved_urls: Vec<String> = catalog
        .get_pref(PREF_MANUAL_RENDERERS)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let mut seen = std::collections::HashSet::new();
    let mut manual_urls = Vec::new();
    let cli_url = renderer_url.trim();
    if !cli_url.is_empty() && seen.insert(cli_url.to_string()) {
        manual_urls.push(cli_url.to_string());
    }
    for u in saved_urls {
        let trimmed = u.trim();
        if !trimmed.is_empty() && seen.insert(trimmed.to_string()) {
            manual_urls.push(trimmed.to_string());
        }
        if manual_urls.len() >= 16 {
            break;
        }
    }
    if manual_urls.is_empty() {
        return;
    }

    let mgr_m = Arc::clone(manager);
    tokio::spawn(async move {
        for u in manual_urls {
            match tokio::time::timeout(Duration::from_secs(5), upnp::connect_by_url(&u)).await {
                Ok(Ok(r)) => {
                    let ctrl = linkplay::wrap(r);
                    let id = ctrl.info().id;
                    mgr_m.register_renderer(ctrl);
                    if let Some(q_str) = saved_qualities.get(&id)
                        && let Some(q) = AudioQuality::parse(q_str)
                    {
                        let _ = mgr_m.set_audio_quality(&id, q).await;
                    }
                }
                Ok(Err(e)) => {
                    tracing::warn!(url = %u, error = %e, "manual renderer connect failed");
                }
                Err(_) => {
                    tracing::warn!(url = %u, "manual renderer connect timed out after 5s");
                }
            }
        }
    });
}

fn start_device_monitors(manager: &Arc<QueueManager>) -> (Arc<Monitor>, Arc<CastMonitor>) {
    let mgr_u1 = Arc::clone(manager);
    let mgr_u2 = Arc::clone(manager);
    let mgr_u3 = Arc::clone(manager);
    let mgr_u4 = Arc::clone(manager);
    let upnp_monitor = Monitor::new(
        MonitorConfig::default(),
        MonitorCallbacks {
            list_upnp: Arc::new(move || mgr_u1.list_renderers_by_kind("upnp")),
            is_active: Arc::new(move |id| mgr_u2.is_zone_active(id)),
            on_found: Arc::new(move |r| mgr_u3.register_renderer(linkplay::wrap(r))),
            on_removed: Arc::new(move |id, name| {
                tracing::info!(id, name, "removing unreachable UPnP renderer");
                mgr_u4.remove_renderer(id);
            }),
        },
    );
    upnp_monitor.start();

    let mgr_c1 = Arc::clone(manager);
    let mgr_c_upnp = Arc::clone(manager);
    let mgr_c2 = Arc::clone(manager);
    let mgr_c3 = Arc::clone(manager);
    let mgr_c4 = Arc::clone(manager);
    let cast_monitor = CastMonitor::new(
        Duration::from_secs(60),
        CastMonitorCallbacks {
            list_cast: Arc::new(move || mgr_c1.list_renderers_by_kind("cast")),
            list_upnp_hosts: Arc::new(move || {
                mgr_c_upnp
                    .list_renderers_by_kind("upnp")
                    .into_iter()
                    .filter(|r| !r.host.is_empty())
                    .map(|r| r.host)
                    .collect()
            }),
            is_active: Arc::new(move |id| mgr_c2.is_zone_active(id)),
            on_found: Arc::new(move |r| mgr_c3.register_renderer(r)),
            on_removed: Arc::new(move |id, name| {
                tracing::info!(id, name, "removing unreachable Cast renderer");
                mgr_c4.remove_renderer(id);
            }),
        },
    );
    cast_monitor.start();
    (upnp_monitor, cast_monitor)
}

async fn serve_with_drain(
    listener: tokio::net::TcpListener,
    router: axum::Router,
    shutdown_tok: tokio_util::sync::CancellationToken,
) {
    let shutdown_wait = shutdown_tok.clone();
    let mut server = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(shutdown_wait.cancelled_owned())
            .await
    });
    let wait_res = tokio::select! {
        res = &mut server => Some(res),
        () = shutdown_signal() => {
            shutdown_tok.cancel();
            match tokio::time::timeout(Duration::from_secs(5), &mut server).await {
                Ok(res) => Some(res),
                Err(_) => {
                    server.abort();
                    None
                }
            }
        }
    };
    log_server_exit(wait_res);
}

fn log_server_exit(wait_res: Option<Result<std::io::Result<()>, tokio::task::JoinError>>) {
    match wait_res {
        Some(Ok(Ok(()))) => {}
        Some(Ok(Err(e))) => tracing::error!(error = %e, "HTTP server exited with error"),
        Some(Err(e)) => tracing::error!(error = %e, "HTTP server task panicked"),
        None => tracing::warn!("HTTP server drain timed out after 5s; forcing shutdown"),
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_cli_rejects_port_zero_and_bad_stream_mode() {
        let mut cli = Cli::parse_from(["tacet", "--port", "0"]);
        let err = validate_cli(&cli).unwrap_err();
        assert!(err.contains("--port"));

        cli = Cli::parse_from(["tacet", "--stream-mode", "bogus"]);
        let err = validate_cli(&cli).unwrap_err();
        assert!(err.contains("--stream-mode"));
        assert!(err.contains("relay"));
        assert!(err.contains("direct"));

        cli = Cli::parse_from(["tacet", "--port", "4320", "--stream-mode", "direct"]);
        assert!(validate_cli(&cli).is_ok());
    }

    #[test]
    fn parse_watch_enabled_matches_go_semantics() {
        assert!(parse_watch_enabled(None));
        assert!(parse_watch_enabled(Some("true")));
        assert!(parse_watch_enabled(Some("TRUE")));
        assert!(parse_watch_enabled(Some("1")));
        assert!(!parse_watch_enabled(Some("false")));
        assert!(!parse_watch_enabled(Some("0")));
        assert!(!parse_watch_enabled(Some("")));
    }
}
