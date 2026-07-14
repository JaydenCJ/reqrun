//! Interpreter for the JetBrains response-handler subset.
//!
//! The editors run `> {% ... %}` blocks in a JavaScript VM. Embedding one
//! would sink the zero-dependency claim, so reqrun interprets the subset that
//! real-world `.http` files actually use for CI-style checks:
//!
//! - `client.test("name", function () { ... })` (and `() => { ... }`)
//! - `client.assert(expr[, message])`
//! - `client.global.set("name", expr)`
//! - `client.log(expr)`
//! - expressions over `response.status`, `response.body` (with `.` / `[i]`
//!   JSON access), `response.headers.valueOf(...)`, `response.contentType`,
//!   string/array helpers (`includes`, `startsWith`, `endsWith`, `length`),
//!   literals, `!`, comparisons, `&&` and `||`.
//!
//! Anything outside the subset fails with a positioned "unsupported" error
//! instead of silently passing — a check that does not run must not look green.

use crate::http::HttpResponse;
use crate::json::{self, Value};

/// Result of one `client.assert` (or an error surfaced as a failed check).
#[derive(Debug, Clone)]
pub struct Assertion {
    /// Enclosing `client.test` name, if any.
    pub test: Option<String>,
    pub passed: bool,
    pub message: String,
}

/// Everything a handler run produced.
#[derive(Debug, Default)]
pub struct Outcome {
    pub assertions: Vec<Assertion>,
    pub logs: Vec<String>,
    /// `client.global.set` pairs, in execution order.
    pub globals: Vec<(String, String)>,
}

impl Outcome {
    pub fn failed(&self) -> usize {
        self.assertions.iter().filter(|a| !a.passed).count()
    }
}

/// Run `script` against `response`. `Err` means the script itself could not
/// be parsed/executed (reported as a request error, not an assertion failure).
pub fn run(script: &str, response: &HttpResponse) -> Result<Outcome, String> {
    let tokens = tokenize(script)?;
    let mut interp = Interp {
        tokens,
        pos: 0,
        src: script,
        response,
        outcome: Outcome::default(),
        current_test: None,
    };
    while !interp.at_end() {
        interp.statement()?;
    }
    Ok(interp.outcome)
}

// ---------------------------------------------------------------- lexer ----

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),
    Str(String),
    Num(f64),
    Punct(&'static str),
}

#[derive(Debug, Clone)]
struct Token {
    tok: Tok,
    /// Byte offset in the script source (for spans and error positions).
    start: usize,
    end: usize,
}

fn tokenize(src: &str) -> Result<Vec<Token>, String> {
    const PUNCTS: [&str; 22] = [
        "===", "!==", "=>", "==", "!=", ">=", "<=", "&&", "||", "(", ")", "{", "}", "[", "]", ",",
        ";", ".", "!", ">", "=", "+",
    ];
    let bytes = src.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        // Line comments inside handlers.
        if src[i..].starts_with("//") {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == '"' || c == '\'' {
            let quote = c;
            let start = i;
            i += 1;
            let mut text = String::new();
            loop {
                if i >= bytes.len() {
                    return Err(format!("unterminated string at offset {start}"));
                }
                let ch = src[i..].chars().next().unwrap();
                if ch == quote {
                    i += 1;
                    break;
                }
                if ch == '\\' {
                    i += 1;
                    let esc = src[i..].chars().next().ok_or("dangling escape")?;
                    text.push(match esc {
                        'n' => '\n',
                        't' => '\t',
                        'r' => '\r',
                        other => other,
                    });
                    i += esc.len_utf8();
                    continue;
                }
                text.push(ch);
                i += ch.len_utf8();
            }
            tokens.push(Token {
                tok: Tok::Str(text),
                start,
                end: i,
            });
            continue;
        }
        if c.is_ascii_digit() || (c == '-' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit)) {
            let start = i;
            i += 1;
            while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                i += 1;
            }
            let text = &src[start..i];
            let n: f64 = text
                .parse()
                .map_err(|_| format!("invalid number '{text}'"))?;
            tokens.push(Token {
                tok: Tok::Num(n),
                start,
                end: i,
            });
            continue;
        }
        if c.is_ascii_alphabetic() || c == '_' || c == '$' {
            let start = i;
            while i < bytes.len()
                && ((bytes[i] as char).is_ascii_alphanumeric() || matches!(bytes[i], b'_' | b'$'))
            {
                i += 1;
            }
            tokens.push(Token {
                tok: Tok::Ident(src[start..i].to_string()),
                start,
                end: i,
            });
            continue;
        }
        if let Some(p) = PUNCTS.iter().find(|p| src[i..].starts_with(**p)) {
            tokens.push(Token {
                tok: Tok::Punct(p),
                start: i,
                end: i + p.len(),
            });
            i += p.len();
            continue;
        }
        if c == '<' {
            // '<' is punct too, but must not shadow '<=' (handled above).
            tokens.push(Token {
                tok: Tok::Punct("<"),
                start: i,
                end: i + 1,
            });
            i += 1;
            continue;
        }
        return Err(format!("unexpected character '{c}' at offset {i}"));
    }
    Ok(tokens)
}

// ---------------------------------------------------------- interpreter ----

struct Interp<'a> {
    tokens: Vec<Token>,
    pos: usize,
    src: &'a str,
    response: &'a HttpResponse,
    outcome: Outcome,
    current_test: Option<String>,
}

/// Intermediate operand: plain values plus the special `response.*` objects
/// that only exist so members/methods can be resolved on them.
#[derive(Debug, Clone)]
enum Operand {
    Value(Value),
    Response,
    Headers,
    ContentType,
}

impl<'a> Interp<'a> {
    fn at_end(&self) -> bool {
        self.pos >= self.tokens.len()
    }

    fn peek(&self) -> Option<&Tok> {
        self.tokens.get(self.pos).map(|t| &t.tok)
    }

    fn line_of(&self, offset: usize) -> usize {
        self.src[..offset.min(self.src.len())]
            .bytes()
            .filter(|&b| b == b'\n')
            .count()
            + 1
    }

    fn here(&self) -> usize {
        self.tokens
            .get(self.pos.min(self.tokens.len().saturating_sub(1)))
            .map(|t| t.start)
            .unwrap_or(self.src.len())
    }

    fn unsupported(&self, what: &str) -> String {
        format!(
            "unsupported handler statement near line {}: {what} (supported: client.test/assert/global.set/log)",
            self.line_of(self.here())
        )
    }

    fn eat_punct(&mut self, p: &str) -> bool {
        if self.peek() == Some(&Tok::Punct(match_punct(p))) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect_punct(&mut self, p: &str) -> Result<(), String> {
        if self.eat_punct(p) {
            Ok(())
        } else {
            Err(format!(
                "expected '{p}' near line {}",
                self.line_of(self.here())
            ))
        }
    }

    fn eat_ident(&mut self, name: &str) -> bool {
        if matches!(self.peek(), Some(Tok::Ident(id)) if id == name) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect_string(&mut self) -> Result<String, String> {
        match self.peek().cloned() {
            Some(Tok::Str(s)) => {
                self.pos += 1;
                Ok(s)
            }
            _ => Err(format!(
                "expected a string literal near line {}",
                self.line_of(self.here())
            )),
        }
    }

    // One `client.*` statement.
    fn statement(&mut self) -> Result<(), String> {
        // Tolerate stray semicolons between statements.
        while self.eat_punct(";") {}
        if self.at_end() {
            return Ok(());
        }
        if !self.eat_ident("client") {
            return Err(self.unsupported("statement does not start with 'client.'"));
        }
        self.expect_punct(".")?;
        if self.eat_ident("test") {
            self.client_test()?;
        } else if self.eat_ident("assert") {
            self.client_assert()?;
        } else if self.eat_ident("global") {
            self.expect_punct(".")?;
            if !self.eat_ident("set") {
                return Err(self.unsupported("only client.global.set(...) is supported"));
            }
            self.client_global_set()?;
        } else if self.eat_ident("log") {
            self.client_log()?;
        } else {
            return Err(self.unsupported("unknown client method"));
        }
        while self.eat_punct(";") {}
        Ok(())
    }

    fn client_test(&mut self) -> Result<(), String> {
        self.expect_punct("(")?;
        let name = self.expect_string()?;
        self.expect_punct(",")?;
        // `function () {` or `() => {`
        if self.eat_ident("function") {
            self.expect_punct("(")?;
            self.expect_punct(")")?;
        } else {
            self.expect_punct("(")?;
            self.expect_punct(")")?;
            self.expect_punct("=>")?;
        }
        self.expect_punct("{")?;
        let before = self.outcome.assertions.len();
        let prev = self.current_test.replace(name.clone());
        // Statements until the closing brace. A runtime error inside a test
        // is recorded as that test failing, mirroring the editors.
        while !self.at_end() && self.peek() != Some(&Tok::Punct("}")) {
            if let Err(e) = self.statement() {
                self.outcome.assertions.push(Assertion {
                    test: Some(name.clone()),
                    passed: false,
                    message: e,
                });
                // Recover: skip to the closing brace of this test body.
                while !self.at_end() && self.peek() != Some(&Tok::Punct("}")) {
                    self.pos += 1;
                }
                break;
            }
        }
        self.current_test = prev;
        self.expect_punct("}")?;
        self.expect_punct(")")?;
        // An empty test body counts as a passing (vacuous) test so it still
        // shows up in output rather than disappearing.
        if self.outcome.assertions.len() == before {
            self.outcome.assertions.push(Assertion {
                test: Some(name),
                passed: true,
                message: "(no assertions)".into(),
            });
        }
        Ok(())
    }

    fn client_assert(&mut self) -> Result<(), String> {
        self.expect_punct("(")?;
        let span_start = self.here();
        let condition = self.expression()?;
        let span_end = self
            .tokens
            .get(self.pos.saturating_sub(1))
            .map(|t| t.end)
            .unwrap_or(span_start);
        let message = if self.eat_punct(",") {
            Some(self.expression_value()?.to_string())
        } else {
            None
        };
        self.expect_punct(")")?;
        let passed = condition.truthy();
        let expr_src = self.src[span_start..span_end].trim().to_string();
        let message = match (passed, message) {
            (true, Some(m)) => m,
            (true, None) => expr_src,
            (false, Some(m)) => format!("{m} (assert: {expr_src})"),
            (false, None) => format!("assert failed: {expr_src}"),
        };
        self.outcome.assertions.push(Assertion {
            test: self.current_test.clone(),
            passed,
            message,
        });
        Ok(())
    }

    fn client_global_set(&mut self) -> Result<(), String> {
        self.expect_punct("(")?;
        let name = self.expect_string()?;
        self.expect_punct(",")?;
        let value = self.expression_value()?;
        self.expect_punct(")")?;
        let text = match &value {
            Value::String(s) => s.clone(),
            other => json::serialize(other),
        };
        self.outcome.globals.push((name, text));
        Ok(())
    }

    fn client_log(&mut self) -> Result<(), String> {
        self.expect_punct("(")?;
        let value = self.expression_value()?;
        self.expect_punct(")")?;
        self.outcome.logs.push(value.to_string());
        Ok(())
    }

    // ------------------------------------------------------ expressions ----

    fn expression_value(&mut self) -> Result<Value, String> {
        self.expression()
    }

    /// expr := and ('||' and)*
    fn expression(&mut self) -> Result<Value, String> {
        let mut left = self.and_expr()?;
        while self.eat_punct("||") {
            let right = self.and_expr()?;
            // JS returns the operand, not a boolean; truthiness is preserved.
            if !left.truthy() {
                left = right;
            }
        }
        Ok(left)
    }

    fn and_expr(&mut self) -> Result<Value, String> {
        let mut left = self.comparison()?;
        while self.eat_punct("&&") {
            let right = self.comparison()?;
            if left.truthy() {
                left = right;
            }
        }
        Ok(left)
    }

    fn comparison(&mut self) -> Result<Value, String> {
        let left = self.additive()?;
        let op = match self.peek() {
            Some(Tok::Punct(p @ ("===" | "!==" | "==" | "!=" | ">" | ">=" | "<" | "<="))) => *p,
            _ => return Ok(left),
        };
        self.pos += 1;
        let right = self.additive()?;
        let result = compare(&left, op, &right)?;
        Ok(Value::Bool(result))
    }

    /// additive := unary ('+' unary)* — JS semantics: number + number adds,
    /// a string on either side concatenates. Common in log/assert messages.
    fn additive(&mut self) -> Result<Value, String> {
        let mut left = self.unary()?;
        while self.eat_punct("+") {
            let right = self.unary()?;
            left = match (&left, &right) {
                (Value::Number(a), Value::Number(b)) => Value::Number(a + b),
                (Value::String(_), _) | (_, Value::String(_)) => {
                    Value::String(format!("{left}{right}"))
                }
                (a, b) => {
                    return Err(format!(
                        "cannot add {} and {}",
                        a.type_name(),
                        b.type_name()
                    ))
                }
            };
        }
        Ok(left)
    }

    fn unary(&mut self) -> Result<Value, String> {
        if self.eat_punct("!") {
            let v = self.unary()?;
            return Ok(Value::Bool(!v.truthy()));
        }
        self.postfix()
    }

    fn postfix(&mut self) -> Result<Value, String> {
        let mut operand = self.primary()?;
        loop {
            if self.eat_punct(".") {
                let name = match self.peek().cloned() {
                    Some(Tok::Ident(id)) => {
                        self.pos += 1;
                        id
                    }
                    _ => {
                        return Err(format!(
                            "expected a property name near line {}",
                            self.line_of(self.here())
                        ))
                    }
                };
                // Method call?
                if self.peek() == Some(&Tok::Punct("(")) {
                    self.pos += 1;
                    let mut args = Vec::new();
                    if self.peek() != Some(&Tok::Punct(")")) {
                        loop {
                            args.push(self.expression()?);
                            if !self.eat_punct(",") {
                                break;
                            }
                        }
                    }
                    self.expect_punct(")")?;
                    operand = Operand::Value(self.call_method(&operand, &name, &args)?);
                } else {
                    operand = self.member(&operand, &name)?;
                }
            } else if self.eat_punct("[") {
                let key = self.expression()?;
                self.expect_punct("]")?;
                let value = self.materialize(&operand)?;
                operand = Operand::Value(match key {
                    Value::Number(n) => value.index(n as usize).cloned().unwrap_or(Value::Null),
                    Value::String(s) => value.get(&s).cloned().unwrap_or(Value::Null),
                    other => {
                        return Err(format!("cannot index with a {} value", other.type_name()))
                    }
                });
            } else {
                break;
            }
        }
        self.materialize(&operand)
    }

    fn primary(&mut self) -> Result<Operand, String> {
        match self.peek().cloned() {
            Some(Tok::Str(s)) => {
                self.pos += 1;
                Ok(Operand::Value(Value::String(s)))
            }
            Some(Tok::Num(n)) => {
                self.pos += 1;
                Ok(Operand::Value(Value::Number(n)))
            }
            Some(Tok::Ident(id)) => {
                self.pos += 1;
                match id.as_str() {
                    "response" => Ok(Operand::Response),
                    "true" => Ok(Operand::Value(Value::Bool(true))),
                    "false" => Ok(Operand::Value(Value::Bool(false))),
                    "null" | "undefined" => Ok(Operand::Value(Value::Null)),
                    other => Err(format!(
                        "unknown identifier '{other}' near line {} (only 'response' and literals are available)",
                        self.line_of(self.here())
                    )),
                }
            }
            Some(Tok::Punct("(")) => {
                self.pos += 1;
                let v = self.expression()?;
                self.expect_punct(")")?;
                Ok(Operand::Value(v))
            }
            _ => Err(format!(
                "expected an expression near line {}",
                self.line_of(self.here())
            )),
        }
    }

    /// Resolve `operand.name` member access.
    fn member(&self, operand: &Operand, name: &str) -> Result<Operand, String> {
        match operand {
            Operand::Response => match name {
                "status" => Ok(Operand::Value(Value::Number(self.response.status as f64))),
                "body" => Ok(Operand::Value(self.parsed_body())),
                "headers" => Ok(Operand::Headers),
                "contentType" => Ok(Operand::ContentType),
                other => Err(format!("response has no property '{other}'")),
            },
            Operand::ContentType => {
                let (mime, charset) = self.response.content_type();
                match name {
                    "mimeType" => Ok(Operand::Value(Value::String(mime))),
                    "charset" => Ok(Operand::Value(Value::String(charset))),
                    other => Err(format!("contentType has no property '{other}'")),
                }
            }
            Operand::Headers => Err(format!(
                "headers has no property '{name}' (use headers.valueOf(\"Name\"))"
            )),
            Operand::Value(v) => match (v, name) {
                (Value::String(s), "length") => {
                    Ok(Operand::Value(Value::Number(s.chars().count() as f64)))
                }
                (Value::Array(items), "length") => {
                    Ok(Operand::Value(Value::Number(items.len() as f64)))
                }
                (Value::Object(_), key) => {
                    Ok(Operand::Value(v.get(key).cloned().unwrap_or(Value::Null)))
                }
                (Value::Null, key) => Err(format!(
                    "cannot read property '{key}' of null (a parent key was missing)"
                )),
                (other, key) => Err(format!(
                    "cannot read property '{key}' of a {} value",
                    other.type_name()
                )),
            },
        }
    }

    fn call_method(&self, operand: &Operand, name: &str, args: &[Value]) -> Result<Value, String> {
        if let Operand::Headers = operand {
            let arg = args
                .first()
                .and_then(|a| a.as_str())
                .ok_or_else(|| format!("headers.{name}() needs a header-name string"))?;
            return match name {
                "valueOf" => Ok(self
                    .response
                    .header(arg)
                    .map(|v| Value::String(v.to_string()))
                    .unwrap_or(Value::Null)),
                "valuesOf" => Ok(Value::Array(
                    self.response
                        .headers
                        .iter()
                        .filter(|(n, _)| n.eq_ignore_ascii_case(arg))
                        .map(|(_, v)| Value::String(v.clone()))
                        .collect(),
                )),
                other => Err(format!("headers has no method '{other}'")),
            };
        }
        let value = self.materialize(operand)?;
        match (&value, name) {
            (Value::String(s), "includes") => Ok(Value::Bool(
                args.first()
                    .and_then(|a| a.as_str())
                    .map(|needle| s.contains(needle))
                    .ok_or("includes() needs a string argument")?,
            )),
            (Value::String(s), "startsWith") => Ok(Value::Bool(
                args.first()
                    .and_then(|a| a.as_str())
                    .map(|p| s.starts_with(p))
                    .ok_or("startsWith() needs a string argument")?,
            )),
            (Value::String(s), "endsWith") => Ok(Value::Bool(
                args.first()
                    .and_then(|a| a.as_str())
                    .map(|p| s.ends_with(p))
                    .ok_or("endsWith() needs a string argument")?,
            )),
            (Value::String(s), "toLowerCase") => Ok(Value::String(s.to_lowercase())),
            (Value::String(s), "trim") => Ok(Value::String(s.trim().to_string())),
            (Value::Array(items), "includes") => {
                let needle = args.first().ok_or("includes() needs an argument")?;
                Ok(Value::Bool(items.contains(needle)))
            }
            (v, m) => Err(format!("{} value has no method '{m}'", v.type_name())),
        }
    }

    /// Convert response-object operands to values when used as plain values.
    fn materialize(&self, operand: &Operand) -> Result<Value, String> {
        match operand {
            Operand::Value(v) => Ok(v.clone()),
            Operand::Response => Err(
                "'response' cannot be used as a value; access a property like response.status"
                    .into(),
            ),
            Operand::Headers => {
                Err("'response.headers' cannot be used as a value; call valueOf(\"Name\")".into())
            }
            Operand::ContentType => Ok(Value::String(self.response.content_type().0)),
        }
    }

    /// `response.body`: parsed JSON when the content type says JSON (or the
    /// body parses as JSON anyway), else the raw text — same as the editors.
    fn parsed_body(&self) -> Value {
        let text = self.response.body_text();
        let (mime, _) = self.response.content_type();
        if mime.contains("json") || matches!(text.trim_start().chars().next(), Some('{' | '[')) {
            if let Ok(v) = json::parse(&text) {
                return v;
            }
        }
        Value::String(text)
    }
}

fn match_punct(p: &str) -> &'static str {
    // Map to the interned punct strings used by the lexer.
    match p {
        "===" => "===",
        "!==" => "!==",
        "=>" => "=>",
        "==" => "==",
        "!=" => "!=",
        ">=" => ">=",
        "<=" => "<=",
        "&&" => "&&",
        "||" => "||",
        "(" => "(",
        ")" => ")",
        "{" => "{",
        "}" => "}",
        "[" => "[",
        "]" => "]",
        "," => ",",
        ";" => ";",
        "." => ".",
        "!" => "!",
        ">" => ">",
        "<" => "<",
        "=" => "=",
        "+" => "+",
        _ => unreachable!("unknown punct {p}"),
    }
}

fn compare(left: &Value, op: &str, right: &Value) -> Result<bool, String> {
    match op {
        "===" => Ok(strict_eq(left, right)),
        "!==" => Ok(!strict_eq(left, right)),
        "==" => Ok(loose_eq(left, right)),
        "!=" => Ok(!loose_eq(left, right)),
        _ => {
            // Ordering: numbers, or strings lexicographically (JS-alike).
            let ord = match (left, right) {
                (Value::Number(a), Value::Number(b)) => a.partial_cmp(b),
                (Value::String(a), Value::String(b)) => Some(a.cmp(b)),
                (Value::Number(a), Value::String(b)) => {
                    b.parse::<f64>().ok().and_then(|b| a.partial_cmp(&b))
                }
                (Value::String(a), Value::Number(b)) => {
                    a.parse::<f64>().ok().and_then(|a| a.partial_cmp(b))
                }
                _ => None,
            }
            .ok_or_else(|| {
                format!(
                    "cannot order {} against {}",
                    left.type_name(),
                    right.type_name()
                )
            })?;
            Ok(match op {
                ">" => ord.is_gt(),
                ">=" => ord.is_ge(),
                "<" => ord.is_lt(),
                "<=" => ord.is_le(),
                _ => unreachable!(),
            })
        }
    }
}

fn strict_eq(a: &Value, b: &Value) -> bool {
    a == b
}

fn loose_eq(a: &Value, b: &Value) -> bool {
    if strict_eq(a, b) {
        return true;
    }
    match (a, b) {
        (Value::Number(n), Value::String(s)) | (Value::String(s), Value::Number(n)) => {
            s.parse::<f64>().map(|p| p == *n).unwrap_or(false)
        }
        (Value::Bool(x), other) | (other, Value::Bool(x)) => {
            loose_eq(&Value::Number(if *x { 1.0 } else { 0.0 }), other)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resp(status: u16, content_type: &str, body: &str) -> HttpResponse {
        HttpResponse {
            status,
            reason: "OK".into(),
            headers: vec![
                ("Content-Type".to_string(), content_type.to_string()),
                ("X-Request-Id".to_string(), "abc-123".to_string()),
                ("Set-Cookie".to_string(), "a=1".to_string()),
                ("Set-Cookie".to_string(), "b=2".to_string()),
            ],
            body: body.as_bytes().to_vec(),
            final_url: "http://example.test/".into(),
        }
    }

    fn json_resp(body: &str) -> HttpResponse {
        resp(200, "application/json", body)
    }

    #[test]
    fn plus_concatenates_strings_and_adds_numbers() {
        // `client.log("x: " + value)` is idiomatic in real JetBrains files;
        // both concat and numeric addition must work, with JS precedence
        // (`+` binds tighter than `===`).
        let r = json_resp("{\"n\": 40, \"name\": \"amy\"}");
        let out = run("client.log(\"user: \" + response.body.name);", &r).unwrap();
        assert_eq!(out.logs, vec!["user: amy".to_string()]);
        let ok = run("client.assert(response.body.n + 2 === 42);", &r).unwrap();
        assert_eq!(ok.failed(), 0);
        // Number + string coerces to string, like JS.
        let coerced = run("client.assert(response.body.n + \"!\" === \"40!\");", &r).unwrap();
        assert_eq!(coerced.failed(), 0);
    }

    #[test]
    fn plus_on_incompatible_types_is_a_loud_error() {
        // null + bool has no honest meaning here — refuse instead of guessing.
        let r = json_resp("{}");
        let err = run("client.assert(null + true);", &r).unwrap_err();
        assert!(err.contains("cannot add"), "got: {err}");
    }

    #[test]
    fn assert_on_status_passes_and_fails() {
        let r = json_resp("{}");
        let ok = run("client.assert(response.status === 200);", &r).unwrap();
        assert_eq!(ok.failed(), 0);
        let bad = run("client.assert(response.status === 201);", &r).unwrap();
        assert_eq!(bad.failed(), 1);
        assert!(bad.assertions[0]
            .message
            .contains("response.status === 201"));
    }

    #[test]
    fn assert_custom_message_is_used_on_failure() {
        let r = json_resp("{}");
        let out = run(
            "client.assert(response.status === 500, \"want a server error\");",
            &r,
        )
        .unwrap();
        assert!(out.assertions[0].message.contains("want a server error"));
    }

    #[test]
    fn json_body_member_and_index_access() {
        let r = json_resp(r#"{"user": {"name": "amy", "roles": ["admin", "ops"]}}"#);
        let out = run(
            "client.assert(response.body.user.name === 'amy'); client.assert(response.body.user.roles[1] === 'ops');",
            &r,
        )
        .unwrap();
        assert_eq!(out.failed(), 0);
        assert_eq!(out.assertions.len(), 2);
    }

    #[test]
    fn missing_json_key_is_null_but_deep_access_errors() {
        let r = json_resp(r#"{"a": 1}"#);
        let out = run("client.assert(response.body.missing === null);", &r).unwrap();
        assert_eq!(out.failed(), 0);
        let err = run("client.assert(response.body.missing.deep === 1);", &r).unwrap_err();
        assert!(err.contains("null"), "got: {err}");
    }

    #[test]
    fn non_json_body_is_a_string() {
        let r = resp(200, "text/plain", "pong");
        let out = run("client.assert(response.body === 'pong');", &r).unwrap();
        assert_eq!(out.failed(), 0);
    }

    #[test]
    fn header_and_content_type_accessors() {
        let r = resp(200, "application/json; charset=utf-8", "{}");
        let out = run(
            concat!(
                "client.assert(response.headers.valueOf('x-request-id') === 'abc-123');",
                "client.assert(response.headers.valueOf('Nope') === null);",
                "client.assert(response.headers.valuesOf('Set-Cookie').length === 2);",
                "client.assert(response.contentType.mimeType === 'application/json');",
                "client.assert(response.contentType.charset === 'utf-8');",
            ),
            &r,
        )
        .unwrap();
        assert_eq!(out.failed(), 0, "{:?}", out.assertions);
    }

    #[test]
    fn client_test_groups_assertions_under_a_name() {
        let r = json_resp(r#"{"ok": true}"#);
        let out = run(
            "client.test(\"login works\", function () { client.assert(response.status === 200); client.assert(response.body.ok); });",
            &r,
        )
        .unwrap();
        assert_eq!(out.assertions.len(), 2);
        assert!(out
            .assertions
            .iter()
            .all(|a| a.test.as_deref() == Some("login works")));
    }

    #[test]
    fn arrow_function_and_empty_test_bodies() {
        let r = json_resp("{}");
        let out = run(
            "client.test('arrow', () => { client.assert(response.status === 200); });",
            &r,
        )
        .unwrap();
        assert_eq!(out.failed(), 0);
        // An empty body is recorded as a vacuous pass, not silently dropped.
        let out = run("client.test('todo', function () { });", &r).unwrap();
        assert_eq!(out.assertions.len(), 1);
        assert!(out.assertions[0].passed);
    }

    #[test]
    fn runtime_error_inside_test_fails_that_test_only() {
        let r = json_resp(r#"{"a": 1}"#);
        let out = run(
            concat!(
                "client.test('broken', function () { client.assert(response.body.a.b.c === 1); });",
                "client.assert(response.status === 200);"
            ),
            &r,
        )
        .unwrap();
        assert_eq!(out.failed(), 1);
        assert_eq!(out.assertions[0].test.as_deref(), Some("broken"));
        assert!(out.assertions[1].passed);
    }

    #[test]
    fn global_set_captures_values() {
        let r = json_resp(r#"{"token": "t-1", "count": 3}"#);
        let out = run(
            "client.global.set('token', response.body.token); client.global.set('count', response.body.count);",
            &r,
        )
        .unwrap();
        assert_eq!(
            out.globals,
            vec![
                ("token".to_string(), "t-1".to_string()),
                ("count".to_string(), "3".to_string()),
            ]
        );
    }

    #[test]
    fn client_log_records_output_and_comments_are_skipped() {
        let r = json_resp(r#"{"n": 42}"#);
        let out = run(
            "// leading note\nclient.log(response.body.n); client.log('done'); // trailing",
            &r,
        )
        .unwrap();
        assert_eq!(out.logs, vec!["42".to_string(), "done".to_string()]);
    }

    #[test]
    fn string_helpers_and_length() {
        let r = resp(200, "text/plain", "hello world");
        let out = run(
            concat!(
                "client.assert(response.body.includes('lo wo'));",
                "client.assert(response.body.startsWith('hello'));",
                "client.assert(response.body.endsWith('world'));",
                "client.assert(response.body.length === 11);",
            ),
            &r,
        )
        .unwrap();
        assert_eq!(out.failed(), 0, "{:?}", out.assertions);
    }

    #[test]
    fn logical_and_or_and_negation() {
        let r = json_resp(r#"{"a": 1, "b": 0}"#);
        let out = run(
            concat!(
                "client.assert(response.body.a === 1 && response.status === 200);",
                "client.assert(response.body.b === 9 || response.body.a === 1);",
                "client.assert(!(response.body.a === 2));",
            ),
            &r,
        )
        .unwrap();
        assert_eq!(out.failed(), 0, "{:?}", out.assertions);
    }

    #[test]
    fn ordering_loose_and_strict_comparisons() {
        let r = json_resp(r#"{"n": 5}"#);
        let out = run(
            concat!(
                "client.assert(response.body.n > 4);",
                "client.assert(response.body.n <= 5);",
                "client.assert(response.status >= 200);",
                "client.assert(response.body.n == '5');",
                "client.assert(!(response.body.n === '5'));",
            ),
            &r,
        )
        .unwrap();
        assert_eq!(out.failed(), 0, "{:?}", out.assertions);
    }

    #[test]
    fn unsupported_statement_is_a_clear_error() {
        let r = json_resp("{}");
        let err = run("var x = 1;", &r).unwrap_err();
        assert!(err.contains("unsupported"), "got: {err}");
        let err = run("client.fetch('x');", &r).unwrap_err();
        assert!(err.contains("client"), "got: {err}");
    }

    #[test]
    fn bracket_access_with_string_key() {
        let r = json_resp(r#"{"weird key": "v"}"#);
        let out = run("client.assert(response.body['weird key'] === 'v');", &r).unwrap();
        assert_eq!(out.failed(), 0);
    }
}
