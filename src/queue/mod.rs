pub mod manager;
pub mod types;

pub use manager::Manager;
pub use types::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::{Catalog, SOURCE_LOCAL, Track};
    use crate::player::{RegisterRequest, Registry, StateReport};
    use std::sync::Arc;

    #[tokio::test]
    async fn zone_queue_and_transport_lifecycle() {
        let cat = Catalog::new_in_memory(vec![]);
        let mgr = Manager::new(cat, None, 4320, StreamMode::Direct);

        let mgr_reg = Arc::clone(&mgr);
        let reg = Registry::new(
            Some(Arc::new(move |c| mgr_reg.register_renderer(c))),
            None,
            None,
        );
        let r = reg
            .register(
                RegisterRequest {
                    player_id: "test".into(),
                    name: "Test Player".into(),
                    ..Default::default()
                },
                "",
            )
            .unwrap();
        let rp = reg.get(&r.zone_id, &r.token).unwrap();
        let mut stream = rp.attach_stream();

        // Auto-ack commands in background
        let rp_bg = Arc::clone(&rp);
        tokio::spawn(async move {
            while let Some(cmd) = stream.commands.recv().await {
                rp_bg.report_state(StateReport {
                    cmd_id: cmd.id,
                    ok: Some(true),
                    state: if cmd.cmd_type == "pause" {
                        "PAUSED_PLAYBACK".into()
                    } else {
                        "PLAYING".into()
                    },
                    ..Default::default()
                });
            }
        });

        let tr = Track {
            id: "local:1".into(),
            source: SOURCE_LOCAL.into(),
            title: "So What".into(),
            artist: "Miles Davis".into(),
            mime: "audio/flac".into(),
            ext: ".flac".into(),
            duration_sec: 560,
            ..Default::default()
        };

        let snap = mgr.set_queue(&r.zone_id, vec![tr], 0, true).await.unwrap();
        assert_eq!(snap.queue.len(), 1);
        assert_eq!(snap.current_index, 0);
        assert_eq!(snap.state, PlaybackState::Playing);

        let snap = mgr.pause(&r.zone_id).await.unwrap();
        assert_eq!(snap.state, PlaybackState::Paused);

        let snap = mgr.set_volume(&r.zone_id, 65).await.unwrap();
        assert_eq!(snap.volume, 65);

        mgr.close();
    }

    #[tokio::test]
    async fn by_id_queue_ops_repeat_shuffle_and_push_auto_advance() {
        let cat = Catalog::new_in_memory(vec![]);
        let mgr = Manager::new(cat, None, 4320, StreamMode::Direct);

        let mgr_reg = Arc::clone(&mgr);
        let mgr_push = Arc::clone(&mgr);
        let reg = Registry::new(
            Some(Arc::new(move |c| mgr_reg.register_renderer(c))),
            None,
            Some(Arc::new(move |ev| {
                mgr_push.apply_device_event(ev);
            })),
        );
        let r = reg
            .register(
                RegisterRequest {
                    player_id: "qtest".into(),
                    name: "Queue Player".into(),
                    ..Default::default()
                },
                "",
            )
            .unwrap();
        let rp = reg.get(&r.zone_id, &r.token).unwrap();
        let mut stream = rp.attach_stream();

        let rp_bg = Arc::clone(&rp);
        let played_urls = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let played_urls_bg = Arc::clone(&played_urls);
        let preload_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let preload_count_bg = Arc::clone(&preload_count);
        let played_urls_all_next = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let next_urls_bg = Arc::clone(&played_urls_all_next);
        tokio::spawn(async move {
            while let Some(cmd) = stream.commands.recv().await {
                let url = cmd
                    .media
                    .as_ref()
                    .map(|m| m.url.clone())
                    .unwrap_or_default();
                if cmd.cmd_type == "set_next_uri" {
                    preload_count_bg.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if !url.is_empty() {
                        next_urls_bg.lock().unwrap().push(url.clone());
                    }
                } else if !url.is_empty() {
                    played_urls_bg.lock().unwrap().push(url.clone());
                }
                rp_bg.report_state(StateReport {
                    cmd_id: cmd.id,
                    ok: Some(true),
                    state: "PLAYING".into(),
                    track_uri: url,
                    ..Default::default()
                });
            }
        });

        let mk = |n: i32| Track {
            id: format!("local:{n}"),
            source: SOURCE_LOCAL.into(),
            title: format!("Track {n}"),
            artist: "Artist".into(),
            mime: "audio/flac".into(),
            ext: ".flac".into(),
            duration_sec: 120,
            ..Default::default()
        };

        let snap = mgr
            .set_queue(&r.zone_id, vec![mk(1), mk(2), mk(3)], 0, true)
            .await
            .unwrap();
        assert_eq!(snap.queue.len(), 3);
        let id0 = snap.queue[0].item_id.clone();
        let id2 = snap.queue[2].item_id.clone();

        // Atomic move_item_by_id, play_item_by_id, remove_item_by_id
        let snap = mgr.move_item_by_id(&r.zone_id, &id2, 1).await.unwrap();
        assert_eq!(snap.queue[1].item_id, id2);

        let snap = mgr.play_index(&r.zone_id, 1).await.unwrap();
        assert_eq!(snap.current_index, 1);

        // Missing item_id returns InvalidIndex
        assert!(matches!(
            mgr.remove_item_by_id(&r.zone_id, "missing_id").await,
            Err(QueueError::InvalidIndex)
        ));

        let snap = mgr.remove_item_by_id(&r.zone_id, &id0).await.unwrap();
        assert_eq!(snap.queue.len(), 2);
        assert_eq!(snap.current_index, 0);

        // Repeat All wraps next() from last to 0
        mgr.set_repeat(&r.zone_id, RepeatMode::All).await.unwrap();
        let snap = mgr.next(&r.zone_id).await.unwrap();
        assert_eq!(snap.current_index, 1);
        let snap = mgr.next(&r.zone_id).await.unwrap();
        assert_eq!(snap.current_index, 0);

        // Shuffle toggle preserves current track
        let cur_before = snap.queue[snap.current_index as usize].track.id.clone();
        let snap = mgr.set_shuffle(&r.zone_id, true).await.unwrap();
        assert!(snap.shuffle);
        assert_eq!(snap.queue[snap.current_index as usize].track.id, cur_before);
        let snap = mgr.set_shuffle(&r.zone_id, false).await.unwrap();
        assert!(!snap.shuffle);

        // Rapid push position ticks near end of track must arm gapless preload at most ONCE
        // (regression test for preload feedback storm on position updates).
        mgr.set_repeat(&r.zone_id, RepeatMode::Off).await.unwrap();
        mgr.play_index(&r.zone_id, 0).await.unwrap();
        let cur_url = played_urls
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap_or_default();
        preload_count.store(0, std::sync::atomic::Ordering::SeqCst);
        for sec in [105.0, 106.0, 107.0, 108.0, 109.0] {
            rp.report_state(StateReport {
                state: "PLAYING".into(),
                track_uri: cur_url.clone(),
                position_sec: Some(sec),
                duration_sec: Some(120.0),
                ..Default::default()
            });
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            preload_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "gapless preload must fire once, not on every position tick"
        );

        // Gapless transition: reporting the preloaded track_uri advances current_index to 1
        let next_url = played_urls_all_next
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap_or_default();
        assert!(!next_url.is_empty());
        rp.report_state(StateReport {
            state: "PLAYING".into(),
            track_uri: next_url,
            position_sec: Some(0.5),
            duration_sec: Some(120.0),
            ..Default::default()
        });
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let after = mgr.zone_snapshot(&r.zone_id).unwrap();
            if after.current_index == 1 {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for gapless advance to index 1 (got {})",
                after.current_index
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        mgr.close();
    }

    #[tokio::test]
    async fn native_queue_controller_routing_and_fallback() {
        use crate::upnp::{
            Controller, MediaItem, QueueController, RendererInfo, TransportStatus, UpnpError,
        };
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::time::Duration;

        struct MockNative {
            info: RendererInfo,
            fail_native: AtomicBool,
            native_calls: AtomicUsize,
            set_uri_calls: AtomicUsize,
        }

        #[async_trait::async_trait]
        impl Controller for MockNative {
            fn info(&self) -> RendererInfo {
                self.info.clone()
            }
            fn local_ip(&self) -> String {
                "127.0.0.1".into()
            }
            async fn set_uri(&self, _item: &MediaItem) -> crate::upnp::Result<()> {
                self.set_uri_calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
            async fn set_next_uri(&self, _item: &MediaItem) -> crate::upnp::Result<()> {
                Ok(())
            }
            async fn play(&self) -> crate::upnp::Result<()> {
                Ok(())
            }
            async fn pause(&self) -> crate::upnp::Result<()> {
                Ok(())
            }
            async fn stop(&self) -> crate::upnp::Result<()> {
                Ok(())
            }
            async fn seek(&self, _pos: Duration) -> crate::upnp::Result<()> {
                Ok(())
            }
            async fn set_volume(&self, _v: i32) -> crate::upnp::Result<()> {
                Ok(())
            }
            async fn set_mute(&self, _m: bool) -> crate::upnp::Result<()> {
                Ok(())
            }
            async fn status(&self) -> crate::upnp::Result<TransportStatus> {
                Ok(TransportStatus {
                    state: "PLAYING".into(),
                    ..Default::default()
                })
            }
            fn as_queue_controller(&self) -> Option<&dyn QueueController> {
                Some(self)
            }
        }

        #[async_trait::async_trait]
        impl QueueController for MockNative {
            async fn replace_queue(
                &self,
                _items: &[MediaItem],
                _start_index: usize,
            ) -> crate::upnp::Result<()> {
                self.native_calls.fetch_add(1, Ordering::SeqCst);
                if self.fail_native.load(Ordering::SeqCst) {
                    return Err(UpnpError::Other("native queue unsupported".into()));
                }
                Ok(())
            }
            async fn append_queue(&self, _items: &[MediaItem]) -> crate::upnp::Result<()> {
                Ok(())
            }
            async fn remove_queue_range(&self, _s: usize, _e: usize) -> crate::upnp::Result<()> {
                Ok(())
            }
            async fn move_queue_item(&self, _from: usize, _to: usize) -> crate::upnp::Result<()> {
                Ok(())
            }
            async fn play_index(&self, _idx: usize) -> crate::upnp::Result<()> {
                Ok(())
            }
            async fn next(&self) -> crate::upnp::Result<()> {
                Ok(())
            }
            async fn previous(&self) -> crate::upnp::Result<()> {
                Ok(())
            }
            async fn set_loop_mode(
                &self,
                _repeat: &str,
                _shuffle: bool,
            ) -> crate::upnp::Result<()> {
                Ok(())
            }
        }

        let cat = Catalog::new_in_memory(vec![]);
        let mgr = Manager::new(cat, None, 4320, StreamMode::Direct);
        let mock = Arc::new(MockNative {
            info: RendererInfo {
                id: "upnp:mock_native".into(),
                name: "Mock Native".into(),
                kind: "linkplay".into(),
                ..Default::default()
            },
            fail_native: AtomicBool::new(false),
            native_calls: AtomicUsize::new(0),
            set_uri_calls: AtomicUsize::new(0),
        });
        mgr.register_renderer(mock.clone());

        let tr = Track {
            id: "local:1".into(),
            source: SOURCE_LOCAL.into(),
            title: "Native Track".into(),
            mime: "audio/flac".into(),
            ext: ".flac".into(),
            ..Default::default()
        };

        // Native queue succeeds
        mgr.set_queue("upnp:mock_native", vec![tr.clone()], 0, true)
            .await
            .unwrap();
        assert_eq!(mock.native_calls.load(Ordering::SeqCst), 1);
        assert_eq!(mock.set_uri_calls.load(Ordering::SeqCst), 0);

        // Native queue fails -> falls back to AVTransport set_uri
        mock.fail_native.store(true, Ordering::SeqCst);
        mgr.set_queue("upnp:mock_native", vec![tr], 0, true)
            .await
            .unwrap();
        assert_eq!(mock.native_calls.load(Ordering::SeqCst), 2);
        assert_eq!(mock.set_uri_calls.load(Ordering::SeqCst), 1);

        mgr.close();
    }
}
