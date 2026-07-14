//! A small HTTP/1.1 client over `std::net::TcpStream`.
//!
//! reqrun opens one connection per request (`Connection: close`), which keeps
//! the client trivially correct and is plenty for a test runner. Supports
//! Content-Length and chunked bodies, read-to-EOF fallbacks, connect/read
//! timeouts and redirect following. Response parsing is factored out so it
//! can be unit-tested against in-memory streams.

use crate::url::{self, Url};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// Maximum redirect hops before giving up (matches common client defaults).
const MAX_REDIRECTS: usize = 10;

/// Render an I/O error for humans. Read/write timeouts surface from the OS as
/// `WouldBlock` ("Resource temporarily unavailable") or `TimedOut`; both mean
/// the same thing to the user, so say so instead of leaking the errno text.
fn io_context(what: &str, e: std::io::Error) -> String {
    use std::io::ErrorKind;
    match e.kind() {
        ErrorKind::WouldBlock | ErrorKind::TimedOut => {
            format!("{what}: timed out (see --timeout)")
        }
        _ => format!("{what}: {e}"),
    }
}

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub reason: String,
    /// Headers in wire order; names are matched case-insensitively.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// The URL that produced this response (after redirects).
    pub final_url: String,
}

impl HttpResponse {
    /// First header value with the given name, case-insensitive.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// `Content-Type` split into mime type and charset, per the editors'
    /// `response.contentType` object.
    pub fn content_type(&self) -> (String, String) {
        let raw = self.header("Content-Type").unwrap_or("");
        let mut mime = String::new();
        let mut charset = String::new();
        for (i, part) in raw.split(';').enumerate() {
            let part = part.trim();
            if i == 0 {
                mime = part.to_ascii_lowercase();
            } else if let Some(cs) = part.to_ascii_lowercase().strip_prefix("charset=") {
                charset = cs.trim_matches('"').to_string();
            }
        }
        (mime, charset)
    }
}

/// Send `method` to `target` and return the final response, following
/// redirects unless `no_redirect`. Any failure returns a human-readable error.
pub fn send(
    method: &str,
    target: &Url,
    headers: &[(String, String)],
    body: &[u8],
    timeout: Duration,
    no_redirect: bool,
) -> Result<HttpResponse, String> {
    let mut current = target.clone();
    let mut method = method.to_string();
    let mut body = body.to_vec();
    for _hop in 0..=MAX_REDIRECTS {
        let response = send_once(&method, &current, headers, &body, timeout)?;
        let redirect = matches!(response.status, 301 | 302 | 303 | 307 | 308);
        if no_redirect || !redirect {
            return Ok(response);
        }
        let location = response
            .header("Location")
            .ok_or_else(|| format!("{} response without a Location header", response.status))?;
        current = url::resolve_location(&current, location)?;
        // Per RFC 9110: 303 always becomes GET; historic 301/302 behavior
        // demotes POST to GET. 307/308 keep the method and body.
        if response.status == 303 || (matches!(response.status, 301 | 302) && method == "POST") {
            method = "GET".to_string();
            body.clear();
        }
    }
    Err(format!("more than {MAX_REDIRECTS} redirects"))
}

fn send_once(
    method: &str,
    target: &Url,
    headers: &[(String, String)],
    body: &[u8],
    timeout: Duration,
) -> Result<HttpResponse, String> {
    let addr = target
        .authority()
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve {}: {e}", target.authority()))?
        .next()
        .ok_or_else(|| format!("cannot resolve {}", target.authority()))?;
    let stream = TcpStream::connect_timeout(&addr, timeout)
        .map_err(|e| format!("cannot connect to {}: {e}", target.authority()))?;
    stream.set_read_timeout(Some(timeout)).ok();
    stream.set_write_timeout(Some(timeout)).ok();

    let wire = build_request(method, target, headers, body);
    let mut stream = stream;
    stream
        .write_all(&wire)
        .map_err(|e| io_context(&format!("write to {}", target.authority()), e))?;

    let mut reader = BufReader::new(stream);
    // read_response errors are already human strings (see io_context).
    let mut response = read_response(&mut reader, method == "HEAD")
        .map_err(|e| format!("read from {}: {e}", target.authority()))?;
    response.final_url = format!("http://{}{}", target.host_header(), target.path_and_query);
    Ok(response)
}

/// Serialize the request head + body. User headers override the defaults
/// (Host, User-Agent, Accept, Connection, Content-Length) name-insensitively.
pub fn build_request(
    method: &str,
    target: &Url,
    headers: &[(String, String)],
    body: &[u8],
) -> Vec<u8> {
    let mut head = format!("{} {} HTTP/1.1\r\n", method, target.path_and_query);
    let has = |name: &str| headers.iter().any(|(n, _)| n.eq_ignore_ascii_case(name));
    if !has("Host") {
        head.push_str(&format!("Host: {}\r\n", target.host_header()));
    }
    if !has("User-Agent") {
        head.push_str(&format!(
            "User-Agent: reqrun/{}\r\n",
            env!("CARGO_PKG_VERSION")
        ));
    }
    if !has("Accept") {
        head.push_str("Accept: */*\r\n");
    }
    if (!body.is_empty() || matches!(method, "POST" | "PUT" | "PATCH")) && !has("Content-Length") {
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    if !has("Connection") {
        head.push_str("Connection: close\r\n");
    }
    head.push_str("\r\n");
    let mut wire = head.into_bytes();
    wire.extend_from_slice(body);
    wire
}

/// Parse an HTTP/1.1 response from any buffered reader.
pub fn read_response<R: BufRead>(
    reader: &mut R,
    head_request: bool,
) -> Result<HttpResponse, String> {
    let status_line = read_line(reader)?;
    let mut parts = status_line.splitn(3, ' ');
    let version = parts.next().unwrap_or("");
    if !version.starts_with("HTTP/") {
        return Err(format!("malformed status line '{status_line}'"));
    }
    let status: u16 = parts
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("malformed status line '{status_line}'"))?;
    let reason = parts.next().unwrap_or("").trim().to_string();

    let mut headers = Vec::new();
    loop {
        let line = read_line(reader)?;
        if line.is_empty() {
            break;
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| format!("malformed header line '{line}'"))?;
        headers.push((name.trim().to_string(), value.trim().to_string()));
    }

    let find = |name: &str| {
        headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    };

    // HEAD responses and 1xx/204/304 have no body by definition.
    let bodyless = head_request || status < 200 || status == 204 || status == 304;
    let body = if bodyless {
        Vec::new()
    } else if find("Transfer-Encoding")
        .map(|v| v.to_ascii_lowercase().contains("chunked"))
        .unwrap_or(false)
    {
        read_chunked(reader)?
    } else if let Some(len) = find("Content-Length") {
        let len: usize = len
            .trim()
            .parse()
            .map_err(|_| format!("invalid Content-Length '{len}'"))?;
        let mut buf = vec![0u8; len];
        reader
            .read_exact(&mut buf)
            .map_err(|e| io_context("truncated body", e))?;
        buf
    } else {
        // No framing: read until the server closes the connection.
        let mut buf = Vec::new();
        reader
            .read_to_end(&mut buf)
            .map_err(|e| io_context("reading body", e))?;
        buf
    };

    Ok(HttpResponse {
        status,
        reason,
        headers,
        body,
        final_url: String::new(),
    })
}

/// Read one CRLF-terminated line (tolerates bare LF).
fn read_line<R: BufRead>(reader: &mut R) -> Result<String, String> {
    let mut line = String::new();
    let n = reader
        .read_line(&mut line)
        .map_err(|e| io_context("reading response", e))?;
    if n == 0 {
        return Err("connection closed mid-response".into());
    }
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

/// Decode a chunked transfer-encoded body, including trailer skipping.
fn read_chunked<R: BufRead>(reader: &mut R) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    loop {
        let size_line = read_line(reader)?;
        let size_text = size_line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_text, 16)
            .map_err(|_| format!("invalid chunk size '{size_line}'"))?;
        if size == 0 {
            // Consume optional trailers until the final blank line.
            loop {
                if read_line(reader)?.is_empty() {
                    break;
                }
            }
            return Ok(body);
        }
        let mut chunk = vec![0u8; size];
        reader
            .read_exact(&mut chunk)
            .map_err(|e| io_context("truncated chunk", e))?;
        body.extend_from_slice(&chunk);
        read_line(reader)?; // trailing CRLF after each chunk
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn parse(raw: &str) -> HttpResponse {
        read_response(&mut Cursor::new(raw.as_bytes()), false).expect("parse failed")
    }

    #[test]
    fn read_timeouts_render_as_timed_out_not_errno() {
        // A read timeout surfaces as WouldBlock on Linux ("Resource
        // temporarily unavailable, os error 11") and TimedOut elsewhere;
        // users must see "timed out", not the raw errno text.
        use std::io::{Error, ErrorKind};
        for kind in [ErrorKind::WouldBlock, ErrorKind::TimedOut] {
            let msg = io_context("reading response", Error::from(kind));
            assert_eq!(msg, "reading response: timed out (see --timeout)");
        }
        let other = io_context(
            "reading response",
            Error::new(ErrorKind::ConnectionReset, "boom"),
        );
        assert!(other.starts_with("reading response: ") && other.contains("boom"));
    }

    #[test]
    fn parses_content_length_response() {
        let r =
            parse("HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 5\r\n\r\nhello");
        assert_eq!(r.status, 200);
        assert_eq!(r.reason, "OK");
        assert_eq!(r.body_text(), "hello");
        assert_eq!(r.header("content-type"), Some("text/plain"));
    }

    #[test]
    fn parses_chunked_response_with_extensions_and_trailers() {
        let r = parse(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4;ext=1\r\nWiki\r\n5\r\npedia\r\n0\r\nX-Trailer: t\r\n\r\n",
        );
        assert_eq!(r.body_text(), "Wikipedia");
    }

    #[test]
    fn parses_read_to_eof_body() {
        let r = parse("HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nstream until close");
        assert_eq!(r.body_text(), "stream until close");
    }

    #[test]
    fn no_content_and_head_have_empty_bodies() {
        let r = parse("HTTP/1.1 204 No Content\r\nContent-Length: 99\r\n\r\n");
        assert!(r.body.is_empty());
        let r = read_response(
            &mut Cursor::new(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n".as_slice()),
            true,
        )
        .unwrap();
        assert!(r.body.is_empty());
    }

    #[test]
    fn truncated_and_malformed_responses_are_errors() {
        let raw = "HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nshort";
        let err = read_response(&mut Cursor::new(raw.as_bytes()), false).unwrap_err();
        assert!(err.contains("truncated"), "got: {err}");
        let err =
            read_response(&mut Cursor::new(b"garbage\r\n\r\n".as_slice()), false).unwrap_err();
        assert!(err.contains("malformed"), "got: {err}");
    }

    #[test]
    fn content_type_splits_mime_and_charset() {
        let r = parse(
            "HTTP/1.1 200 OK\r\nContent-Type: application/JSON; charset=\"UTF-8\"\r\nContent-Length: 0\r\n\r\n",
        );
        assert_eq!(
            r.content_type(),
            ("application/json".to_string(), "utf-8".to_string())
        );
    }

    #[test]
    fn build_request_sets_defaults_and_respects_overrides() {
        let u = url::parse("http://example.test:8080/api?x=1").unwrap();
        let wire = build_request(
            "POST",
            &u,
            &[("Host".into(), "override.test".into())],
            b"{}",
        );
        let text = String::from_utf8(wire).unwrap();
        assert!(text.starts_with("POST /api?x=1 HTTP/1.1\r\n"));
        assert!(text.contains("Host: override.test\r\n"));
        assert!(
            !text.contains("Host: example.test"),
            "default Host must yield"
        );
        assert!(text.contains("Content-Length: 2\r\n"));
        assert!(text.contains("Connection: close\r\n"));
        assert!(text.ends_with("\r\n\r\n{}"));
        // A bodyless GET must not advertise a Content-Length.
        let u = url::parse("http://example.test/").unwrap();
        let text = String::from_utf8(build_request("GET", &u, &[], b"")).unwrap();
        assert!(!text.contains("Content-Length"));
        assert!(text.contains("Host: example.test\r\n"));
        assert!(text.contains(&format!(
            "User-Agent: reqrun/{}\r\n",
            env!("CARGO_PKG_VERSION")
        )));
    }
}
