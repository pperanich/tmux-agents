//! The transition-history document: the wire form of the daemon's bounded ring, plus its two
//! renderings. The daemon encodes ([`render_document`]), `tma debug transitions` decodes
//! ([`parse_document`]) and prints. The shared shape lives here in tier 2 because tma-daemon is a
//! leaf: the encoder and the decoder must agree, and only one of them may own the format.
//!
//! Line-based `key=value` records (tab-separated), the same shape as the daemon's status file, so
//! the decode stays a split rather than a JSON parser the workspace does not carry.

use crate::json::{JsonWriter, JSON_SCHEMA};

/// One recorded state transition, as it crosses the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransitionRecord {
    pub pane: String,
    /// The previously-observed state, `None` on a pane's first observation.
    pub from: Option<String>,
    pub to: String,
    /// The transition epoch (`@agent_since`), in ms.
    pub at: u64,
    /// Provenance of the state transitioned into (`@agent_source`).
    pub source: String,
}

/// A decoded history document: the ring's records (oldest first) with its bound and lifetime count.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Transitions {
    pub records: Vec<TransitionRecord>,
    /// The ring's cap, so a full ring is distinguishable from a short history.
    pub cap: usize,
    /// Transitions recorded over the daemon's life (monotone; exceeds `records.len()` once the ring
    /// has wrapped).
    pub recorded: u64,
}

/// The field separator. A tab cannot appear in a pane id, state token, or provenance token, and the
/// only free-form field (there is none today) would be the one to reconsider it for.
const SEP: char = '\t';

/// Encode the document the daemon answers a history request with: a header line, then one line per
/// record, oldest first.
pub fn render_document(t: &Transitions) -> String {
    let mut out = format!("cap={}{SEP}recorded={}\n", t.cap, t.recorded);
    for r in &t.records {
        out.push_str(&format!(
            "pane={}{SEP}from={}{SEP}to={}{SEP}at={}{SEP}source={}\n",
            r.pane,
            r.from.as_deref().unwrap_or(""),
            r.to,
            r.at,
            r.source,
        ));
    }
    out
}

/// Decode a history document. Unparseable lines are skipped rather than failing the whole read: a
/// future daemon may add a field, and a partial history beats an error for a diagnostic surface.
pub fn parse_document(text: &str) -> Transitions {
    let mut t = Transitions::default();
    for line in text.lines() {
        let fields = |key: &str| -> Option<String> {
            line.split(SEP)
                .find_map(|f| f.strip_prefix(&format!("{key}=")))
                .map(str::to_string)
        };
        if let Some(cap) = fields("cap") {
            t.cap = cap.parse().unwrap_or(0);
            t.recorded = fields("recorded").and_then(|v| v.parse().ok()).unwrap_or(0);
            continue;
        }
        let (Some(pane), Some(to), Some(at), Some(source)) = (
            fields("pane"),
            fields("to"),
            fields("at").and_then(|v| v.parse::<u64>().ok()),
            fields("source"),
        ) else {
            continue;
        };
        t.records.push(TransitionRecord {
            pane,
            from: fields("from").filter(|f| !f.is_empty()),
            to,
            at,
            source,
        });
    }
    t
}

/// The human-readable rendering (`tma debug transitions`), newest LAST so it reads like a log.
pub fn render_text(t: &Transitions) -> String {
    if t.records.is_empty() {
        return format!(
            "no transitions recorded yet (ring cap {}, {} recorded over the daemon's life)\n",
            t.cap, t.recorded
        );
    }
    let mut out = format!(
        "transitions ({} held, cap {}, {} recorded over the daemon's life):\n",
        t.records.len(),
        t.cap,
        t.recorded
    );
    for r in &t.records {
        out.push_str(&format!(
            "  {:<6} {:<8} -> {:<8} at={} src={}\n",
            r.pane,
            r.from.as_deref().unwrap_or("-"),
            r.to,
            r.at,
            r.source
        ));
    }
    out
}

/// The `--json` rendering: the additive-only schema-1 document, `from` explicitly null on a first
/// observation.
pub fn render_json(t: &Transitions) -> String {
    let mut j = JsonWriter::new();
    j.begin_object();
    j.number("schema", JSON_SCHEMA);
    j.number("cap", t.cap as i64);
    j.number("recorded", t.recorded as i64);
    j.key("transitions");
    j.begin_array();
    for r in &t.records {
        j.begin_object();
        j.string("pane", &r.pane);
        match &r.from {
            Some(from) => j.string("from", from),
            None => j.null("from"),
        }
        j.string("to", &r.to);
        j.number("at", r.at as i64);
        j.string("source", &r.source);
        j.end_object();
    }
    j.end_array();
    j.end_object();
    j.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Transitions {
        Transitions {
            records: vec![
                TransitionRecord {
                    pane: "%1".to_string(),
                    from: None,
                    to: "working".to_string(),
                    at: 1_700_000_000_000,
                    source: "hook".to_string(),
                },
                TransitionRecord {
                    pane: "%1".to_string(),
                    from: Some("working".to_string()),
                    to: "blocked".to_string(),
                    at: 1_700_000_001_000,
                    source: "capture".to_string(),
                },
            ],
            cap: 256,
            recorded: 42,
        }
    }

    #[test]
    fn document_round_trips_through_the_wire_form() {
        assert_eq!(parse_document(&render_document(&sample())), sample());
        // An empty ring still carries its header.
        let empty = Transitions {
            records: vec![],
            cap: 256,
            recorded: 0,
        };
        assert_eq!(parse_document(&render_document(&empty)), empty);
    }

    #[test]
    fn an_unparseable_line_is_skipped_not_fatal() {
        // A record missing `to` (a future field rename, or a truncated write) drops that row only.
        let doc = "cap=8\trecorded=2\npane=%1\tfrom=\tat=5\tsource=hook\npane=%2\tfrom=idle\tto=working\tat=6\tsource=hook\n";
        let t = parse_document(doc);
        assert_eq!(t.cap, 8);
        assert_eq!(t.records.len(), 1);
        assert_eq!(t.records[0].pane, "%2");
    }

    #[test]
    fn json_pins_the_document_shape() {
        assert_eq!(
            render_json(&sample()),
            r#"{"schema":1,"cap":256,"recorded":42,"transitions":[{"pane":"%1","from":null,"to":"working","at":1700000000000,"source":"hook"},{"pane":"%1","from":"working","to":"blocked","at":1700000001000,"source":"capture"}]}"#
        );
    }

    #[test]
    fn text_reads_oldest_first_and_says_so_when_empty() {
        let text = render_text(&sample());
        let first = text.lines().nth(1).unwrap();
        assert!(first.contains("-> working"), "oldest first: {text}");
        assert!(text.contains("cap 256") && text.contains("42 recorded"));
        assert!(render_text(&Transitions::default()).contains("no transitions recorded yet"));
    }
}
