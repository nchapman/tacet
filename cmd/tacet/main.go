package main

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"log"
	"net"
	"net/http"
	"os"
	"os/signal"
	"strings"
	"syscall"
	"time"

	"tacet/internal/api"
	"tacet/internal/library"
	"tacet/internal/qobuz"
	"tacet/internal/queue"
	"tacet/internal/upnp"
)

func main() {
	port := flag.Int("port", envInt("TACET_PORT", 8080), "HTTP port for media serving, API, and web remote")
	musicDir := flag.String("music-dir", os.Getenv("TACET_MUSIC_DIR"), "Comma-separated local music directories to index")
	publicURL := flag.String("public-url", os.Getenv("TACET_PUBLIC_URL"), "Optional base URL override for renderers (e.g. http://192.168.1.10:8080)")
	rendererURL := flag.String("renderer-url", os.Getenv("TACET_RENDERER_URL"), "Optional UPnP description XML URL to connect directly (e.g. http://<wiim-ip>:49152/description.xml)")
	streamMode := flag.String("stream-mode", envDefault("TACET_STREAM_MODE", "direct"), "Default Qobuz delivery mode: direct or relay")
	flag.Parse()

	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()

	// 1. Local NAS library catalog.
	var roots []string
	for _, r := range strings.Split(*musicDir, ",") {
		if r = strings.TrimSpace(r); r != "" {
			roots = append(roots, r)
		}
	}
	cat := library.NewCatalog(roots...)
	if len(roots) > 0 {
		log.Printf("scanning local music directories: %v", roots)
		n, err := cat.Scan()
		if err != nil {
			log.Printf("warning: library scan error: %v", err)
		} else {
			log.Printf("indexed %d local tracks across %d albums", n, len(cat.Albums()))
		}
	}

	// 2. Qobuz client (optional credentials from env or web UI login).
	qb := qobuz.NewClient(qobuz.Config{
		AppID:     os.Getenv("QOBUZ_APP_ID"),
		AppSecret: os.Getenv("QOBUZ_APP_SECRET"),
		AuthToken: os.Getenv("QOBUZ_AUTH_TOKEN"),
		FormatID:  qobuz.FormatFLAC192,
	})
	if email, pass := os.Getenv("QOBUZ_EMAIL"), os.Getenv("QOBUZ_PASSWORD"); email != "" && pass != "" && !qb.Status().Authenticated {
		lctx, cancel := context.WithTimeout(ctx, 10*time.Second)
		if st, err := qb.Login(lctx, email, pass); err != nil {
			log.Printf("warning: Qobuz env login failed: %v", err)
		} else {
			log.Printf("Qobuz authenticated as %s (%s)", st.UserName, st.Subscription)
		}
		cancel()
	}

	// 3. Queue / playback manager + UPnP GENA event subscriber.
	mgr := queue.NewManager(*port, *publicURL, qb)
	if err := mgr.SetStreamMode(queue.StreamMode(*streamMode)); err != nil {
		log.Fatalf("invalid --stream-mode: %v", err)
	}
	gena := upnp.NewGENAClient(mgr.HandleDeviceEvent)
	mgr.SetGENA(gena)

	// 4. Bind HTTP server first so UPnP GENA NOTIFY callbacks are immediately
	// reachable the moment discovery subscribes to renderers.
	apiServer := api.NewServer(cat, qb, mgr)
	apiServer.SetGENA(gena)
	httpSrv := &http.Server{
		Addr:              fmt.Sprintf(":%d", *port),
		Handler:           apiServer.Handler(),
		ReadHeaderTimeout: 10 * time.Second,
	}
	ln, err := net.Listen("tcp", httpSrv.Addr)
	if err != nil {
		log.Fatalf("listen %s: %v", httpSrv.Addr, err)
	}

	mgr.Start(ctx)
	defer mgr.Close()

	// 5. Initial UPnP renderer discovery (plus optional explicit URL).
	if *rendererURL != "" {
		cctx, cancel := context.WithTimeout(ctx, 5*time.Second)
		if dev, err := upnp.ConnectByURL(cctx, *rendererURL); err != nil {
			log.Printf("warning: failed to connect to %s: %v", *rendererURL, err)
		} else {
			mgr.RegisterRenderer(dev)
			info := dev.Info()
			log.Printf("connected renderer: %s (%s, gapless=%v, events=%v)",
				info.Name, info.Location, info.SupportsSetNext, info.SupportsEvents)
		}
		cancel()
	}

	go func() {
		dctx, cancel := context.WithTimeout(ctx, 5*time.Second)
		defer cancel()
		devices, err := upnp.Discover(dctx)
		if err != nil {
			log.Printf("SSDP discovery warning: %v", err)
			return
		}
		for _, d := range devices {
			mgr.RegisterRenderer(d)
			info := d.Info()
			log.Printf("discovered UPnP renderer: %s [%s] (%s, %d protocols, gapless=%v, events=%v)",
				info.Name, info.Model, info.Host, len(info.SinkProtocols), info.SupportsSetNext, info.SupportsEvents)
		}
		if len(devices) == 0 {
			log.Printf("no UPnP renderers discovered yet — click Discover or + URL in the web remote")
		}
	}()

	go func() {
		<-ctx.Done()
		shCtx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		_ = httpSrv.Shutdown(shCtx)
	}()

	lanIP := detectLANIP()
	log.Printf("tacet listening on http://%s:%d (local: http://localhost:%d)", lanIP, *port, *port)
	if err := httpSrv.Serve(ln); err != nil && !errors.Is(err, http.ErrServerClosed) {
		log.Fatalf("http server error: %v", err)
	}
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

func detectLANIP() string {
	conn, err := net.DialTimeout("udp4", "1.1.1.1:80", 300*time.Millisecond)
	if err == nil {
		defer conn.Close()
		if addr, ok := conn.LocalAddr().(*net.UDPAddr); ok && addr.IP != nil {
			return addr.IP.String()
		}
	}
	return "localhost"
}
