//! Minimal JSON parser, serializer and path accessor (std only).
//!
//! reqrun needs JSON in three places: `http-client.env.json` environment
//! files, `response.body` member access inside response-handler scripts, and
//! pretty-printing JSON responses in verbose mode. A hand-rolled recursive
//! descent parser keeps the binary dependency-free.

use std::collections::BTreeMap;
use std::fmt;

/// A parsed JSON value. Objects use a `BTreeMap` so serialization is
/// deterministic (keys sorted), which keeps test output stable.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Value>),
    Object(BTreeMap<String, Value>),
}

impl Value {
    /// Member access: `value.get("key")` on objects, `None` otherwise.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(map) => map.get(key),
            _ => None,
        }
    }

    /// Index access on arrays.
    pub fn index(&self, idx: usize) -> Option<&Value> {
        match self {
            Value::Array(items) => items.get(idx),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    /// JS-style truthiness: `false`, `null`, `0` and `""` are falsy.
    pub fn truthy(&self) -> bool {
        match self {
            Value::Null => false,
            Value::Bool(b) => *b,
            Value::Number(n) => *n != 0.0 && !n.is_nan(),
            Value::String(s) => !s.is_empty(),
            Value::Array(_) | Value::Object(_) => true,
        }
    }

    /// Human-readable type name used in script error messages.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "boolean",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        }
    }
}

impl fmt::Display for Value {
    /// Compact JSON serialization; bare strings render without quotes so
    /// assertion messages read naturally (`expected "ok", got error`).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::String(s) => write!(f, "{s}"),
            other => write!(f, "{}", serialize(other)),
        }
    }
}

/// Serialize a value to compact JSON text.
pub fn serialize(value: &Value) -> String {
    let mut out = String::new();
    write_value(value, &mut out);
    out
}

fn write_value(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => {
            if n.fract() == 0.0 && n.abs() < 1e15 {
                out.push_str(&format!("{}", *n as i64));
            } else {
                out.push_str(&format!("{n}"));
            }
        }
        Value::String(s) => write_string(s, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (i, (k, v)) in map.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(k, out);
                out.push(':');
                write_value(v, out);
            }
            out.push('}');
        }
    }
}

fn write_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Parse a JSON document. Errors carry the byte offset of the failure so
/// env-file problems point at the right spot.
pub fn parse(input: &str) -> Result<Value, String> {
    let mut p = Parser {
        bytes: input.as_bytes(),
        pos: 0,
    };
    p.skip_ws();
    let value = p.parse_value()?;
    p.skip_ws();
    if p.pos != p.bytes.len() {
        return Err(format!("trailing characters at offset {}", p.pos));
    }
    Ok(value)
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn expect(&mut self, byte: u8) -> Result<(), String> {
        if self.peek() == Some(byte) {
            self.pos += 1;
            Ok(())
        } else {
            Err(format!(
                "expected '{}' at offset {}",
                byte as char, self.pos
            ))
        }
    }

    fn parse_value(&mut self) -> Result<Value, String> {
        self.skip_ws();
        match self.peek() {
            Some(b'{') => self.parse_object(),
            Some(b'[') => self.parse_array(),
            Some(b'"') => Ok(Value::String(self.parse_string()?)),
            Some(b't') => self.parse_keyword("true", Value::Bool(true)),
            Some(b'f') => self.parse_keyword("false", Value::Bool(false)),
            Some(b'n') => self.parse_keyword("null", Value::Null),
            Some(c) if c == b'-' || c.is_ascii_digit() => self.parse_number(),
            _ => Err(format!("unexpected input at offset {}", self.pos)),
        }
    }

    fn parse_keyword(&mut self, word: &str, value: Value) -> Result<Value, String> {
        if self.bytes[self.pos..].starts_with(word.as_bytes()) {
            self.pos += word.len();
            Ok(value)
        } else {
            Err(format!("invalid literal at offset {}", self.pos))
        }
    }

    fn parse_number(&mut self) -> Result<Value, String> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        while matches!(self.peek(), Some(c) if c.is_ascii_digit() || matches!(c, b'.' | b'e' | b'E' | b'+' | b'-'))
        {
            self.pos += 1;
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos]).unwrap();
        text.parse::<f64>()
            .map(Value::Number)
            .map_err(|_| format!("invalid number '{text}' at offset {start}"))
    }

    fn parse_string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            match self.peek() {
                None => return Err("unterminated string".into()),
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.pos += 1;
                    match self.peek() {
                        Some(b'"') => out.push('"'),
                        Some(b'\\') => out.push('\\'),
                        Some(b'/') => out.push('/'),
                        Some(b'n') => out.push('\n'),
                        Some(b't') => out.push('\t'),
                        Some(b'r') => out.push('\r'),
                        Some(b'b') => out.push('\u{8}'),
                        Some(b'f') => out.push('\u{c}'),
                        Some(b'u') => {
                            let cp = self.parse_unicode_escape()?;
                            out.push(cp);
                            continue;
                        }
                        _ => return Err(format!("invalid escape at offset {}", self.pos)),
                    }
                    self.pos += 1;
                }
                Some(_) => {
                    // Consume one UTF-8 code point.
                    let rest = std::str::from_utf8(&self.bytes[self.pos..])
                        .map_err(|_| "invalid UTF-8 in string".to_string())?;
                    let c = rest.chars().next().unwrap();
                    out.push(c);
                    self.pos += c.len_utf8();
                }
            }
        }
    }

    fn parse_unicode_escape(&mut self) -> Result<char, String> {
        // self.pos points at the 'u'.
        let hex_at = |p: &Self, at: usize| -> Result<u32, String> {
            let slice = p
                .bytes
                .get(at..at + 4)
                .ok_or_else(|| "truncated \\u escape".to_string())?;
            let text = std::str::from_utf8(slice).map_err(|_| "bad \\u escape".to_string())?;
            u32::from_str_radix(text, 16).map_err(|_| "bad \\u escape".to_string())
        };
        let mut code = hex_at(self, self.pos + 1)?;
        self.pos += 5;
        // Surrogate pair handling for characters outside the BMP.
        if (0xD800..0xDC00).contains(&code)
            && self.bytes.get(self.pos) == Some(&b'\\')
            && self.bytes.get(self.pos + 1) == Some(&b'u')
        {
            let low = hex_at(self, self.pos + 2)?;
            if (0xDC00..0xE000).contains(&low) {
                code = 0x10000 + ((code - 0xD800) << 10) + (low - 0xDC00);
                self.pos += 6;
            }
        }
        char::from_u32(code).ok_or_else(|| "invalid \\u code point".to_string())
    }

    fn parse_array(&mut self) -> Result<Value, String> {
        self.expect(b'[')?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Value::Array(items));
        }
        loop {
            items.push(self.parse_value()?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Value::Array(items));
                }
                _ => return Err(format!("expected ',' or ']' at offset {}", self.pos)),
            }
        }
    }

    fn parse_object(&mut self) -> Result<Value, String> {
        self.expect(b'{')?;
        let mut map = BTreeMap::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Value::Object(map));
        }
        loop {
            self.skip_ws();
            let key = self.parse_string()?;
            self.skip_ws();
            self.expect(b':')?;
            let value = self.parse_value()?;
            map.insert(key, value);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Value::Object(map));
                }
                _ => return Err(format!("expected ',' or '}}' at offset {}", self.pos)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_scalars() {
        assert_eq!(parse("null").unwrap(), Value::Null);
        assert_eq!(parse("true").unwrap(), Value::Bool(true));
        assert_eq!(parse("42").unwrap(), Value::Number(42.0));
        assert_eq!(parse("-3.5").unwrap(), Value::Number(-3.5));
        assert_eq!(parse("\"hi\"").unwrap(), Value::String("hi".into()));
    }

    #[test]
    fn parses_nested_structures() {
        let v = parse(r#"{"a": [1, {"b": "c"}], "d": null}"#).unwrap();
        assert_eq!(
            v.get("a").unwrap().index(1).unwrap().get("b").unwrap(),
            &Value::String("c".into())
        );
        assert_eq!(v.get("d").unwrap(), &Value::Null);
    }

    #[test]
    fn parses_string_escapes_and_unicode() {
        let v = parse(r#""a\n\t\"\\ é 😀""#).unwrap();
        assert_eq!(v, Value::String("a\n\t\"\\ é 😀".into()));
    }

    #[test]
    fn rejects_malformed_documents() {
        let err = parse("{} extra").unwrap_err();
        assert!(err.contains("trailing"), "got: {err}");
        assert!(parse("\"open").is_err(), "unterminated string");
        assert!(parse("nope").is_err(), "bare word");
        assert!(parse("[1,]").is_err(), "trailing comma");
    }

    #[test]
    fn serializes_round_trip_with_integer_numbers() {
        let src = r#"{"arr":[1,2.5,true,null],"s":"a\"b"}"#;
        let v = parse(src).unwrap();
        assert_eq!(serialize(&v), src);
        assert_eq!(serialize(&Value::Number(200.0)), "200");
        assert_eq!(serialize(&Value::Number(0.5)), "0.5");
    }

    #[test]
    fn truthiness_matches_javascript() {
        assert!(!Value::Null.truthy());
        assert!(!Value::Bool(false).truthy());
        assert!(!Value::Number(0.0).truthy());
        assert!(!Value::String(String::new()).truthy());
        assert!(Value::String("x".into()).truthy());
        assert!(Value::Array(vec![]).truthy());
        // Display renders strings bare, everything else as JSON.
        assert_eq!(Value::String("token-1".into()).to_string(), "token-1");
        assert_eq!(Value::Number(404.0).to_string(), "404");
    }
}
