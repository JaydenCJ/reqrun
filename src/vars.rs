//! Variable resolution: `{{name}}` substitution, `http-client.env.json`
//! environments, `--var` overrides, response-handler globals and the
//! JetBrains dynamic variables (`{{$uuid}}`, `{{$timestamp}}`, ...).

use crate::json::{self, Value};
use std::collections::HashMap;
use std::path::Path;

/// Resolution order (first hit wins), matching what editor users expect:
/// 1. `--var` command-line overrides
/// 2. values set by `client.global.set(...)` during this run
/// 3. `@name = value` file variables
/// 4. the selected environment from `http-client.env.json` (+ private file)
#[derive(Debug, Default)]
pub struct Vars {
    overrides: HashMap<String, String>,
    globals: HashMap<String, String>,
    file_vars: HashMap<String, String>,
    env_vars: HashMap<String, String>,
    rng: Rng,
}

impl Vars {
    pub fn new() -> Self {
        Vars {
            rng: Rng::seeded_from_time(),
            ..Default::default()
        }
    }

    pub fn set_override(&mut self, name: &str, value: &str) {
        self.overrides.insert(name.to_string(), value.to_string());
    }

    pub fn set_global(&mut self, name: &str, value: &str) {
        self.globals.insert(name.to_string(), value.to_string());
    }

    pub fn set_env_var(&mut self, name: &str, value: &str) {
        self.env_vars.insert(name.to_string(), value.to_string());
    }

    /// Install file variables; the *values* may themselves reference earlier
    /// variables (`@base = http://{{host}}`), so each is resolved as it is
    /// declared, in order.
    pub fn set_file_vars(&mut self, vars: &[(String, String)]) -> Result<(), String> {
        for (name, value) in vars {
            let resolved = self.substitute(value)?;
            self.file_vars.insert(name.clone(), resolved);
        }
        Ok(())
    }

    fn lookup(&self, name: &str) -> Option<String> {
        self.overrides
            .get(name)
            .or_else(|| self.globals.get(name))
            .or_else(|| self.file_vars.get(name))
            .or_else(|| self.env_vars.get(name))
            .cloned()
    }

    /// Replace every `{{name}}` in `input`. Unknown variables are an error —
    /// silently sending a literal `{{token}}` to a server is how CI lies to you.
    pub fn substitute(&mut self, input: &str) -> Result<String, String> {
        self.substitute_with(input, false)
    }

    /// Like [`substitute`](Self::substitute), but unknown variables render as
    /// their literal `{{name}}` text. Used by `--dry-run`, where values set by
    /// response handlers (e.g. a captured token) cannot exist yet.
    pub fn substitute_lenient(&mut self, input: &str) -> Result<String, String> {
        self.substitute_with(input, true)
    }

    fn substitute_with(&mut self, input: &str, lenient: bool) -> Result<String, String> {
        let mut out = String::with_capacity(input.len());
        let mut rest = input;
        while let Some(start) = rest.find("{{") {
            out.push_str(&rest[..start]);
            let after = &rest[start + 2..];
            let end = after
                .find("}}")
                .ok_or_else(|| format!("unclosed '{{{{' in '{}'", input.trim()))?;
            let name = after[..end].trim();
            let value = if let Some(dynamic) = name.strip_prefix('$') {
                self.dynamic(dynamic)?
            } else {
                match (self.lookup(name), lenient) {
                    (Some(v), _) => v,
                    (None, true) => format!("{{{{{name}}}}}"),
                    (None, false) => return Err(format!("undefined variable '{{{{{name}}}}}'")),
                }
            };
            out.push_str(&value);
            rest = &after[end + 2..];
        }
        out.push_str(rest);
        Ok(out)
    }

    /// JetBrains dynamic variables. Each occurrence is evaluated independently.
    fn dynamic(&mut self, name: &str) -> Result<String, String> {
        match name {
            "uuid" | "random.uuid" => Ok(self.rng.uuid_v4()),
            "timestamp" => Ok(unix_now().to_string()),
            "isoTimestamp" => Ok(iso_utc(unix_now())),
            "randomInt" => Ok((self.rng.next() % 1000).to_string()),
            _ => {
                if let Some(env_name) = name.strip_prefix("env.") {
                    return std::env::var(env_name).map_err(|_| {
                        format!("environment variable '{env_name}' is not set (from {{{{$env.{env_name}}}}})")
                    });
                }
                if let Some(args) = name
                    .strip_prefix("random.integer(")
                    .and_then(|s| s.strip_suffix(')'))
                {
                    let (lo, hi) = args
                        .split_once(',')
                        .ok_or_else(|| format!("bad arguments in {{{{${name}}}}}"))?;
                    let lo: u64 = lo
                        .trim()
                        .parse()
                        .map_err(|_| format!("bad arguments in {{{{${name}}}}}"))?;
                    let hi: u64 = hi
                        .trim()
                        .parse()
                        .map_err(|_| format!("bad arguments in {{{{${name}}}}}"))?;
                    if hi <= lo {
                        return Err(format!("empty range in {{{{${name}}}}}"));
                    }
                    return Ok((lo + self.rng.next() % (hi - lo)).to_string());
                }
                Err(format!("unknown dynamic variable '{{{{${name}}}}}'"))
            }
        }
    }
}

/// Load an environment from JetBrains `http-client.env.json` format:
/// `{ "envName": { "key": "value", ... }, ... }`. If a sibling
/// `http-client.private.env.json` exists its values win, mirroring the editor.
pub fn load_environment(env_file: &Path, env_name: &str, vars: &mut Vars) -> Result<(), String> {
    apply_env_file(env_file, env_name, vars, true)?;
    let private = env_file.with_file_name("http-client.private.env.json");
    if private != env_file && private.is_file() {
        apply_env_file(&private, env_name, vars, false)?;
    }
    Ok(())
}

fn apply_env_file(
    path: &Path,
    env_name: &str,
    vars: &mut Vars,
    required: bool,
) -> Result<(), String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let doc = json::parse(&text).map_err(|e| format!("{}: invalid JSON: {e}", path.display()))?;
    let envs = match &doc {
        Value::Object(map) => map,
        _ => return Err(format!("{}: expected a top-level object", path.display())),
    };
    let env = match envs.get(env_name) {
        Some(Value::Object(map)) => map,
        Some(_) => {
            return Err(format!(
                "{}: environment '{env_name}' is not an object",
                path.display()
            ))
        }
        None if required => {
            let known: Vec<&str> = envs.keys().map(|k| k.as_str()).collect();
            return Err(format!(
                "{}: no environment named '{env_name}' (available: {})",
                path.display(),
                if known.is_empty() {
                    "none".to_string()
                } else {
                    known.join(", ")
                }
            ));
        }
        None => return Ok(()),
    };
    for (key, value) in env {
        let text = match value {
            Value::String(s) => s.clone(),
            other => json::serialize(other),
        };
        vars.set_env_var(key, &text);
    }
    Ok(())
}

/// Seconds since the Unix epoch.
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Format a Unix timestamp as `YYYY-MM-DDThh:mm:ssZ` without a date crate.
/// Civil-from-days algorithm (Howard Hinnant's) — exact for all of Unix time.
pub fn iso_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!("{year:04}-{month:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Small xorshift64* PRNG — good enough for request IDs, zero dependencies.
/// Not cryptographic, and does not need to be.
#[derive(Debug)]
struct Rng(u64);

impl Default for Rng {
    fn default() -> Self {
        Rng(0x9E3779B97F4A7C15)
    }
}

impl Rng {
    fn seeded_from_time() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x5DEECE66D);
        Rng(nanos | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }

    /// RFC 4122 version-4 formatted UUID.
    fn uuid_v4(&mut self) -> String {
        let a = self.next();
        let b = self.next();
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&a.to_be_bytes());
        bytes[8..].copy_from_slice(&b.to_be_bytes());
        bytes[6] = (bytes[6] & 0x0F) | 0x40; // version 4
        bytes[8] = (bytes[8] & 0x3F) | 0x80; // RFC variant
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        format!(
            "{}-{}-{}-{}-{}",
            &hex[0..8],
            &hex[8..12],
            &hex[12..16],
            &hex[16..20],
            &hex[20..32]
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn substitutes_known_variables() {
        let mut v = Vars::new();
        v.set_env_var("host", "example.test");
        v.set_env_var("port", "8080");
        assert_eq!(
            v.substitute("http://{{host}}:{{ port }}/x").unwrap(),
            "http://example.test:8080/x"
        );
    }

    #[test]
    fn substitution_edge_cases() {
        let mut v = Vars::new();
        let err = v.substitute("{{missing}}").unwrap_err();
        assert!(
            err.contains("missing"),
            "undefined variable must error: {err}"
        );
        assert!(
            v.substitute("{{oops").is_err(),
            "unclosed braces must error"
        );
        assert!(
            v.substitute("{{$bogus}}").is_err(),
            "unknown dynamic var must error"
        );
        assert_eq!(v.substitute("plain } text {").unwrap(), "plain } text {");
    }

    #[test]
    fn precedence_override_beats_global_beats_file_beats_env() {
        let mut v = Vars::new();
        v.set_env_var("k", "env");
        v.set_file_vars(&[("k".to_string(), "file".to_string())])
            .unwrap();
        assert_eq!(v.substitute("{{k}}").unwrap(), "file");
        v.set_global("k", "global");
        assert_eq!(v.substitute("{{k}}").unwrap(), "global");
        v.set_override("k", "cli");
        assert_eq!(v.substitute("{{k}}").unwrap(), "cli");
    }

    #[test]
    fn file_vars_resolve_against_earlier_ones() {
        let mut v = Vars::new();
        v.set_file_vars(&[
            ("host".to_string(), "example.test".to_string()),
            ("base".to_string(), "http://{{host}}/api".to_string()),
        ])
        .unwrap();
        assert_eq!(v.substitute("{{base}}").unwrap(), "http://example.test/api");
    }

    #[test]
    fn dynamic_uuid_has_v4_shape_and_varies() {
        let mut v = Vars::new();
        let a = v.substitute("{{$uuid}}").unwrap();
        let b = v.substitute("{{$uuid}}").unwrap();
        assert_ne!(a, b);
        let parts: Vec<&str> = a.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12]
        );
        assert!(parts[2].starts_with('4'), "version nibble: {a}");
        assert!(matches!(parts[3].as_bytes()[0], b'8' | b'9' | b'a' | b'b'));
    }

    #[test]
    fn dynamic_random_int_is_bounded() {
        let mut v = Vars::new();
        for _ in 0..50 {
            let n: u64 = v.substitute("{{$randomInt}}").unwrap().parse().unwrap();
            assert!(n < 1000);
            let m: u64 = v
                .substitute("{{$random.integer(5, 8)}}")
                .unwrap()
                .parse()
                .unwrap();
            assert!((5..8).contains(&m), "out of range: {m}");
        }
    }

    #[test]
    fn dynamic_timestamps_have_expected_shape() {
        let mut v = Vars::new();
        let ts: u64 = v.substitute("{{$timestamp}}").unwrap().parse().unwrap();
        assert!(ts > 1_600_000_000);
        let iso = v.substitute("{{$isoTimestamp}}").unwrap();
        assert_eq!(iso.len(), 20);
        assert!(iso.ends_with('Z') && iso.as_bytes()[10] == b'T');
    }

    #[test]
    fn iso_utc_known_values() {
        assert_eq!(iso_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_utc(951_782_400), "2000-02-29T00:00:00Z"); // leap day
        assert_eq!(iso_utc(1_767_225_599), "2025-12-31T23:59:59Z");
    }

    #[test]
    fn env_file_selection_and_private_merge() {
        let dir = std::env::temp_dir().join(format!("reqrun-vars-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pub_path = dir.join("http-client.env.json");
        let mut f = std::fs::File::create(&pub_path).unwrap();
        write!(
            f,
            r#"{{"dev": {{"host": "127.0.0.1", "token": "public", "retries": 3}}, "prod": {{"host": "example.test"}}}}"#
        )
        .unwrap();
        let mut f = std::fs::File::create(dir.join("http-client.private.env.json")).unwrap();
        write!(f, r#"{{"dev": {{"token": "sekrit"}}}}"#).unwrap();

        let mut v = Vars::new();
        load_environment(&pub_path, "dev", &mut v).unwrap();
        assert_eq!(v.substitute("{{host}}").unwrap(), "127.0.0.1");
        assert_eq!(v.substitute("{{token}}").unwrap(), "sekrit"); // private wins
        assert_eq!(v.substitute("{{retries}}").unwrap(), "3"); // non-string coerced

        let mut v = Vars::new();
        let err = load_environment(&pub_path, "staging", &mut v).unwrap_err();
        assert!(err.contains("staging") && err.contains("dev"), "got: {err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
