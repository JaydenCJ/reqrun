//! Executes parsed `.http` files: resolves variables, sends requests in file
//! order, runs response handlers, and collects per-request results.

use crate::http;
use crate::parser::{self, Body, HttpFile, Request};
use crate::script;
use crate::url;
use crate::vars::{self, Vars};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Options assembled by the CLI layer.
#[derive(Debug, Clone)]
pub struct Options {
    /// Environment name from http-client.env.json (`--env`).
    pub env: Option<String>,
    /// Explicit env file path (`--env-file`); default is discovered next to
    /// each `.http` file.
    pub env_file: Option<PathBuf>,
    /// `--var name=value` overrides.
    pub overrides: Vec<(String, String)>,
    /// `--request` name filters (empty = run everything).
    pub filter: Vec<String>,
    pub timeout: Duration,
    pub fail_fast: bool,
    /// `--strict`: a request without assertions fails on status >= 400.
    pub strict: bool,
    /// `--dry-run`: resolve and render, but do not connect.
    pub dry_run: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            env: None,
            env_file: None,
            overrides: Vec::new(),
            filter: Vec::new(),
            timeout: Duration::from_secs(30),
            fail_fast: false,
            strict: false,
            dry_run: false,
        }
    }
}

/// Outcome of one request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Passed,
    Failed,
    /// Could not even exchange HTTP (connect error, bad URL, script error...).
    Error,
    /// Not run: filtered out earlier by `--fail-fast`.
    Skipped,
}

#[derive(Debug)]
pub struct RequestResult {
    pub name: String,
    pub method: String,
    pub url: String,
    pub status: Status,
    pub http_status: Option<u16>,
    pub http_reason: String,
    pub duration: Duration,
    pub assertions: Vec<script::Assertion>,
    /// Error text when `status == Error`.
    pub error: Option<String>,
    pub logs: Vec<String>,
    /// Rendered wire request (dry-run) or response head (verbose).
    pub rendered: Option<String>,
}

#[derive(Debug)]
pub struct FileResult {
    pub path: String,
    pub results: Vec<RequestResult>,
}

impl FileResult {
    pub fn counts(&self) -> (usize, usize, usize, usize) {
        let mut c = (0, 0, 0, 0);
        for r in &self.results {
            match r.status {
                Status::Passed => c.0 += 1,
                Status::Failed => c.1 += 1,
                Status::Error => c.2 += 1,
                Status::Skipped => c.3 += 1,
            }
        }
        c
    }
}

/// Run every file. `Err` is a setup-level problem (unreadable file, parse
/// error, bad env) and maps to exit code 2; per-request failures live inside
/// the results and map to exit code 1.
pub fn run_files(paths: &[PathBuf], options: &Options) -> Result<Vec<FileResult>, String> {
    // client.global.set values persist across files within one invocation,
    // so a login.http can feed tokens to the files after it.
    let mut globals: HashMap<String, String> = HashMap::new();
    let mut out = Vec::new();
    let mut abort = false;
    for path in paths {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let file = parser::parse(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        if file.requests.is_empty() {
            return Err(format!("{}: no requests found", path.display()));
        }
        let mut vars = build_vars(path, options, &globals)?;
        vars.set_file_vars(&file.variables)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let result = run_file(path, &file, &mut vars, options, &mut globals, &mut abort)?;
        out.push(result);
    }
    Ok(out)
}

/// List request names without running anything (`--list`).
pub fn list_requests(paths: &[PathBuf]) -> Result<Vec<(String, Vec<String>)>, String> {
    let mut out = Vec::new();
    for path in paths {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let file = parser::parse(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        let names = file
            .requests
            .iter()
            .enumerate()
            .map(|(i, r)| format!("{} {}", display_name(r, i), r.method))
            .collect();
        out.push((path.display().to_string(), names));
    }
    Ok(out)
}

fn build_vars(
    http_path: &Path,
    options: &Options,
    globals: &HashMap<String, String>,
) -> Result<Vars, String> {
    let mut vars = Vars::new();
    if let Some(env_name) = &options.env {
        let env_file = match &options.env_file {
            Some(p) => p.clone(),
            None => {
                let dir = http_path.parent().unwrap_or_else(|| Path::new("."));
                let candidate = dir.join("http-client.env.json");
                if !candidate.is_file() {
                    return Err(format!(
                        "--env {env_name}: no http-client.env.json next to {} (or pass --env-file)",
                        http_path.display()
                    ));
                }
                candidate
            }
        };
        vars::load_environment(&env_file, env_name, &mut vars)?;
    }
    for (k, v) in globals {
        vars.set_global(k, v);
    }
    for (k, v) in &options.overrides {
        vars.set_override(k, v);
    }
    Ok(vars)
}

fn display_name(request: &Request, index: usize) -> String {
    request
        .name
        .clone()
        .unwrap_or_else(|| format!("request #{}", index + 1))
}

fn run_file(
    path: &Path,
    file: &HttpFile,
    vars: &mut Vars,
    options: &Options,
    globals: &mut HashMap<String, String>,
    abort: &mut bool,
) -> Result<FileResult, String> {
    let base_dir = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let mut results = Vec::new();
    for (index, request) in file.requests.iter().enumerate() {
        let name = display_name(request, index);
        if !options.filter.is_empty() && !options.filter.iter().any(|f| f == &name) {
            continue;
        }
        if *abort {
            results.push(RequestResult {
                name,
                method: request.method.clone(),
                url: request.url.clone(),
                status: Status::Skipped,
                http_status: None,
                http_reason: String::new(),
                duration: Duration::ZERO,
                assertions: Vec::new(),
                error: None,
                logs: Vec::new(),
                rendered: None,
            });
            continue;
        }
        let result = run_request(request, name, &base_dir, vars, options, globals);
        if options.fail_fast && matches!(result.status, Status::Failed | Status::Error) {
            *abort = true;
        }
        results.push(result);
    }
    if !options.filter.is_empty() && results.is_empty() {
        return Err(format!(
            "{}: no request matches --request {}",
            path.display(),
            options.filter.join(", ")
        ));
    }
    Ok(FileResult {
        path: path.display().to_string(),
        results,
    })
}

fn run_request(
    request: &Request,
    name: String,
    base_dir: &Path,
    vars: &mut Vars,
    options: &Options,
    globals: &mut HashMap<String, String>,
) -> RequestResult {
    let mut result = RequestResult {
        name,
        method: request.method.clone(),
        url: request.url.clone(),
        status: Status::Error,
        http_status: None,
        http_reason: String::new(),
        duration: Duration::ZERO,
        assertions: Vec::new(),
        error: None,
        logs: Vec::new(),
        rendered: None,
    };
    match execute(request, base_dir, vars, options, globals, &mut result) {
        Ok(()) => {}
        Err(e) => {
            result.status = Status::Error;
            result.error = Some(e);
        }
    }
    result
}

/// The fallible core of one request; fills `result` in place.
fn execute(
    request: &Request,
    base_dir: &Path,
    vars: &mut Vars,
    options: &Options,
    globals: &mut HashMap<String, String>,
    result: &mut RequestResult,
) -> Result<(), String> {
    // 1. Resolve variables everywhere they may appear. In dry-run, values a
    // response handler would have captured (e.g. a token) cannot exist yet,
    // so unknown variables render literally instead of erroring.
    let resolve = |vars: &mut Vars, text: &str| -> Result<String, String> {
        if options.dry_run {
            vars.substitute_lenient(text)
        } else {
            vars.substitute(text)
        }
    };
    let raw_url = resolve(vars, &request.url)?;
    result.url = raw_url.clone();
    let target = url::parse(&raw_url)?;
    let mut headers = Vec::with_capacity(request.headers.len());
    for (name, value) in &request.headers {
        headers.push((name.clone(), resolve(vars, value)?));
    }
    let body: Vec<u8> = match &request.body {
        None => Vec::new(),
        Some(Body::Inline(text)) => resolve(vars, text)?.into_bytes(),
        Some(Body::FromFile { path, substitute }) => {
            let resolved = resolve(vars, path)?;
            let full = base_dir.join(&resolved);
            let bytes = std::fs::read(&full)
                .map_err(|e| format!("cannot read body file {}: {e}", full.display()))?;
            if *substitute {
                let text = String::from_utf8(bytes).map_err(|_| {
                    format!(
                        "body file {} is not UTF-8; use '<' instead of '<@'",
                        full.display()
                    )
                })?;
                resolve(vars, &text)?.into_bytes()
            } else {
                bytes
            }
        }
    };

    // 2. Dry run: render the wire request and stop before connecting.
    if options.dry_run {
        let wire = http::build_request(&request.method, &target, &headers, &body);
        result.rendered = Some(String::from_utf8_lossy(&wire).into_owned());
        result.status = Status::Passed;
        return Ok(());
    }

    // 3. Send.
    let started = Instant::now();
    let response = http::send(
        &request.method,
        &target,
        &headers,
        &body,
        options.timeout,
        request.no_redirect,
    )?;
    result.duration = started.elapsed();
    result.http_status = Some(response.status);
    result.http_reason = response.reason.clone();
    result.rendered = Some(format!(
        "HTTP/1.1 {} {}\n{}",
        response.status,
        response.reason,
        response
            .headers
            .iter()
            .map(|(n, v)| format!("{n}: {v}"))
            .collect::<Vec<_>>()
            .join("\n")
    ));

    // 4. Save the body if requested (path resolved against the .http file).
    if let Some((save_path, force)) = &request.save_response {
        let resolved = vars.substitute(save_path)?;
        let full = base_dir.join(&resolved);
        if full.exists() && !force {
            return Err(format!(
                "refusing to overwrite {} (use '>>!' to force)",
                full.display()
            ));
        }
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        std::fs::write(&full, &response.body)
            .map_err(|e| format!("cannot write {}: {e}", full.display()))?;
    }

    // 5. Run the response handler, if any.
    if let Some(handler) = &request.handler {
        let outcome = script::run(&handler.script, &response)
            .map_err(|e| format!("response handler (line {}): {e}", handler.line))?;
        for (name, value) in &outcome.globals {
            vars.set_global(name, value);
            globals.insert(name.clone(), value.clone());
        }
        result.status = if outcome.failed() > 0 {
            Status::Failed
        } else {
            Status::Passed
        };
        result.logs = outcome.logs;
        result.assertions = outcome.assertions;
        return Ok(());
    }

    // 6. No handler: pass unless --strict and the status says otherwise.
    if options.strict && response.status >= 400 {
        result.status = Status::Failed;
        result.assertions.push(script::Assertion {
            test: None,
            passed: false,
            message: format!(
                "strict mode: expected status < 400, got {} {}",
                response.status, response.reason
            ),
        });
    } else {
        result.status = Status::Passed;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Runner tests that need no sockets: dry-run rendering, filtering,
    //! variable plumbing and error surfacing. Live-socket behavior is covered
    //! by the integration tests in `tests/cli.rs`.
    use super::*;
    use std::io::Write;

    fn write_file(dir: &Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        path
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("reqrun-runner-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn dry() -> Options {
        Options {
            dry_run: true,
            ..Options::default()
        }
    }

    #[test]
    fn dry_run_renders_the_wire_request() {
        let dir = tmpdir("dry");
        let f = write_file(
            &dir,
            "a.http",
            "POST http://example.test/login\nContent-Type: application/json\n\n{\"u\":1}\n",
        );
        let results = run_files(&[f], &dry()).unwrap();
        let rendered = results[0].results[0].rendered.as_ref().unwrap();
        assert!(rendered.starts_with("POST /login HTTP/1.1\r\n"));
        assert!(rendered.contains("Host: example.test\r\n"));
        assert!(rendered.contains("Content-Type: application/json\r\n"));
        assert!(rendered.ends_with("{\"u\":1}"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn file_variables_flow_into_urls_headers_and_bodies() {
        let dir = tmpdir("vars");
        let f = write_file(
            &dir,
            "a.http",
            concat!(
                "@host = example.test\n",
                "@tok = t-99\n",
                "POST http://{{host}}/x\n",
                "Authorization: Bearer {{tok}}\n",
                "\n",
                "token={{tok}}\n",
            ),
        );
        let results = run_files(&[f], &dry()).unwrap();
        let rendered = results[0].results[0].rendered.as_ref().unwrap();
        assert!(rendered.contains("Authorization: Bearer t-99"));
        assert!(rendered.ends_with("token=t-99"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cli_var_overrides_file_variable() {
        let dir = tmpdir("override");
        let f = write_file(
            &dir,
            "a.http",
            "@who = file\nGET http://example.test/{{who}}\n",
        );
        let mut options = dry();
        options
            .overrides
            .push(("who".to_string(), "cli".to_string()));
        let results = run_files(&[f], &options).unwrap();
        assert_eq!(results[0].results[0].url, "http://example.test/cli");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn request_filter_selects_by_name() {
        let dir = tmpdir("filter");
        let f = write_file(
            &dir,
            "a.http",
            "# @name one\nGET http://example.test/1\n###\n# @name two\nGET http://example.test/2\n",
        );
        let mut options = dry();
        options.filter.push("two".to_string());
        let results = run_files(std::slice::from_ref(&f), &options).unwrap();
        assert_eq!(results[0].results.len(), 1);
        assert_eq!(results[0].results[0].name, "two");

        options.filter = vec!["missing".to_string()];
        let err = run_files(&[f], &options).unwrap_err();
        assert!(err.contains("no request matches"), "got: {err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn body_from_file_is_read_and_optionally_substituted() {
        let dir = tmpdir("bodyfile");
        write_file(&dir, "payload.json", "{\"v\": \"{{version}}\"}");
        let raw = write_file(
            &dir,
            "raw.http",
            "@version = 9\nPOST http://example.test/a\n\n< payload.json\n",
        );
        let sub = write_file(
            &dir,
            "sub.http",
            "@version = 9\nPOST http://example.test/a\n\n<@ payload.json\n",
        );
        let results = run_files(&[raw, sub], &dry()).unwrap();
        let raw_body = results[0].results[0].rendered.as_ref().unwrap();
        assert!(
            raw_body.ends_with("{\"v\": \"{{version}}\"}"),
            "raw form must not substitute"
        );
        let sub_body = results[1].results[0].rendered.as_ref().unwrap();
        assert!(sub_body.ends_with("{\"v\": \"9\"}"), "<@ must substitute");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn request_level_errors_carry_context() {
        let dir = tmpdir("reqerr");
        // A missing body file marks just that request as errored instead of
        // aborting the whole run.
        let f = write_file(
            &dir,
            "a.http",
            "POST http://example.test/a\n\n< gone.json\n###\nGET http://example.test/{{later}}\n",
        );
        let results = run_files(&[f], &dry()).unwrap();
        let r = &results[0].results[0];
        assert_eq!(r.status, Status::Error);
        assert!(r.error.as_ref().unwrap().contains("gone.json"));
        // Dry-run leaves handler-captured variables literal instead of failing.
        let r = &results[0].results[1];
        assert_eq!(r.status, Status::Passed);
        assert!(r.url.contains("{{later}}"));

        // A real run resolves strictly: undefined variables error before any
        // socket is opened.
        let g = write_file(&dir, "b.http", "GET http://example.test/{{nope}}\n");
        let results = run_files(&[g], &Options::default()).unwrap();
        let r = &results[0].results[0];
        assert_eq!(r.status, Status::Error);
        assert!(r.error.as_ref().unwrap().contains("nope"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn setup_errors_abort_the_run() {
        let dir = tmpdir("setup");
        let bad = write_file(&dir, "bad.http", "GET http://example.test/a\nBROKEN\n");
        let err = run_files(&[bad], &dry()).unwrap_err();
        assert!(
            err.contains("bad.http") && err.contains("line 2"),
            "got: {err}"
        );
        let empty = write_file(&dir, "empty.http", "# only a comment\n");
        let err = run_files(&[empty], &dry()).unwrap_err();
        assert!(err.contains("no requests"), "got: {err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn env_file_discovery_and_selection() {
        let dir = tmpdir("env");
        let f = write_file(&dir, "a.http", "GET {{base}}/health\n");
        let mut options = dry();
        options.env = Some("prod".to_string());
        // No env file next to the .http file yet: a clear setup error.
        let err = run_files(std::slice::from_ref(&f), &options).unwrap_err();
        assert!(err.contains("http-client.env.json"), "got: {err}");
        write_file(
            &dir,
            "http-client.env.json",
            r#"{"dev": {"base": "http://127.0.0.1:1"}, "prod": {"base": "http://example.test"}}"#,
        );
        let results = run_files(&[f], &options).unwrap();
        assert_eq!(results[0].results[0].url, "http://example.test/health");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn https_url_is_an_error_not_a_crash() {
        let dir = tmpdir("https");
        let f = write_file(&dir, "a.http", "GET https://example.test/\n");
        let mut options = dry();
        options.dry_run = false; // URL parsing happens before any connect
        options.timeout = Duration::from_millis(50);
        let results = run_files(&[f], &options).unwrap();
        let r = &results[0].results[0];
        assert_eq!(r.status, Status::Error);
        assert!(r.error.as_ref().unwrap().contains("v0.1.0"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn list_requests_reports_names_and_methods() {
        let dir = tmpdir("list");
        let f = write_file(
            &dir,
            "a.http",
            "# @name login\nPOST http://example.test/l\n###\nGET http://example.test/x\n",
        );
        let listing = list_requests(&[f]).unwrap();
        assert_eq!(listing[0].1, vec!["login POST", "request #2 GET"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
