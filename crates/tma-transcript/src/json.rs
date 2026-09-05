//! A small recursive-descent JSON reader, sized for one transcript record at a time.
//!
//! The workspace has no `serde_json` and deliberately hand-rolls its JSON writer
//! ([`tma_runtime::json`]); this is the matching reader. It keeps object keys in source order
//! (tool argument key lists are part of the event contract) and keeps every number as its source
//! literal, so re-serializing a value is byte-faithful for integers.
//!
//! A malformed document is an [`Err`], never a panic, and nesting is capped: a transcript record is
//! untrusted input written by another process, and an unbounded recursive parser turns a deep
//! literal into a stack overflow.

use std::fmt::Write as _;

/// How deep an object/array nest may go before the parse is refused.
const MAX_DEPTH: usize = 64;

/// A parsed JSON value. Objects keep insertion order, so `arg_keys` is the store's own key order
/// rather than a hash order that would churn between runs.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Value {
    Null,
    Bool(bool),
    /// The number's source literal, kept verbatim so re-serialization does not turn `1000` into
    /// `1000.0` and does not lose precision past f64.
    Num(String),
    Str(String),
    Arr(Vec<Value>),
    Obj(Vec<(String, Value)>),
}

/// Why a record would not parse. The reader turns this into an `Unknown` event and a counter bump,
/// never an error that ends the read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParseError {
    pub(crate) at: usize,
    pub(crate) what: &'static str,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} at byte {}", self.what, self.at)
    }
}

impl Value {
    /// The value at `key`, for an object; `None` for every other kind.
    pub(crate) fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Obj(pairs) => pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// Walk a chain of object keys (`v.path(["info", "last_token_usage"])`).
    pub(crate) fn path(&self, keys: &[&str]) -> Option<&Value> {
        keys.iter().try_fold(self, |v, k| v.get(k))
    }

    pub(crate) fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub(crate) fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub(crate) fn as_arr(&self) -> Option<&[Value]> {
        match self {
            Value::Arr(items) => Some(items),
            _ => None,
        }
    }

    /// A non-negative integer, from the source literal. A fractional or negative literal is `None`
    /// rather than a silent truncation.
    pub(crate) fn as_u64(&self) -> Option<u64> {
        match self {
            Value::Num(raw) => raw.parse().ok(),
            _ => None,
        }
    }

    /// The object's keys in source order; empty for every other kind.
    pub(crate) fn keys(&self) -> Vec<String> {
        match self {
            Value::Obj(pairs) => pairs.iter().map(|(k, _)| k.clone()).collect(),
            _ => Vec::new(),
        }
    }

    /// Compact JSON text for this value. This is the definition of a value's "bytes" everywhere in
    /// the crate: source whitespace is not preserved, so the count is a property of the value
    /// rather than of how the store happened to format it.
    pub(crate) fn to_compact(&self) -> String {
        let mut out = String::new();
        self.write_compact(&mut out);
        out
    }

    fn write_compact(&self, out: &mut String) {
        match self {
            Value::Null => out.push_str("null"),
            Value::Bool(true) => out.push_str("true"),
            Value::Bool(false) => out.push_str("false"),
            Value::Num(raw) => out.push_str(raw),
            Value::Str(s) => write_string(out, s),
            Value::Arr(items) => {
                out.push('[');
                for (i, v) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    v.write_compact(out);
                }
                out.push(']');
            }
            Value::Obj(pairs) => {
                out.push('{');
                for (i, (k, v)) in pairs.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_string(out, k);
                    out.push(':');
                    v.write_compact(out);
                }
                out.push('}');
            }
        }
    }
}

fn write_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Parse one complete JSON document. Trailing whitespace is fine; trailing non-whitespace is not.
pub(crate) fn parse(src: &str) -> Result<Value, ParseError> {
    let mut p = Parser {
        b: src.as_bytes(),
        i: 0,
        depth: 0,
    };
    p.ws();
    let v = p.value()?;
    p.ws();
    if p.i != p.b.len() {
        return Err(p.err("trailing bytes after the document"));
    }
    Ok(v)
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
    depth: usize,
}

impl<'a> Parser<'a> {
    fn err(&self, what: &'static str) -> ParseError {
        ParseError { at: self.i, what }
    }

    fn ws(&mut self) {
        while matches!(self.b.get(self.i), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.i += 1;
        }
    }

    fn eat(&mut self, c: u8) -> bool {
        if self.b.get(self.i) == Some(&c) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn lit(&mut self, word: &[u8]) -> bool {
        if self.b[self.i..].starts_with(word) {
            self.i += word.len();
            true
        } else {
            false
        }
    }

    fn value(&mut self) -> Result<Value, ParseError> {
        match self.b.get(self.i) {
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => Ok(Value::Str(self.string()?)),
            Some(b'n') if self.lit(b"null") => Ok(Value::Null),
            Some(b't') if self.lit(b"true") => Ok(Value::Bool(true)),
            Some(b'f') if self.lit(b"false") => Ok(Value::Bool(false)),
            Some(c) if *c == b'-' || c.is_ascii_digit() => self.number(),
            _ => Err(self.err("expected a value")),
        }
    }

    fn object(&mut self) -> Result<Value, ParseError> {
        self.enter()?;
        self.i += 1; // '{'
        let mut pairs = Vec::new();
        self.ws();
        if self.eat(b'}') {
            self.depth -= 1;
            return Ok(Value::Obj(pairs));
        }
        loop {
            self.ws();
            let key = self.string()?;
            self.ws();
            if !self.eat(b':') {
                return Err(self.err("expected ':' after an object key"));
            }
            self.ws();
            pairs.push((key, self.value()?));
            self.ws();
            if self.eat(b',') {
                continue;
            }
            if self.eat(b'}') {
                self.depth -= 1;
                return Ok(Value::Obj(pairs));
            }
            return Err(self.err("expected ',' or '}' in an object"));
        }
    }

    fn array(&mut self) -> Result<Value, ParseError> {
        self.enter()?;
        self.i += 1; // '['
        let mut items = Vec::new();
        self.ws();
        if self.eat(b']') {
            self.depth -= 1;
            return Ok(Value::Arr(items));
        }
        loop {
            self.ws();
            items.push(self.value()?);
            self.ws();
            if self.eat(b',') {
                continue;
            }
            if self.eat(b']') {
                self.depth -= 1;
                return Ok(Value::Arr(items));
            }
            return Err(self.err("expected ',' or ']' in an array"));
        }
    }

    fn enter(&mut self) -> Result<(), ParseError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.err("nesting deeper than the parser's cap"));
        }
        Ok(())
    }

    fn string(&mut self) -> Result<String, ParseError> {
        if !self.eat(b'"') {
            return Err(self.err("expected a string"));
        }
        let mut out = String::new();
        loop {
            let Some(&c) = self.b.get(self.i) else {
                return Err(self.err("unterminated string"));
            };
            match c {
                b'"' => {
                    self.i += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.i += 1;
                    self.escape(&mut out)?;
                }
                // A raw control byte is malformed JSON; the store never writes one, and accepting
                // it here would let a truncated write look like a valid record.
                0x00..=0x1f => return Err(self.err("raw control byte in a string")),
                _ => {
                    let start = self.i;
                    while self
                        .b
                        .get(self.i)
                        .is_some_and(|c| !matches!(c, b'"' | b'\\' | 0x00..=0x1f))
                    {
                        self.i += 1;
                    }
                    match std::str::from_utf8(&self.b[start..self.i]) {
                        Ok(s) => out.push_str(s),
                        Err(_) => return Err(self.err("invalid UTF-8 in a string")),
                    }
                }
            }
        }
    }

    fn escape(&mut self, out: &mut String) -> Result<(), ParseError> {
        let Some(&c) = self.b.get(self.i) else {
            return Err(self.err("truncated escape"));
        };
        self.i += 1;
        let ch = match c {
            b'"' => '"',
            b'\\' => '\\',
            b'/' => '/',
            b'b' => '\u{8}',
            b'f' => '\u{c}',
            b'n' => '\n',
            b'r' => '\r',
            b't' => '\t',
            b'u' => return self.unicode_escape(out),
            _ => return Err(self.err("unknown escape")),
        };
        out.push(ch);
        Ok(())
    }

    /// `\uXXXX`, joining a surrogate pair when the high half is followed by its low half. A lone
    /// surrogate becomes U+FFFD rather than failing: it is a store bug, not a reason to drop a record.
    fn unicode_escape(&mut self, out: &mut String) -> Result<(), ParseError> {
        let hi = self.hex4()?;
        let cp = if (0xd800..0xdc00).contains(&hi) {
            let saved = self.i;
            if self.eat(b'\\') && self.eat(b'u') {
                let lo = self.hex4()?;
                if (0xdc00..0xe000).contains(&lo) {
                    0x10000 + ((hi - 0xd800) << 10) + (lo - 0xdc00)
                } else {
                    self.i = saved;
                    0xfffd
                }
            } else {
                self.i = saved;
                0xfffd
            }
        } else {
            hi
        };
        out.push(char::from_u32(cp).unwrap_or('\u{fffd}'));
        Ok(())
    }

    fn hex4(&mut self) -> Result<u32, ParseError> {
        let end = self.i + 4;
        if end > self.b.len() {
            return Err(self.err("truncated \\u escape"));
        }
        let mut n = 0u32;
        for &c in &self.b[self.i..end] {
            let d = (c as char).to_digit(16).ok_or(ParseError {
                at: self.i,
                what: "non-hex digit in a \\u escape",
            })?;
            n = n * 16 + d;
        }
        self.i = end;
        Ok(n)
    }

    fn number(&mut self) -> Result<Value, ParseError> {
        let start = self.i;
        self.eat(b'-');
        // JSON's integer part is `0` or a non-zero leading digit: `01` is malformed, and accepting
        // it would let a half-written byte run read as a number.
        if self.eat(b'0') {
            if self.b.get(self.i).is_some_and(u8::is_ascii_digit) {
                return Err(self.err("leading zero in a number"));
            }
        } else if !self.digits() {
            return Err(self.err("expected a digit"));
        }
        if self.eat(b'.') && !self.digits() {
            return Err(self.err("expected a digit after '.'"));
        }
        if matches!(self.b.get(self.i), Some(b'e' | b'E')) {
            self.i += 1;
            let _ = self.eat(b'+') || self.eat(b'-');
            if !self.digits() {
                return Err(self.err("expected a digit in the exponent"));
            }
        }
        // Every byte consumed above is ASCII, so the slice is valid UTF-8 by construction.
        let raw = String::from_utf8_lossy(&self.b[start..self.i]).into_owned();
        Ok(Value::Num(raw))
    }

    fn digits(&mut self) -> bool {
        let start = self.i;
        while self.b.get(self.i).is_some_and(u8::is_ascii_digit) {
            self.i += 1;
        }
        self.i > start
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_shapes_a_record_uses() {
        let v = parse(r#"{"a":1,"b":[true,null,"x"],"c":{"d":-2.5e3}}"#).unwrap();
        assert_eq!(v.get("a").and_then(Value::as_u64), Some(1));
        assert_eq!(v.get("b").and_then(Value::as_arr).map(<[_]>::len), Some(3));
        assert_eq!(
            v.path(&["c", "d"]).map(Value::to_compact).as_deref(),
            Some("-2.5e3")
        );
        assert_eq!(v.keys(), vec!["a", "b", "c"]);
    }

    #[test]
    fn keeps_integer_literals_byte_faithful() {
        let v = parse(r#"{"n":1000,"big":18446744073709551615}"#).unwrap();
        assert_eq!(v.to_compact(), r#"{"n":1000,"big":18446744073709551615}"#);
        assert_eq!(
            v.get("big").and_then(Value::as_u64),
            Some(18_446_744_073_709_551_615)
        );
    }

    #[test]
    fn decodes_escapes_and_surrogate_pairs() {
        let v = parse(r#""a\nbA😀""#).unwrap();
        assert_eq!(v.as_str(), Some("a\nbA\u{1f600}"));
        // A lone high surrogate degrades to the replacement char instead of failing the record.
        assert_eq!(parse(r#""\ud83d""#).unwrap().as_str(), Some("\u{fffd}"));
    }

    #[test]
    fn rejects_malformed_input_without_panicking() {
        for bad in [
            "{",
            r#"{"a":}"#,
            r#"{"a" 1}"#,
            "[1,]",
            r#""unterminated"#,
            "01",
            r#"{"a":1}x"#,
        ] {
            assert!(parse(bad).is_err(), "{bad:?} must not parse");
        }
    }

    #[test]
    fn refuses_nesting_past_the_cap() {
        let deep = format!("{}{}", "[".repeat(200), "]".repeat(200));
        assert_eq!(
            parse(&deep).unwrap_err().what,
            "nesting deeper than the parser's cap"
        );
    }
}
