package main

import (
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"log"
	"net"
	"net/http"
	"os"
	"os/signal"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
	"time"

	"tacet/internal/api"
	"tacet/internal/cache"
	"tacet/internal/cast"
	"tacet/internal/library"
	"tacet/internal/linkplay"
	"tacet/internal/media"
	"tacet/internal/qobuz"
	"tacet/internal/queue"
	"tacet/internal/upnp"
)

func main() {
	if err := run(); err != nil {
		log.Printf("fatal: %v", err)
		os.Exit(1)
	}
}

func run() error {
	port := flag.Int("port", envInt("TACET_PORT", 4320), "HTTP port for media serving, API, and web remote")
	musicDir := flag.String("music-dir", os.Getenv("TACET_MUSIC_DIR"), "Comma-separated local music directories to index")
	dataDir := flag.String("data-dir", envDefault("TACET_DATA_DIR", defaultDataDir()), "Persistent data directory for the SQLite unified library")
	missingDays := flag.Int("missing-retention-days", envNonNegInt("TACET_MISSING_RETENTION_DAYS", 30), "Days to retain soft-deleted missing music before purging on scan (0 = never purge)")
	publicURL := flag.String("public-url", os.Getenv("TACET_PUBLIC_URL"), "Optional base URL override for renderers (e.g. http://192.168.1.10:4320)")
	rendererURL := flag.String("renderer-url", os.Getenv("TACET_RENDERER_URL"), "Optional UPnP description XML URL to connect directly (e.g. http://<wiim-ip>:49152/description.xml)")
	streamMode := flag.String("stream-mode", envDefault("TACET_STREAM_MODE", "direct"), "Default Qobuz delivery mode: direct or relay")
	corsOrigin := flag.String("cors-origin", os.Getenv("TACET_CORS_ORIGIN"), "Optional CORS Allow-Origin header for external web/hybrid clients (e.g. * or http://localhost:5173)")
	flag.Parse()

	// Track which flags were explicitly set on the CLI so explicit flags/env vars
	// take precedence over persisted SQLite preferences.
	explicitFlags := make(map[string]bool)
	flag.Visit(func(f *flag.Flag) {
		explicitFlags[f.Name] = true
	})

	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()

	// 1. Unified SQLite library catalog (local NAS + registered remote sources).
	var roots []string
	for _, r := range strings.Split(*musicDir, ",") {
		if r = strings.TrimSpace(r); r != "" {
			roots = append(roots, r)
		}
	}
	dbPath := ":memory:"
	if dir := strings.TrimSpace(*dataDir); dir != "" {
		dbPath = filepath.Join(dir, "library.db")
	}
	cat, err := library.NewCatalogWithDB(dbPath, roots...)
	if err != nil {
		return fmt.Errorf("open library database: %w", err)
	}
	defer func() { _ = cat.Close() }()
	if *missingDays >= 0 {
		cat.SetMissingRetention(time.Duration(*missingDays) * 24 * time.Hour)
	}
	if dbPath != ":memory:" {
		log.Printf("unified library database: %s", dbPath)
	}
	initStatus := cat.Status()
	if initStatus.Tracks > 0 {
		log.Printf("loaded %d cached tracks across %d albums from SQLite", initStatus.Tracks, initStatus.Albums)
	}

	// 2. Shared SQLite KV cache (<data-dir>/cache/cache.db) and Qobuz client.
	cacheDBPath := ":memory:"
	if dir := strings.TrimSpace(*dataDir); dir != "" {
		cacheDBPath = filepath.Join(dir, "cache", "cache.db")
		// Clean up obsolete legacy JSON cache file if present.
		_ = os.Remove(filepath.Join(dir, "cache", "qobuz.json"))
	}
	appCache, cacheErr := cache.Open(cacheDBPath, cache.WithPruneInterval(time.Hour))
	if cacheErr != nil {
		log.Printf("warning: failed to open SQLite cache %s (%v); falling back to in-memory cache", cacheDBPath, cacheErr)
		appCache, _ = cache.Open(":memory:", cache.WithPruneInterval(time.Hour))
	}
	if appCache != nil {
		defer func() { _ = appCache.Close() }()
	}

	qb := qobuz.NewClient(qobuz.Config{
		AppID:     os.Getenv("QOBUZ_APP_ID"),
		AppSecret: os.Getenv("QOBUZ_APP_SECRET"),
		AuthToken: os.Getenv("QOBUZ_AUTH_TOKEN"),
		FormatID:  qobuz.FormatFLAC192,
		Cache:     appCache,
	})
	defer func() { _ = qb.Close() }()
	if albs, arts, _, searches := qb.CacheStats(); albs+arts+searches > 0 {
		log.Printf("loaded Qobuz SQLite cache (%d albums, %d artists, %d searches)", albs, arts, searches)
	}
	var savedQobuz qobuz.PersistedCredentials
	if found, credErr := cat.GetProviderCredentials(ctx, "qobuz", &savedQobuz); credErr != nil {
		log.Printf("warning: failed to load saved Qobuz credentials: %v", credErr)
	} else if found {
		wasAuth := qb.Status().Authenticated
		qb.RestoreCredentials(savedQobuz)
		if !wasAuth && qb.Status().Authenticated {
			st := qb.Status()
			log.Printf("restored Qobuz session from SQLite (%s)", st.UserName)
		}
	}
	if maxQ, ok, prefErr := cat.GetPref(ctx, library.PrefQobuzMaxQuality); prefErr == nil && ok && maxQ != "" {
		if qErr := qb.SetMaxQuality(maxQ); qErr != nil {
			log.Printf("warning: ignoring invalid saved qobuz_max_quality %q: %v", maxQ, qErr)
		}
	}
	cat.RegisterRemoteSource(qb.AsRemoteSource())

	// 3. Queue / playback manager + UPnP GENA event subscriber.
	mgr := queue.NewManager(*port, *publicURL, qb)
	transcoder := media.DetectTranscoder()
	mgr.SetTranscodeAvailable(transcoder.Available())
	if transcoder.Available() {
		log.Printf("audio transcoder ready: %s (libopus + wav, soxr=%v)", transcoder.FFmpegPath, transcoder.HasLibSoxr)
	} else {
		log.Printf("audio transcoder unavailable (install ffmpeg with libopus for Opus/WAV output)")
	}
	if rawQ, ok, prefErr := cat.GetPref(ctx, library.PrefZoneQualities); prefErr == nil && ok && strings.TrimSpace(rawQ) != "" {
		var savedQ map[string]queue.AudioQuality
		if uErr := json.Unmarshal([]byte(rawQ), &savedQ); uErr == nil {
			mgr.LoadZoneQualities(savedQ)
		}
	}
	if err = mgr.SetStreamMode(queue.StreamMode(*streamMode)); err != nil {
		return fmt.Errorf("invalid --stream-mode: %w", err)
	}
	if !explicitFlags["stream-mode"] && strings.TrimSpace(os.Getenv("TACET_STREAM_MODE")) == "" {
		if savedMode, ok, prefErr := cat.GetPref(ctx, library.PrefStreamMode); prefErr == nil && ok && savedMode != "" {
			if modeErr := mgr.SetStreamMode(queue.StreamMode(savedMode)); modeErr != nil {
				log.Printf("warning: ignoring invalid saved stream_mode %q: %v", savedMode, modeErr)
			}
		}
	}
	mgr.SetOnTrackPlayed(cat.RecordPlay)
	gena := upnp.NewGENAClient(mgr.HandleDeviceEvent)
	mgr.SetGENA(gena)

	// 4. Bind HTTP server first so the API, web UI, and UPnP GENA NOTIFY callbacks
	// are immediately reachable without waiting for disk scans or remote syncs.
	apiServer := api.NewServer(cat, qb, mgr)
	if dir := strings.TrimSpace(*dataDir); dir != "" {
		apiServer.SetArtCacheDir(filepath.Join(dir, "cache", "art"))
	}
	apiServer.SetGENA(gena)
	if *corsOrigin != "" {
		apiServer.SetCORSOrigin(*corsOrigin)
	}
	httpSrv := &http.Server{
		Addr:              fmt.Sprintf(":%d", *port),
		Handler:           apiServer.Handler(),
		ReadHeaderTimeout: 10 * time.Second,
		IdleTimeout:       90 * time.Second,
	}
	lc := net.ListenConfig{KeepAlive: 30 * time.Second}
	ln, err := lc.Listen(ctx, "tcp", httpSrv.Addr)
	if err != nil {
		return fmt.Errorf("listen %s: %w", httpSrv.Addr, err)
	}

	mgr.Start(ctx)
	defer func() {
		apiServer.Close()
		mgr.Close()
	}()

	// 5. Background non-blocking library imports (local incremental scan + auto-scanner + Qobuz favorites).
	watchEnabled := library.DefaultLibraryWatchEnabled
	scanIntervalMin := library.DefaultLibraryScanIntervalMin
	if raw, ok, prefErr := cat.GetPref(ctx, library.PrefLibraryWatchEnabled); prefErr == nil && ok {
		watchEnabled = raw == "true" || raw == "1"
	}
	if raw, ok, prefErr := cat.GetPref(ctx, library.PrefLibraryScanIntervalMin); prefErr == nil && ok {
		if v, parseErr := strconv.Atoi(strings.TrimSpace(raw)); parseErr == nil && v >= 0 && v <= library.MaxLibraryScanIntervalMin {
			scanIntervalMin = v
		}
	}
	autoScanner := library.NewAutoScanner(cat, library.AutoScanConfig{
		WatchEnabled: watchEnabled,
		Interval:     time.Duration(scanIntervalMin) * time.Minute,
	}, func(scanCtx context.Context, _ int) {
		apiServer.PrewarmArt(scanCtx)
		apiServer.PrewarmArtists(scanCtx)
	})
	apiServer.SetAutoScanner(autoScanner)
	defer autoScanner.Stop()

	if len(roots) > 0 {
		autoScanner.Start(ctx)
		go func() {
			start := time.Now()
			log.Printf("background scan started for local music directories: %v", roots)
			n, scanErr := cat.ScanContext(ctx)
			if scanErr != nil && !errors.Is(scanErr, context.Canceled) {
				log.Printf("warning: library scan error: %v", scanErr)
				return
			}
			if scanErr == nil {
				st := cat.Status()
				log.Printf("indexed %d local tracks across %d albums in %s", n, st.LocalAlbums, time.Since(start).Round(time.Millisecond))
				apiServer.PrewarmArt(ctx)
				apiServer.PrewarmArtists(ctx)
			}
		}()
	}
	go func() {
		if email, pass := os.Getenv("QOBUZ_EMAIL"), os.Getenv("QOBUZ_PASSWORD"); email != "" && pass != "" && !qb.Status().Authenticated {
			lctx, cancel := context.WithTimeout(ctx, 10*time.Second)
			if st, loginErr := qb.Login(lctx, email, pass); loginErr != nil {
				log.Printf("warning: Qobuz env login failed: %v", loginErr)
			} else {
				log.Printf("Qobuz authenticated as %s (%s)", st.UserName, st.Subscription)
				mgr.Publish("qobuz.status", st)
			}
			cancel()
		}
		if qb.IsConnected() {
			sctx, cancel := context.WithTimeout(ctx, 2*time.Minute)
			defer cancel()
			if n, syncErr := cat.SyncRemote(sctx, "qobuz"); syncErr != nil {
				log.Printf("warning: initial Qobuz favorites sync failed: %v", syncErr)
			} else {
				log.Printf("synced %d Qobuz library tracks from favorites", n)
			}
			apiServer.PrewarmArtists(ctx)
		}
	}()

	// 6. Initial UPnP renderer discovery + saved manual renderers (non-blocking).
	var manualURLs []string
	if raw, ok, prefErr := cat.GetPref(ctx, library.PrefManualRenderers); prefErr != nil {
		log.Printf("warning: failed to load saved manual_renderers: %v", prefErr)
	} else if ok && strings.TrimSpace(raw) != "" {
		if unmarshalErr := json.Unmarshal([]byte(raw), &manualURLs); unmarshalErr != nil {
			log.Printf("warning: ignoring corrupt saved manual_renderers: %v", unmarshalErr)
			manualURLs = nil
		}
	}
	if *rendererURL != "" {
		seen := false
		for _, u := range manualURLs {
			if u == *rendererURL {
				seen = true
				break
			}
		}
		if !seen {
			manualURLs = append([]string{*rendererURL}, manualURLs...)
		}
	}
	if len(manualURLs) > 16 {
		manualURLs = manualURLs[:16]
	}
	if len(manualURLs) > 0 {
		go func(urls []string) {
			for _, u := range urls {
				if ctx.Err() != nil {
					return
				}
				cctx, cancel := context.WithTimeout(ctx, 5*time.Second)
				if dev, connErr := upnp.ConnectByURL(cctx, u); connErr != nil {
					log.Printf("warning: failed to reconnect renderer %s: %v", u, connErr)
				} else {
					wrapped := linkplay.Wrap(dev)
					mgr.RegisterRenderer(wrapped)
					info := wrapped.Info()
					log.Printf("connected renderer: %s (%s, queue=%v, gapless=%v, events=%v)",
						info.Name, info.Location, info.SupportsQueue, info.SupportsSetNext, info.SupportsEvents)
				}
				cancel()
			}
		}(manualURLs)
	}

	upnpMonitor := upnp.NewMonitor(upnp.MonitorConfig{}, upnp.MonitorCallbacks{
		ListUPnP: mgr.UPnPRenderers,
		IsActive: mgr.IsZoneActive,
		OnFound: func(r *upnp.Renderer) {
			mgr.RegisterRenderer(linkplay.Wrap(r))
		},
		OnRemoved: func(id, _ string) {
			mgr.UnregisterRenderer(id)
		},
	})
	upnpMonitor.Start(ctx)
	defer upnpMonitor.Stop()

	castMonitor := cast.NewMonitor(cast.MonitorConfig{}, cast.MonitorCallbacks{
		ListUPnPHosts: mgr.UPnPHosts,
		ListCast:      mgr.CastRenderers,
		IsActive:      mgr.IsZoneActive,
		OnFound: func(r *cast.Renderer) {
			mgr.RegisterRenderer(r)
		},
		OnRemoved: func(id, _ string) {
			mgr.UnregisterRenderer(id)
		},
	})
	castMonitor.Start(ctx)
	defer castMonitor.Stop()

	go func() {
		<-ctx.Done()
		// Close SSE event/command hubs before Shutdown so open streams exit immediately.
		apiServer.Close()
		mgr.Close()
		shCtx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		_ = httpSrv.Shutdown(shCtx)
	}()

	lanIP := detectLANIP(ctx)
	log.Printf("tacet listening on http://%s:%d (local: http://localhost:%d)", lanIP, *port, *port)
	if err := httpSrv.Serve(ln); err != nil && !errors.Is(err, http.ErrServerClosed) {
		return fmt.Errorf("http server error: %w", err)
	}
	return nil
}

func defaultDataDir() string {
	if dir, err := os.UserConfigDir(); err == nil && dir != "" {
		return filepath.Join(dir, "tacet")
	}
	return ""
}

func envDefault(key, fallback string) string {
	if v := strings.TrimSpace(os.Getenv(key)); v != "" {
		return v
	}
	return fallback
}

func envInt(key string, fallback int) int {
	v := strings.TrimSpace(os.Getenv(key))
	if v == "" {
		return fallback
	}
	var n int
	if _, err := fmt.Sscanf(v, "%d", &n); err != nil || n <= 0 {
		return fallback
	}
	return n
}

func envNonNegInt(key string, fallback int) int {
	v := strings.TrimSpace(os.Getenv(key))
	if v == "" {
		return fallback
	}
	var n int
	if _, err := fmt.Sscanf(v, "%d", &n); err != nil || n < 0 {
		return fallback
	}
	return n
}

func detectLANIP(ctx context.Context) string {
	d := net.Dialer{Timeout: 300 * time.Millisecond}
	conn, err := d.DialContext(ctx, "udp4", "1.1.1.1:80")
	if err == nil {
		defer func() { _ = conn.Close() }()
		if addr, ok := conn.LocalAddr().(*net.UDPAddr); ok && addr.IP != nil {
			return addr.IP.String()
		}
	}
	return "localhost"
}
