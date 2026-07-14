//! Integration tests: drive the compiled `reqrun` binary end to end against
//! a minimal in-process HTTP server bound to 127.0.0.1:0 (ephemeral port).
//! Fully offline and deterministic — no DNS, no external services, no sleeps.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Start a canned API server on an ephemeral loopback port; returns the port.
/// The accept loop runs in a detached thread for the life of the test process.
fn start_server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            std::thread::spawn(move || {
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .ok();
                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                // Read the head (and, thanks to Content-Length, the body).
                while !contains_head_end(&buf) {
                    match stream.read(&mut chunk) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let head_end = head_end(&buf);
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
                let request_line = head.lines().next().unwrap_or("").to_string();
                let auth = head
                    .lines()
                    .find_map(|l| l.strip_prefix("Authorization: "))
                    .unwrap_or("")
                    .to_string();
                let response = route(&request_line, &auth, &body);
                stream.write_all(response.as_bytes()).ok();
            });
        }
    });
    port
}

fn contains_head_end(buf: &[u8]) -> bool {
    buf.windows(4).any(|w| w == b"\r\n\r\n")
}

fn head_end(buf: &[u8]) -> usize {
    buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap()
}

fn route(request_line: &str, auth: &str, body: &[u8]) -> String {
    let respond = |status: &str, ctype: &str, body: &str| {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    };
    match request_line {
        "GET /health HTTP/1.1" => respond("200 OK", "application/json", r#"{"status":"ok","version":"2.4.1"}"#),
        "POST /login HTTP/1.1" => {
            if String::from_utf8_lossy(body).contains("\"admin\"") {
                respond("200 OK", "application/json", r#"{"token":"tok-123","ttl":3600}"#)
            } else {
                respond("401 Unauthorized", "application/json", r#"{"error":"bad credentials"}"#)
            }
        }
        "GET /me HTTP/1.1" => {
            if auth == "Bearer tok-123" {
                respond("200 OK", "application/json", r#"{"user":"amy","roles":["admin"]}"#)
            } else {
                respond("401 Unauthorized", "application/json", r#"{"error":"missing token"}"#)
            }
        }
        "GET /redirect HTTP/1.1" => {
            "HTTP/1.1 302 Found\r\nLocation: /health\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
        }
        _ => respond("404 Not Found", "text/plain", "no such route"),
    }
}

// ------------------------------------------------------------- helpers ----

fn workdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("reqrun-cli-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(dir: &Path, name: &str, content: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, content).unwrap();
    path
}

fn reqrun(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_reqrun"))
        .args(args)
        .env("NO_COLOR", "1")
        .output()
        .expect("run reqrun")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

// --------------------------------------------------------------- tests ----

#[test]
fn passing_file_exits_zero_with_pass_report() {
    let port = start_server();
    let dir = workdir("pass");
    let file = write(
        &dir,
        "health.http",
        &format!(
            concat!(
                "# @name health\n",
                "GET http://127.0.0.1:{port}/health\n",
                "Accept: application/json\n",
                "\n",
                "> {{%\n",
                "  client.test(\"service is healthy\", function () {{\n",
                "    client.assert(response.status === 200, \"status\");\n",
                "    client.assert(response.body.status === 'ok', \"body\");\n",
                "  }});\n",
                "%}}\n",
            ),
            port = port
        ),
    );
    let out = reqrun(&[file.to_str().unwrap()]);
    let text = stdout(&out);
    assert!(
        out.status.success(),
        "stdout: {text}\nstderr: {}",
        stderr(&out)
    );
    assert!(text.contains("PASS  health"), "got: {text}");
    assert!(text.contains("[2/2 checks]"), "got: {text}");
    assert!(
        text.contains("1 request(s): 1 passed, 0 failed — 2 check(s)"),
        "got: {text}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn failing_assertion_exits_one_with_details() {
    let port = start_server();
    let dir = workdir("fail");
    let file = write(
        &dir,
        "wrong.http",
        &format!(
            "GET http://127.0.0.1:{port}/health\n\n> {{% client.assert(response.body.version === '9.9.9', \"version pinned\"); %}}\n"
        ),
    );
    let out = reqrun(&[file.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    let text = stdout(&out);
    assert!(text.contains("FAIL"), "got: {text}");
    assert!(text.contains("version pinned"), "got: {text}");
    assert!(
        text.contains("response.body.version === '9.9.9'"),
        "got: {text}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn token_captured_by_global_set_chains_into_next_request() {
    let port = start_server();
    let dir = workdir("chain");
    let file = write(
        &dir,
        "flow.http",
        &format!(
            concat!(
                "### login\n",
                "POST http://127.0.0.1:{port}/login\n",
                "Content-Type: application/json\n",
                "\n",
                "{{\"user\": \"admin\"}}\n",
                "\n",
                "> {{% client.assert(response.status === 200); client.global.set(\"token\", response.body.token); %}}\n",
                "\n",
                "### whoami\n",
                "GET http://127.0.0.1:{port}/me\n",
                "Authorization: Bearer {{{{token}}}}\n",
                "\n",
                "> {{% client.assert(response.body.user === 'amy'); client.assert(response.body.roles.includes('admin')); %}}\n",
            ),
            port = port
        ),
    );
    let out = reqrun(&[file.to_str().unwrap()]);
    let text = stdout(&out);
    assert!(
        out.status.success(),
        "stdout: {text}\nstderr: {}",
        stderr(&out)
    );
    assert!(text.contains("PASS  login"), "got: {text}");
    assert!(text.contains("PASS  whoami"), "got: {text}");
    assert!(text.contains("2 request(s): 2 passed"), "got: {text}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn junit_report_is_written_for_ci() {
    let port = start_server();
    let dir = workdir("junit");
    let file = write(
        &dir,
        "mix.http",
        &format!(
            concat!(
                "# @name ok\n",
                "GET http://127.0.0.1:{port}/health\n",
                "\n",
                "> {{% client.assert(response.status === 200); %}}\n",
                "\n",
                "### broken\n",
                "GET http://127.0.0.1:{port}/health\n",
                "\n",
                "> {{% client.assert(response.status === 500, \"expected an error page\"); %}}\n",
            ),
            port = port
        ),
    );
    let report = dir.join("report.xml");
    let out = reqrun(&[file.to_str().unwrap(), "--report", report.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    let xml = std::fs::read_to_string(&report).unwrap();
    assert!(
        xml.contains("tests=\"2\" failures=\"1\" errors=\"0\""),
        "got: {xml}"
    );
    assert!(xml.contains("<testcase name=\"ok\""), "got: {xml}");
    assert!(
        xml.contains("<failure message=\"expected an error page"),
        "got: {xml}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn redirects_follow_by_default_and_stop_with_no_redirect() {
    let port = start_server();
    let dir = workdir("redir");
    let file = write(
        &dir,
        "redir.http",
        &format!(
            concat!(
                "# @name followed\n",
                "GET http://127.0.0.1:{port}/redirect\n",
                "\n",
                "> {{% client.assert(response.status === 200); client.assert(response.body.status === 'ok'); %}}\n",
                "\n",
                "###\n",
                "# @name raw\n",
                "# @no-redirect\n",
                "GET http://127.0.0.1:{port}/redirect\n",
                "\n",
                "> {{% client.assert(response.status === 302); client.assert(response.headers.valueOf('Location') === '/health'); %}}\n",
            ),
            port = port
        ),
    );
    let out = reqrun(&[file.to_str().unwrap()]);
    let text = stdout(&out);
    assert!(
        out.status.success(),
        "stdout: {text}\nstderr: {}",
        stderr(&out)
    );
    assert!(
        text.contains("PASS  followed") && text.contains("PASS  raw"),
        "got: {text}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn strict_mode_fails_a_404_and_fail_fast_skips_the_rest() {
    let port = start_server();
    let dir = workdir("strict");
    let file = write(
        &dir,
        "strict.http",
        &format!(
            concat!(
                "# @name gone\n",
                "GET http://127.0.0.1:{port}/nope\n",
                "###\n",
                "# @name never-runs\n",
                "GET http://127.0.0.1:{port}/health\n",
            ),
            port = port
        ),
    );
    // Without --strict both pass (no assertions, responses received).
    let out = reqrun(&[file.to_str().unwrap()]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    // With --strict + --fail-fast the 404 fails and the rest is skipped.
    let out = reqrun(&[file.to_str().unwrap(), "--strict", "--fail-fast"]);
    assert_eq!(out.status.code(), Some(1));
    let text = stdout(&out);
    assert!(text.contains("FAIL  gone"), "got: {text}");
    assert!(text.contains("SKIP  never-runs"), "got: {text}");
    assert!(text.contains("404"), "got: {text}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn connection_refused_is_an_error_exit_one() {
    // Port 1 on loopback is essentially guaranteed closed; connect fails fast.
    let dir = workdir("refused");
    let file = write(&dir, "down.http", "GET http://127.0.0.1:1/health\n");
    let out = reqrun(&[file.to_str().unwrap(), "--timeout", "2s"]);
    assert_eq!(out.status.code(), Some(1));
    let text = stdout(&out);
    assert!(text.contains("ERROR"), "got: {text}");
    assert!(text.contains("cannot connect"), "got: {text}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn version_help_and_usage_errors() {
    let out = reqrun(&["--version"]);
    assert_eq!(
        stdout(&out).trim(),
        format!("reqrun {}", env!("CARGO_PKG_VERSION"))
    );
    assert!(out.status.success());
    let out = reqrun(&["--help"]);
    assert!(stdout(&out).contains("USAGE:"));
    assert!(stdout(&out).contains("--report"));
    assert!(out.status.success());

    let out = reqrun(&["--frobnicate"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("--frobnicate"));

    let dir = workdir("badfile");
    let file = write(&dir, "bad.http", "GET http://example.test/\nNOT A HEADER\n");
    let out = reqrun(&[file.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("line 2"), "got: {}", stderr(&out));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn list_and_dry_run_need_no_server() {
    let dir = workdir("offline");
    let file = write(
        &dir,
        "api.http",
        concat!(
            "@base = http://example.test\n",
            "# @name ping\n",
            "GET {{base}}/ping\n",
            "###\n",
            "# @name create\n",
            "POST {{base}}/items\n",
            "Content-Type: application/json\n",
            "\n",
            "{\"name\": \"widget\"}\n",
        ),
    );
    let out = reqrun(&["--list", file.to_str().unwrap()]);
    assert!(out.status.success());
    let text = stdout(&out);
    assert!(
        text.contains("ping GET") && text.contains("create POST"),
        "got: {text}"
    );

    let out = reqrun(&["--dry-run", file.to_str().unwrap()]);
    assert!(out.status.success());
    let text = stdout(&out);
    assert!(text.contains("POST /items HTTP/1.1"), "got: {text}");
    assert!(text.contains("Host: example.test"), "got: {text}");
    assert!(text.contains("{\"name\": \"widget\"}"), "got: {text}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn environment_selection_via_env_file() {
    let port = start_server();
    let dir = workdir("envsel");
    write(
        &dir,
        "http-client.env.json",
        &format!(
            r#"{{"local": {{"base": "http://127.0.0.1:{port}"}}, "prod": {{"base": "http://example.test"}}}}"#
        ),
    );
    let file = write(
        &dir,
        "env.http",
        "GET {{base}}/health\n\n> {% client.assert(response.body.status === 'ok'); %}\n",
    );
    let out = reqrun(&[file.to_str().unwrap(), "--env", "local"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));

    // Selecting a missing environment is a setup error (exit 2) that lists
    // what is available.
    let out = reqrun(&[file.to_str().unwrap(), "--env", "staging"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains("local, prod"),
        "got: {}",
        stderr(&out)
    );
    std::fs::remove_dir_all(&dir).unwrap();
}
