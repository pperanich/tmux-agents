//! A minimal, dependency-free JSON value model with a *deterministic* pretty-printer, used
//! only by the Claude `install-hooks` adapter to edit `~/.claude/settings.json`.
//!
//! Why not `serde_json`: the acceptance requirement is a **byte-identical** install/uninstall
//! round-trip. A general serializer that reorders keys or normalizes whitespace defeats
//! that; here we preserve object key order and number text verbatim, and control the exact
//! output bytes, so removing precisely what we added restores the original file. The
//! existing `json.rs` is a *writer* only (for `--json` output); this module also parses.

use std::fmt::Write as _;

/// A parsed JSON value. Objects keep insertion order (a `Vec`, not a map) and numbers keep
/// their original text, so serialize∘parse is the identity on canonical input.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Value {
    Null,
    Bool(bool),
    /// The original numeric token (e.g. `1`, `-2.5`, `1e9`), preserved verbatim.
    Num(String),
    Str(String),
    Arr(Vec<Value>),
    Obj(Vec<(String, Value)>),
}

impl Value {
    pub(super) fn as_object_mut(&mut self) -> Option<&mut Vec<(String, Value)>> {
        match self {
            Value::Obj(o) => Some(o),
            _ => None,
        }
    }

    pub(super) fn as_array_mut(&mut self) -> Option<&mut Vec<Value>> {
        match self {
            Value::Arr(a) => Some(a),
            _ => None,
        }
    }

    /// The value for `key` in an object, if this is an object with that key.
    pub(super) fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Obj(o) => o.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub(super) fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    /// Insert or replace `key` in an object, appending when new (preserving prior order).
    pub(super) fn obj_set(&mut self, key: &str, value: Value) {
        if let Value::Obj(o) = self {
            if let Some(slot) = o.iter_mut().find(|(k, _)| k == key) {
                slot.1 = value;
            } else {
                o.push((key.to_string(), value));
            }
        }
    }

    /// Remove `key` from an object, returning whether it was present.
    pub(super) fn obj_remove(&mut self, key: &str) -> bool {
        if let Value::Obj(o) = self {
            let before = o.len();
            o.retain(|(k, _)| k != key);
            return o.len() != before;
        }
        false
    }
}

/// Maximum object/array nesting depth. The parser recurses, so an unbounded document would overflow
/// the stack (an abort, not the parse error this module promises); past the cap it returns an error
/// and the installer declines the file. Generous for a real `settings.json`, but bounded.
const MAX_DEPTH: usize = 128;

/// Parse a JSON document. Returns an error message on malformed input (the installer then refuses
/// to touch the file rather than risk clobbering it), including on nesting past [`MAX_DEPTH`].
pub(super) fn parse(input: &str) -> Result<Value, String> {
    let mut p = Parser {
        chars: input.chars().collect(),
        pos: 0,
        depth: 0,
    };
    p.skip_ws();
    let v = p.value()?;
    p.skip_ws();
    if p.pos != p.chars.len() {
        return Err(format!("trailing content at byte offset {}", p.pos));
    }
    Ok(v)
}

struct Parser {
    chars: Vec<char>,
    pos: usize,
    /// Current object/array nesting depth, bounded by [`MAX_DEPTH`] in [`Parser::value`].
    depth: usize,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t' | '\n' | '\r')) {
            self.pos += 1;
        }
    }

    fn value(&mut self) -> Result<Value, String> {
        // Bound recursion: increment on entry, decrement on exit so siblings share one level (a
        // long flat array does not accumulate depth). Past the cap, return the promised parse error.
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(format!(
                "maximum nesting depth {MAX_DEPTH} exceeded at {}",
                self.pos
            ));
        }
        let result = match self.peek() {
            Some('{') => self.object(),
            Some('[') => self.array(),
            Some('"') => Ok(Value::Str(self.string()?)),
            Some('t') | Some('f') => self.boolean(),
            Some('n') => self.null(),
            Some(c) if c == '-' || c.is_ascii_digit() => self.number(),
            Some(c) => Err(format!("unexpected character {c:?} at {}", self.pos)),
            None => Err("unexpected end of input".to_string()),
        };
        self.depth -= 1;
        result
    }

    fn object(&mut self) -> Result<Value, String> {
        self.bump(); // {
        let mut out = Vec::new();
        self.skip_ws();
        if self.peek() == Some('}') {
            self.bump();
            return Ok(Value::Obj(out));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some('"') {
                return Err(format!("expected string key at {}", self.pos));
            }
            let key = self.string()?;
            self.skip_ws();
            if self.bump() != Some(':') {
                return Err(format!("expected ':' after key at {}", self.pos));
            }
            self.skip_ws();
            let val = self.value()?;
            out.push((key, val));
            self.skip_ws();
            match self.bump() {
                Some(',') => continue,
                Some('}') => break,
                other => return Err(format!("expected ',' or '}}', got {other:?}")),
            }
        }
        Ok(Value::Obj(out))
    }

    fn array(&mut self) -> Result<Value, String> {
        self.bump(); // [
        let mut out = Vec::new();
        self.skip_ws();
        if self.peek() == Some(']') {
            self.bump();
            return Ok(Value::Arr(out));
        }
        loop {
            self.skip_ws();
            out.push(self.value()?);
            self.skip_ws();
            match self.bump() {
                Some(',') => continue,
                Some(']') => break,
                other => return Err(format!("expected ',' or ']', got {other:?}")),
            }
        }
        Ok(Value::Arr(out))
    }

    fn string(&mut self) -> Result<String, String> {
        self.bump(); // opening quote
        let mut out = String::new();
        loop {
            match self.bump() {
                None => return Err("unterminated string".to_string()),
                Some('"') => return Ok(out),
                Some('\\') => match self.bump() {
                    Some('"') => out.push('"'),
                    Some('\\') => out.push('\\'),
                    Some('/') => out.push('/'),
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some('r') => out.push('\r'),
                    Some('b') => out.push('\u{8}'),
                    Some('f') => out.push('\u{c}'),
                    Some('u') => {
                        let hi = self.read_hex4()?;
                        // UTF-16 surrogate handling: a high surrogate (0xD800–0xDBFF) needs a
                        // following `\uXXXX` low surrogate to form an astral scalar. Malformed cases
                        // emit U+FFFD; a valid pair must NOT split (that would corrupt settings.json).
                        if (0xD800..=0xDBFF).contains(&hi) {
                            match self.peek_low_surrogate_escape()? {
                                Some(lo) => {
                                    let c = 0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00);
                                    out.push(char::from_u32(c).unwrap_or('\u{fffd}'));
                                }
                                // Lone high surrogate: any follower is left in the stream to
                                // decode on its own.
                                None => out.push('\u{fffd}'),
                            }
                        } else if (0xDC00..=0xDFFF).contains(&hi) {
                            // Lone low surrogate.
                            out.push('\u{fffd}');
                        } else {
                            out.push(char::from_u32(hi).unwrap_or('\u{fffd}'));
                        }
                    }
                    other => return Err(format!("invalid escape \\{other:?}")),
                },
                Some(c) => out.push(c),
            }
        }
    }

    /// Read exactly four hex digits as a UTF-16 code unit (called after consuming `\u`).
    fn read_hex4(&mut self) -> Result<u32, String> {
        let mut code = 0u32;
        for _ in 0..4 {
            let d = self.bump().ok_or("truncated \\u escape")?;
            code = code * 16 + d.to_digit(16).ok_or("invalid \\u hex digit")?;
        }
        Ok(code)
    }

    /// After a high surrogate, consume a following `\uXXXX` low surrogate and return `Some(low)`;
    /// otherwise consume nothing and return `None`, leaving any other follower to decode next loop.
    fn peek_low_surrogate_escape(&mut self) -> Result<Option<u32>, String> {
        if self.peek() != Some('\\') {
            return Ok(None);
        }
        let save = self.pos;
        self.bump(); // '\'
        if self.peek() != Some('u') {
            self.pos = save; // not a `\u` escape: put the backslash back
            return Ok(None);
        }
        self.bump(); // 'u'
        let lo = self.read_hex4()?;
        if (0xDC00..=0xDFFF).contains(&lo) {
            Ok(Some(lo))
        } else {
            // A valid `\uXXXX` that is not a low surrogate — keep it consumed and hand it back
            // so it decodes as its own scalar (do not corrupt it).
            self.pos = save;
            Ok(None)
        }
    }

    fn number(&mut self) -> Result<Value, String> {
        let start = self.pos;
        while matches!(self.peek(), Some(c) if c == '-' || c == '+' || c == '.' || c == 'e' || c == 'E' || c.is_ascii_digit())
        {
            self.pos += 1;
        }
        Ok(Value::Num(self.chars[start..self.pos].iter().collect()))
    }

    fn boolean(&mut self) -> Result<Value, String> {
        if self.take_lit("true") {
            Ok(Value::Bool(true))
        } else if self.take_lit("false") {
            Ok(Value::Bool(false))
        } else {
            Err(format!("invalid literal at {}", self.pos))
        }
    }

    fn null(&mut self) -> Result<Value, String> {
        if self.take_lit("null") {
            Ok(Value::Null)
        } else {
            Err(format!("invalid literal at {}", self.pos))
        }
    }

    fn take_lit(&mut self, lit: &str) -> bool {
        let end = self.pos + lit.len();
        if end <= self.chars.len() && self.chars[self.pos..end].iter().collect::<String>() == lit {
            self.pos = end;
            true
        } else {
            false
        }
    }
}

/// Serialize with two-space indentation and a trailing newline. Deterministic (key order and number
/// text preserved), so it is a left inverse of [`parse`] on its own output — the round-trip property.
pub(super) fn to_pretty(value: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, value, 0);
    out.push('\n');
    out
}

fn write_value(out: &mut String, value: &Value, indent: usize) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Num(n) => out.push_str(n),
        Value::Str(s) => write_string(out, s),
        Value::Arr(a) => {
            if a.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, v) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push('\n');
                pad(out, indent + 1);
                write_value(out, v, indent + 1);
            }
            out.push('\n');
            pad(out, indent);
            out.push(']');
        }
        Value::Obj(o) => {
            if o.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (i, (k, v)) in o.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push('\n');
                pad(out, indent + 1);
                write_string(out, k);
                out.push_str(": ");
                write_value(out, v, indent + 1);
            }
            out.push('\n');
            pad(out, indent);
            out.push('}');
        }
    }
}

fn pad(out: &mut String, indent: usize) {
    for _ in 0..indent {
        out.push_str("  ");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_canonical_forms() {
        for src in [
            "{}\n",
            "[]\n",
            "{\n  \"a\": 1\n}\n",
            "{\n  \"model\": \"opus\",\n  \"nested\": {\n    \"x\": [\n      1,\n      2\n    ]\n  }\n}\n",
        ] {
            let v = parse(src).unwrap();
            assert_eq!(to_pretty(&v), src, "round-trip must be byte-identical");
        }
    }

    #[test]
    fn preserves_number_text_and_key_order() {
        let v = parse("{\"z\": 1e9, \"a\": -2.50}").unwrap();
        assert_eq!(to_pretty(&v), "{\n  \"z\": 1e9,\n  \"a\": -2.50\n}\n");
    }

    #[test]
    fn obj_set_appends_then_replaces() {
        let mut v = parse("{\"a\": 1}").unwrap();
        v.obj_set("b", Value::Bool(true));
        assert_eq!(v.get("b"), Some(&Value::Bool(true)));
        v.obj_set("a", Value::Num("2".into()));
        assert_eq!(v.get("a"), Some(&Value::Num("2".into())));
        // order preserved: a before b
        assert_eq!(to_pretty(&v), "{\n  \"a\": 2,\n  \"b\": true\n}\n");
    }

    #[test]
    fn rejects_malformed() {
        assert!(parse("{").is_err());
        assert!(parse("{\"a\":}").is_err());
        assert!(parse("nul").is_err());
        assert!(parse("{} junk").is_err());
    }

    #[test]
    fn escapes_round_trip() {
        let v = parse(r#"{"k":"a\"b\\c\n"}"#).unwrap();
        assert_eq!(v.get("k").unwrap().as_str(), Some("a\"b\\c\n"));
        assert_eq!(to_pretty(&v), "{\n  \"k\": \"a\\\"b\\\\c\\n\"\n}\n");
    }

    #[test]
    fn surrogate_pair_decodes_to_astral_scalar() {
        // `😀` is the surrogate pair for U+1F600; it must combine into one astral scalar,
        // not degrade to two U+FFFD. `é` is the BMP scalar U+00E9.
        let input = "{\"k\":\"a\\uD83D\\uDE00b\\u00e9\"}";
        let v = parse(input).unwrap();
        assert_eq!(v.get("k").unwrap().as_str(), Some("a\u{1F600}b\u{E9}"));
    }

    #[test]
    fn astral_and_bmp_round_trip_byte_identical() {
        // The serializer emits astral and BMP scalars as literal UTF-8, so parse∘serialize is
        // the identity on that canonical form — the property install/uninstall relies on.
        let src = "{\n  \"k\": \"a\u{1F600}b\u{E9}\"\n}\n";
        let v = parse(src).unwrap();
        assert_eq!(
            to_pretty(&v),
            src,
            "astral + BMP round-trip must be byte-identical"
        );
    }

    #[test]
    fn deep_nesting_within_cap_parses() {
        // Well under MAX_DEPTH (128): a legitimately-nested document still parses.
        let n = 120;
        let deep = format!("{}{}", "[".repeat(n), "]".repeat(n));
        assert!(
            parse(&deep).is_ok(),
            "a {n}-deep nest (< MAX_DEPTH) must parse"
        );
    }

    #[test]
    fn nesting_past_cap_errors_instead_of_overflowing() {
        // Far past MAX_DEPTH: the parser returns the promised error rather than overflowing the
        // stack (which would abort the process, not decline the file).
        let n = 5000;
        let too_deep = format!("{}{}", "[".repeat(n), "]".repeat(n));
        let err = parse(&too_deep).unwrap_err();
        assert!(
            err.contains("maximum nesting depth"),
            "expected a depth-cap error, got {err:?}"
        );
    }

    #[test]
    fn lone_surrogates_do_not_panic_and_preserve_neighbors() {
        // Every malformed case decodes deterministically (U+FFFD for the bad unit) without
        // panicking, and never corrupts a valid neighboring scalar.
        assert_eq!(parse("\"\\uD83D\"").unwrap(), Value::Str("\u{fffd}".into())); // lone high
        assert_eq!(parse("\"\\uDE00\"").unwrap(), Value::Str("\u{fffd}".into())); // lone low
                                                                                  // high surrogate followed by a literal char: replacement, then the char intact.
        assert_eq!(
            parse("\"\\uD83DA\"").unwrap(),
            Value::Str("\u{fffd}A".into())
        );
        // high surrogate followed by a BMP escape (not a low surrogate): 'A' preserved.
        assert_eq!(
            parse("\"\\uD83D\\u0041\"").unwrap(),
            Value::Str("\u{fffd}A".into())
        );
        // high followed by another high: each replaced independently.
        assert_eq!(
            parse("\"\\uD83D\\uD83D\"").unwrap(),
            Value::Str("\u{fffd}\u{fffd}".into())
        );
    }
}
