use std::collections::HashSet;
use std::time::Duration;

pub const DLNA_FEATURES_SEEKABLE: &str =
    "DLNA.ORG_OP=01;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000000000000000000000000000";
pub const DLNA_FEATURES_STREAMING: &str =
    "DLNA.ORG_OP=00;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000000000000000000000000000";
pub const DLNA_FEATURES_TRANSCODED: &str =
    "DLNA.ORG_OP=00;DLNA.ORG_CI=1;DLNA.ORG_FLAGS=01700000000000000000000000000000";

#[derive(Debug, Clone, Default)]
pub struct MediaItem {
    pub id: String,
    pub url: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub album_art_url: String,
    pub track_number: i32,
    pub mime: String,
    pub duration: Duration,
    pub size: i64,
    pub sample_rate: i32,
    pub bit_depth: i32,
    pub channels: i32,
    pub seekable: bool,
    pub transcoded: bool,
}

/// Picks the MIME type spelling a renderer actually advertises in
/// ConnectionManager.GetProtocolInfo. WiiM / LinkPlay renderers advertise
/// "audio/flac" (RFC 9639) and reject "audio/x-flac".
pub fn negotiate_mime(preferred: &str, sink_protocols: &[String]) -> String {
    let mut pref = preferred.trim().to_ascii_lowercase();
    if pref.is_empty() {
        pref = "audio/flac".into();
    }
    if sink_protocols.is_empty() {
        return pref;
    }

    let mut sink_set: HashSet<String> = HashSet::with_capacity(sink_protocols.len());
    for entry in sink_protocols {
        let mut parts = entry.trim().split(':');
        let _ = parts.next();
        let _ = parts.next();
        if let Some(mime) = parts.next() {
            sink_set.insert(mime.trim().to_ascii_lowercase());
        }
    }

    if sink_set.contains(&pref) {
        return pref;
    }

    let alts: &[&str] = match pref.as_str() {
        "audio/flac" => &["audio/x-flac"],
        "audio/x-flac" => &["audio/flac"],
        "audio/wav" => &["audio/x-wav", "audio/wave"],
        "audio/x-wav" => &["audio/wav", "audio/wave"],
        "audio/aiff" => &["audio/x-aiff"],
        "audio/x-aiff" => &["audio/aiff"],
        "audio/mp4" => &["audio/x-m4a", "audio/m4a", "audio/alac"],
        "audio/x-dsf" => &["audio/dsf", "audio/x-dsd", "audio/dsd"],
        "audio/dsf" => &["audio/x-dsf", "audio/x-dsd", "audio/dsd"],
        "audio/mpeg" => &["audio/mp3"],
        "audio/ogg" => &["application/ogg", "audio/x-ogg", "audio/opus"],
        "audio/opus" => &["audio/ogg", "application/ogg"],
        "audio/webm" => &["video/webm"],
        _ => &[],
    };
    for &alt in alts {
        if sink_set.contains(alt) {
            return alt.into();
        }
    }
    pref
}

/// Builds raw (unescaped) DIDL-Lite XML for AVTransport URI metadata.
pub fn build_didl(m: &MediaItem, sink_protocols: &[String]) -> String {
    let mime = negotiate_mime(&m.mime, sink_protocols);
    let features = if m.transcoded {
        DLNA_FEATURES_TRANSCODED
    } else if m.seekable {
        DLNA_FEATURES_SEEKABLE
    } else {
        DLNA_FEATURES_STREAMING
    };
    let protocol_info = format!("http-get:*:{mime}:{features}");

    let item_id = if m.id.is_empty() { "0" } else { &m.id };
    let title = if m.title.is_empty() {
        "Unknown Track"
    } else {
        &m.title
    };

    let mut b = String::with_capacity(512);
    b.push_str(r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" "#);
    b.push_str(r#"xmlns:dc="http://purl.org/dc/elements/1.1/" "#);
    b.push_str(r#"xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">"#);
    b.push_str(r#"<item id=""#);
    b.push_str(&escape_xml(item_id));
    b.push_str(r#"" parentID="0" restricted="1">"#);

    b.push_str("<dc:title>");
    b.push_str(&escape_xml(title));
    b.push_str("</dc:title>");
    b.push_str("<upnp:class>object.item.audioItem.musicTrack</upnp:class>");

    if !m.artist.is_empty() {
        let esc = escape_xml(&m.artist);
        b.push_str("<dc:creator>");
        b.push_str(&esc);
        b.push_str("</dc:creator>");
        b.push_str("<upnp:artist>");
        b.push_str(&esc);
        b.push_str("</upnp:artist>");
    }
    if !m.album.is_empty() {
        b.push_str("<upnp:album>");
        b.push_str(&escape_xml(&m.album));
        b.push_str("</upnp:album>");
    }
    if m.track_number > 0 {
        b.push_str("<upnp:originalTrackNumber>");
        b.push_str(&m.track_number.to_string());
        b.push_str("</upnp:originalTrackNumber>");
    }
    if !m.album_art_url.is_empty() {
        b.push_str("<upnp:albumArtURI>");
        b.push_str(&escape_xml(&m.album_art_url));
        b.push_str("</upnp:albumArtURI>");
    }

    b.push_str(r#"<res protocolInfo=""#);
    b.push_str(&escape_xml(&protocol_info));
    b.push('"');
    if !m.duration.is_zero() {
        b.push_str(r#" duration=""#);
        b.push_str(&format_upnp_duration(m.duration));
        b.push('"');
    }
    if m.size > 0 {
        b.push_str(&format!(r#" size="{}""#, m.size));
    }
    if m.sample_rate > 0 {
        b.push_str(&format!(r#" sampleFrequency="{}""#, m.sample_rate));
    }
    if m.bit_depth > 0 {
        b.push_str(&format!(r#" bitsPerSample="{}""#, m.bit_depth));
    }
    if m.channels > 0 {
        b.push_str(&format!(r#" nrAudioChannels="{}""#, m.channels));
    }
    b.push('>');
    b.push_str(&escape_xml(&m.url));
    b.push_str("</res>");
    b.push_str("</item></DIDL-Lite>");
    b
}

pub fn format_upnp_duration(d: Duration) -> String {
    let total_secs = d.as_secs_f64().round().max(0.0) as u64;
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    format!("{h}:{m:02}:{s:02}")
}

pub fn parse_upnp_duration(raw: &str) -> Duration {
    let raw = raw.trim();
    if raw.is_empty() || raw == "NOT_IMPLEMENTED" || raw == "UNKNOWN" {
        return Duration::ZERO;
    }
    let parts: Vec<&str> = raw.split(':').collect();
    if parts.len() != 3 {
        return Duration::ZERO;
    }
    let Ok(h) = parts[0].parse::<u64>() else {
        return Duration::ZERO;
    };
    let Ok(m) = parts[1].parse::<u64>() else {
        return Duration::ZERO;
    };
    let (sec_str, ms) = if let Some((s, frac)) = parts[2].split_once('.') {
        if frac.is_empty() {
            return Duration::ZERO;
        }
        let mut f = frac.chars().take(3).collect::<String>();
        while f.len() < 3 {
            f.push('0');
        }
        let Ok(ms) = f.parse::<u64>() else {
            return Duration::ZERO;
        };
        (s, ms)
    } else {
        (parts[2], 0)
    };
    let Ok(s) = sec_str.parse::<u64>() else {
        return Duration::ZERO;
    };
    if h > 999 || m > 59 || s > 59 {
        return Duration::ZERO;
    }
    Duration::from_secs(h * 3600 + m * 60 + s) + Duration::from_millis(ms)
}

pub fn escape_xml(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}
