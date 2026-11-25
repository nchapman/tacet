pub mod didl;
pub mod discovery;
pub mod gena;
pub(crate) mod http;
pub mod monitor;
pub mod renderer;
pub(crate) mod xml;

pub use didl::{
    DLNA_FEATURES_SEEKABLE, DLNA_FEATURES_STREAMING, DLNA_FEATURES_TRANSCODED,
    DLNA_FEATURES_TRANSCODED_SEEKABLE, MediaItem, build_didl, escape_xml, escape_xml_attr,
    escape_xml_text, format_upnp_duration, negotiate_mime, parse_upnp_duration,
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
        assert!(xml.contains(r#"duration="00:09:22""#));
        assert!(xml.contains(r#"sampleFrequency="96000""#));

        // Parameterized MIME types in sink protocols are stripped to base MIME.
        let param_sinks = vec!["http-get:*:audio/flac;rate=44100:*".into()];
        assert_eq!(negotiate_mime("audio/x-flac", &param_sinks), "audio/flac");

        // Seekable transcoded WAV advertises OP=01;CI=1 (not OP=00).
        let mut wav_item = item.clone();
        wav_item.mime = "audio/wav".into();
        wav_item.transcoded = true;
        wav_item.seekable = true;
        let wav_didl = build_didl(&wav_item, &[]);
        assert!(wav_didl.contains(DLNA_FEATURES_TRANSCODED_SEEKABLE));

        // IPv6 Host header formatting always wraps literal IPv6 addresses in brackets.
        assert_eq!(
            http::format_host_header("fd00::5", Some(1400)),
            "[fd00::5]:1400"
        );
        assert_eq!(
            http::format_host_header("[fd00::5]", Some(1400)),
            "[fd00::5]:1400"
        );
        assert_eq!(
            http::format_host_header("192.168.1.5", Some(80)),
            "192.168.1.5:80"
        );

        // Short GENA timeouts (e.g. Second-10) are preserved rather than clamped above expiry.
        assert_eq!(parse_gena_timeout("Second-10"), Duration::from_secs(10));

        // Numeric control-char entity &#0; is not decoded into a NUL byte.
        assert!(!xml::unescape_lenient("a&#0;b").contains('\0'));
    }

    #[test]
    fn upnp_duration_parsing() {
        assert_eq!(parse_upnp_duration("0:03:45"), Duration::from_secs(225));
        assert_eq!(
            parse_upnp_duration("01:02:03.250"),
            Duration::from_secs(3723) + Duration::from_millis(250)
        );
        assert_eq!(parse_upnp_duration("NOT_IMPLEMENTED"), Duration::ZERO);
        assert_eq!(format_upnp_duration(Duration::from_secs(225)), "00:03:45");
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

    #[test]
    fn lenient_xml_tolerates_nul_undeclared_prefixes_latin1_and_qobuz_ampersands() {
        // Trailing NUL bytes and undeclared namespace prefix in SOAP response.
        let soap_with_nul = b"<?xml version=\"1.0\"?>\
<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\">\
<s:Body><u:GetTransportInfoResponse xmlns:u=\"urn:schemas-upnp-org:service:AVTransport:1\">\
<r:VendorFlag>1</r:VendorFlag>\
<CurrentTransportState>PLAYING</CurrentTransportState>\
</u:GetTransportInfoResponse></s:Body></s:Envelope>\0\0";
        let decoded = xml::decode_xml_bytes(soap_with_nul);
        assert_eq!(
            extract_xml_text(&decoded, "CurrentTransportState").as_deref(),
            Some("PLAYING")
        );

        // Latin-1 friendlyName ("Séjour") is preserved rather than garbled.
        let latin1_desc = b"<root><device><friendlyName>S\xe9jour</friendlyName></device></root>";
        let loc = url::Url::parse("http://192.168.1.50:1400/xml/device_description.xml").unwrap();
        let parsed = discovery::parse_description_xml(&xml::decode_xml_bytes(latin1_desc), &loc)
            .expect("parse Latin-1 description");
        assert_eq!(parsed.friendly_name, "Séjour");

        // GENA LastChange with unescaped '&' in Qobuz signed CDN URL and undeclared <r:...> tag.
        let gena_body = br#"<e:propertyset xmlns:e="urn:schemas-upnp-org:event-1-0"><e:property><LastChange>&lt;Event&gt;&lt;InstanceID val="0"&gt;&lt;r:EnqueuedTransportURI val="x"/&gt;&lt;TransportState val="PLAYING"/&gt;&lt;CurrentTrackURI val="https://streaming.qobuz.com/file?uid=1&amp;sig=abc&expires=999"/&gt;&lt;/InstanceID&gt;&lt;/Event&gt;</LastChange></e:property></e:propertyset>"#;
        let ev = parse_notify_body(SERVICE_AV_TRANSPORT, gena_body);
        assert!(ev.has_state);
        assert_eq!(ev.state, "PLAYING");
        assert!(ev.has_track_uri);
        assert!(ev.track_uri.contains("expires=999"));
    }

    #[test]
    fn description_scopes_connection_manager_to_avtransport_subdevice() {
        // Sonos-style root device with both MediaServer and MediaRenderer sub-devices.
        let sonos_xml = r#"<root xmlns="urn:schemas-upnp-org:device-1-0">
          <device>
            <deviceType>urn:schemas-upnp-org:device:ZonePlayer:1</deviceType>
            <friendlyName>Living Room</friendlyName>
            <UDN>uuid:RINCON_1234</UDN>
            <deviceList>
              <device>
                <deviceType>urn:schemas-upnp-org:device:MediaServer:1</deviceType>
                <serviceList>
                  <service>
                    <serviceType>urn:schemas-upnp-org:service:ConnectionManager:1</serviceType>
                    <controlURL>/MediaServer/ConnectionManager/Control</controlURL>
                  </service>
                </serviceList>
              </device>
              <device>
                <deviceType>urn:schemas-upnp-org:device:MediaRenderer:1</deviceType>
                <serviceList>
                  <service>
                    <serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType>
                    <controlURL>/MediaRenderer/AVTransport/Control</controlURL>
                  </service>
                  <service>
                    <serviceType>urn:schemas-upnp-org:service:ConnectionManager:1</serviceType>
                    <controlURL>/MediaRenderer/ConnectionManager/Control</controlURL>
                  </service>
                  <service>
                    <serviceType>urn:schemas-upnp-org:service:RenderingControl:1</serviceType>
                    <controlURL>/MediaRenderer/RenderingControl/Control</controlURL>
                  </service>
                </serviceList>
              </device>
            </deviceList>
          </device>
        </root>"#;
        let loc = url::Url::parse("http://192.168.1.50:1400/xml/device_description.xml").unwrap();
        let parsed = discovery::parse_description_xml(sonos_xml, &loc).unwrap();
        let (av_idx, av) = discovery::find_service_in_devices(&parsed.devices, "AVTransport")
            .expect("AVTransport");
        assert_eq!(av.control_url, "/MediaRenderer/AVTransport/Control");
        let cm =
            discovery::find_service_preferring_device(&parsed.devices, av_idx, "ConnectionManager")
                .expect("ConnectionManager");
        assert_eq!(cm.control_url, "/MediaRenderer/ConnectionManager/Control");
    }

    #[test]
    fn msearch_payload_and_lenient_response_parser() {
        let req = discovery::build_msearch_payload(URN_AV_TRANSPORT_1, 2);
        assert!(req.starts_with("M-SEARCH * HTTP/1.1\r\n"));
        assert!(req.contains("HOST: 239.255.255.250:1900\r\n"));
        assert!(req.contains("MAN: \"ssdp:discover\"\r\n"));
        assert!(req.contains("MX: 2\r\n"));
        assert!(req.contains(&format!("ST: {URN_AV_TRANSPORT_1}\r\n")));

        // Tolerates LF endings and HTTP/1.0 responses without SERVER/USN headers.
        let resp = "HTTP/1.0 200 OK\nlocation: http://192.168.1.88:49152/desc.xml\n\n";
        assert_eq!(
            discovery::parse_msearch_location(resp).as_deref(),
            Some("http://192.168.1.88:49152/desc.xml")
        );
    }

    #[tokio::test]
    async fn raw_soap_sends_uppercase_headers_unescaped_quotes_and_detects_http_200_fault() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let captured = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let cap_clone = std::sync::Arc::clone(&captured);

        let srv = tokio::spawn(async move {
            // 1st request: SetAVTransportURI -> capture exact wire bytes, return 200 OK
            let (mut s1, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let n = s1.read(&mut buf).await.unwrap();
            *cap_clone.lock().unwrap() = String::from_utf8_lossy(&buf[..n]).into_owned();
            let ok_body = r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:SetAVTransportURIResponse xmlns:u="urn:schemas-upnp-org:service:AVTransport:1"/></s:Body></s:Envelope>"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{ok_body}",
                ok_body.len()
            );
            s1.write_all(resp.as_bytes()).await.unwrap();

            // 2nd request: Play -> return HTTP 200 with embedded SOAP Fault 701
            let (mut s2, _) = listener.accept().await.unwrap();
            let _ = s2.read(&mut buf).await.unwrap();
            let fault_body = r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><s:Fault><detail><UPnPError><errorCode>701</errorCode><errorDescription>Transition not available</errorDescription></UPnPError></detail></s:Fault></s:Body></s:Envelope>"#;
            let resp2 = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{fault_body}",
                fault_body.len()
            );
            s2.write_all(resp2.as_bytes()).await.unwrap();
        });

        let r = Renderer::new_internal(
            RendererInfo {
                id: "upnp:wire".into(),
                name: "WireTest".into(),
                ..Default::default()
            },
            renderer::RendererEndpoints {
                local_ip: "127.0.0.1".into(),
                av_control_url: format!("http://{addr}/AVT"),
                ..Default::default()
            },
        );
        let item = MediaItem {
            id: "1".into(),
            url: "http://192.168.1.10:4320/media/local/1.flac".into(),
            title: "Don't Stop".into(),
            mime: "audio/flac".into(),
            ..Default::default()
        };
        r.set_uri(&item).await.expect("SetAVTransportURI");
        let wire = captured.lock().unwrap().clone();
        assert!(wire.contains(
            "SOAPACTION: \"urn:schemas-upnp-org:service:AVTransport:1#SetAVTransportURI\"\r\n"
        ));
        assert!(wire.contains("CONTENT-TYPE: text/xml; charset=\"utf-8\"\r\n"));
        assert!(!wire.to_ascii_lowercase().contains("accept-encoding: gzip"));
        // DIDL attributes inside <CurrentURIMetaData> keep literal quotes (not &quot;)
        assert!(wire.contains(r#"&lt;item id="1""#));
        assert!(!wire.contains("&#39;"));

        let err = r.play().await.expect_err("HTTP 200 SOAP fault must fail");
        assert!(matches!(err, UpnpError::SoapFault { code: 701, .. }));
        srv.await.unwrap();
    }
}
