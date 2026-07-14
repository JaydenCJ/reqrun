//! Parser for JetBrains-style `.http` request files.
//!
//! The grammar is the one IntelliJ/WebStorm/VS Code REST-client users already
//! know: requests separated by `###`, `# @name` directives, `@var = value`
//! file variables, headers until a blank line, a body until the next
//! separator, `< file` body includes and `> {% ... %}` response handlers.
//! reqrun parses that format as-is — the whole point is that the file in the
//! repo keeps working in the editor.

/// A parsed `.http` file.
#[derive(Debug, Clone, Default)]
pub struct HttpFile {
    /// File-scope `@name = value` variables, in declaration order.
    pub variables: Vec<(String, String)>,
    pub requests: Vec<Request>,
}

/// One request block between `###` separators.
#[derive(Debug, Clone)]
pub struct Request {
    /// From `# @name` or the text after `###`; anonymous requests get a
    /// positional name like `request #2` at run time.
    pub name: Option<String>,
    /// 1-based line number of the request line (for error messages).
    pub line: usize,
    pub method: String,
    /// Raw URL, `{{variables}}` not yet substituted.
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Body>,
    /// `> {% ... %}` response handler script, with its starting line.
    pub handler: Option<Handler>,
    /// `# @no-redirect` directive: don't follow 3xx responses.
    pub no_redirect: bool,
    /// `>> path` / `>>! path`: save the response body (bool = force overwrite).
    pub save_response: Option<(String, bool)>,
}

/// Request body: inline text, or included from a file.
/// `FromFile { substitute: true }` is the `<@ file` form where `{{vars}}`
/// inside the file are resolved; plain `< file` sends bytes verbatim.
#[derive(Debug, Clone, PartialEq)]
pub enum Body {
    Inline(String),
    FromFile { path: String, substitute: bool },
}

#[derive(Debug, Clone)]
pub struct Handler {
    pub script: String,
    pub line: usize,
}

/// A parse failure, positioned for the user.
#[derive(Debug)]
pub struct ParseError {
    pub line: usize,
    pub message: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

const METHODS: [&str; 9] = [
    "GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS", "TRACE", "CONNECT",
];

/// Parse the text of a `.http` file.
pub fn parse(input: &str) -> Result<HttpFile, ParseError> {
    let mut file = HttpFile::default();
    let lines: Vec<&str> = input.lines().collect();
    let mut i = 0;

    // Pending request-level directives seen before the request line.
    let mut pending_name: Option<String> = None;
    let mut pending_no_redirect = false;

    while i < lines.len() {
        let raw = lines[i];
        let line = raw.trim();
        let lineno = i + 1;

        if line.is_empty() {
            i += 1;
            continue;
        }
        if let Some(rest) = line.strip_prefix("###") {
            // Separator; trailing text names the next request.
            let title = rest.trim();
            if !title.is_empty() {
                pending_name = Some(title.to_string());
            }
            i += 1;
            continue;
        }
        if is_comment(line) {
            let text = comment_text(line);
            if let Some(rest) = text.strip_prefix("@name") {
                let name = rest.trim_start_matches([' ', '=']).trim();
                if name.is_empty() {
                    return Err(err(lineno, "@name directive without a name"));
                }
                pending_name = Some(name.to_string());
            } else if text.trim() == "@no-redirect" {
                pending_no_redirect = true;
            }
            // All other comments (including @no-cookie-jar etc.) are ignored,
            // exactly like the editors ignore reqrun-unknown directives.
            i += 1;
            continue;
        }
        if let Some(rest) = line.strip_prefix('@') {
            // File variable: @name = value
            let (name, value) = rest
                .split_once('=')
                .ok_or_else(|| err(lineno, "file variable must look like '@name = value'"))?;
            let name = name.trim();
            if name.is_empty() || !is_ident(name) {
                return Err(err(lineno, "invalid file variable name"));
            }
            file.variables
                .push((name.to_string(), value.trim().to_string()));
            i += 1;
            continue;
        }

        // Anything else must start a request.
        let (request, next) = parse_request(&lines, i, pending_name.take(), pending_no_redirect)?;
        pending_no_redirect = false;
        file.requests.push(request);
        i = next;
    }
    Ok(file)
}

fn err(line: usize, message: impl Into<String>) -> ParseError {
    ParseError {
        line,
        message: message.into(),
    }
}

fn is_comment(line: &str) -> bool {
    line.starts_with('#') || line.starts_with("//")
}

fn comment_text(line: &str) -> &str {
    line.trim_start_matches(['#', '/']).trim_start()
}

fn is_ident(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '$'))
}

/// Parse one request starting at `lines[start]`; returns it plus the index of
/// the first line after the request block.
fn parse_request(
    lines: &[&str],
    start: usize,
    name: Option<String>,
    no_redirect: bool,
) -> Result<(Request, usize), ParseError> {
    let lineno = start + 1;
    let first = lines[start].trim();

    // Request line: `METHOD URL [HTTP/x.y]` or a bare URL (implicit GET).
    let mut parts = first.split_whitespace();
    let head = parts.next().unwrap();
    let (method, mut url) = if METHODS.contains(&head.to_ascii_uppercase().as_str()) {
        let url = parts
            .next()
            .ok_or_else(|| err(lineno, format!("{head} without a URL")))?;
        (head.to_ascii_uppercase(), url.to_string())
    } else if head.contains("://") || head.starts_with("{{") {
        ("GET".to_string(), head.to_string())
    } else {
        return Err(err(
            lineno,
            format!("expected a request line (METHOD URL), found '{first}'"),
        ));
    };
    // Optional protocol version token is accepted and ignored (reqrun always
    // speaks HTTP/1.1 on the wire).
    if let Some(extra) = parts.next() {
        if !extra.to_ascii_uppercase().starts_with("HTTP/") {
            return Err(err(lineno, format!("unexpected token '{extra}' after URL")));
        }
    }

    let mut i = start + 1;

    // Multi-line URL continuation: indented lines starting with ? or &.
    while i < lines.len() {
        let t = lines[i].trim();
        if (lines[i].starts_with(' ') || lines[i].starts_with('\t'))
            && (t.starts_with('?') || t.starts_with('&'))
        {
            url.push_str(t);
            i += 1;
        } else {
            break;
        }
    }

    // Headers until a blank line, a separator, or a handler.
    let mut headers = Vec::new();
    while i < lines.len() {
        let t = lines[i].trim();
        if t.is_empty() || t.starts_with("###") || t.starts_with("> {%") || t.starts_with(">>") {
            break;
        }
        if is_comment(t) {
            i += 1;
            continue;
        }
        let (name, value) = t
            .split_once(':')
            .ok_or_else(|| err(i + 1, format!("expected 'Header: value', found '{t}'")))?;
        let name = name.trim();
        if name.is_empty() || name.contains(char::is_whitespace) {
            return Err(err(i + 1, format!("invalid header name '{name}'")));
        }
        headers.push((name.to_string(), value.trim().to_string()));
        i += 1;
    }
    // Skip the blank line after headers, if present.
    if i < lines.len() && lines[i].trim().is_empty() {
        i += 1;
    }

    // Body: everything up to the next separator or handler/save directive.
    let mut body_lines: Vec<&str> = Vec::new();
    let mut body_from_file: Option<Body> = None;
    let mut handler: Option<Handler> = None;
    let mut save_response: Option<(String, bool)> = None;

    while i < lines.len() {
        let raw = lines[i];
        let t = raw.trim();
        if t.starts_with("###") {
            break;
        }
        if t.starts_with("> {%") {
            let (script, next) = collect_handler(lines, i)?;
            handler = Some(Handler {
                script,
                line: i + 1,
            });
            i = next;
            continue;
        }
        if let Some(rest) = t.strip_prefix("> ") {
            // External handler script file: recognized but unsupported.
            return Err(err(
                i + 1,
                format!(
                    "external response handler '{}' is not supported; inline it as > {{% ... %}}",
                    rest.trim()
                ),
            ));
        }
        if let Some(rest) = t.strip_prefix(">>!") {
            save_response = Some((rest.trim().to_string(), true));
            i += 1;
            continue;
        }
        if let Some(rest) = t.strip_prefix(">>") {
            save_response = Some((rest.trim().to_string(), false));
            i += 1;
            continue;
        }
        if t.starts_with("<>") {
            // Saved-response reference (editor diff feature) — no runtime meaning.
            i += 1;
            continue;
        }
        if handler.is_none() && body_from_file.is_none() && body_lines.is_empty() {
            if let Some(rest) = t.strip_prefix("<@") {
                body_from_file = Some(Body::FromFile {
                    path: rest.trim().to_string(),
                    substitute: true,
                });
                i += 1;
                continue;
            }
            if let Some(rest) = t.strip_prefix('<') {
                body_from_file = Some(Body::FromFile {
                    path: rest.trim().to_string(),
                    substitute: false,
                });
                i += 1;
                continue;
            }
        }
        if handler.is_some() || save_response.is_some() {
            if t.is_empty() {
                i += 1;
                continue;
            }
            return Err(err(
                i + 1,
                "unexpected content after the response handler; start a new request with ###",
            ));
        }
        body_lines.push(raw);
        i += 1;
    }

    let body = if let Some(from_file) = body_from_file {
        Some(from_file)
    } else {
        // Trim trailing blank lines, keep interior ones.
        while matches!(body_lines.last(), Some(l) if l.trim().is_empty()) {
            body_lines.pop();
        }
        if body_lines.is_empty() {
            None
        } else {
            Some(Body::Inline(body_lines.join("\n")))
        }
    };

    Ok((
        Request {
            name,
            line: lineno,
            method,
            url,
            headers,
            body,
            handler,
            no_redirect,
            save_response,
        },
        i,
    ))
}

/// Collect a `> {% ... %}` block (single- or multi-line); returns the script
/// text and the index after the closing `%}`.
fn collect_handler(lines: &[&str], start: usize) -> Result<(String, usize), ParseError> {
    let first = lines[start].trim().strip_prefix("> {%").unwrap();
    if let Some(inline) = first.trim_end().strip_suffix("%}") {
        return Ok((inline.trim().to_string(), start + 1));
    }
    let mut script = vec![first.trim_start().to_string()];
    let mut i = start + 1;
    while i < lines.len() {
        let t = lines[i].trim_end();
        if t.trim() == "%}" {
            return Ok((script.join("\n"), i + 1));
        }
        if let Some(last) = t.trim_end().strip_suffix("%}") {
            script.push(last.to_string());
            return Ok((script.join("\n"), i + 1));
        }
        script.push(lines[i].to_string());
        i += 1;
    }
    Err(err(start + 1, "unterminated response handler (missing %})"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(input: &str) -> Request {
        let file = parse(input).expect("parse failed");
        assert_eq!(file.requests.len(), 1, "expected exactly one request");
        file.requests.into_iter().next().unwrap()
    }

    #[test]
    fn parses_minimal_get() {
        let r = one("GET http://example.test/users\n");
        assert_eq!(r.method, "GET");
        assert_eq!(r.url, "http://example.test/users");
        assert!(r.headers.is_empty());
        assert!(r.body.is_none());
    }

    #[test]
    fn bare_urls_default_to_get() {
        let r = one("http://example.test/ping\n");
        assert_eq!(r.method, "GET");
        let r = one("{{base}}/ping\n");
        assert_eq!(r.method, "GET");
        assert_eq!(r.url, "{{base}}/ping");
    }

    #[test]
    fn method_casing_and_version_token_are_tolerated() {
        let r = one("GET http://example.test/ HTTP/1.1\n");
        assert_eq!(r.url, "http://example.test/");
        let r = one("post http://example.test/x\n");
        assert_eq!(r.method, "POST");
    }

    #[test]
    fn crlf_input_parses_identically() {
        // Files saved by Windows editors come with CRLF line endings.
        let unix = parse("POST http://example.test/a\nA: b\n\nbody\n").unwrap();
        let dos = parse("POST http://example.test/a\r\nA: b\r\n\r\nbody\r\n").unwrap();
        assert_eq!(unix.requests[0].headers, dos.requests[0].headers);
        assert_eq!(unix.requests[0].body, dos.requests[0].body);
    }

    #[test]
    fn parses_headers_and_body() {
        let r = one(concat!(
            "POST http://example.test/login\n",
            "Content-Type: application/json\n",
            "X-Trace: abc\n",
            "\n",
            "{\"user\": \"amy\",\n",
            " \"pass\": \"secret\"}\n",
        ));
        assert_eq!(
            r.headers,
            vec![
                ("Content-Type".to_string(), "application/json".to_string()),
                ("X-Trace".to_string(), "abc".to_string()),
            ]
        );
        assert_eq!(
            r.body,
            Some(Body::Inline(
                "{\"user\": \"amy\",\n \"pass\": \"secret\"}".to_string()
            ))
        );
        // Interior blank lines are body content; trailing ones are trimmed.
        let r = one("POST http://example.test/t\n\nline1\n\nline2\n\n\n");
        assert_eq!(r.body, Some(Body::Inline("line1\n\nline2".to_string())));
    }

    #[test]
    fn separators_split_requests_and_name_them() {
        let file = parse(concat!(
            "### first\n",
            "GET http://example.test/a\n",
            "\n",
            "###\n",
            "GET http://example.test/b\n",
        ))
        .unwrap();
        assert_eq!(file.requests.len(), 2);
        assert_eq!(file.requests[0].name.as_deref(), Some("first"));
        assert_eq!(file.requests[1].name, None);
        // A separator also terminates a body in progress.
        let file = parse(concat!(
            "POST http://example.test/a\n",
            "\n",
            "body text\n",
            "### next\n",
            "GET http://example.test/b\n",
        ))
        .unwrap();
        assert_eq!(
            file.requests[0].body,
            Some(Body::Inline("body text".to_string()))
        );
        assert_eq!(file.requests[1].name.as_deref(), Some("next"));
    }

    #[test]
    fn at_name_directive_wins_over_separator_title() {
        let file = parse(concat!(
            "### separator title\n",
            "# @name real-name\n",
            "GET http://example.test/a\n",
        ))
        .unwrap();
        assert_eq!(file.requests[0].name.as_deref(), Some("real-name"));
    }

    #[test]
    fn slash_slash_comments_and_directives_work() {
        let file = parse(concat!(
            "// @name login\n",
            "// just a note\n",
            "POST http://example.test/login\n",
            "# a note between headers\n",
            "Accept: application/json\n",
        ))
        .unwrap();
        assert_eq!(file.requests[0].name.as_deref(), Some("login"));
        assert_eq!(file.requests[0].headers.len(), 1);
    }

    #[test]
    fn no_redirect_directive_is_per_request() {
        let file = parse(concat!(
            "# @no-redirect\n",
            "GET http://example.test/a\n",
            "###\n",
            "GET http://example.test/b\n",
        ))
        .unwrap();
        assert!(file.requests[0].no_redirect);
        assert!(!file.requests[1].no_redirect);
    }

    #[test]
    fn file_variables_are_collected_in_order() {
        let file = parse(concat!(
            "@host = example.test\n",
            "@base = http://{{host}}\n",
            "GET {{base}}/x\n",
        ))
        .unwrap();
        assert_eq!(
            file.variables,
            vec![
                ("host".to_string(), "example.test".to_string()),
                ("base".to_string(), "http://{{host}}".to_string()),
            ]
        );
    }

    #[test]
    fn multiline_url_continuation() {
        let r = one(concat!(
            "GET http://example.test/search\n",
            "    ?q=rust\n",
            "    &page=2\n",
        ));
        assert_eq!(r.url, "http://example.test/search?q=rust&page=2");
    }

    #[test]
    fn body_from_file_variants() {
        let r = one("POST http://example.test/u\n\n< ./payload.json\n");
        assert_eq!(
            r.body,
            Some(Body::FromFile {
                path: "./payload.json".to_string(),
                substitute: false
            })
        );
        let r = one("POST http://example.test/u\n\n<@ ./payload.json\n");
        assert_eq!(
            r.body,
            Some(Body::FromFile {
                path: "./payload.json".to_string(),
                substitute: true
            })
        );
    }

    #[test]
    fn handler_blocks_single_and_multi_line() {
        let r =
            one("GET http://example.test/a\n\n> {% client.assert(response.status === 200); %}\n");
        let h = r.handler.unwrap();
        assert_eq!(h.script, "client.assert(response.status === 200);");
        let r = one(concat!(
            "GET http://example.test/a\n",
            "\n",
            "> {%\n",
            "  client.test(\"ok\", function() {\n",
            "    client.assert(response.status === 200, \"want 200\");\n",
            "  });\n",
            "%}\n",
        ));
        let h = r.handler.unwrap();
        assert!(h.script.contains("client.test"));
        assert!(h.script.contains("want 200"));
        assert_eq!(h.line, 3);
    }

    #[test]
    fn handler_after_body() {
        let r = one(concat!(
            "POST http://example.test/login\n",
            "\n",
            "{\"u\": 1}\n",
            "\n",
            "> {% client.global.set(\"id\", response.body.id); %}\n",
        ));
        assert_eq!(r.body, Some(Body::Inline("{\"u\": 1}".to_string())));
        assert!(r.handler.is_some());
    }

    #[test]
    fn handler_error_cases_are_positioned() {
        let e = parse("GET http://example.test/a\n\n> {%\nclient.log(1);\n").unwrap_err();
        assert_eq!(e.line, 3);
        assert!(e.message.contains("unterminated"));
        let e = parse("GET http://example.test/a\n\n> ./handler.js\n").unwrap_err();
        assert!(e.message.contains("not supported"));
    }

    #[test]
    fn save_response_directives() {
        let r = one("GET http://example.test/f\n\n>> out/body.json\n");
        assert_eq!(r.save_response, Some(("out/body.json".to_string(), false)));
        let r = one("GET http://example.test/f\n\n>>! out/body.json\n");
        assert_eq!(r.save_response, Some(("out/body.json".to_string(), true)));
    }

    #[test]
    fn malformed_lines_are_positioned_errors() {
        let e = parse("\n\nnot a request\n").unwrap_err();
        assert_eq!(e.line, 3);
        assert!(e.message.contains("expected a request line"));
        let e = parse("GET http://example.test/a\nBadHeader\n").unwrap_err();
        assert_eq!(e.line, 2);
    }

    #[test]
    fn saved_response_reference_is_ignored() {
        let r = one("GET http://example.test/a\n\n<> 2026-07-01T000000.200.json\n");
        assert!(r.body.is_none());
    }
}
