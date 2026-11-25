use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const DEFAULT_MAX_BYTES: i64 = 256 << 20; // 256 MiB
const ACCESS_TOUCH_GRANULARITY_MS: i64 = 60_000; // 1 minute

#[derive(Debug, Error)]
pub enum CacheError {
    #[error("cache: closed")]
    Closed,
    #[error("cache key must not be empty")]
    EmptyKey,
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, CacheError>;

/// TTL policy for `set`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ttl {
    /// Use the cache's configured default TTL (or no expiration if unset).
    Default,
    /// Never expire even if a default TTL is configured.
    Never,
    /// Expire after this duration.
    After(Duration),
}

/// Remaining lifetime returned by `ttl()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TtlRemaining {
    Never,
    Remaining(Duration),
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Stats {
    pub hits: i64,
    pub misses: i64,
    pub hit_rate: f64,
    pub items: i64,
    pub bytes: i64,
    pub max_bytes: i64,
}

pub struct CacheOptions {
    pub default_ttl: Option<Duration>,
    pub max_bytes: i64,
    pub prune_interval: Option<Duration>,
    pub now_fn: Option<Arc<dyn Fn() -> i64 + Send + Sync>>,
}

impl Default for CacheOptions {
    fn default() -> Self {
        Self {
            default_ttl: None,
            max_bytes: DEFAULT_MAX_BYTES,
            prune_interval: None,
            now_fn: None,
        }
    }
}

struct Inner {
    conn: Option<Connection>,
}

/// Thread-safe, SQLite-backed KV cache with TTL, LRU eviction, and namespaces.
pub struct Cache {
    inner: Mutex<Inner>,
    default_ttl: Option<Duration>,
    max_bytes: i64,
    now_fn: Arc<dyn Fn() -> i64 + Send + Sync>,
    hits: AtomicI64,
    misses: AtomicI64,
    prune_cancel: CancellationToken,
    prune_task: Mutex<Option<JoinHandle<()>>>,
}

fn system_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl Cache {
    /// Open (or create) a SQLite-backed KV cache at `db_path`.
    /// Pass `""` or `":memory:"` for an in-memory SQLite database.
    pub fn open(db_path: &str, opts: CacheOptions) -> Result<Arc<Self>> {
        let trimmed = db_path.trim();
        let is_memory = trimmed.is_empty() || trimmed == ":memory:";
        let conn = if is_memory {
            Connection::open_in_memory()?
        } else {
            let path = Path::new(trimmed);
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent)?;
            }
            Connection::open(path)?
        };

        let mut pragmas = vec![
            "PRAGMA busy_timeout = 5000;",
            "PRAGMA synchronous = NORMAL;",
            "PRAGMA temp_store = MEMORY;",
        ];
        if !is_memory {
            pragmas.insert(0, "PRAGMA journal_mode = WAL;");
        }
        for p in pragmas {
            conn.execute_batch(p)?;
        }

        conn.execute_batch(
            r#"
CREATE TABLE IF NOT EXISTS kv_cache (
    key         TEXT PRIMARY KEY,
    value       BLOB NOT NULL,
    size        INTEGER NOT NULL,
    expires_at  INTEGER,
    accessed_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_kv_cache_expires ON kv_cache(expires_at) WHERE expires_at IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_kv_cache_accessed ON kv_cache(accessed_at);
"#,
        )?;

        let now_fn = opts.now_fn.unwrap_or_else(|| Arc::new(system_now_ms));
        let prune_cancel = CancellationToken::new();

        let cache = Arc::new(Self {
            inner: Mutex::new(Inner { conn: Some(conn) }),
            default_ttl: opts.default_ttl,
            max_bytes: opts.max_bytes.max(0),
            now_fn,
            hits: AtomicI64::new(0),
            misses: AtomicI64::new(0),
            prune_cancel: prune_cancel.clone(),
            prune_task: Mutex::new(None),
        });

        if let Some(interval) = opts.prune_interval
            && !interval.is_zero()
            && tokio::runtime::Handle::try_current().is_ok()
        {
            let weak = Arc::downgrade(&cache);
            let cancel = prune_cancel;
            let handle = tokio::spawn(async move {
                let mut ticker = tokio::time::interval(interval);
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                ticker.tick().await; // skip immediate tick
                loop {
                    tokio::select! {
                        _ = cancel.cancelled() => break,
                        _ = ticker.tick() => {
                            if let Some(c) = weak.upgrade() {
                                let _ = c.prune();
                            } else {
                                break;
                            }
                        }
                    }
                }
            });
            *cache.prune_task.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
        }

        Ok(cache)
    }

    pub fn close(&self) {
        self.prune_cancel.cancel();
        if let Some(h) = self
            .prune_task
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            h.abort();
        }
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.conn.take();
    }

    pub fn namespace(self: &Arc<Self>, prefix: &str) -> Namespaced {
        let trimmed = prefix.trim().trim_end_matches(':');
        let p = if trimmed.is_empty() {
            String::new()
        } else {
            format!("{trimmed}:")
        };
        Namespaced {
            parent: Arc::clone(self),
            prefix: p,
        }
    }

    fn now_ms(&self) -> i64 {
        (self.now_fn)()
    }

    pub fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_ref().ok_or(CacheError::Closed)?;
        let now = self.now_ms();

        let row: Option<(Vec<u8>, Option<i64>, i64)> = conn
            .query_row(
                "SELECT value, expires_at, accessed_at FROM kv_cache WHERE key = ?1",
                params![key],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;

        let Some((val, expires_at, accessed_at)) = row else {
            self.misses.fetch_add(1, Ordering::Relaxed);
            return Ok(None);
        };

        if let Some(exp) = expires_at
            && exp <= now
        {
            let _ = conn.execute("DELETE FROM kv_cache WHERE key = ?1", params![key]);
            self.misses.fetch_add(1, Ordering::Relaxed);
            return Ok(None);
        }

        if now - accessed_at >= ACCESS_TOUCH_GRANULARITY_MS {
            let _ = conn.execute(
                "UPDATE kv_cache SET accessed_at = ?1 WHERE key = ?2",
                params![now, key],
            );
        }
        self.hits.fetch_add(1, Ordering::Relaxed);
        Ok(Some(val))
    }

    pub fn set(&self, key: &str, value: &[u8], ttl: Ttl) -> Result<()> {
        if key.is_empty() {
            return Err(CacheError::EmptyKey);
        }
        // Skip caching blobs larger than the entire cache budget so one item never wipes the cache.
        if self.max_bytes > 0 && (value.len() as i64) > self.max_bytes {
            return Ok(());
        }
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_mut().ok_or(CacheError::Closed)?;
        let now = self.now_ms();

        let add_ttl = |d: Duration| -> i64 {
            let ms = i64::try_from(d.as_millis()).unwrap_or(i64::MAX);
            now.saturating_add(ms)
        };
        let expires_at: Option<i64> = match ttl {
            Ttl::Never => None,
            Ttl::Default => self.default_ttl.map(add_ttl),
            Ttl::After(d) => {
                if d.is_zero() {
                    self.default_ttl.map(add_ttl)
                } else {
                    Some(add_ttl(d))
                }
            }
        };

        conn.execute(
            "INSERT INTO kv_cache(key, value, size, expires_at, accessed_at)
             VALUES(?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(key) DO UPDATE SET
                value = excluded.value,
                size = excluded.size,
                expires_at = excluded.expires_at,
                accessed_at = excluded.accessed_at",
            params![key, value, value.len() as i64, expires_at, now],
        )?;

        if self.max_bytes > 0 {
            evict_locked(conn, now, self.max_bytes);
        }
        Ok(())
    }

    pub fn del(&self, key: &str) -> Result<bool> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_ref().ok_or(CacheError::Closed)?;
        let n = conn.execute("DELETE FROM kv_cache WHERE key = ?1", params![key])?;
        Ok(n > 0)
    }

    pub fn del_prefix(&self, prefix: &str) -> Result<u64> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_ref().ok_or(CacheError::Closed)?;
        if prefix.is_empty() {
            let n = conn.execute("DELETE FROM kv_cache", [])?;
            return Ok(n as u64);
        }
        let n = conn.execute(
            "DELETE FROM kv_cache WHERE SUBSTR(key, 1, LENGTH(?1)) = ?1",
            params![prefix],
        )?;
        Ok(n as u64)
    }

    pub fn exists(&self, key: &str) -> Result<bool> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_ref().ok_or(CacheError::Closed)?;
        let now = self.now_ms();
        let row: Option<Option<i64>> = conn
            .query_row(
                "SELECT expires_at FROM kv_cache WHERE key = ?1",
                params![key],
                |r| r.get(0),
            )
            .optional()?;
        let Some(expires_at) = row else {
            return Ok(false);
        };
        if let Some(exp) = expires_at
            && exp <= now
        {
            let _ = conn.execute("DELETE FROM kv_cache WHERE key = ?1", params![key]);
            return Ok(false);
        }
        Ok(true)
    }

    pub fn keys(&self, pattern: &str) -> Result<Vec<String>> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_ref().ok_or(CacheError::Closed)?;
        let now = self.now_ms();
        let mut out = Vec::new();
        if pattern.is_empty() || pattern == "*" {
            let mut stmt = conn.prepare(
                "SELECT key FROM kv_cache WHERE expires_at IS NULL OR expires_at > ?1 ORDER BY key ASC",
            )?;
            let rows = stmt.query_map(params![now], |r| r.get::<_, String>(0))?;
            for r in rows {
                out.push(r?);
            }
        } else {
            let mut stmt = conn.prepare(
                "SELECT key FROM kv_cache WHERE key GLOB ?1 AND (expires_at IS NULL OR expires_at > ?2) ORDER BY key ASC",
            )?;
            let rows = stmt.query_map(params![pattern, now], |r| r.get::<_, String>(0))?;
            for r in rows {
                out.push(r?);
            }
        }
        Ok(out)
    }

    pub fn expire(&self, key: &str, ttl: Duration) -> Result<bool> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_ref().ok_or(CacheError::Closed)?;
        let now = self.now_ms();
        if ttl.is_zero() {
            let n = conn.execute(
                "DELETE FROM kv_cache WHERE key = ?1 AND (expires_at IS NULL OR expires_at > ?2)",
                params![key, now],
            )?;
            return Ok(n > 0);
        }
        let ms = i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX);
        let n = conn.execute(
            "UPDATE kv_cache SET expires_at = ?1 WHERE key = ?2 AND (expires_at IS NULL OR expires_at > ?3)",
            params![now.saturating_add(ms), key, now],
        )?;
        Ok(n > 0)
    }

    pub fn ttl(&self, key: &str) -> Result<Option<TtlRemaining>> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_ref().ok_or(CacheError::Closed)?;
        let now = self.now_ms();
        let row: Option<Option<i64>> = conn
            .query_row(
                "SELECT expires_at FROM kv_cache WHERE key = ?1",
                params![key],
                |r| r.get(0),
            )
            .optional()?;
        let Some(expires_at) = row else {
            return Ok(None);
        };
        let Some(exp) = expires_at else {
            return Ok(Some(TtlRemaining::Never));
        };
        let rem = exp - now;
        if rem <= 0 {
            let _ = conn.execute("DELETE FROM kv_cache WHERE key = ?1", params![key]);
            return Ok(None);
        }
        Ok(Some(TtlRemaining::Remaining(Duration::from_millis(
            rem as u64,
        ))))
    }

    pub fn persist(&self, key: &str) -> Result<bool> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_ref().ok_or(CacheError::Closed)?;
        let now = self.now_ms();
        let n = conn.execute(
            "UPDATE kv_cache SET expires_at = NULL WHERE key = ?1 AND (expires_at IS NULL OR expires_at > ?2)",
            params![key, now],
        )?;
        Ok(n > 0)
    }

    pub fn touch(&self, key: &str) -> Result<bool> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_ref().ok_or(CacheError::Closed)?;
        let now = self.now_ms();
        let n = conn.execute(
            "UPDATE kv_cache SET accessed_at = ?1 WHERE key = ?2 AND (expires_at IS NULL OR expires_at > ?3)",
            params![now, key, now],
        )?;
        Ok(n > 0)
    }

    pub fn clear(&self) -> Result<()> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_ref().ok_or(CacheError::Closed)?;
        conn.execute("DELETE FROM kv_cache", [])?;
        Ok(())
    }

    pub fn prune(&self) -> Result<u64> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_ref().ok_or(CacheError::Closed)?;
        let now = self.now_ms();
        let n = conn.execute(
            "DELETE FROM kv_cache WHERE expires_at IS NOT NULL AND expires_at <= ?1",
            params![now],
        )?;
        Ok(n as u64)
    }

    pub fn size(&self) -> Result<i64> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_ref().ok_or(CacheError::Closed)?;
        let now = self.now_ms();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM kv_cache WHERE expires_at IS NULL OR expires_at > ?1",
            params![now],
            |r| r.get(0),
        )?;
        Ok(n)
    }

    pub fn stats(&self) -> Stats {
        let hits = self.hits.load(Ordering::Relaxed);
        let misses = self.misses.load(Ordering::Relaxed);
        let total = hits + misses;
        let hit_rate = if total > 0 {
            hits as f64 / total as f64
        } else {
            0.0
        };
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let (items, bytes) = if let Some(conn) = inner.conn.as_ref() {
            let now = self.now_ms();
            conn.query_row(
                "SELECT COUNT(*), COALESCE(SUM(size), 0) FROM kv_cache WHERE expires_at IS NULL OR expires_at > ?1",
                params![now],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
            )
            .unwrap_or((0, 0))
        } else {
            (0, 0)
        };
        Stats {
            hits,
            misses,
            hit_rate,
            items,
            bytes,
            max_bytes: self.max_bytes,
        }
    }

    pub fn get_json<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        let Some(raw) = self.get(key)? else {
            return Ok(None);
        };
        match serde_json::from_slice(&raw) {
            Ok(v) => Ok(Some(v)),
            Err(_) => {
                let _ = self.del(key);
                Ok(None)
            }
        }
    }

    pub fn set_json<T: Serialize>(&self, key: &str, value: &T, ttl: Ttl) -> Result<()> {
        let raw = serde_json::to_vec(value)?;
        self.set(key, &raw, ttl)
    }
}

impl Drop for Cache {
    fn drop(&mut self) {
        self.prune_cancel.cancel();
    }
}

/// Key-prefixed view over a `Cache`.
#[derive(Clone)]
pub struct Namespaced {
    parent: Arc<Cache>,
    prefix: String,
}

impl Namespaced {
    fn qualify(&self, key: &str) -> String {
        format!("{}{key}", self.prefix)
    }

    pub fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        self.parent.get(&self.qualify(key))
    }

    pub fn set(&self, key: &str, value: &[u8], ttl: Ttl) -> Result<()> {
        self.parent.set(&self.qualify(key), value, ttl)
    }

    pub fn del(&self, key: &str) -> Result<bool> {
        self.parent.del(&self.qualify(key))
    }

    pub fn del_prefix(&self, prefix: &str) -> Result<u64> {
        self.parent.del_prefix(&format!("{}{prefix}", self.prefix))
    }

    pub fn clear(&self) -> Result<()> {
        let _ = self.parent.del_prefix(&self.prefix)?;
        Ok(())
    }

    pub fn exists(&self, key: &str) -> Result<bool> {
        self.parent.exists(&self.qualify(key))
    }

    pub fn keys(&self, pattern: &str) -> Result<Vec<String>> {
        let pat = if pattern.is_empty() { "*" } else { pattern };
        let raw = self.parent.keys(&format!("{}{pat}", self.prefix))?;
        if self.prefix.is_empty() {
            return Ok(raw);
        }
        Ok(raw
            .into_iter()
            .map(|k| k.strip_prefix(&self.prefix).unwrap_or(&k).to_string())
            .collect())
    }

    pub fn expire(&self, key: &str, ttl: Duration) -> Result<bool> {
        self.parent.expire(&self.qualify(key), ttl)
    }

    pub fn ttl(&self, key: &str) -> Result<Option<TtlRemaining>> {
        self.parent.ttl(&self.qualify(key))
    }

    pub fn persist(&self, key: &str) -> Result<bool> {
        self.parent.persist(&self.qualify(key))
    }

    pub fn touch(&self, key: &str) -> Result<bool> {
        self.parent.touch(&self.qualify(key))
    }

    pub fn get_json<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        self.parent.get_json(&self.qualify(key))
    }

    pub fn set_json<T: Serialize>(&self, key: &str, value: &T, ttl: Ttl) -> Result<()> {
        self.parent.set_json(&self.qualify(key), value, ttl)
    }
}

fn evict_locked(conn: &mut Connection, now: i64, max_bytes: i64) {
    let total: i64 = conn
        .query_row("SELECT COALESCE(SUM(size), 0) FROM kv_cache", [], |r| {
            r.get(0)
        })
        .unwrap_or(0);
    if total <= max_bytes {
        return;
    }
    let _ = conn.execute(
        "DELETE FROM kv_cache WHERE expires_at IS NOT NULL AND expires_at <= ?1",
        params![now],
    );
    let total: i64 = conn
        .query_row("SELECT COALESCE(SUM(size), 0) FROM kv_cache", [], |r| {
            r.get(0)
        })
        .unwrap_or(0);
    if total <= max_bytes {
        return;
    }

    let excess = total - max_bytes;
    let mut victims = Vec::new();
    let mut freed = 0i64;
    if let Ok(mut stmt) = conn.prepare("SELECT key, size FROM kv_cache ORDER BY accessed_at ASC")
        && let Ok(rows) = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
    {
        for row in rows.flatten() {
            victims.push(row.0);
            freed += row.1;
            if freed >= excess {
                break;
            }
        }
    }
    if victims.is_empty() {
        return;
    }
    if let Ok(tx) = conn.transaction() {
        {
            if let Ok(mut stmt) = tx.prepare("DELETE FROM kv_cache WHERE key = ?1") {
                for k in &victims {
                    let _ = stmt.execute(params![k]);
                }
            }
        }
        let _ = tx.commit();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicI64;

    #[test]
    fn basic_crud_and_ttl() {
        let clock = Arc::new(AtomicI64::new(1_000_000));
        let clk = Arc::clone(&clock);
        let c = Cache::open(
            ":memory:",
            CacheOptions {
                now_fn: Some(Arc::new(move || clk.load(Ordering::SeqCst))),
                ..Default::default()
            },
        )
        .unwrap();

        c.set("k1", b"v1", Ttl::After(Duration::from_secs(10)))
            .unwrap();
        assert_eq!(c.get("k1").unwrap().as_deref(), Some(b"v1".as_slice()));
        assert!(c.exists("k1").unwrap());
        assert_eq!(
            c.ttl("k1").unwrap(),
            Some(TtlRemaining::Remaining(Duration::from_secs(10)))
        );

        // Advance past expiration.
        clock.fetch_add(10_000, Ordering::SeqCst);
        assert_eq!(c.get("k1").unwrap(), None);
        assert!(!c.exists("k1").unwrap());
    }

    #[test]
    fn lru_eviction_and_namespaces() {
        let clock = Arc::new(AtomicI64::new(1_000_000));
        let clk = Arc::clone(&clock);
        let c = Cache::open(
            ":memory:",
            CacheOptions {
                max_bytes: 10,
                now_fn: Some(Arc::new(move || clk.load(Ordering::SeqCst))),
                ..Default::default()
            },
        )
        .unwrap();

        let ns = c.namespace("qobuz");
        ns.set("a", b"12345", Ttl::Never).unwrap();
        clock.fetch_add(1_000, Ordering::SeqCst);
        ns.set("b", b"67890", Ttl::Never).unwrap();
        clock.fetch_add(1_000, Ordering::SeqCst);
        ns.set("c", b"abcde", Ttl::Never).unwrap();

        // "a" should have been evicted as LRU.
        assert!(!ns.exists("a").unwrap());
        assert!(ns.exists("b").unwrap());
        assert!(ns.exists("c").unwrap());
        assert_eq!(ns.keys("*").unwrap(), vec!["b", "c"]);
        // Oversized entry (> max_bytes) must not wipe existing entries
        ns.set("huge", b"0123456789ABCDEF", Ttl::Never).unwrap();
        assert!(ns.exists("b").unwrap());
        assert!(ns.exists("c").unwrap());
        ns.clear().unwrap();
        assert!(ns.keys("*").unwrap().is_empty());
    }

    #[test]
    fn del_prefix_handles_non_ascii_utf8_cleanly() {
        let c = Cache::open(":memory:", CacheOptions::default()).unwrap();
        c.set("café:1", b"a", Ttl::Never).unwrap();
        c.set("café:2", b"b", Ttl::Never).unwrap();
        c.set("cafê:1", b"c", Ttl::Never).unwrap();
        assert_eq!(c.del_prefix("café:").unwrap(), 2);
        assert!(!c.exists("café:1").unwrap());
        assert!(c.exists("cafê:1").unwrap());
    }

    #[test]
    fn corrupt_json_self_heals() {
        let c = Cache::open(":memory:", CacheOptions::default()).unwrap();
        c.set("bad", b"{not json", Ttl::Never).unwrap();
        let res: Option<serde_json::Value> = c.get_json("bad").unwrap();
        assert!(res.is_none());
        assert!(!c.exists("bad").unwrap());
    }
}
