//! Raw HTTP/1.1 client for UPnP SOAP 1.1 and GENA requests.
//!
//! Embedded UPnP stacks (`libupnp`, lwIP, older Denon/Marantz/Yamaha/Sony AVRs)
//! frequently perform case-sensitive matching on uppercase wire headers
//! (`SOAPACTION`, `CONTENT-TYPE`, `CALLBACK`, `NT`, `TIMEOUT`, `SID`) and can
//! reject `reqwest`/`hyper`'s lowercased headers or `accept-encoding: gzip`.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use url::Url;

const MAX_HEADER_BYTES: usize = 32 * 1024;
const DRAIN_LIMIT_BYTES: usize = 4096;

#[derive(Debug)]
pub struct RawHttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl RawHttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Debug)]
pub enum RawHttpError {
    /// Connection closed early or reset by peer (safe to retry for idempotent UPnP quirks).
    EofOrReset(String),
    /// Timed out connecting, writing, or reading (do NOT blindly retry non-idempotent actions).
    Timeout(String),
    /// Any other URL, I/O, or protocol error.
    Other(String),
}

impl std::fmt::Display for RawHttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EofOrReset(s) | Self::Timeout(s) | Self::Other(s) => f.write_str(s),
        }
    }
}

/// Sends a SOAP 1.1 POST over raw HTTP/1.1 with exact uppercase `SOAPACTION`
/// and `CONTENT-TYPE` headers (matching `huin/goupnp/soap/soap.go`).
pub async fn send_raw_soap_post(
    control_url: &str,
    soap_action_quoted: &str,
    envelope: &str,
    timeout: Duration,
    max_body_bytes: usize,
) -> std::result::Result<RawHttpResponse, RawHttpError> {
    let u = Url::parse(control_url).map_err(|e| RawHttpError::Other(e.to_string()))?;
    if u.scheme() != "http" {
        return Err(RawHttpError::Other(format!(
            "raw SOAP requires http:// URL (got {})",
            u.scheme()
        )));
    }
    let host = u
        .host_str()
        .ok_or_else(|| RawHttpError::Other("missing host".into()))?;
    let port = u.port_or_known_default().unwrap_or(80);
    let path = url_path_and_query(&u);
    let host_hdr = format_host_header(host, u.port());

    let body_bytes = envelope.as_bytes();
    let req = format!(
        "POST {path} HTTP/1.1\r\n\
         HOST: {host_hdr}\r\n\
         CONTENT-TYPE: text/xml; charset=\"utf-8\"\r\n\
         SOAPACTION: {soap_action_quoted}\r\n\
         CONTENT-LENGTH: {}\r\n\
         CONNECTION: close\r\n\
         \r\n",
        body_bytes.len()
    );
    let mut wire = Vec::with_capacity(req.len() + body_bytes.len());
    wire.extend_from_slice(req.as_bytes());
    wire.extend_from_slice(body_bytes);

    send_raw_http_bytes(host, port, &wire, timeout, max_body_bytes, true).await
}

/// Sends a raw HTTP/1.1 request (e.g. GENA `SUBSCRIBE` / `UNSUBSCRIBE`) and
/// reads the status, headers, and (optionally drained) body cleanly before
/// closing the TCP connection so embedded stacks do not see a TCP RST.
pub async fn send_raw_http_bytes(
    host: &str,
    port: u16,
    req: &[u8],
    timeout: Duration,
    max_body_bytes: usize,
    read_full_body: bool,
) -> std::result::Result<RawHttpResponse, RawHttpError> {
    let connect_host = host.trim_matches(['[', ']']);
    let mut stream = tokio::time::timeout(timeout, TcpStream::connect((connect_host, port)))
        .await
        .map_err(|_| RawHttpError::Timeout("connect timed out".into()))?
        .map_err(classify_io_err)?;

    tokio::time::timeout(timeout, stream.write_all(req))
        .await
        .map_err(|_| RawHttpError::Timeout("write timed out".into()))?
        .map_err(classify_io_err)?;

    let deadline = tokio::time::Instant::now() + timeout;
    let mut buf = Vec::with_capacity(4096);

    // Loop to skip any 1xx interim responses (e.g. HTTP/1.1 100 Continue).
    let (status, headers, body_offset) = loop {
        let header_end = read_until_header_end(&mut stream, &mut buf, deadline).await?;
        let head_str = String::from_utf8_lossy(&buf[..header_end]);
        let (status, headers) = parse_head(&head_str)?;
        let body_start = header_end + 4;
        if (100..200).contains(&status) {
            buf.drain(..body_start);
            continue;
        }
        break (status, headers, body_start);
    };

    let mut body = buf.split_off(body_offset);
    let is_chunked = headers.iter().any(|(k, v)| {
        k.eq_ignore_ascii_case("transfer-encoding") && v.to_ascii_lowercase().contains("chunked")
    });
    let content_length = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse::<usize>().ok());

    if !read_full_body || matches!(status, 204 | 304) {
        // Drain up to 4 KiB with a short timeout so lwIP/libupnp does not receive RST.
        if !matches!(status, 204 | 304)
            && let Some(cl) = content_length
            && cl > 0
            && body.len() < cl.min(DRAIN_LIMIT_BYTES)
        {
            let target = cl.min(DRAIN_LIMIT_BYTES);
            let drain_deadline = tokio::time::Instant::now() + Duration::from_millis(150);
            let _ = read_up_to(&mut stream, &mut body, target, drain_deadline).await;
        }
        if matches!(status, 204 | 304) {
            body.clear();
        }
        return Ok(RawHttpResponse {
            status,
            headers,
            body,
        });
    }

    if is_chunked {
        body = read_chunked_body(&mut stream, body, max_body_bytes, deadline).await?;
    } else if let Some(cl) = content_length {
        if cl > max_body_bytes {
            return Err(RawHttpError::Other(format!(
                "HTTP response body ({cl} bytes) exceeds limit ({max_body_bytes} bytes)"
            )));
        }
        read_up_to(&mut stream, &mut body, cl, deadline).await?;
        if body.len() < cl {
            return Err(RawHttpError::EofOrReset(format!(
                "truncated HTTP body: got {} of {cl} bytes",
                body.len()
            )));
        }
        body.truncate(cl);
    } else {
        read_to_eof(&mut stream, &mut body, max_body_bytes, deadline).await?;
    }

    Ok(RawHttpResponse {
        status,
        headers,
        body,
    })
}

pub fn url_path_and_query(u: &Url) -> String {
    let p = if u.path().is_empty() { "/" } else { u.path() };
    if let Some(q) = u.query() {
        format!("{p}?{q}")
    } else {
        p.to_string()
    }
}

pub(crate) fn format_host_header(host: &str, port: Option<u16>) -> String {
    let bare = host.trim_matches(['[', ']']);
    let formatted_host = if bare.contains(':') {
        format!("[{bare}]")
    } else {
        bare.to_string()
    };
    match port {
        Some(p) => format!("{formatted_host}:{p}"),
        None => formatted_host,
    }
}

fn classify_io_err(e: std::io::Error) -> RawHttpError {
    use std::io::ErrorKind;
    match e.kind() {
        ErrorKind::UnexpectedEof
        | ErrorKind::ConnectionReset
        | ErrorKind::ConnectionAborted
        | ErrorKind::BrokenPipe => RawHttpError::EofOrReset(e.to_string()),
        ErrorKind::TimedOut => RawHttpError::Timeout(e.to_string()),
        _ => RawHttpError::Other(e.to_string()),
    }
}

async fn read_until_header_end(
    stream: &mut TcpStream,
    buf: &mut Vec<u8>,
    deadline: tokio::time::Instant,
) -> std::result::Result<usize, RawHttpError> {
    let mut chunk = [0u8; 2048];
    loop {
        if let Some(pos) = find_crlf_crlf(buf) {
            return Ok(pos);
        }
        if buf.len() >= MAX_HEADER_BYTES {
            return Err(RawHttpError::Other(
                "HTTP response headers too large".into(),
            ));
        }
        let n = tokio::time::timeout_at(deadline, stream.read(&mut chunk))
            .await
            .map_err(|_| RawHttpError::Timeout("header read timed out".into()))?
            .map_err(classify_io_err)?;
        if n == 0 {
            return Err(RawHttpError::EofOrReset("EOF before HTTP headers".into()));
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn find_crlf_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn parse_head(head: &str) -> std::result::Result<(u16, Vec<(String, String)>), RawHttpError> {
    let mut lines = head.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| RawHttpError::Other("empty HTTP response".into()))?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| RawHttpError::Other(format!("invalid HTTP status line: {status_line}")))?;
    let mut headers = Vec::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    Ok((status, headers))
}

async fn read_up_to(
    stream: &mut TcpStream,
    buf: &mut Vec<u8>,
    target: usize,
    deadline: tokio::time::Instant,
) -> std::result::Result<(), RawHttpError> {
    let mut chunk = [0u8; 4096];
    while buf.len() < target {
        let n = tokio::time::timeout_at(deadline, stream.read(&mut chunk))
            .await
            .map_err(|_| RawHttpError::Timeout("body read timed out".into()))?
            .map_err(classify_io_err)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    Ok(())
}

async fn read_to_eof(
    stream: &mut TcpStream,
    buf: &mut Vec<u8>,
    max_bytes: usize,
    deadline: tokio::time::Instant,
) -> std::result::Result<(), RawHttpError> {
    let mut chunk = [0u8; 4096];
    while buf.len() < max_bytes {
        let n = tokio::time::timeout_at(deadline, stream.read(&mut chunk))
            .await
            .map_err(|_| RawHttpError::Timeout("body read timed out".into()))?
            .map_err(classify_io_err)?;
        if n == 0 {
            break;
        }
        let take = n.min(max_bytes - buf.len());
        buf.extend_from_slice(&chunk[..take]);
    }
    Ok(())
}

async fn read_chunked_body(
    stream: &mut TcpStream,
    mut raw: Vec<u8>,
    max_bytes: usize,
    deadline: tokio::time::Instant,
) -> std::result::Result<Vec<u8>, RawHttpError> {
    let mut out = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        let line_end = loop {
            if let Some(p) = raw.windows(2).position(|w| w == b"\r\n") {
                break p;
            }
            let n = tokio::time::timeout_at(deadline, stream.read(&mut tmp))
                .await
                .map_err(|_| RawHttpError::Timeout("chunked read timed out".into()))?
                .map_err(classify_io_err)?;
            if n == 0 {
                return Err(RawHttpError::EofOrReset(
                    "EOF before chunked terminator".into(),
                ));
            }
            raw.extend_from_slice(&tmp[..n]);
        };
        let size_line = String::from_utf8_lossy(&raw[..line_end]);
        let size_hex = size_line.split(';').next().unwrap_or("").trim();
        let chunk_len = usize::from_str_radix(size_hex, 16)
            .map_err(|_| RawHttpError::Other(format!("invalid chunk size: {size_hex}")))?;
        raw.drain(..line_end + 2);
        if chunk_len == 0 {
            return Ok(out);
        }
        if out.len().saturating_add(chunk_len) > max_bytes {
            return Err(RawHttpError::Other(format!(
                "chunked HTTP body exceeds limit ({max_bytes} bytes)"
            )));
        }
        let need = chunk_len
            .checked_add(2)
            .ok_or_else(|| RawHttpError::Other("chunk size overflow".into()))?;
        while raw.len() < need {
            let n = tokio::time::timeout_at(deadline, stream.read(&mut tmp))
                .await
                .map_err(|_| RawHttpError::Timeout("chunked payload read timed out".into()))?
                .map_err(classify_io_err)?;
            if n == 0 {
                return Err(RawHttpError::EofOrReset(
                    "EOF mid-chunk in HTTP response".into(),
                ));
            }
            raw.extend_from_slice(&tmp[..n]);
        }
        out.extend_from_slice(&raw[..chunk_len]);
        raw.drain(..need);
    }
}
