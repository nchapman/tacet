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
}
