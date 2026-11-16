//! Just enough JSON for the benchmark side files: run notes and compare projects.
//!
//! This crate takes no dependencies (ADR 0010), and the files involved are small, flat
//! and written by this program, so a hundred-odd lines of parser is cheaper than an
//! exception to that rule. It reads any well-formed JSON - a hand-edited project file
//! must not be rejected for using a feature we never write - but it is not fast and
//! does not try to be.

use std::collections::BTreeMap;
use std::fmt::Write;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Value>),
    /// Ordered by key, so writing a value twice gives the same bytes.
    Object(BTreeMap<String, Value>),
}

impl Value {
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(m) => m.get(key),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Number(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(a) => Some(a),
            _ => None,
        }
    }

    /// A string field of an object, or `None` if absent, null or not a string.
    pub fn str_field(&self, key: &str) -> Option<String> {
        self.get(key).and_then(Value::as_str).map(str::to_string)
    }

    /// Indented, for files a person may open.
    pub fn pretty(&self) -> String {
        let mut out = String::new();
        self.write(&mut out, 0);
        out.push('\n');
        out
    }

    fn write(&self, out: &mut String, depth: usize) {
        let pad = |out: &mut String, d: usize| {
            for _ in 0..d {
                out.push_str("  ");
            }
        };
        match self {
            Value::Null => out.push_str("null"),
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Value::Number(n) => out.push_str(&number(*n)),
            Value::String(s) => out.push_str(&quote(s)),
            Value::Array(a) if a.is_empty() => out.push_str("[]"),
            Value::Array(a) => {
                out.push_str("[\n");
                for (i, v) in a.iter().enumerate() {
                    pad(out, depth + 1);
                    v.write(out, depth + 1);
                    out.push_str(if i + 1 < a.len() { ",\n" } else { "\n" });
                }
                pad(out, depth);
                out.push(']');
            }
            Value::Object(m) if m.is_empty() => out.push_str("{}"),
            Value::Object(m) => {
                out.push_str("{\n");
                for (i, (k, v)) in m.iter().enumerate() {
                    pad(out, depth + 1);
                    out.push_str(&quote(k));
                    out.push_str(": ");
                    v.write(out, depth + 1);
                    out.push_str(if i + 1 < m.len() { ",\n" } else { "\n" });
                }
                pad(out, depth);
                out.push('}');
            }
        }
    }
}

/// A finite number as JSON writes it; JSON has no NaN or infinity, so those are null.
pub fn number(n: f64) -> String {
    if !n.is_finite() {
        return "null".into();
    }
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

/// A JSON string literal. Also escapes `<`, `>` and `&`, so the result can be placed
/// inside an HTML `<script>` element without closing it.
pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '<' | '>' | '&' => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

pub fn parse(text: &str) -> Result<Value, String> {
    let mut p = Parser {
        s: text.as_bytes(),
        i: 0,
    };
    let v = p.value()?;
    p.ws();
    if p.i != p.s.len() {
        return Err(format!("unexpected text at byte {}", p.i));
    }
    Ok(v)
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn ws(&mut self) {
        while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }

    fn eat(&mut self, lit: &str) -> bool {
        if self.s[self.i..].starts_with(lit.as_bytes()) {
            self.i += lit.len();
            true
        } else {
            false
        }
    }

    fn value(&mut self) -> Result<Value, String> {
        self.ws();
        match self.s.get(self.i) {
            None => Err("unexpected end".into()),
            Some(b'{') => {
                self.i += 1;
                let mut m = BTreeMap::new();
                self.ws();
                if self.eat("}") {
                    return Ok(Value::Object(m));
                }
                loop {
                    self.ws();
                    let k = self.string()?;
                    self.ws();
                    if !self.eat(":") {
                        return Err(format!("expected ':' at byte {}", self.i));
                    }
                    let v = self.value()?;
                    m.insert(k, v);
                    self.ws();
                    if self.eat(",") {
                        continue;
                    }
                    if self.eat("}") {
                        return Ok(Value::Object(m));
                    }
                    return Err(format!("expected ',' or '}}' at byte {}", self.i));
                }
            }
            Some(b'[') => {
                self.i += 1;
                let mut a = Vec::new();
                self.ws();
                if self.eat("]") {
                    return Ok(Value::Array(a));
                }
                loop {
                    a.push(self.value()?);
                    self.ws();
                    if self.eat(",") {
                        continue;
                    }
                    if self.eat("]") {
                        return Ok(Value::Array(a));
                    }
                    return Err(format!("expected ',' or ']' at byte {}", self.i));
                }
            }
            Some(b'"') => Ok(Value::String(self.string()?)),
            Some(b't') if self.eat("true") => Ok(Value::Bool(true)),
            Some(b'f') if self.eat("false") => Ok(Value::Bool(false)),
            Some(b'n') if self.eat("null") => Ok(Value::Null),
            Some(_) => {
                let start = self.i;
                while self.i < self.s.len()
                    && matches!(
                        self.s[self.i],
                        b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9'
                    )
                {
                    self.i += 1;
                }
                std::str::from_utf8(&self.s[start..self.i])
                    .ok()
                    .and_then(|t| t.parse::<f64>().ok())
                    .map(Value::Number)
                    .ok_or_else(|| format!("bad value at byte {start}"))
            }
        }
    }

    fn string(&mut self) -> Result<String, String> {
        if !self.eat("\"") {
            return Err(format!("expected a string at byte {}", self.i));
        }
        let mut out: Vec<u8> = Vec::new();
        loop {
            let Some(&b) = self.s.get(self.i) else {
                return Err("unterminated string".into());
            };
            self.i += 1;
            match b {
                b'"' => break,
                b'\\' => {
                    let Some(&e) = self.s.get(self.i) else {
                        return Err("unterminated escape".into());
                    };
                    self.i += 1;
                    let c = match e {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => {
                            let mut code = self.hex4()?;
                            // A surrogate pair is two escapes making one character.
                            if (0xd800..0xdc00).contains(&code) && self.eat("\\u") {
                                let low = self.hex4()?;
                                code = 0x10000
                                    + ((code - 0xd800) << 10)
                                    + (low.wrapping_sub(0xdc00) & 0x3ff);
                            }
                            char::from_u32(code).unwrap_or('\u{fffd}')
                        }
                        _ => return Err(format!("bad escape at byte {}", self.i)),
                    };
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                }
                b => out.push(b),
            }
        }
        String::from_utf8(out).map_err(|_| "string is not UTF-8".into())
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let digits = self
            .s
            .get(self.i..self.i + 4)
            .and_then(|d| std::str::from_utf8(d).ok())
            .and_then(|d| u32::from_str_radix(d, 16).ok())
            .ok_or_else(|| format!("bad \\u escape at byte {}", self.i))?;
        self.i += 4;
        Ok(digits)
    }
}

/// Build an object from `(key, value)` pairs, leaving out the `None`s - an absent field
/// and an empty one mean the same in every file this writes.
pub fn object<'a>(fields: impl IntoIterator<Item = (&'a str, Option<Value>)>) -> Value {
    Value::Object(
        fields
            .into_iter()
            .filter_map(|(k, v)| v.map(|v| (k.to_string(), v)))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_is_written_reads_back() {
        let v = object([
            ("name", Some(Value::String("CP2077 \"quiet\"\nrun".into()))),
            ("baseline", Some(Value::Number(2.0))),
            ("ratio", Some(Value::Number(-0.125))),
            ("skipped", None),
            (
                "runs",
                Some(Value::Array(vec![
                    Value::Bool(true),
                    Value::Null,
                    Value::String("é ✓ 🎮".into()),
                ])),
            ),
        ]);
        assert_eq!(parse(&v.pretty()).unwrap(), v);
        assert!(v.get("skipped").is_none());
    }

    #[test]
    fn script_closing_text_cannot_escape_a_string() {
        let q = quote("</script><b>&");
        assert!(!q.contains('<') && !q.contains('>') && !q.contains('&'));
        assert_eq!(parse(&q).unwrap().as_str(), Some("</script><b>&"));
    }

    #[test]
    fn json_written_by_someone_else_is_read() {
        let v = parse(r#" { "a" : [1, 2.5e1, -3] , "b": "\u00e9\ud83c\udfae\/" } "#).unwrap();
        assert_eq!(
            v.get("a").unwrap().as_array().unwrap()[1].as_f64(),
            Some(25.0)
        );
        assert_eq!(v.str_field("b").as_deref(), Some("é🎮/"));
    }

    #[test]
    fn broken_json_is_an_error_not_a_panic() {
        for bad in [
            "",
            "{",
            "{\"a\":}",
            "[1,]x",
            "\"abc",
            "{\"a\" 1}",
            "nul",
            "\"\\u12\"",
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn numbers_are_written_without_noise() {
        assert_eq!(number(3.0), "3");
        assert_eq!(number(0.5), "0.5");
        assert_eq!(number(f64::NAN), "null");
    }
}
