pub mod didl;
pub mod discovery;
pub mod gena;
pub mod monitor;
pub mod renderer;

pub use didl::{
    DLNA_FEATURES_SEEKABLE, DLNA_FEATURES_STREAMING, DLNA_FEATURES_TRANSCODED, MediaItem,
    build_didl, escape_xml, format_upnp_duration, negotiate_mime, parse_upnp_duration,
};
pub use discovery::{
    connect_by_url, discover, is_allowed_renderer_ip, local_ip_for_host, probe_location,
    validate_renderer_url,
};
pub use gena::{
    DeviceEvent, GenaClient, SERVICE_AV_TRANSPORT, SERVICE_RENDERING_CONTROL, parse_gena_timeout,
    parse_notify_body,
};
pub use monitor::{Monitor, MonitorCallbacks, MonitorConfig};
pub use renderer::{
    Controller, QueueController, Renderer, RendererInfo, Result, TransportStatus,
    URN_AV_TRANSPORT_1, URN_CONNECTION_MANAGER_1, URN_LINKPLAY_PLAYQUEUE_1,
    URN_RENDERING_CONTROL_1, UpnpError, extract_xml_text, new_soap_http_client,
};

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;
    use std::time::Duration;

    #[test]
    fn didl_and_mime_negotiation() {
        let sinks = vec!["http-get:*:audio/flac:*".into()];
        assert_eq!(negotiate_mime("audio/x-flac", &sinks), "audio/flac");

        let item = MediaItem {
            id: "t1".into(),
            url: "http://192.168.1.10:4320/media/local/t1.flac".into(),
            title: "So What & All Blues".into(),
            artist: "Miles Davis".into(),
            album: "Kind of Blue".into(),
            mime: "audio/x-flac".into(),
            duration: Duration::from_secs(562),
            sample_rate: 96000,
            bit_depth: 24,
            channels: 2,
            seekable: true,
            ..Default::default()
        };
        let xml = build_didl(&item, &sinks);
        assert!(xml.contains("audio/flac"));
        assert!(xml.contains("So What &amp; All Blues"));
        assert!(xml.contains(r#"duration="0:09:22""#));
        assert!(xml.contains(r#"sampleFrequency="96000""#));
    }

    #[test]
    fn upnp_duration_parsing() {
        assert_eq!(parse_upnp_duration("0:03:45"), Duration::from_secs(225));
        assert_eq!(
            parse_upnp_duration("01:02:03.250"),
            Duration::from_secs(3723) + Duration::from_millis(250)
        );
        assert_eq!(parse_upnp_duration("NOT_IMPLEMENTED"), Duration::ZERO);
        assert_eq!(format_upnp_duration(Duration::from_secs(225)), "0:03:45");
    }

    #[test]
    fn ssrf_ip_guard_allows_lan_and_cgnat_only() {
        assert!(is_allowed_renderer_ip(
            "192.168.1.50".parse::<IpAddr>().unwrap()
        ));
        assert!(is_allowed_renderer_ip(
            "10.0.0.5".parse::<IpAddr>().unwrap()
        ));
        assert!(is_allowed_renderer_ip(
            "100.100.1.2".parse::<IpAddr>().unwrap()
        )); // Tailscale CGNAT
        assert!(!is_allowed_renderer_ip(
            "127.0.0.1".parse::<IpAddr>().unwrap()
        ));
        assert!(!is_allowed_renderer_ip(
            "169.254.169.254".parse::<IpAddr>().unwrap()
        ));
        assert!(!is_allowed_renderer_ip(
            "8.8.8.8".parse::<IpAddr>().unwrap()
        ));
    }

    #[test]
    fn gena_notify_parses_last_change() {
        let body = br#"<e:propertyset xmlns:e="urn:schemas-upnp-org:event-1-0">
            <e:property><LastChange>&lt;Event xmlns="urn:schemas-upnp-org:metadata-1-0/AVT/"&gt;
            &lt;InstanceID val="0"&gt;
              &lt;TransportState val="PLAYING"/&gt;
              &lt;CurrentTrackURI val="http://192.168.1.10:4320/media/local/1.flac"/&gt;
              &lt;CurrentTrackDuration val="0:04:10"/&gt;
            &lt;/InstanceID&gt;&lt;/Event&gt;</LastChange></e:property></e:propertyset>"#;
        let ev = parse_notify_body(SERVICE_AV_TRANSPORT, body);
        assert!(ev.has_state);
        assert_eq!(ev.state, "PLAYING");
        assert!(ev.has_track_uri);
        assert_eq!(ev.duration, Duration::from_secs(250));
    }

    #[tokio::test]
    async fn gena_notify_enforces_strict_sid_and_delivers_event() {
        use axum::{
            Router,
            body::Bytes,
            http::{HeaderMap, HeaderValue, StatusCode},
        };

        // Spin up a mock UPnP eventSubURL that captures the CALLBACK token from SUBSCRIBE and returns a SID
        let (tok_tx, mut tok_rx) = tokio::sync::mpsc::channel::<String>(4);
        let mock_upnp = Router::new().route(
            "/evt",
            axum::routing::any(move |headers: HeaderMap| {
                let tok_tx = tok_tx.clone();
                async move {
                    if let Some(cb) = headers.get("callback").and_then(|v| v.to_str().ok())
                        && let Some(tok) =
                            cb.trim_matches(|c| c == '<' || c == '>').rsplit('/').next()
                    {
                        let _ = tok_tx.try_send(tok.to_string());
                    }
                    (
                        StatusCode::OK,
                        [("sid", "uuid:good-sid-123"), ("timeout", "Second-300")],
                        "",
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = tokio::spawn(async move {
            axum::serve(listener, mock_upnp).await.unwrap();
        });

        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::channel::<DeviceEvent>(4);
        let gena = GenaClient::new(move |ev| {
            let _ = ev_tx.try_send(ev);
        });

        gena.watch_renderer(
            "upnp:z1",
            "http://127.0.0.1:4320",
            &format!("http://{addr}/evt"),
            "",
        );
        let token = tokio::time::timeout(Duration::from_secs(2), tok_rx.recv())
            .await
            .expect("timed out waiting for SUBSCRIBE")
            .expect("token channel closed");
        assert!(!token.is_empty(), "SUBSCRIBE must send CALLBACK token");

        // Wait until GenaClient has stored the active SID from the SUBSCRIBE response
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while !gena.is_active("upnp:z1") {
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for GENA subscription to become active"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        let notify_method = axum::http::Method::from_bytes(b"NOTIFY").unwrap();
        let notify_body = Bytes::from_static(br#"<e:propertyset xmlns:e="urn:schemas-upnp-org:event-1-0"><e:property><LastChange>&lt;Event&gt;&lt;InstanceID val="0"&gt;&lt;TransportState val="PLAYING"/&gt;&lt;/InstanceID&gt;&lt;/Event&gt;</LastChange></e:property></e:propertyset>"#);

        // Missing SID -> 412 PRECONDITION_FAILED
        let r_missing = gena
            .handle_notify(
                notify_method.clone(),
                &token,
                &HeaderMap::new(),
                notify_body.clone(),
            )
            .await;
        assert_eq!(r_missing, StatusCode::PRECONDITION_FAILED);

        // Wrong SID -> 412 PRECONDITION_FAILED
        let mut wrong_hdrs = HeaderMap::new();
        wrong_hdrs.insert("sid", HeaderValue::from_static("uuid:wrong-sid"));
        let r_wrong = gena
            .handle_notify(
                notify_method.clone(),
                &token,
                &wrong_hdrs,
                notify_body.clone(),
            )
            .await;
        assert_eq!(r_wrong, StatusCode::PRECONDITION_FAILED);

        // Matching SID -> 200 OK & event delivered
        let mut ok_hdrs = HeaderMap::new();
        ok_hdrs.insert("sid", HeaderValue::from_static("uuid:good-sid-123"));
        let r_ok = gena
            .handle_notify(notify_method, &token, &ok_hdrs, notify_body)
            .await;
        assert_eq!(r_ok, StatusCode::OK);
        let ev = tokio::time::timeout(Duration::from_secs(2), ev_rx.recv())
            .await
            .expect("timed out waiting for GENA event")
            .expect("event channel closed");
        assert_eq!(ev.state, "PLAYING");

        gena.close();
        srv.abort();
    }

    #[test]
    fn ssdp_notify_parser_handles_alive_byebye_and_filters() {
        let alive = "NOTIFY * HTTP/1.1\r\n\
HOST: 239.255.255.250:1900\r\n\
NT: urn:schemas-upnp-org:service:AVTransport:1\r\n\
NTS: ssdp:alive\r\n\
LOCATION: http://192.168.1.50:49152/description.xml\r\n\
USN: uuid:speaker-1::urn:schemas-upnp-org:service:AVTransport:1\r\n\r\n";
        assert_eq!(
            monitor::parse_ssdp_notify(alive),
            Some(monitor::SsdpNotify::Alive {
                location: "http://192.168.1.50:49152/description.xml".into(),
            })
        );

        let byebye = "NOTIFY * HTTP/1.1\r\n\
NT: urn:schemas-upnp-org:device:MediaRenderer:1\r\n\
NTS: ssdp:byebye\r\n\
USN: uuid:speaker-1::urn:schemas-upnp-org:device:MediaRenderer:1\r\n\r\n";
        assert_eq!(
            monitor::parse_ssdp_notify(byebye),
            Some(monitor::SsdpNotify::Byebye {
                usn: "uuid:speaker-1::urn:schemas-upnp-org:device:MediaRenderer:1".into(),
                location: String::new(),
            })
        );

        let root_byebye = "NOTIFY * HTTP/1.1\r\n\
NT: upnp:rootdevice\r\n\
NTS: ssdp:byebye\r\n\
USN: uuid:speaker-1::upnp:rootdevice\r\n\r\n";
        assert_eq!(
            monitor::parse_ssdp_notify(root_byebye),
            Some(monitor::SsdpNotify::Byebye {
                usn: "uuid:speaker-1::upnp:rootdevice".into(),
                location: String::new(),
            })
        );

        let non_renderer = "NOTIFY * HTTP/1.1\r\n\
NT: urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\n\
NTS: ssdp:alive\r\n\
LOCATION: http://192.168.1.1/rootDesc.xml\r\n\r\n";
        assert_eq!(monitor::parse_ssdp_notify(non_renderer), None);
    }
}
