use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

/// Async singleflight group that deduplicates concurrent in-flight computations
/// keyed by `K`.
///
/// The first caller runs the producer future; concurrent callers for the same
/// key wait for that result via a `watch` channel without duplicating work.
/// Unlike Go's `singleflight`, if a waiter drops its future, the producer still
/// completes for other waiters (the producer itself is spawned or driven by the
/// leader — here we spawn it on Tokio so leader cancellation never aborts
type InflightMap<K, V, E> = HashMap<K, watch::Receiver<Option<Result<V, E>>>>;

/// shared work for other waiters).
pub struct SingleFlight<K, V, E> {
    inflight: Arc<Mutex<InflightMap<K, V, E>>>,
}

impl<K, V, E> Default for SingleFlight<K, V, E>
where
    K: Eq + Hash + Clone + Send + 'static,
    V: Clone + Send + Sync + 'static,
    E: Clone + Send + Sync + 'static,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<K, V, E> Clone for SingleFlight<K, V, E> {
    fn clone(&self) -> Self {
        Self {
            inflight: Arc::clone(&self.inflight),
        }
    }
}

impl<K, V, E> SingleFlight<K, V, E>
where
    K: Eq + Hash + Clone + Send + 'static,
    V: Clone + Send + Sync + 'static,
    E: Clone + Send + Sync + 'static,
{
    pub fn new() -> Self {
        Self {
            inflight: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Execute `fut` for `key`, coalescing concurrent callers for the same key.
    ///
    /// The producer future is spawned on the Tokio runtime so that if the first
    /// caller is cancelled (e.g. HTTP client disconnects), other waiters still
    /// receive the result and the map entry is always cleaned up.
    pub async fn work<F>(&self, key: K, fut: F) -> Result<V, E>
    where
        F: Future<Output = Result<V, E>> + Send + 'static,
    {
        let mut rx = {
            let mut map = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(existing) = map.get(&key) {
                existing.clone()
            } else {
                let (tx, rx) = watch::channel(None);
                map.insert(key.clone(), rx.clone());
                let inflight = Arc::clone(&self.inflight);
                let key_clone = key.clone();
                tokio::spawn(async move {
                    let res = fut.await;
                    // Remove before sending so any new caller arriving after wake
                    // starts a fresh flight rather than seeing a completed entry.
                    {
                        let mut map = inflight.lock().unwrap_or_else(|e| e.into_inner());
                        map.remove(&key_clone);
                    }
                    let _ = tx.send(Some(res));
                });
                rx
            }
        };

        loop {
            if let Some(res) = rx.borrow().clone() {
                return res;
            }
            if rx.changed().await.is_err() {
                if let Some(res) = rx.borrow().clone() {
                    return res;
                }
                unreachable!("singleflight producer dropped without sending result");
            }
        }
    }

    /// Execute `fut` inline in the leader task (no `'static` bound required),
    /// coalescing concurrent callers for the same key.
    pub async fn work_inline<F>(&self, key: K, fut: F) -> Result<V, E>
    where
        F: Future<Output = Result<V, E>>,
    {
        enum Role<V, E> {
            Leader(watch::Sender<Option<Result<V, E>>>),
            Follower(watch::Receiver<Option<Result<V, E>>>),
        }

        let role = {
            let mut map = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(existing) = map.get(&key) {
                Role::Follower(existing.clone())
            } else {
                let (tx, rx) = watch::channel(None);
                map.insert(key.clone(), rx);
                Role::Leader(tx)
            }
        };

        match role {
            Role::Leader(tx) => {
                struct Cleanup<'a, K: Eq + Hash, V, E> {
                    map: &'a Mutex<InflightMap<K, V, E>>,
                    key: Option<K>,
                }
                impl<K: Eq + Hash, V, E> Drop for Cleanup<'_, K, V, E> {
                    fn drop(&mut self) {
                        if let Some(k) = self.key.take() {
                            self.map
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .remove(&k);
                        }
                    }
                }
                let _guard = Cleanup {
                    map: &self.inflight,
                    key: Some(key),
                };
                let res = fut.await;
                drop(_guard);
                let _ = tx.send(Some(res.clone()));
                res
            }
            Role::Follower(mut rx) => loop {
                if let Some(res) = rx.borrow().clone() {
                    return res;
                }
                if rx.changed().await.is_err() {
                    if let Some(res) = rx.borrow().clone() {
                        return res;
                    }
                    // Leader was cancelled; fall back to running `fut` directly.
                    return fut.await;
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn coalesces_concurrent_calls() {
        let sf: SingleFlight<String, usize, String> = SingleFlight::new();
        let runs = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..8 {
            let sf = sf.clone();
            let runs = Arc::clone(&runs);
            handles.push(tokio::spawn(async move {
                sf.work("key".to_string(), async move {
                    runs.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    Ok(42)
                })
                .await
            }));
        }

        for h in handles {
            assert_eq!(h.await.unwrap(), Ok(42));
        }
        assert_eq!(runs.load(Ordering::SeqCst), 1);

        // Subsequent call after completion runs again.
        let runs2 = Arc::clone(&runs);
        let res = sf
            .work("key".to_string(), async move {
                runs2.fetch_add(1, Ordering::SeqCst);
                Ok(99)
            })
            .await;
        assert_eq!(res, Ok(99));
        assert_eq!(runs.load(Ordering::SeqCst), 2);
    }
}
