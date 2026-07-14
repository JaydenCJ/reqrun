//! Tiny std-only demo API used by scripts/smoke.sh. Compiled with plain
//! `rustc`, binds 127.0.0.1:39642, serves the routes examples/quickstart.http
//! expects, and exits after --max-requests connections.
//! Not part of the reqrun binary or its test suite.

use std::io::{Read, Write};
use std::net::TcpListener;

fn respond(status: &str, ctype: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn main() {
    let max_requests: usize = std::env::args()
        .skip_while(|a| a != "--max-requests")
        .nth(1)
        .and_then(|n| n.parse().ok())
        .unwrap_or(16);
    let listener = TcpListener::bind("127.0.0.1:39642").expect("bind 127.0.0.1:39642");
    println!("READY");
    for stream in listener.incoming().take(max_requests) {
        let Ok(mut stream) = stream else { continue };
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .ok();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        // Read head; Content-Length bodies arrive in the same segments here.
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        let head_end = match buf.windows(4).position(|w| w == b"\r\n\r\n") {
            Some(p) => p,
            None => continue,
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
        let content_length = head
            .lines()
            .find_map(|l| {
                l.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
            })
            .unwrap_or(0);
        let mut body = buf[head_end + 4..].to_vec();
        while body.len() < content_length {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => body.extend_from_slice(&chunk[..n]),
            }
        }
        let request_line = head.lines().next().unwrap_or("");
        let auth = head
            .lines()
            .find_map(|l| l.strip_prefix("Authorization: "))
            .unwrap_or("");
        let response = match request_line {
            "GET /health HTTP/1.1" => respond(
                "200 OK",
                "application/json",
                r#"{"status":"ok","version":"2.4.1"}"#,
            ),
            "POST /login HTTP/1.1" => {
                if String::from_utf8_lossy(&body).contains("\"admin\"") {
                    respond(
                        "200 OK",
                        "application/json",
                        r#"{"token":"tok-123","ttl":3600}"#,
                    )
                } else {
                    respond(
                        "401 Unauthorized",
                        "application/json",
                        r#"{"error":"bad credentials"}"#,
                    )
                }
            }
            "GET /me HTTP/1.1" => {
                if auth == "Bearer tok-123" {
                    respond(
                        "200 OK",
                        "application/json",
                        r#"{"user":"amy","roles":["admin"]}"#,
                    )
                } else {
                    respond(
                        "401 Unauthorized",
                        "application/json",
                        r#"{"error":"missing token"}"#,
                    )
                }
            }
            _ => respond("404 Not Found", "text/plain", "no such route"),
        };
        stream.write_all(response.as_bytes()).ok();
    }
}
