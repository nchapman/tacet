//! Lenient UPnP XML sanitization, entity decoding, and streaming extraction.
//!
//! Real-world UPnP/DLNA renderers (WiiM/Linkplay, Sonos, Denon/Marantz HEOS,
//! KEF, Naim, BubbleUPnP, upmpdcli) frequently emit XML with trailing NUL
//! bytes, undeclared namespace prefixes (`<r:...>`, `<dlna:...>`), Latin-1
//! room names, or unescaped `&` in query-string URLs (e.g. Qobuz signed URLs
//! inside GENA `<LastChange>`). Strict XML 1.0+Namespaces parsers reject these
//! payloads; this module mirrors Go's `encoding/xml` with `Strict = false`.

use quick_xml::Reader;
use quick_xml::events::Event;

/// Decodes raw XML bytes as UTF-8 when valid, falling back to ISO-8859-1
/// (Latin-1) so accented room names (e.g. "Séjour", "Küche") are preserved.
pub fn decode_xml_bytes(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => bytes.iter().map(|&b| b as char).collect(),
    }
}

/// Strips UTF-8 BOM, leading whitespace before `<?xml`, trailing/embedded NULs,
/// and invalid XML 1.0 control characters (`< 0x20` except `\t`, `\n`, `\r`).
pub fn sanitize_xml(raw: &str) -> String {
    let trimmed = raw.trim_start_matches('\u{FEFF}').trim();
    let mut out = String::with_capacity(trimmed.len());
    for ch in trimmed.chars() {
        let code = ch as u32;
        if code >= 0x20 || matches!(ch, '\t' | '\n' | '\r') {
            out.push(ch);
        }
    }
    out
}

/// Returns the local name of an XML tag (stripping any `prefix:` namespace).
pub fn local_name(raw: &[u8]) -> String {
    let s = std::str::from_utf8(raw).unwrap_or("");
    s.rsplit(':').next().unwrap_or(s).to_string()
}

/// Leniently decodes XML named and numeric entities, leaving bare unescaped
/// `&foo=` sequences intact (e.g. signed CDN URLs in GENA `LastChange`).
pub fn unescape_lenient(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'&'
            && let Some(rel_semi) = bytes[i + 1..].iter().take(12).position(|&b| b == b';')
        {
            let entity = &s[i + 1..i + 1 + rel_semi];
            if let Some(decoded) = decode_entity(entity) {
                out.push(decoded);
                i += rel_semi + 2;
                continue;
            }
        }
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn decode_entity(entity: &str) -> Option<char> {
    match entity {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        _ if entity.starts_with("#x") || entity.starts_with("#X") => {
            u32::from_str_radix(&entity[2..], 16)
                .ok()
                .and_then(char::from_u32)
        }
        _ if entity.starts_with('#') => entity[1..].parse::<u32>().ok().and_then(char::from_u32),
        _ => None,
    }
}

/// Constructs a lenient `quick_xml::Reader` matching Go's `Strict = false` behavior.
pub fn lenient_reader(xml: &str) -> Reader<&[u8]> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().check_end_names = false;
    reader.config_mut().trim_text(false);
    reader
}

/// Extracts the text content of the first element whose local name matches `target`
/// (case-insensitive), tolerating undeclared namespace prefixes and bare `&`.
pub fn extract_element_text(xml: &str, target: &str) -> Option<String> {
    let clean = sanitize_xml(xml);
    let mut reader = lenient_reader(&clean);
    let mut depth_in_target: usize = 0;
    let mut buf = String::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                if depth_in_target > 0 {
                    depth_in_target += 1;
                } else if local_name(e.name().as_ref()).eq_ignore_ascii_case(target) {
                    depth_in_target = 1;
                }
            }
            Ok(Event::Empty(e)) => {
                if depth_in_target == 0
                    && local_name(e.name().as_ref()).eq_ignore_ascii_case(target)
                {
                    return Some(String::new());
                }
            }
            Ok(Event::Text(t)) if depth_in_target > 0 => {
                if let Ok(raw) = std::str::from_utf8(t.as_ref()) {
                    buf.push_str(&unescape_lenient(raw));
                }
            }
            Ok(Event::CData(c)) if depth_in_target > 0 => {
                if let Ok(raw) = std::str::from_utf8(c.as_ref()) {
                    buf.push_str(raw);
                }
            }
            Ok(Event::End(_)) if depth_in_target > 0 => {
                depth_in_target -= 1;
                if depth_in_target == 0 {
                    return Some(buf.trim().to_string());
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    None
}

/// Extracts a UPnP SOAP fault `(errorCode, errorDescription)` if present in `xml`.
/// Handles both HTTP 500 and buggy HTTP 200 SOAP fault responses.
pub fn parse_soap_fault_xml(xml: &str) -> Option<(i32, String)> {
    // Fast path for normal non-fault responses
    if !xml.contains("Fault") && !xml.contains("UPnPError") && !xml.contains("errorCode") {
        return None;
    }
    let code = extract_element_text(xml, "errorCode").and_then(|s| s.parse::<i32>().ok());
    let desc = extract_element_text(xml, "errorDescription")
        .or_else(|| extract_element_text(xml, "faultstring"))
        .unwrap_or_default();
    if let Some(c) = code {
        return Some((c, desc));
    }
    if !desc.is_empty() && xml.contains("Fault") {
        return Some((500, desc));
    }
    None
}
