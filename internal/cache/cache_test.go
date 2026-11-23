package cache

import (
	"context"
	"errors"
	"fmt"
	"path/filepath"
	"reflect"
	"sync"
	"sync/atomic"
	"testing"
	"time"
)

func TestCache_CoreAndTTL(t *testing.T) {
	now := time.Date(2026, 6, 1, 12, 0, 0, 0, time.UTC)
	c, err := Open(":memory:", WithNow(func() time.Time { return now }))
	if err != nil {
		t.Fatalf("Open: %v", err)
	}
	defer func() { _ = c.Close() }()

	if _, ok, getErr := c.Get("missing"); getErr != nil || ok {
		t.Fatalf("Get(missing) = (_, %v, %v), want false/nil", ok, getErr)
	}

	if setErr := c.Set("k1", []byte("hello"), time.Minute); setErr != nil {
		t.Fatalf("Set: %v", setErr)
	}
	got, ok, err := c.Get("k1")
	if err != nil || !ok || string(got) != "hello" {
		t.Fatalf("Get(k1) = (%q, %v, %v), want hello/true/nil", string(got), ok, err)
	}

	ttl, ok, err := c.TTL("k1")
	if err != nil || !ok || ttl != time.Minute {
		t.Fatalf("TTL(k1) = (%v, %v, %v), want 1m/true/nil", ttl, ok, err)
	}

	// Persist removes expiration.
	persisted, err := c.Persist("k1")
	if err != nil || !persisted {
		t.Fatalf("Persist(k1) = (%v, %v), want true/nil", persisted, err)
	}
	ttl, ok, err = c.TTL("k1")
	if err != nil || !ok || ttl != NoExpiration {
		t.Fatalf("TTL after Persist = (%v, %v, %v), want NoExpiration/true/nil", ttl, ok, err)
	}

	// Expire restores expiration; advancing past it causes lazy miss.
	if ok, err := c.Expire("k1", 10*time.Second); err != nil || !ok {
		t.Fatalf("Expire(k1) = (%v, %v), want true/nil", ok, err)
	}
	now = now.Add(15 * time.Second)
	if _, ok, err := c.Get("k1"); err != nil || ok {
		t.Fatalf("Get expired k1 = (_, %v, %v), want false/nil", ok, err)
	}
	if exists, err := c.Exists("k1"); err != nil || exists {
		t.Fatalf("Exists expired k1 = (%v, %v), want false/nil", exists, err)
	}
}

func TestCache_KeysDelPrefixAndNamespace(t *testing.T) {
	c, err := Open(":memory:")
	if err != nil {
		t.Fatalf("Open: %v", err)
	}
	defer func() { _ = c.Close() }()

	qb := c.Namespace("qobuz")
	mb := c.Namespace("mb")

	_ = qb.Set("artist:1", []byte("a1"), time.Hour)
	_ = qb.Set("artist:2", []byte("a2"), time.Hour)
	_ = qb.Set("album:9", []byte("al9"), time.Hour)
	_ = mb.Set("artist:1", []byte("mb1"), time.Hour)

	keys, err := qb.Keys("artist:*")
	if err != nil {
		t.Fatalf("Keys: %v", err)
	}
	want := []string{"artist:1", "artist:2"}
	if !reflect.DeepEqual(keys, want) {
		t.Fatalf("Keys = %v, want %v", keys, want)
	}

	// Clearing qobuz namespace leaves mb untouched.
	if err := qb.Clear(); err != nil {
		t.Fatalf("qb.Clear: %v", err)
	}
	if exists, _ := qb.Exists("album:9"); exists {
		t.Fatal("expected qb album:9 to be deleted")
	}
	if got, ok, _ := mb.Get("artist:1"); !ok || string(got) != "mb1" {
		t.Fatalf("mb artist:1 = (%q, %v), want mb1/true", string(got), ok)
	}
}

func TestCache_LRUEviction(t *testing.T) {
	now := time.Date(2026, 6, 1, 12, 0, 0, 0, time.UTC)
	c, err := Open(":memory:",
		WithMaxBytes(10),
		WithNow(func() time.Time { return now }),
	)
	if err != nil {
		t.Fatalf("Open: %v", err)
	}
	defer func() { _ = c.Close() }()

	_ = c.Set("a", []byte("12345"), time.Hour) // 5 bytes, t=0
	now = now.Add(2 * time.Minute)
	_ = c.Set("b", []byte("67890"), time.Hour) // 5 bytes, t=2m (total 10)
	now = now.Add(2 * time.Minute)
	// Touch "a" so "b" becomes LRU.
	_, _, _ = c.Get("a") // t=4m
	now = now.Add(2 * time.Minute)
	_ = c.Set("c", []byte("abcde"), time.Hour) // 5 bytes, total 15 -> evicts "b"

	if ok, _ := c.Exists("b"); ok {
		t.Fatal("expected LRU key 'b' to be evicted")
	}
	if ok, _ := c.Exists("a"); !ok {
		t.Fatal("expected touched key 'a' to survive")
	}
	if ok, _ := c.Exists("c"); !ok {
		t.Fatal("expected new key 'c' to survive")
	}
}

func TestCache_GetOrSetSingleflightAndPersistence(t *testing.T) {
	dir := t.TempDir()
	dbPath := filepath.Join(dir, "cache.db")

	c, err := Open(dbPath)
	if err != nil {
		t.Fatalf("Open: %v", err)
	}

	type payload struct {
		Name  string `json:"name"`
		Count int    `json:"count"`
	}

	var calls atomic.Int32
	var wg sync.WaitGroup
	for range 8 {
		wg.Add(1)
		go func() {
			defer wg.Done()
			v, gsErr := GetOrSet(context.Background(), c, "artist:adele", time.Hour, func(context.Context) (payload, error) {
				calls.Add(1)
				time.Sleep(25 * time.Millisecond)
				return payload{Name: "Adele", Count: 42}, nil
			})
			if gsErr != nil || v.Name != "Adele" || v.Count != 42 {
				t.Errorf("GetOrSet = (%+v, %v)", v, gsErr)
			}
		}()
	}
	wg.Wait()
	if got := calls.Load(); got != 1 {
		t.Fatalf("singleflight calls = %d, want 1", got)
	}
	_ = c.Close()

	// Re-open from disk and verify entry survived restart.
	c2, err := Open(dbPath)
	if err != nil {
		t.Fatalf("re-Open: %v", err)
	}
	defer func() { _ = c2.Close() }()

	got, ok, err := GetJSON[payload](c2, "artist:adele")
	if err != nil || !ok || got.Name != "Adele" || got.Count != 42 {
		t.Fatalf("persisted GetJSON = (%+v, %v, %v), want Adele/42/true/nil", got, ok, err)
	}
}

func TestCache_ClosedReturnsErrClosed(t *testing.T) {
	c, err := Open(filepath.Join(t.TempDir(), "closed.db"))
	if err != nil {
		t.Fatalf("Open: %v", err)
	}
	if err := c.Close(); err != nil {
		t.Fatalf("Close: %v", err)
	}
	if _, _, err := c.Get("k"); !errors.Is(err, ErrClosed) {
		t.Fatalf("Get after Close = %v, want ErrClosed", err)
	}
	if err := c.Set("k", []byte("v"), time.Minute); !errors.Is(err, ErrClosed) {
		t.Fatalf("Set after Close = %v, want ErrClosed", err)
	}
	if _, err := c.Del("k"); !errors.Is(err, ErrClosed) {
		t.Fatalf("Del after Close = %v, want ErrClosed", err)
	}
	if _, err := c.Prune(); !errors.Is(err, ErrClosed) {
		t.Fatalf("Prune after Close = %v, want ErrClosed", err)
	}
}

func TestCache_SingleflightRespectsCallerCancel(t *testing.T) {
	c, err := Open(filepath.Join(t.TempDir(), "sf-cancel.db"))
	if err != nil {
		t.Fatalf("Open: %v", err)
	}
	defer func() { _ = c.Close() }()

	started := make(chan struct{})
	release := make(chan struct{})
	leaderDone := make(chan error, 1)

	go func() {
		_, gsErr := GetOrSet[string](context.Background(), c, "slow", time.Minute, func(context.Context) (string, error) {
			close(started)
			<-release
			return "ok", nil
		})
		leaderDone <- gsErr
	}()

	<-started

	waiterCtx, cancel := context.WithTimeout(context.Background(), 20*time.Millisecond)
	defer cancel()
	_, err = GetOrSet[string](waiterCtx, c, "slow", time.Minute, func(context.Context) (string, error) {
		t.Error("waiter should coalesce instead of invoking loader")
		return "", nil
	})
	if !errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("cancelled waiter err = %v, want DeadlineExceeded", err)
	}

	close(release)
	if err := <-leaderDone; err != nil {
		t.Fatalf("leader err = %v, want nil", err)
	}
}

func BenchmarkCache_GetDisk(b *testing.B) {
	dir := b.TempDir()
	c, err := Open(filepath.Join(dir, "bench.db"))
	if err != nil {
		b.Fatalf("Open: %v", err)
	}
	defer func() { _ = c.Close() }()

	// Simulate realistic ~8KB artist JSON payloads across 200 artists.
	blob := make([]byte, 8192)
	for i := range blob {
		blob[i] = 'a'
	}
	for i := range 200 {
		if err := c.Set(fmt.Sprintf("qobuz:artist:%d:50", i), blob, time.Hour); err != nil {
			b.Fatalf("Set: %v", err)
		}
	}

	b.ResetTimer()
	for i := 0; i < b.N; i++ {
		if _, ok, err := c.Get(fmt.Sprintf("qobuz:artist:%d:50", i%200)); err != nil || !ok {
			b.Fatalf("Get failed: ok=%v err=%v", ok, err)
		}
	}
}

func BenchmarkCache_SetDisk(b *testing.B) {
	dir := b.TempDir()
	c, err := Open(filepath.Join(dir, "bench.db"))
	if err != nil {
		b.Fatalf("Open: %v", err)
	}
	defer func() { _ = c.Close() }()

	blob := make([]byte, 8192)
	for i := range blob {
		blob[i] = 'a'
	}

	b.ResetTimer()
	for i := 0; i < b.N; i++ {
		if err := c.Set(fmt.Sprintf("qobuz:artist:%d:50", i%200), blob, time.Hour); err != nil {
			b.Fatalf("Set: %v", err)
		}
	}
}
