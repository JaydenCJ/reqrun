//! Tiny URL splitter for the subset reqrun needs: `http://host[:port]/path?query`.

/// Components of a parsed request URL.
#[derive(Debug, Clone, PartialEq)]
pub struct Url {
    pub scheme: String,
    pub host: String,
    pub port: u16,
    /// Path + query exactly as it goes on the request line (always starts with `/`).
    pub path_and_query: String,
}

impl Url {
    /// `Host` header value: omits the port when it is the scheme default.
    pub fn host_header(&self) -> String {
        if self.port == 80 {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    /// Address string for the socket connect.
    pub fn authority(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// Parse an absolute URL. Only `http` is accepted in v0.1.0; `https` gets a
/// dedicated, honest error so users know it is a roadmap item rather than a bug.
pub fn parse(input: &str) -> Result<Url, String> {
    let input = input.trim();
    let (scheme, rest) = input
        .split_once("://")
        .ok_or_else(|| format!("URL '{input}' has no scheme (expected http://...)"))?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme == "https" {
        return Err(
            "https:// is not supported in v0.1.0 (TLS needs a dependency; see the roadmap) — point reqrun at the plain-HTTP port".into(),
        );
    }
    if scheme != "http" {
        return Err(format!("unsupported URL scheme '{scheme}'"));
    }
    if rest.is_empty() {
        return Err(format!("URL '{input}' has no host"));
    }
    let (authority, path_and_query) = match rest.find(['/', '?']) {
        Some(idx) if rest.as_bytes()[idx] == b'/' => (&rest[..idx], rest[idx..].to_string()),
        Some(idx) => (&rest[..idx], format!("/{}", &rest[idx..])),
        None => (rest, "/".to_string()),
    };
    if authority.is_empty() {
        return Err(format!("URL '{input}' has no host"));
    }
    // IPv6 literals come bracketed: http://[::1]:8080/
    let (host, port) = if let Some(rest6) = authority.strip_prefix('[') {
        let (host, after) = rest6
            .split_once(']')
            .ok_or_else(|| format!("unclosed IPv6 literal in '{authority}'"))?;
        let port = match after.strip_prefix(':') {
            Some(p) => parse_port(p)?,
            None if after.is_empty() => 80,
            _ => return Err(format!("invalid authority '{authority}'")),
        };
        (host.to_string(), port)
    } else if let Some((host, port)) = authority.rsplit_once(':') {
        (host.to_string(), parse_port(port)?)
    } else {
        (authority.to_string(), 80)
    };
    if host.is_empty() {
        return Err(format!("URL '{input}' has no host"));
    }
    Ok(Url {
        scheme,
        host,
        port,
        path_and_query,
    })
}

fn parse_port(text: &str) -> Result<u16, String> {
    text.parse::<u16>()
        .map_err(|_| format!("invalid port '{text}'"))
}

/// Resolve a redirect `Location` against the URL that produced it.
/// Handles absolute URLs, absolute paths and (minimally) relative paths.
pub fn resolve_location(base: &Url, location: &str) -> Result<Url, String> {
    if location.contains("://") {
        return parse(location);
    }
    let mut out = base.clone();
    if location.starts_with('/') {
        out.path_and_query = location.to_string();
    } else {
        // Relative to the base path's directory.
        let path = base.path_and_query.split(['?', '#']).next().unwrap_or("/");
        let dir = match path.rfind('/') {
            Some(idx) => &path[..=idx],
            None => "/",
        };
        out.path_and_query = format!("{dir}{location}");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_host_only() {
        let u = parse("http://example.test").unwrap();
        assert_eq!(u.host, "example.test");
        assert_eq!(u.port, 80);
        assert_eq!(u.path_and_query, "/");
        assert_eq!(u.host_header(), "example.test");
    }

    #[test]
    fn parses_port_path_and_query() {
        let u = parse("http://127.0.0.1:8080/api/users?limit=10&x=%20").unwrap();
        assert_eq!(u.port, 8080);
        assert_eq!(u.path_and_query, "/api/users?limit=10&x=%20");
        assert_eq!(u.host_header(), "127.0.0.1:8080");
        assert_eq!(u.authority(), "127.0.0.1:8080");
        // Query with no path gets the implicit "/".
        let u = parse("http://example.test?q=1").unwrap();
        assert_eq!(u.path_and_query, "/?q=1");
    }

    #[test]
    fn parses_ipv6_literal() {
        let u = parse("http://[::1]:9000/x").unwrap();
        assert_eq!(u.host, "::1");
        assert_eq!(u.port, 9000);
    }

    #[test]
    fn rejects_https_with_roadmap_hint() {
        let err = parse("https://example.test/").unwrap_err();
        assert!(err.contains("v0.1.0"), "got: {err}");
    }

    #[test]
    fn rejects_missing_scheme_and_bad_port() {
        assert!(parse("example.test/path").is_err());
        assert!(parse("http://example.test:99999/").is_err());
        assert!(parse("http:///nohost").is_err());
    }

    #[test]
    fn resolves_absolute_and_relative_locations() {
        let base = parse("http://example.test:8080/a/b?q=1").unwrap();
        let abs = resolve_location(&base, "http://other.test/z").unwrap();
        assert_eq!(abs.host, "other.test");
        let rooted = resolve_location(&base, "/login").unwrap();
        assert_eq!(rooted.path_and_query, "/login");
        assert_eq!(rooted.host, "example.test");
        let relative = resolve_location(&base, "c").unwrap();
        assert_eq!(relative.path_and_query, "/a/c");
    }
}
