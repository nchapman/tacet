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

    struct TestPlayerHarness {
        mgr: Arc<Manager>,
        zone_id: String,
        rp: Arc<crate::player::RemotePlayer>,
        played_urls: Arc<std::sync::Mutex<Vec<String>>>,
        next_urls: Arc<std::sync::Mutex<Vec<String>>>,
        preload_count: Arc<std::sync::atomic::AtomicUsize>,
    }

    fn test_track(n: i32) -> Track {
        Track {
            id: format!("local:{n}"),
            source: SOURCE_LOCAL.into(),
            title: format!("Track {n}"),
            artist: "Artist".into(),
            mime: "audio/flac".into(),
            ext: ".flac".into(),
            duration_sec: 120,
            ..Default::default()
        }
    }

    fn setup_player_harness(player_id: &str) -> TestPlayerHarness {
        let cat = Catalog::new_in_memory(vec![]);
        let mgr = Manager::new(cat, None, 4320, StreamMode::Direct);
        let mgr_reg = Arc::clone(&mgr);
        let mgr_push = Arc::clone(&mgr);
        let reg = Registry::new(
            Some(Arc::new(move |c| mgr_reg.register_renderer(c))),
            None,
            Some(Arc::new(move |ev| mgr_push.apply_device_event(ev))),
        );
        let r = reg
            .register(
                RegisterRequest {
                    player_id: player_id.into(),
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
        let next_urls = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let next_urls_bg = Arc::clone(&next_urls);
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
        TestPlayerHarness {
            mgr,
            zone_id: r.zone_id,
            rp,
            played_urls,
            next_urls,
            preload_count,
        }
    }

    #[tokio::test]
    async fn by_id_queue_ops_repeat_and_shuffle() {
        let h = setup_player_harness("qtest");
        let snap = h
            .mgr
            .set_queue(
                &h.zone_id,
                vec![test_track(1), test_track(2), test_track(3)],
                0,
                true,
            )
            .await
            .unwrap();
        assert_eq!(snap.queue.len(), 3);
        let id0 = snap.queue[0].item_id.clone();
        let id2 = snap.queue[2].item_id.clone();

        let snap = h.mgr.move_item_by_id(&h.zone_id, &id2, 1).await.unwrap();
        assert_eq!(snap.queue[1].item_id, id2);
        let snap = h.mgr.play_index(&h.zone_id, 1).await.unwrap();
        assert_eq!(snap.current_index, 1);

        assert!(matches!(
            h.mgr.remove_item_by_id(&h.zone_id, "missing_id").await,
            Err(QueueError::InvalidIndex)
        ));
        let snap = h.mgr.remove_item_by_id(&h.zone_id, &id0).await.unwrap();
        assert_eq!(snap.queue.len(), 2);
        assert_eq!(snap.current_index, 0);

        // Repeat All wraps next() from last to 0
        h.mgr.set_repeat(&h.zone_id, RepeatMode::All).await.unwrap();
        assert_eq!(h.mgr.next(&h.zone_id).await.unwrap().current_index, 1);
        let snap = h.mgr.next(&h.zone_id).await.unwrap();
        assert_eq!(snap.current_index, 0);

        // Shuffle toggle preserves current track
        let cur_before = snap.queue[snap.current_index as usize].track.id.clone();
        let snap = h.mgr.set_shuffle(&h.zone_id, true).await.unwrap();
        assert!(snap.shuffle);
        assert_eq!(snap.queue[snap.current_index as usize].track.id, cur_before);
        assert!(!h.mgr.set_shuffle(&h.zone_id, false).await.unwrap().shuffle);
        h.mgr.close();
    }

    #[tokio::test]
    async fn gapless_preload_fires_once_and_advances_on_uri_change() {
        let h = setup_player_harness("qgapless");
        h.mgr
            .set_queue(&h.zone_id, vec![test_track(1), test_track(2)], 0, true)
            .await
            .unwrap();
        let cur_url = h
            .played_urls
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap_or_default();
        h.preload_count
            .store(0, std::sync::atomic::Ordering::SeqCst);
        for sec in [105.0, 106.0, 107.0, 108.0, 109.0] {
            h.rp.report_state(StateReport {
                state: "PLAYING".into(),
                track_uri: cur_url.clone(),
                position_sec: Some(sec),
                duration_sec: Some(120.0),
                ..Default::default()
            });
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            h.preload_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "gapless preload must fire once, not on every position tick"
        );

        let next_url = h
            .next_urls
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap_or_default();
        assert!(!next_url.is_empty());
        h.rp.report_state(StateReport {
            state: "PLAYING".into(),
            track_uri: next_url,
            position_sec: Some(0.5),
            duration_sec: Some(120.0),
            ..Default::default()
        });
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let after = h.mgr.zone_snapshot(&h.zone_id).unwrap();
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
        h.mgr.close();
    }

    struct MockNative {
        info: crate::upnp::RendererInfo,
        fail_native: std::sync::atomic::AtomicBool,
        native_calls: std::sync::atomic::AtomicUsize,
        set_uri_calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl crate::upnp::Controller for MockNative {
        fn info(&self) -> crate::upnp::RendererInfo {
            self.info.clone()
        }
        fn local_ip(&self) -> String {
            "127.0.0.1".into()
        }
        async fn set_uri(&self, _item: &crate::upnp::MediaItem) -> crate::upnp::Result<()> {
            self.set_uri_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        async fn set_next_uri(&self, _item: &crate::upnp::MediaItem) -> crate::upnp::Result<()> {
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
        async fn seek(&self, _pos: std::time::Duration) -> crate::upnp::Result<()> {
            Ok(())
        }
        async fn set_volume(&self, _v: i32) -> crate::upnp::Result<()> {
            Ok(())
        }
        async fn set_mute(&self, _m: bool) -> crate::upnp::Result<()> {
            Ok(())
        }
        async fn status(&self) -> crate::upnp::Result<crate::upnp::TransportStatus> {
            Ok(crate::upnp::TransportStatus {
                state: "PLAYING".into(),
                ..Default::default()
            })
        }
        fn as_queue_controller(&self) -> Option<&dyn crate::upnp::QueueController> {
            Some(self)
        }
    }

    #[async_trait::async_trait]
    impl crate::upnp::QueueController for MockNative {
        async fn replace_queue(
            &self,
            _items: &[crate::upnp::MediaItem],
            _start_index: usize,
        ) -> crate::upnp::Result<()> {
            self.native_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.fail_native.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(crate::upnp::UpnpError::Other(
                    "native queue unsupported".into(),
                ));
            }
            Ok(())
        }
        async fn append_queue(&self, _items: &[crate::upnp::MediaItem]) -> crate::upnp::Result<()> {
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
        async fn set_loop_mode(&self, _repeat: &str, _shuffle: bool) -> crate::upnp::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn native_queue_controller_routing_and_fallback() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        let cat = Catalog::new_in_memory(vec![]);
        let mgr = Manager::new(cat, None, 4320, StreamMode::Direct);
        let mock = Arc::new(MockNative {
            info: crate::upnp::RendererInfo {
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

        let tr = test_track(1);
        mgr.set_queue("upnp:mock_native", vec![tr.clone()], 0, true)
            .await
            .unwrap();
        assert_eq!(mock.native_calls.load(Ordering::SeqCst), 1);
        assert_eq!(mock.set_uri_calls.load(Ordering::SeqCst), 0);

        mock.fail_native.store(true, Ordering::SeqCst);
        mgr.set_queue("upnp:mock_native", vec![tr], 0, true)
            .await
            .unwrap();
        assert_eq!(mock.native_calls.load(Ordering::SeqCst), 2);
        assert_eq!(mock.set_uri_calls.load(Ordering::SeqCst), 1);

        mgr.close();
    }

    #[tokio::test]
    async fn event_log_writes_jsonl_for_sse_and_gena() {
        struct SharedBuf(Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for SharedBuf {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let buf = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let cat = Catalog::new_in_memory(vec![]);
        let mgr = Manager::new(cat, None, 4320, StreamMode::Direct);
        mgr.set_event_log(Some(Box::new(SharedBuf(Arc::clone(&buf)))));

        mgr.broadcast("settings.updated", &serde_json::json!({"ok": true}));
        mgr.apply_device_event(crate::upnp::DeviceEvent {
            renderer_id: "zone:test".into(),
            service: "AVTransport".into(),
            has_state: true,
            state: "PLAYING".into(),
            ..Default::default()
        });
        mgr.close();

        let text = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        let lines: Vec<serde_json::Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["src"], "sse");
        assert!(lines[0]["ts"].as_str().unwrap().ends_with('Z'));
        assert_eq!(lines[1]["src"], "gena");
        assert_eq!(lines[1]["zoneId"], "zone:test");
    }
}
