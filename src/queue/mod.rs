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
}
