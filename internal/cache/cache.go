// Package cache provides a general-purpose, SQLite-backed key-value cache with
// Redis-style TTL operations, LRU size eviction, key namespacing, and typed
// JSON helpers with singleflight stampede protection.
package cache

import (
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	_ "modernc.org/sqlite" // Pure-Go zero-CGo SQLite driver.
)

// NoExpiration explicitly disables TTL for a Set call even when DefaultTTL is configured.
const NoExpiration time.Duration = -1

// ErrClosed is returned by Cache operations invoked after Close.
var ErrClosed = errors.New("cache: closed")

const (
	defaultMaxBytes        int64 = 256 << 20 // 256 MiB
	accessTouchGranularity       = time.Minute
)

// Store is the common KV interface implemented by both *Cache and *Namespaced.
type Store interface {
	Get(key string) ([]byte, bool, error)
	Set(key string, value []byte, ttl time.Duration) error
	Del(key string) (bool, error)
	DelPrefix(prefix string) (int64, error)
	Exists(key string) (bool, error)
	Keys(pattern string) ([]string, error)
	Expire(key string, ttl time.Duration) (bool, error)
	TTL(key string) (time.Duration, bool, error)
	Persist(key string) (bool, error)
	Touch(key string) (bool, error)
	singleflight() *flightGroup
	qualifyKey(key string) string
}

// Stats summarizes cache hit rate and disk footprint.
type Stats struct {
	Hits     int64
	Misses   int64
	HitRate  float64
	Items    int64
	Bytes    int64
	MaxBytes int64
}

// Option configures a Cache opened with Open.
type Option func(*options)

type options struct {
	defaultTTL    time.Duration
	maxBytes      int64
	pruneInterval time.Duration
	nowFn         func() time.Time
}

// WithDefaultTTL sets the fallback TTL used when Set is called with ttl == 0.
func WithDefaultTTL(d time.Duration) Option {
	return func(o *options) {
		if d > 0 {
			o.defaultTTL = d
		}
	}
}

// WithMaxBytes sets the maximum total payload bytes stored in SQLite before
// LRU eviction runs. Pass 0 to disable size-based eviction.
func WithMaxBytes(n int64) Option {
	return func(o *options) {
		o.maxBytes = max(0, n)
	}
}

// WithPruneInterval enables periodic background pruning of expired entries.
func WithPruneInterval(d time.Duration) Option {
	return func(o *options) {
		if d > 0 {
			o.pruneInterval = d
		}
	}
}

// WithNow overrides the clock function (primarily for deterministic tests).
func WithNow(fn func() time.Time) Option {
	return func(o *options) {
		if fn != nil {
			o.nowFn = fn
		}
	}
}

// Cache is a thread-safe SQLite-backed KV cache.
type Cache struct {
	mu         sync.Mutex
	db         *sql.DB
	dbPath     string
	closed     bool
	defaultTTL time.Duration
	maxBytes   int64
	nowFn      func() time.Time

	hits   atomic.Int64
	misses atomic.Int64
	sf     flightGroup

	pruneStop chan struct{}
	pruneDone chan struct{}
}

// Open opens (or creates) a SQLite-backed KV cache at dbPath.
// Pass "" or ":memory:" for an in-memory SQLite database.
func Open(dbPath string, opts ...Option) (*Cache, error) {
	cfg := options{
		maxBytes: defaultMaxBytes,
		nowFn:    func() time.Time { return time.Now().UTC() },
	}
	for _, opt := range opts {
		if opt != nil {
			opt(&cfg)
		}
	}

	dsn := ":memory:"
	if strings.TrimSpace(dbPath) != "" && dbPath != ":memory:" {
		abs, err := filepath.Abs(dbPath)
		if err != nil {
			return nil, fmt.Errorf("resolve cache db path: %w", err)
		}
		if err := os.MkdirAll(filepath.Dir(abs), 0o750); err != nil {
			return nil, fmt.Errorf("create cache dir: %w", err)
		}
		dsn = abs
	}

	db, err := sql.Open("sqlite", dsn)
	if err != nil {
		return nil, fmt.Errorf("open cache sqlite %s: %w", dsn, err)
	}
	db.SetMaxOpenConns(1)
	db.SetMaxIdleConns(1)

	ctx := context.Background()
	pragmas := []string{
		"PRAGMA busy_timeout = 5000;",
		"PRAGMA synchronous = NORMAL;",
		"PRAGMA temp_store = MEMORY;",
	}
	if dsn != ":memory:" {
		pragmas = append([]string{"PRAGMA journal_mode = WAL;"}, pragmas...)
	}
	for _, p := range pragmas {
		if _, err := db.ExecContext(ctx, p); err != nil {
			_ = db.Close()
			return nil, fmt.Errorf("cache sqlite pragma %q: %w", p, err)
		}
	}

	schema := `
CREATE TABLE IF NOT EXISTS kv_cache (
	key         TEXT PRIMARY KEY,
	value       BLOB NOT NULL,
	size        INTEGER NOT NULL,
	expires_at  INTEGER,
	accessed_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_kv_cache_expires ON kv_cache(expires_at) WHERE expires_at IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_kv_cache_accessed ON kv_cache(accessed_at);
`
	if _, err := db.ExecContext(ctx, schema); err != nil {
		_ = db.Close()
		return nil, fmt.Errorf("init cache schema: %w", err)
	}

	c := &Cache{
		db:         db,
		dbPath:     dsn,
		defaultTTL: cfg.defaultTTL,
		maxBytes:   cfg.maxBytes,
		nowFn:      cfg.nowFn,
	}

	if cfg.pruneInterval > 0 {
		c.pruneStop = make(chan struct{})
		c.pruneDone = make(chan struct{})
		go c.pruneLoop(cfg.pruneInterval)
	}

	return c, nil
}

func (c *Cache) pruneLoop(interval time.Duration) {
	defer close(c.pruneDone)
	ticker := time.NewTicker(interval)
	defer ticker.Stop()
	for {
		select {
		case <-c.pruneStop:
			return
		case <-ticker.C:
			_, _ = c.Prune()
		}
	}
}

// Close stops any background pruner and closes the underlying SQLite database.
func (c *Cache) Close() error {
	c.mu.Lock()
	if c.closed {
		c.mu.Unlock()
		return nil
	}
	c.closed = true
	stop := c.pruneStop
	done := c.pruneDone
	db := c.db
	c.mu.Unlock()

	if stop != nil {
		close(stop)
		<-done
	}
	if db != nil {
		return db.Close()
	}
	return nil
}

func (c *Cache) singleflight() *flightGroup { return &c.sf }
func (c *Cache) qualifyKey(key string) string {
	return key
}

// Namespace returns a scoped Store view whose keys are prefixed with prefix + ":".
func (c *Cache) Namespace(prefix string) *Namespaced {
	p := strings.TrimRight(strings.TrimSpace(prefix), ":")
	if p != "" {
		p += ":"
	}
	return &Namespaced{parent: c, prefix: p}
}

// Get retrieves the raw bytes stored under key.
// Returns (nil, false, nil) if the key does not exist or has expired.
func (c *Cache) Get(key string) ([]byte, bool, error) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.closed {
		return nil, false, ErrClosed
	}

	ctx := context.Background()
	now := c.nowFn().UnixMilli()
	var val []byte
	var expiresAt sql.NullInt64
	var accessedAt int64
	err := c.db.QueryRowContext(
		ctx,
		`SELECT value, expires_at, accessed_at FROM kv_cache WHERE key = ?`,
		key,
	).Scan(&val, &expiresAt, &accessedAt)
	if errors.Is(err, sql.ErrNoRows) {
		c.misses.Add(1)
		return nil, false, nil
	}
	if err != nil {
		c.misses.Add(1)
		return nil, false, fmt.Errorf("cache get %q: %w", key, err)
	}
	if expiresAt.Valid && expiresAt.Int64 <= now {
		_, _ = c.db.ExecContext(ctx, `DELETE FROM kv_cache WHERE key = ?`, key)
		c.misses.Add(1)
		return nil, false, nil
	}

	// Only bump accessed_at if stale to keep hot reads pure SELECTs.
	if now-accessedAt >= accessTouchGranularity.Milliseconds() {
		_, _ = c.db.ExecContext(ctx, `UPDATE kv_cache SET accessed_at = ? WHERE key = ?`, now, key)
	}
	c.hits.Add(1)
	return val, true, nil
}

// Set writes value under key with an optional TTL.
//   - ttl > 0: expires after ttl
//   - ttl == 0: uses DefaultTTL (or no expiration if DefaultTTL is 0)
//   - ttl < 0 (NoExpiration): never expires
func (c *Cache) Set(key string, value []byte, ttl time.Duration) error {
	if key == "" {
		return errors.New("cache key must not be empty")
	}
	if value == nil {
		value = []byte{}
	}

	c.mu.Lock()
	defer c.mu.Unlock()
	if c.closed {
		return ErrClosed
	}

	now := c.nowFn().UnixMilli()
	var expiresAt any
	effectiveTTL := ttl
	if effectiveTTL == 0 {
		effectiveTTL = c.defaultTTL
	}
	if effectiveTTL > 0 {
		expiresAt = now + effectiveTTL.Milliseconds()
	}

	ctx := context.Background()
	_, err := c.db.ExecContext(
		ctx,
		`INSERT INTO kv_cache(key, value, size, expires_at, accessed_at)
		 VALUES(?, ?, ?, ?, ?)
		 ON CONFLICT(key) DO UPDATE SET
			value = excluded.value,
			size = excluded.size,
			expires_at = excluded.expires_at,
			accessed_at = excluded.accessed_at`,
		key, value, int64(len(value)), expiresAt, now,
	)
	if err != nil {
		return fmt.Errorf("cache set %q: %w", key, err)
	}

	if c.maxBytes > 0 {
		c.evictLocked(ctx, now)
	}
	return nil
}

// evictLocked enforces maxBytes by pruning expired rows first, then deleting
// least-recently-accessed rows until total payload bytes <= maxBytes.
func (c *Cache) evictLocked(ctx context.Context, now int64) {
	var total int64
	if err := c.db.QueryRowContext(ctx, `SELECT COALESCE(SUM(size), 0) FROM kv_cache`).Scan(&total); err != nil || total <= c.maxBytes {
		return
	}
	_, _ = c.db.ExecContext(ctx, `DELETE FROM kv_cache WHERE expires_at IS NOT NULL AND expires_at <= ?`, now)
	if err := c.db.QueryRowContext(ctx, `SELECT COALESCE(SUM(size), 0) FROM kv_cache`).Scan(&total); err != nil || total <= c.maxBytes {
		return
	}

	excess := total - c.maxBytes
	rows, err := c.db.QueryContext(ctx, `SELECT key, size FROM kv_cache ORDER BY accessed_at ASC`)
	if err != nil {
		return
	}
	var victims []string
	var freed int64
	for rows.Next() {
		var k string
		var sz int64
		if scanErr := rows.Scan(&k, &sz); scanErr != nil {
			break
		}
		victims = append(victims, k)
		freed += sz
		if freed >= excess {
			break
		}
	}
	_ = rows.Close()

	if len(victims) == 0 {
		return
	}
	tx, err := c.db.BeginTx(ctx, nil)
	if err != nil {
		return
	}
	stmt, err := tx.PrepareContext(ctx, `DELETE FROM kv_cache WHERE key = ?`)
	if err != nil {
		_ = tx.Rollback()
		return
	}
	for _, k := range victims {
		_, _ = stmt.ExecContext(ctx, k)
	}
	_ = stmt.Close()
	_ = tx.Commit()
}

// Del deletes a single key. Returns true if a row was removed.
func (c *Cache) Del(key string) (bool, error) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.closed {
		return false, ErrClosed
	}
	res, err := c.db.ExecContext(context.Background(), `DELETE FROM kv_cache WHERE key = ?`, key)
	if err != nil {
		return false, fmt.Errorf("cache del %q: %w", key, err)
	}
	n, _ := res.RowsAffected()
	return n > 0, nil
}

// DelPrefix removes all keys starting with prefix using a primary-key range scan.
func (c *Cache) DelPrefix(prefix string) (int64, error) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.closed {
		return 0, ErrClosed
	}
	ctx := context.Background()
	if prefix == "" {
		res, err := c.db.ExecContext(ctx, `DELETE FROM kv_cache`)
		if err != nil {
			return 0, fmt.Errorf("cache del prefix: %w", err)
		}
		n, _ := res.RowsAffected()
		return n, nil
	}
	upper, ok := prefixUpperBound(prefix)
	var (
		res sql.Result
		err error
	)
	if ok {
		res, err = c.db.ExecContext(ctx, `DELETE FROM kv_cache WHERE key >= ? AND key < ?`, prefix, upper)
	} else {
		res, err = c.db.ExecContext(ctx, `DELETE FROM kv_cache WHERE key >= ?`, prefix)
	}
	if err != nil {
		return 0, fmt.Errorf("cache del prefix %q: %w", prefix, err)
	}
	n, _ := res.RowsAffected()
	return n, nil
}

// Exists reports whether key is present and not expired.
func (c *Cache) Exists(key string) (bool, error) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.closed {
		return false, ErrClosed
	}
	ctx := context.Background()
	now := c.nowFn().UnixMilli()
	var expiresAt sql.NullInt64
	err := c.db.QueryRowContext(ctx, `SELECT expires_at FROM kv_cache WHERE key = ?`, key).Scan(&expiresAt)
	if errors.Is(err, sql.ErrNoRows) {
		return false, nil
	}
	if err != nil {
		return false, fmt.Errorf("cache exists %q: %w", key, err)
	}
	if expiresAt.Valid && expiresAt.Int64 <= now {
		_, _ = c.db.ExecContext(ctx, `DELETE FROM kv_cache WHERE key = ?`, key)
		return false, nil
	}
	return true, nil
}

// Keys returns all non-expired keys matching the glob pattern (e.g. "qobuz:artist:*").
// An empty pattern returns all non-expired keys.
func (c *Cache) Keys(pattern string) ([]string, error) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.closed {
		return nil, ErrClosed
	}
	ctx := context.Background()
	now := c.nowFn().UnixMilli()
	var (
		rows *sql.Rows
		err  error
	)
	if pattern == "" || pattern == "*" {
		rows, err = c.db.QueryContext(
			ctx,
			`SELECT key FROM kv_cache WHERE expires_at IS NULL OR expires_at > ? ORDER BY key ASC`,
			now,
		)
	} else {
		rows, err = c.db.QueryContext(
			ctx,
			`SELECT key FROM kv_cache WHERE key GLOB ? AND (expires_at IS NULL OR expires_at > ?) ORDER BY key ASC`,
			pattern, now,
		)
	}
	if err != nil {
		return nil, fmt.Errorf("cache keys %q: %w", pattern, err)
	}
	defer func() { _ = rows.Close() }()

	var out []string
	for rows.Next() {
		var k string
		if err := rows.Scan(&k); err != nil {
			return nil, fmt.Errorf("cache keys scan: %w", err)
		}
		out = append(out, k)
	}
	return out, rows.Err()
}

// Expire updates the TTL on an existing non-expired key.
// Passing ttl <= 0 deletes the key immediately if present.
func (c *Cache) Expire(key string, ttl time.Duration) (bool, error) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.closed {
		return false, ErrClosed
	}
	ctx := context.Background()
	now := c.nowFn().UnixMilli()
	if ttl <= 0 {
		res, err := c.db.ExecContext(ctx, `DELETE FROM kv_cache WHERE key = ? AND (expires_at IS NULL OR expires_at > ?)`, key, now)
		if err != nil {
			return false, fmt.Errorf("cache expire %q: %w", key, err)
		}
		n, _ := res.RowsAffected()
		return n > 0, nil
	}
	res, err := c.db.ExecContext(
		ctx,
		`UPDATE kv_cache SET expires_at = ? WHERE key = ? AND (expires_at IS NULL OR expires_at > ?)`,
		now+ttl.Milliseconds(), key, now,
	)
	if err != nil {
		return false, fmt.Errorf("cache expire %q: %w", key, err)
	}
	n, _ := res.RowsAffected()
	return n > 0, nil
}

// TTL returns the remaining lifetime of key.
// Returns (0, false, nil) if key does not exist or is expired.
// Returns (NoExpiration, true, nil) if key exists with no expiration.
func (c *Cache) TTL(key string) (time.Duration, bool, error) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.closed {
		return 0, false, ErrClosed
	}
	ctx := context.Background()
	now := c.nowFn().UnixMilli()
	var expiresAt sql.NullInt64
	err := c.db.QueryRowContext(ctx, `SELECT expires_at FROM kv_cache WHERE key = ?`, key).Scan(&expiresAt)
	if errors.Is(err, sql.ErrNoRows) {
		return 0, false, nil
	}
	if err != nil {
		return 0, false, fmt.Errorf("cache ttl %q: %w", key, err)
	}
	if !expiresAt.Valid {
		return NoExpiration, true, nil
	}
	rem := expiresAt.Int64 - now
	if rem <= 0 {
		_, _ = c.db.ExecContext(ctx, `DELETE FROM kv_cache WHERE key = ?`, key)
		return 0, false, nil
	}
	return time.Duration(rem) * time.Millisecond, true, nil
}

// Persist removes the expiration from an existing non-expired key.
func (c *Cache) Persist(key string) (bool, error) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.closed {
		return false, ErrClosed
	}
	now := c.nowFn().UnixMilli()
	res, err := c.db.ExecContext(
		context.Background(),
		`UPDATE kv_cache SET expires_at = NULL WHERE key = ? AND (expires_at IS NULL OR expires_at > ?)`,
		key, now,
	)
	if err != nil {
		return false, fmt.Errorf("cache persist %q: %w", key, err)
	}
	n, _ := res.RowsAffected()
	return n > 0, nil
}

// Touch refreshes a key's LRU timestamp without reading its value.
func (c *Cache) Touch(key string) (bool, error) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.closed {
		return false, ErrClosed
	}
	now := c.nowFn().UnixMilli()
	res, err := c.db.ExecContext(
		context.Background(),
		`UPDATE kv_cache SET accessed_at = ? WHERE key = ? AND (expires_at IS NULL OR expires_at > ?)`,
		now, key, now,
	)
	if err != nil {
		return false, fmt.Errorf("cache touch %q: %w", key, err)
	}
	n, _ := res.RowsAffected()
	return n > 0, nil
}

// Clear removes every entry from the cache.
func (c *Cache) Clear() error {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.closed {
		return ErrClosed
	}
	if _, err := c.db.ExecContext(context.Background(), `DELETE FROM kv_cache`); err != nil {
		return fmt.Errorf("cache clear: %w", err)
	}
	return nil
}

// Prune deletes all expired entries and returns the number removed.
func (c *Cache) Prune() (int64, error) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.closed {
		return 0, ErrClosed
	}
	now := c.nowFn().UnixMilli()
	res, err := c.db.ExecContext(context.Background(), `DELETE FROM kv_cache WHERE expires_at IS NOT NULL AND expires_at <= ?`, now)
	if err != nil {
		return 0, fmt.Errorf("cache prune: %w", err)
	}
	n, _ := res.RowsAffected()
	return n, nil
}

// Size returns the count of non-expired items currently in the cache.
func (c *Cache) Size() (int64, error) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.closed {
		return 0, ErrClosed
	}
	now := c.nowFn().UnixMilli()
	var n int64
	if err := c.db.QueryRowContext(
		context.Background(),
		`SELECT COUNT(*) FROM kv_cache WHERE expires_at IS NULL OR expires_at > ?`,
		now,
	).Scan(&n); err != nil {
		return 0, fmt.Errorf("cache size: %w", err)
	}
	return n, nil
}

// Stats returns current cache hit/miss counters and live item/byte totals.
func (c *Cache) Stats() Stats {
	hits := c.hits.Load()
	misses := c.misses.Load()
	var rate float64
	if total := hits + misses; total > 0 {
		rate = float64(hits) / float64(total)
	}

	c.mu.Lock()
	defer c.mu.Unlock()
	var items, bytes int64
	if !c.closed {
		now := c.nowFn().UnixMilli()
		_ = c.db.QueryRowContext(
			context.Background(),
			`SELECT COUNT(*), COALESCE(SUM(size), 0) FROM kv_cache WHERE expires_at IS NULL OR expires_at > ?`,
			now,
		).Scan(&items, &bytes)
	}
	return Stats{
		Hits:     hits,
		Misses:   misses,
		HitRate:  rate,
		Items:    items,
		Bytes:    bytes,
		MaxBytes: c.maxBytes,
	}
}

// Namespaced is a key-prefixed view over a Cache.
type Namespaced struct {
	parent *Cache
	prefix string
}

func (n *Namespaced) singleflight() *flightGroup { return n.parent.singleflight() }
func (n *Namespaced) qualifyKey(key string) string {
	return n.prefix + key
}

// Get retrieves the raw bytes stored under the namespaced key.
func (n *Namespaced) Get(key string) ([]byte, bool, error) {
	return n.parent.Get(n.prefix + key)
}

// Set writes value under the namespaced key with an optional TTL.
func (n *Namespaced) Set(key string, value []byte, ttl time.Duration) error {
	return n.parent.Set(n.prefix+key, value, ttl)
}

// Del deletes a single key within this namespace.
func (n *Namespaced) Del(key string) (bool, error) {
	return n.parent.Del(n.prefix + key)
}

// DelPrefix removes all keys in this namespace starting with prefix.
func (n *Namespaced) DelPrefix(prefix string) (int64, error) {
	return n.parent.DelPrefix(n.prefix + prefix)
}

// Clear removes every entry in this namespace.
func (n *Namespaced) Clear() error {
	_, err := n.parent.DelPrefix(n.prefix)
	return err
}

// Exists reports whether the namespaced key is present and not expired.
func (n *Namespaced) Exists(key string) (bool, error) {
	return n.parent.Exists(n.prefix + key)
}

// Keys returns all non-expired keys in this namespace matching pattern (with prefix stripped).
func (n *Namespaced) Keys(pattern string) ([]string, error) {
	if pattern == "" {
		pattern = "*"
	}
	raw, err := n.parent.Keys(n.prefix + pattern)
	if err != nil {
		return nil, err
	}
	if n.prefix == "" {
		return raw, nil
	}
	out := make([]string, 0, len(raw))
	for _, k := range raw {
		out = append(out, strings.TrimPrefix(k, n.prefix))
	}
	return out, nil
}

// Expire updates the TTL on an existing non-expired namespaced key.
func (n *Namespaced) Expire(key string, ttl time.Duration) (bool, error) {
	return n.parent.Expire(n.prefix+key, ttl)
}

// TTL returns the remaining lifetime of a namespaced key.
func (n *Namespaced) TTL(key string) (time.Duration, bool, error) {
	return n.parent.TTL(n.prefix + key)
}

// Persist removes the expiration from an existing non-expired namespaced key.
func (n *Namespaced) Persist(key string) (bool, error) {
	return n.parent.Persist(n.prefix + key)
}

// Touch refreshes a namespaced key's LRU timestamp without reading its value.
func (n *Namespaced) Touch(key string) (bool, error) {
	return n.parent.Touch(n.prefix + key)
}

// GetJSON reads and unmarshals a JSON-encoded value from s.
// If the stored payload is corrupt JSON, it is deleted and treated as a miss.
func GetJSON[T any](s Store, key string) (T, bool, error) {
	var zero T
	if s == nil {
		return zero, false, nil
	}
	raw, ok, err := s.Get(key)
	if err != nil || !ok {
		return zero, false, err
	}
	var out T
	if unmarshalErr := json.Unmarshal(raw, &out); unmarshalErr != nil {
		_, _ = s.Del(key)
		return zero, false, nil //nolint:nilerr // corrupt cached payload self-heals as a cache miss
	}
	return out, true, nil
}

// SetJSON marshals value as JSON and writes it to s with ttl.
func SetJSON(s Store, key string, value any, ttl time.Duration) error {
	if s == nil {
		return nil
	}
	raw, err := json.Marshal(value)
	if err != nil {
		return fmt.Errorf("cache marshal %q: %w", key, err)
	}
	return s.Set(key, raw, ttl)
}

// GetOrSet returns the cached JSON value for key, or invokes fn once (with
// singleflight deduplication across concurrent callers) and caches the result.
func GetOrSet[T any](ctx context.Context, s Store, key string, ttl time.Duration, fn func(context.Context) (T, error)) (T, error) {
	return GetOrSetConditional(ctx, s, key, ttl, func(c context.Context) (T, bool, error) {
		v, err := fn(c)
		return v, err == nil, err
	})
}

// GetOrSetConditional is like GetOrSet, but fn also returns shouldCache so
// callers can return degraded/partial results without poisoning the cache.
func GetOrSetConditional[T any](
	ctx context.Context,
	s Store,
	key string,
	ttl time.Duration,
	fn func(context.Context) (T, bool, error),
) (T, error) {
	if s == nil {
		v, _, err := fn(ctx)
		return v, err
	}
	if v, ok, err := GetJSON[T](s, key); err == nil && ok {
		return v, nil
	}
	fullKey := s.qualifyKey(key)
	res, err := s.singleflight().doCtx(ctx, fullKey, func() (any, error) {
		if v, ok, err := GetJSON[T](s, key); err == nil && ok {
			return v, nil
		}
		v, shouldCache, err := fn(ctx)
		if err != nil {
			return v, err
		}
		if shouldCache {
			_ = SetJSON(s, key, v, ttl)
		}
		return v, nil
	})
	if err != nil {
		var zero T
		return zero, err
	}
	out, _ := res.(T)
	return out, nil
}

// prefixUpperBound computes the smallest string strictly greater than every
// string with prefix p, enabling an index range scan (key >= p AND key < upper).
func prefixUpperBound(p string) (string, bool) {
	b := []byte(p)
	for i := len(b) - 1; i >= 0; i-- {
		if b[i] < 0xff {
			b[i]++
			return string(b[:i+1]), true
		}
	}
	return "", false
}

// flightGroup provides minimal per-key singleflight deduplication without
// pulling in golang.org/x/sync.
type flightGroup struct {
	mu    sync.Mutex
	calls map[string]*flightCall
}

type flightCall struct {
	done chan struct{}
	val  any
	err  error
}

func (g *flightGroup) doCtx(ctx context.Context, key string, fn func() (any, error)) (any, error) {
	g.mu.Lock()
	if g.calls == nil {
		g.calls = make(map[string]*flightCall)
	}
	if c, ok := g.calls[key]; ok {
		g.mu.Unlock()
		select {
		case <-c.done:
			return c.val, c.err
		case <-ctx.Done():
			return nil, ctx.Err()
		}
	}
	c := &flightCall{done: make(chan struct{})}
	g.calls[key] = c
	g.mu.Unlock()

	defer func() {
		g.mu.Lock()
		delete(g.calls, key)
		g.mu.Unlock()
		close(c.done)
	}()

	c.val, c.err = fn()
	return c.val, c.err
}
