package library

import (
	"context"
	"errors"
	"io/fs"
	"log"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"time"

	"github.com/fsnotify/fsnotify"
)

const defaultWatchDebounce = 5 * time.Second

// AutoScanConfig controls automatic local library scanning (filesystem watching
// and periodic scheduled incremental rescans).
type AutoScanConfig struct {
	WatchEnabled bool
	Interval     time.Duration // 0 disables periodic scheduled rescans
	Debounce     time.Duration // defaults to 5s when <= 0
}

// AutoScanner watches local music directories for changes via fsnotify and
// performs periodic incremental rescans on a schedule. Both mechanisms can run
// together (recommended for NAS/SMB mounts where kernel watch events can be
// missed) and can be reconfigured live without restarting the server.
type AutoScanner struct {
	cat        *Catalog
	onScanDone func(ctx context.Context, trackCount int)

	mu      sync.RWMutex
	cfg     AutoScanConfig
	started bool
	cancel  context.CancelFunc
	done    chan struct{}
	reconf  chan reconfReq
}

type reconfReq struct {
	cfg AutoScanConfig
	ack chan struct{}
}

// NewAutoScanner creates an AutoScanner for cat. Call Start to begin background
// watching and scheduled rescans.
func NewAutoScanner(cat *Catalog, cfg AutoScanConfig, onScanDone func(ctx context.Context, trackCount int)) *AutoScanner {
	if cfg.Debounce <= 0 {
		cfg.Debounce = defaultWatchDebounce
	}
	return &AutoScanner{
		cat:        cat,
		onScanDone: onScanDone,
		cfg:        cfg,
		reconf:     make(chan reconfReq, 1),
	}
}

// Config returns the current AutoScanConfig.
func (a *AutoScanner) Config() AutoScanConfig {
	a.mu.RLock()
	defer a.mu.RUnlock()
	return a.cfg
}

// Configure updates WatchEnabled and Interval live. If the scanner is already
// running, the watcher and periodic ticker are reconfigured immediately.
func (a *AutoScanner) Configure(cfg AutoScanConfig) {
	if cfg.Debounce <= 0 {
		a.mu.RLock()
		cfg.Debounce = a.cfg.Debounce
		a.mu.RUnlock()
		if cfg.Debounce <= 0 {
			cfg.Debounce = defaultWatchDebounce
		}
	}
	a.mu.Lock()
	a.cfg = cfg
	started := a.started
	done := a.done
	a.mu.Unlock()
	if !started {
		return
	}
	req := reconfReq{cfg: cfg, ack: make(chan struct{})}
	select {
	case a.reconf <- req:
	case <-done:
		return
	}
	select {
	case <-req.ack:
	case <-done:
	}
}

// Start launches the background coordinator goroutine and waits until the
// initial watcher / ticker setup is armed. Calling Start more than once is a no-op.
func (a *AutoScanner) Start(ctx context.Context) {
	a.mu.Lock()
	if a.started {
		a.mu.Unlock()
		return
	}
	runCtx, cancel := context.WithCancel(ctx)
	a.started = true
	a.cancel = cancel
	a.done = make(chan struct{})
	cfg := a.cfg
	a.mu.Unlock()

	ready := make(chan struct{})
	go a.run(runCtx, cfg, ready)
	<-ready
}

// Stop stops the watcher, periodic ticker, and background goroutine, waiting
// for clean shutdown.
func (a *AutoScanner) Stop() {
	a.mu.Lock()
	if !a.started {
		a.mu.Unlock()
		return
	}
	cancel := a.cancel
	done := a.done
	a.mu.Unlock()

	cancel()
	<-done
}

func (a *AutoScanner) run(ctx context.Context, cfg AutoScanConfig, ready chan<- struct{}) {
	defer close(a.done)

	var (
		watcher      *fsnotify.Watcher
		watchEvents  <-chan fsnotify.Event
		watchErrors  <-chan error
		ticker       *time.Ticker
		tickerCh     <-chan time.Time
		debounce     *time.Timer
		debounceCh   <-chan time.Time
		scanDoneCh   = make(chan struct{}, 1)
		scanning     bool
		pendingScan  bool
		pendingLabel string
	)

	stopWatcher := func() {
		if watcher != nil {
			_ = watcher.Close()
			watcher = nil
			watchEvents = nil
			watchErrors = nil
		}
	}
	startWatcher := func() {
		stopWatcher()
		roots := a.cat.Roots()
		if len(roots) == 0 {
			return
		}
		w, err := fsnotify.NewWatcher()
		if err != nil {
			log.Printf("warning: library file watcher unavailable (periodic rescan still active): %v", err)
			return
		}
		watched := 0
		limitWarned := false
		for _, root := range roots {
			n, wErr := addWatchRecursive(w, root)
			watched += n
			if wErr != nil && !limitWarned {
				limitWarned = true
				log.Printf("warning: library file watcher could not watch all directories under %s (periodic rescan will cover missed paths): %v", root, wErr)
			}
		}
		watcher = w
		watchEvents = w.Events
		watchErrors = w.Errors
		log.Printf("library file watcher active (%d directories across %d root(s))", watched, len(roots))
	}

	stopTicker := func() {
		if ticker != nil {
			ticker.Stop()
			ticker = nil
			tickerCh = nil
		}
	}
	resetTicker := func(interval time.Duration) {
		stopTicker()
		if interval > 0 && len(a.cat.Roots()) > 0 {
			ticker = time.NewTicker(interval)
			tickerCh = ticker.C
		}
	}

	stopDebounce := func() {
		if debounce != nil {
			if !debounce.Stop() {
				select {
				case <-debounce.C:
				default:
				}
			}
			debounce = nil
			debounceCh = nil
		}
	}

	triggerScan := func(reason string) {
		if len(a.cat.Roots()) == 0 {
			return
		}
		if scanning {
			pendingScan = true
			pendingLabel = reason
			return
		}
		scanning = true
		go func(r string) {
			defer func() {
				select {
				case scanDoneCh <- struct{}{}:
				default:
				}
			}()
			start := time.Now()
			n, err := a.cat.ScanContext(ctx)
			if err != nil {
				if !errors.Is(err, context.Canceled) {
					log.Printf("warning: auto library scan (%s) failed: %v", r, err)
				}
				return
			}
			st := a.cat.Status()
			log.Printf("auto library scan (%s): %d local tracks across %d albums in %s",
				r, n, st.LocalAlbums, time.Since(start).Round(time.Millisecond))
			if a.onScanDone != nil {
				a.onScanDone(ctx, n)
			}
		}(reason)
	}

	applyConfig := func(next AutoScanConfig) {
		if next.WatchEnabled {
			if watcher == nil {
				startWatcher()
			}
		} else {
			stopWatcher()
			stopDebounce()
		}
		resetTicker(next.Interval)
		cfg = next
	}

	applyConfig(cfg)
	close(ready)
	defer func() {
		stopWatcher()
		stopTicker()
		stopDebounce()
	}()

	for {
		select {
		case <-ctx.Done():
			return

		case req := <-a.reconf:
			applyConfig(req.cfg)
			close(req.ack)

		case <-tickerCh:
			triggerScan("scheduled")

		case <-debounceCh:
			debounce = nil
			debounceCh = nil
			triggerScan("file-change")

		case <-scanDoneCh:
			scanning = false
			if pendingScan {
				r := pendingLabel
				pendingScan = false
				pendingLabel = ""
				triggerScan(r)
			}

		case ev, ok := <-watchEvents:
			if !ok {
				watcher = nil
				watchEvents = nil
				watchErrors = nil
				continue
			}
			if !isRelevantWatchEvent(ev) {
				continue
			}
			// If a new directory was created (e.g. copying an album folder),
			// watch it recursively immediately so files landing inside trigger events.
			if ev.Has(fsnotify.Create) && watcher != nil {
				if fi, err := os.Stat(ev.Name); err == nil && fi.IsDir() && !shouldSkipWatchDir(fi.Name()) {
					_, _ = addWatchRecursive(watcher, ev.Name)
				}
			}
			// Reset debounce window so multi-file album copies coalesce into one scan.
			if debounce == nil {
				debounce = time.NewTimer(cfg.Debounce)
				debounceCh = debounce.C
			} else {
				if !debounce.Stop() {
					select {
					case <-debounce.C:
					default:
					}
				}
				debounce.Reset(cfg.Debounce)
			}

		case err, ok := <-watchErrors:
			if !ok {
				continue
			}
			log.Printf("warning: library file watcher error: %v", err)
		}
	}
}

func addWatchRecursive(w *fsnotify.Watcher, root string) (int, error) {
	count := 0
	var firstErr error
	_ = filepath.WalkDir(root, func(path string, d fs.DirEntry, walkErr error) error {
		if walkErr != nil {
			return nil //nolint:nilerr // keep walking remaining directories on unreadable subdir
		}
		if !d.IsDir() {
			return nil
		}
		if path != root && shouldSkipWatchDir(d.Name()) {
			return filepath.SkipDir
		}
		if addErr := w.Add(path); addErr != nil {
			if firstErr == nil {
				firstErr = addErr
			}
			return nil
		}
		count++
		return nil
	})
	return count, firstErr
}

func shouldSkipWatchDir(name string) bool {
	if name == "" {
		return false
	}
	if strings.HasPrefix(name, ".") {
		return true
	}
	switch strings.ToLower(name) {
	case "@eadir", "#recycle", "$recycle.bin", "system volume information":
		return true
	}
	return false
}

func isRelevantWatchEvent(ev fsnotify.Event) bool {
	if ev.Op&(fsnotify.Create|fsnotify.Write|fsnotify.Remove|fsnotify.Rename) == 0 {
		return false
	}
	base := filepath.Base(ev.Name)
	if base == "" || strings.HasPrefix(base, ".") || shouldSkipWatchDir(base) {
		return false
	}
	lower := strings.ToLower(base)
	if strings.HasSuffix(lower, ".tmp") || strings.HasSuffix(lower, ".part") || strings.HasSuffix(lower, ".crdownload") {
		return false
	}
	// Removals and renames cannot be stat'd and may be album directories whose
	// names contain dots (e.g. "Standards (Vol. 1)"), so always treat as relevant.
	if ev.Has(fsnotify.Remove) || ev.Has(fsnotify.Rename) {
		return true
	}
	// Newly created directories (including ones with dots in their name) must
	// always be watched and scanned.
	if ev.Has(fsnotify.Create) {
		if fi, err := os.Stat(ev.Name); err == nil && fi.IsDir() {
			return true
		}
	}
	ext := strings.ToLower(filepath.Ext(base))
	if ext == "" {
		return true
	}
	if isSupportedAudioExt(ext) {
		return true
	}
	switch ext {
	case ".jpg", ".jpeg", ".png", ".webp":
		return true
	}
	return false
}

func isSupportedAudioExt(ext string) bool {
	switch ext {
	case ".flac", ".wav", ".aiff", ".aif", ".m4a", ".mp4", ".mp3", ".ogg", ".opus", ".dsf":
		return true
	default:
		return false
	}
}
