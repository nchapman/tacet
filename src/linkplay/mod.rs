use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::upnp::{
    Controller, MediaItem, QueueController, Renderer, RendererInfo, Result, TransportStatus,
    URN_LINKPLAY_PLAYQUEUE_1, UpnpError, build_didl, escape_xml, extract_xml_text,
    new_soap_http_client,
};

const TACET_LIST_NAME: &str = "tacet";
const CURRENT_QUEUE: &str = "CurrentQueue";
const LINKPLAY_BATCH_SIZE: usize = 50;

/// Wraps a Linkplay UPnP `Renderer` (WiiM, AudioPro, Arylic, etc.) and implements
/// `QueueController` via Linkplay's native `wiimu:PlayQueue:1` SOAP service.
pub struct LinkplayRenderer {
    base: Arc<Renderer>,
    play_queue_url: String,
    http: reqwest::Client,
}

/// Promotes `r` to a `LinkplayRenderer` when the UPnP device advertises
/// `urn:schemas-wiimu-com:service:PlayQueue:1`; otherwise returns `r` unchanged.
pub fn wrap(r: Arc<Renderer>) -> Arc<dyn Controller> {
    if r.play_queue_control_url().is_empty() {
        return r;
    }
    let url = r.play_queue_control_url().to_string();
    Arc::new(LinkplayRenderer::new(r, url))
}

impl LinkplayRenderer {
    pub fn new(base: Arc<Renderer>, play_queue_url: String) -> Self {
        Self {
            base,
            play_queue_url,
            http: new_soap_http_client(),
        }
    }

    async fn soap_call(&self, action: &str, inner_body: &str) -> Result<String> {
        let env = format!(
            r#"<?xml version="1.0" encoding="utf-8"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><u:{action} xmlns:u="{URN_LINKPLAY_PLAYQUEUE_1}">{inner_body}</u:{action}></s:Body></s:Envelope>"#
        );
        let soap_action_hdr = format!(r#""{URN_LINKPLAY_PLAYQUEUE_1}#{action}""#);
        let url = self.play_queue_url.clone();
        self.base
            .do_soap(|| {
                let env = env.clone();
                let soap_action_hdr = soap_action_hdr.clone();
                let url = url.clone();
                async move {
                    let resp = self
                        .http
                        .post(&url)
                        .header("Content-Type", r#"text/xml; charset="utf-8""#)
                        .header("SOAPACTION", soap_action_hdr)
                        .header("Connection", "close")
                        .body(env)
                        .send()
                        .await?;
                    let status = resp.status().as_u16();
                    let body = resp.text().await?;
                    if (200..300).contains(&status) {
                        return Ok(body);
                    }
                    if let Some(code_str) = crate::upnp::extract_xml_text(&body, "errorCode") {
                        let code = code_str.trim().parse::<i32>().unwrap_or(0);
                        let desc = crate::upnp::extract_xml_text(&body, "errorDescription")
                            .unwrap_or_default();
                        if code == 401 || code == 602 {
                            return Err(UpnpError::ActionUnsupported);
                        }
                        return Err(UpnpError::SoapFault { code, desc });
                    }
                    Err(UpnpError::HttpStatus { status, body })
                }
            })
            .await
    }

    async fn get_queue_index(&self) -> Option<i32> {
        let raw = self
            .soap_call(
                "GetQueueIndex",
                &format!("<QueueName>{CURRENT_QUEUE}</QueueName>"),
            )
            .await
            .ok()?;
        parse_get_queue_index(&raw)
    }

    async fn append_queue_named(&self, list_name: &str, items: &[MediaItem]) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let sinks = self.base.info().sink_protocols;
        let qctx = build_queue_context_xml(list_name, items, &sinks);
        let body = format!("<QueueContext>{}</QueueContext>", escape_xml(&qctx));
        self.soap_call("AppendTracksInQueue", &body).await?;
        Ok(())
    }
}

#[async_trait]
impl Controller for LinkplayRenderer {
    fn info(&self) -> RendererInfo {
        let mut info = self.base.info();
        info.supports_queue = true;
        info
    }

    fn local_ip(&self) -> String {
        self.base.local_ip()
    }

    async fn set_uri(&self, item: &MediaItem) -> Result<()> {
        self.base.set_uri(item).await
    }

    async fn set_next_uri(&self, item: &MediaItem) -> Result<()> {
        self.base.set_next_uri(item).await
    }

    async fn play(&self) -> Result<()> {
        self.base.play().await
    }

    async fn pause(&self) -> Result<()> {
        self.base.pause().await
    }

    async fn stop(&self) -> Result<()> {
        self.base.stop().await
    }

    async fn seek(&self, pos: Duration) -> Result<()> {
        self.base.seek(pos).await
    }

    async fn status(&self) -> Result<TransportStatus> {
        let mut st = self.base.status().await?;
        if matches!(
            st.state.as_str(),
            "PLAYING" | "PAUSED_PLAYBACK" | "TRANSITIONING"
        ) && let Some(idx) = self.get_queue_index().await
        {
            st.queue_index = idx;
            st.has_queue_index = true;
        }
        Ok(st)
    }

    async fn set_volume(&self, vol: i32) -> Result<()> {
        self.base.set_volume(vol).await
    }

    async fn set_mute(&self, muted: bool) -> Result<()> {
        self.base.set_mute(muted).await
    }

    fn event_urls(&self) -> Option<(String, String)> {
        self.base.event_urls()
    }

    fn as_queue_controller(&self) -> Option<&dyn QueueController> {
        Some(self)
    }
}

#[async_trait]
impl QueueController for LinkplayRenderer {
    async fn replace_queue(&self, items: &[MediaItem], start_index: usize) -> Result<()> {
        if items.is_empty() {
            return Err(UpnpError::Other(
                "linkplay ReplaceQueue: empty items".into(),
            ));
        }
        let start_idx = if start_index < items.len() {
            start_index
        } else {
            0
        };
        let sinks = self.base.info().sink_protocols;
        let first_end = items.len().min(LINKPLAY_BATCH_SIZE);
        let qctx = build_queue_context_xml(TACET_LIST_NAME, &items[..first_end], &sinks);
        let body = format!("<QueueContext>{}</QueueContext>", escape_xml(&qctx));
        self.soap_call("CreateQueue", &body).await?;

        let mut start = first_end;
        while start < items.len() {
            let end = (start + LINKPLAY_BATCH_SIZE).min(items.len());
            self.append_queue_named(TACET_LIST_NAME, &items[start..end])
                .await?;
            start = end;
        }

        // Linkplay PlayQueue indices are 1-based.
        let play_body = format!(
            "<QueueName>{}</QueueName><Index>{}</Index>",
            escape_xml(TACET_LIST_NAME),
            start_idx + 1
        );
        self.soap_call("PlayQueueWithIndex", &play_body).await?;
        Ok(())
    }

    async fn append_queue(&self, items: &[MediaItem]) -> Result<()> {
        self.append_queue_named(CURRENT_QUEUE, items).await
    }

    async fn remove_queue_range(&self, start: usize, end: usize) -> Result<()> {
        if end < start {
            return Ok(());
        }
        // Note: Linkplay firmware literally spells the tag "RangStart" / "RangEnd".
        let body = format!(
            "<QueueName>{CURRENT_QUEUE}</QueueName><RangStart>{}</RangStart><RangEnd>{}</RangEnd><Action>0</Action>",
            start + 1,
            end + 1
        );
        self.soap_call("RemoveTracksInQueue", &body).await?;
        Ok(())
    }

    async fn move_queue_item(&self, from: usize, to: usize) -> Result<()> {
        if from == to {
            return Ok(());
        }
        let body = format!(
            "<QueueName>{CURRENT_QUEUE}</QueueName><IndexList>{}</IndexList><ToIndex>{}</ToIndex>",
            from + 1,
            to + 1
        );
        self.soap_call("MoveTracksInQueue", &body).await?;
        Ok(())
    }

    async fn play_index(&self, index: usize) -> Result<()> {
        let body = format!(
            "<QueueName>{CURRENT_QUEUE}</QueueName><Index>{}</Index>",
            index + 1
        );
        self.soap_call("PlayQueueWithIndex", &body).await?;
        Ok(())
    }

    async fn next(&self) -> Result<()> {
        self.base.next().await
    }

    async fn previous(&self) -> Result<()> {
        self.base.previous().await
    }

    async fn set_loop_mode(&self, repeat: &str, _shuffle: bool) -> Result<()> {
        // Linkplay loop modes: 0=repeat all, 1=repeat one, 4=no repeat (normal).
        let mode = match repeat.trim().to_ascii_lowercase().as_str() {
            "all" => 0,
            "one" => 1,
            _ => 4,
        };
        let body = format!("<LoopMode>{mode}</LoopMode>");
        self.soap_call("SetQueueLoopMode", &body).await?;
        Ok(())
    }
}

pub fn build_queue_context_xml(
    list_name: &str,
    items: &[MediaItem],
    sink_protocols: &[String],
) -> String {
    let mut b = String::with_capacity(items.len() * 512 + 256);
    b.push_str(r#"<?xml version="1.0"?><PlayList><ListName>"#);
    b.push_str(&escape_xml(list_name));
    b.push_str("</ListName><ListInfo><Radio>0</Radio><SourceName>Local</SourceName><PicUrl>");
    if let Some(first) = items.first()
        && !first.album_art_url.is_empty()
    {
        b.push_str(&escape_xml(&first.album_art_url));
    }
    b.push_str("</PicUrl><TrackNumber>");
    b.push_str(&items.len().to_string());
    b.push_str("</TrackNumber><SearchUrl></SearchUrl><Quality>0</Quality></ListInfo><Tracks>");

    for (i, item) in items.iter().enumerate() {
        let tag = format!("Track{}", i + 1);
        b.push('<');
        b.push_str(&tag);
        b.push_str("><URL>");
        b.push_str(&escape_xml(&item.url));
        b.push_str("</URL><Source>OnlineMusic</Source><Key></Key><Id>");
        b.push_str(&escape_xml(&item.id));
        b.push_str("</Id><Metadata>");
        // Double-escaped DIDL inside <Metadata>, then the outer <QueueContext> escapes again.
        b.push_str(&escape_xml(&build_didl(item, sink_protocols)));
        b.push_str("</Metadata></");
        b.push_str(&tag);
        b.push('>');
    }
    b.push_str("</Tracks></PlayList>");
    b
}

pub fn parse_get_queue_index(soap_xml: &str) -> Option<i32> {
    let txt = extract_xml_text(soap_xml, "CurrentIndex")?;
    let n: i32 = txt.trim().parse().ok()?;
    if n > 0 { Some(n - 1) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_context_and_index_parsing() {
        let items = vec![MediaItem {
            id: "t1".into(),
            url: "http://192.168.1.10:4320/media/local/t1.flac".into(),
            title: "Track One".into(),
            mime: "audio/flac".into(),
            ..Default::default()
        }];
        let xml = build_queue_context_xml("tacet", &items, &[]);
        assert!(xml.contains("<Track1>"));
        assert!(xml.contains("<TrackNumber>1</TrackNumber>"));
        assert!(xml.contains("&lt;DIDL-Lite"));

        let resp = r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:GetQueueIndexResponse xmlns:u="urn:schemas-wiimu-com:service:PlayQueue:1"><CurrentIndex>3</CurrentIndex></u:GetQueueIndexResponse></s:Body></s:Envelope>"#;
        assert_eq!(parse_get_queue_index(resp), Some(2));
    }
}
