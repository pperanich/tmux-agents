//! A tiny hand-rolled JSON writer with correct escaping and no external dependency, shared
//! by every `--json` surface (`tma debug explain`, `tma ls`). Additive object/array building is
//! all the versioned schemas need. Keeping the writer here rather than pulling in `serde_json`
//! holds the binary's dependency surface small.

/// The single `"schema"` version every `--json` surface (`tma ls`, `tma doctor`,
/// `tma debug explain`) emits as its first key. Shared here so the three emitters cannot disagree;
/// the schema is **additive-only** — new keys never bump it, and a dropped or renamed key (which the
/// JSON-shape drift tests catch) is a breaking change. Bump only on a genuinely incompatible reshape.
pub const JSON_SCHEMA: i64 = 1;

/// A minimal streaming JSON writer. Tracks per-scope comma state so callers emit keys and
/// values without threading separators.
pub struct JsonWriter {
    buf: String,
    needs_comma: Vec<bool>,
}

impl JsonWriter {
    pub fn new() -> Self {
        JsonWriter {
            buf: String::new(),
            needs_comma: Vec::new(),
        }
    }

    fn sep(&mut self) {
        if let Some(last) = self.needs_comma.last_mut() {
            if *last {
                self.buf.push(',');
            }
            *last = true;
        }
    }

    pub fn begin_object(&mut self) {
        self.sep();
        self.buf.push('{');
        self.needs_comma.push(false);
    }
    pub fn end_object(&mut self) {
        self.buf.push('}');
        self.needs_comma.pop();
    }
    pub fn begin_array(&mut self) {
        self.sep();
        self.buf.push('[');
        self.needs_comma.push(false);
    }
    pub fn end_array(&mut self) {
        self.buf.push(']');
        self.needs_comma.pop();
    }

    /// Write a bare key for a value that follows (object/array/null via `raw_null`).
    pub fn key(&mut self, k: &str) {
        self.sep();
        // A key suppresses the automatic separator on the value that follows.
        write_json_string(&mut self.buf, k);
        self.buf.push(':');
        if let Some(last) = self.needs_comma.last_mut() {
            *last = false;
        }
    }

    pub fn string(&mut self, k: &str, v: &str) {
        self.key(k);
        write_json_string(&mut self.buf, v);
        self.mark();
    }
    pub fn number(&mut self, k: &str, v: i64) {
        self.key(k);
        self.buf.push_str(&v.to_string());
        self.mark();
    }
    pub fn bool(&mut self, k: &str, v: bool) {
        self.key(k);
        self.buf.push_str(if v { "true" } else { "false" });
        self.mark();
    }
    pub fn null(&mut self, k: &str) {
        self.key(k);
        self.buf.push_str("null");
        self.mark();
    }
    pub fn raw_null(&mut self) {
        self.sep();
        self.buf.push_str("null");
    }

    /// Write a bare string as an array element (no key), separator-managed like [`Self::raw_null`].
    pub fn raw_string(&mut self, v: &str) {
        self.sep();
        write_json_string(&mut self.buf, v);
    }

    fn mark(&mut self) {
        if let Some(last) = self.needs_comma.last_mut() {
            *last = true;
        }
    }

    pub fn finish(self) -> String {
        self.buf
    }
}

impl Default for JsonWriter {
    fn default() -> Self {
        Self::new()
    }
}

fn write_json_string(buf: &mut String, s: &str) {
    buf.push('"');
    for c in s.chars() {
        match c {
            '"' => buf.push_str("\\\""),
            '\\' => buf.push_str("\\\\"),
            '\n' => buf.push_str("\\n"),
            '\r' => buf.push_str("\\r"),
            '\t' => buf.push_str("\\t"),
            c if (c as u32) < 0x20 => buf.push_str(&format!("\\u{:04x}", c as u32)),
            c => buf.push(c),
        }
    }
    buf.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_control_and_quotes() {
        let mut buf = String::new();
        write_json_string(&mut buf, "a\"b\\c\n\t\u{1}");
        assert_eq!(buf, "\"a\\\"b\\\\c\\n\\t\\u0001\"");
    }

    #[test]
    fn builds_object() {
        let mut j = JsonWriter::new();
        j.begin_object();
        j.number("schema", 1);
        j.string("s", "x");
        j.key("arr");
        j.begin_array();
        j.begin_object();
        j.bool("m", true);
        j.end_object();
        j.end_array();
        j.key("v");
        j.raw_null();
        j.end_object();
        assert_eq!(
            j.finish(),
            r#"{"schema":1,"s":"x","arr":[{"m":true}],"v":null}"#
        );
    }
}
