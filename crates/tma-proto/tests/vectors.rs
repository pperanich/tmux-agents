//! The golden corpus: a snapshot of the wire, which is the one artefact a refactor can change
//! while every other test stays green.
//!
//! There is a single serde implementation shared by the host and the app, so this lane arbitrates
//! nothing. What it catches is an accidental wire change, a field added without a vector, and an
//! enum variant nobody covered.

mod corpus;

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use corpus::{corpus, Vector};
use serde_json::Value;
use tma_proto::*;

fn vectors_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("vectors")
}

fn read(vector: &Vector) -> String {
    let path = vectors_dir().join(vector.file);
    fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e}. Regenerate with TMA_PROTO_BLESS=1", path.display()))
}

/// Rewrite every vector from its constructed value. Deliberate, opt-in, and never part of a run
/// that is checking anything.
#[test]
fn bless() {
    if std::env::var_os("TMA_PROTO_BLESS").is_none() {
        return;
    }
    let dir = vectors_dir();
    fs::create_dir_all(&dir).expect("vectors dir");
    for vector in corpus() {
        fs::write(dir.join(vector.file), &vector.json).expect("write vector");
    }
}

/// A-100: each committed vector, emitted from a constructed value, is byte-identical to the file.
///
/// Byte equality includes key order, so under serde the declared field order is part of the wire
/// contract and re-ordering a struct's fields fails here rather than shipping silently.
#[test]
fn every_vector_is_byte_identical_to_its_constructed_value() {
    for vector in corpus() {
        assert_eq!(
            read(&vector),
            vector.json,
            "{} drifted from the value that builds it",
            vector.file
        );
    }
}

/// A-113: every vector parses and re-emits byte-identically.
#[test]
fn every_vector_round_trips_byte_identically() {
    for vector in corpus() {
        let text = read(&vector);
        let out = (vector.reparse)(&text)
            .unwrap_or_else(|e| panic!("{} does not parse as its own type: {e}", vector.file));
        assert_eq!(out, text, "{} did not survive a round trip", vector.file);
    }
}

/// A-102: no vector carries the key `title`, at any depth.
///
/// A key scan, not a substring grep: a `title` inside a redacted string is not a violation and a
/// grep would report it.
#[test]
fn no_vector_carries_a_title_key() {
    for vector in corpus() {
        let mut found = Vec::new();
        walk(&parse(&read(&vector)), String::new(), &mut |path, _| {
            if path.rsplit('.').next() == Some("title") {
                found.push(path.to_string());
            }
        });
        assert!(
            found.is_empty(),
            "{} carries a title key at {found:?}",
            vector.file
        );
    }
}

/// A-103: no snapshot, edge, card, receipt or notification vector carries agent-supplied text.
///
/// Two halves. The banned keys are where a transcript body would be smuggled in wearing a field
/// name; the leaf rule is what catches one wearing a label. Every string leaf must be a closed
/// vocabulary token or `x`-fill, with exactly one exemption, stated with its own bound below.
#[test]
fn the_acting_surfaces_carry_no_free_text() {
    const BANNED_KEYS: [&str; 5] = ["text", "body", "content", "args", "result"];
    /// The enum discriminants the envelope is made of. Their values come from this crate's own type
    /// system rather than from anything an agent wrote, so the leaf rule has nothing to say about
    /// them; the length bound still applies.
    const STRUCTURAL_KEYS: [&str; 3] = ["t", "card", "kind"];
    const LEAF_MAX: usize = 512;
    /// The dialog's own option line is committed verbatim (R24): it is what the user reads, and a
    /// generic label is the consent bug this field exists to prevent. Claude's command-prefix grant
    /// embeds a whole command string, so it gets 4 KiB where every other leaf gets 512.
    const OPTION_NAME_MAX: usize = 4096;

    for vector in corpus() {
        if !acting_surface(vector.file) {
            continue;
        }
        let value = parse(&read(&vector));
        walk(&value, String::new(), &mut |path, leaf| {
            let key = path.rsplit('.').next().unwrap_or_default();
            assert!(
                !BANNED_KEYS.contains(&key),
                "{}: {path} is a banned key on an acting surface",
                vector.file
            );
            let Value::String(text) = leaf else { return };
            if key == "name" && path.contains("options") {
                assert!(
                    text.len() <= OPTION_NAME_MAX,
                    "{}: {path} is {} bytes, over the option-label bound",
                    vector.file,
                    text.len()
                );
                return;
            }
            assert!(
                text.len() <= LEAF_MAX,
                "{}: {path} is {} bytes, over the leaf bound",
                vector.file,
                text.len()
            );
            if STRUCTURAL_KEYS.contains(&key) {
                return;
            }
            assert!(
                synthesized(text) || vocabulary_token(text),
                "{}: {path} = {text:?} is neither a vocabulary token nor x-fill",
                vector.file
            );
        });
    }
}

/// A-111: the pinned `MAXIMAL` notification payload fits the byte bound it was built to prove.
/// Measured on the wire form, which is what a sender would carry.
#[test]
fn the_maximal_notification_payload_fits_its_bound() {
    let vector = corpus()
        .into_iter()
        .find(|v| v.file == "notify-MAXIMAL.json")
        .expect("the MAXIMAL vector is committed");
    assert!(
        vector.wire.len() <= 2000,
        "MAXIMAL is {} wire bytes, over the 2000-byte bound",
        vector.wire.len()
    );
}

/// A-115: an option the dialog printed no index for carries none, and no vector anywhere invents a
/// positional one. The card may order index-free options; a position the host made up must never
/// reach the device as a keycap.
#[test]
fn index_free_options_stay_index_free() {
    let cursor: ResponseFrame = decode(
        &fs::read_to_string(vectors_dir().join("card-permission-cursor.json"))
            .expect("the cursor vector is committed"),
    )
    .expect("it parses");
    let Response::Card(Card::Permission(card)) = cursor.body else {
        panic!("the cursor vector is a permission card");
    };
    assert_eq!(card.options.len(), 4);
    for option in &card.options {
        assert_eq!(
            option.option_id, None,
            "cursor prints no index, so {:?} carries none",
            option.name
        );
    }

    // And the screen lane, where the dialog does print one, carries the digit rather than a
    // position: the two cases differ in the data, not in how the card was built.
    let screen: ResponseFrame = decode(
        &fs::read_to_string(vectors_dir().join("card-permission-screen.json"))
            .expect("the screen vector is committed"),
    )
    .expect("it parses");
    let Response::Card(Card::Permission(card)) = screen.body else {
        panic!("the screen vector is a permission card");
    };
    assert!(card.options.iter().all(|o| o.option_id.is_some()));
}

/// A-108: every type and every enum variant this crate declares appears in at least one vector.
///
/// The inventory is scanned out of the crate's own sources, so a variant added without a vector
/// fails here rather than shipping untested. Four files are not the wire and are skipped by name:
/// `lib.rs` (the codec and its error), `vocabulary.rs` (the macro), `money.rs` (a serde adapter with
/// no types of its own) and `props.rs` (test scaffolding). A new module is scanned by default.
#[test]
fn every_type_and_variant_has_a_vector() {
    let declared = declared_inventory();
    assert!(
        declared.len() > 100,
        "the source scan found only {} names, which means it stopped matching",
        declared.len()
    );

    let mut covered = BTreeSet::new();
    for vector in corpus() {
        covered.extend(vector.covers.iter().map(|s| s.to_string()));
    }

    let missing: Vec<_> = declared.difference(&covered).cloned().collect();
    assert!(
        missing.is_empty(),
        "no vector covers: {missing:?}. Add one to tests/corpus/mod.rs and list it in `covers`."
    );

    let unknown: Vec<_> = covered.difference(&declared).cloned().collect();
    assert!(
        unknown.is_empty(),
        "a vector claims to cover names this crate does not declare: {unknown:?}"
    );
}

/// A frame serializes to one line with no newline in it, and that line carries the same value the
/// corpus file does: the pretty form is a review convenience, not a second encoding.
#[test]
fn every_frame_is_one_line_carrying_the_committed_value() {
    for vector in corpus() {
        assert!(
            !vector.wire.contains('\n'),
            "{} is not one line",
            vector.file
        );
        let out = (vector.reparse)(&vector.wire)
            .unwrap_or_else(|e| panic!("{}: the wire line does not parse: {e}", vector.file));
        assert_eq!(
            out,
            read(&vector),
            "{} disagrees between its wire and corpus forms",
            vector.file
        );
    }
}

// ---- helpers ------------------------------------------------------------------------------------

fn parse(text: &str) -> Value {
    serde_json::from_str(text).expect("a committed vector is JSON")
}

/// Whether this vector rides one of the surfaces A-103 governs.
fn acting_surface(file: &str) -> bool {
    ["snapshot", "edge", "card", "receipt", "notify"]
        .iter()
        .any(|prefix| file.starts_with(prefix))
}

/// Visit every leaf, carrying a dotted path. Array indices are not in the path: the rules here are
/// about which key a value sits under, never about which element.
fn walk(value: &Value, path: String, visit: &mut impl FnMut(&str, &Value)) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                let child_path = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                visit(&child_path, child);
                walk(child, child_path, visit);
            }
        }
        Value::Array(items) => {
            for item in items {
                // An element carries its array's key: a string leaf in a list is still a leaf.
                visit(&path, item);
                walk(item, path.clone(), visit);
            }
        }
        _ => {}
    }
}

/// Whether a string is synthesized rather than observed: `x`-fill plus the punctuation and digits a
/// pane id, a locator, a path or a version number is made of.
fn synthesized(text: &str) -> bool {
    text.bytes()
        .all(|b| b == b'x' || b.is_ascii_digit() || b"%:./_-".contains(&b))
}

/// Every closed vocabulary this crate publishes, plus the two the host publishes that a row quotes
/// verbatim: the agent names and the bundled action names.
fn vocabulary_token(text: &str) -> bool {
    const AGENTS: [&str; 6] = ["claude", "codex", "gemini", "pi", "cursor", "opencode"];
    const ACTIONS: [&str; 6] = ["approve", "deny", "interrupt", "steer", "answer", "compact"];
    const QUOTA_WINDOWS: [&str; 5] = ["5h", "7d", "spend", "primary", "secondary"];

    [
        State::TOKENS,
        Detail::TOKENS,
        StateFilter::TOKENS,
        Outcome::TOKENS,
        Reason::TOKENS,
        OptionKind::TOKENS,
        Lane::TOKENS,
        Extraction::TOKENS,
        ErrorCode::TOKENS,
        Scope::TOKENS,
        ResultStatus::TOKENS,
        TurnKind::TOKENS,
        BodyKind::TOKENS,
        &AGENTS,
        &ACTIONS,
        &QUOTA_WINDOWS,
    ]
    .iter()
    .any(|set| set.contains(&text))
}

/// Every `pub struct` and every enum variant declared in the wire modules.
fn declared_inventory() -> BTreeSet<String> {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut names = BTreeSet::new();
    let mut files: Vec<_> = fs::read_dir(&src)
        .expect("src is readable")
        .map(|e| e.expect("a src entry").path())
        .filter(|p| p.extension().is_some_and(|e| e == "rs"))
        .filter(|p| {
            !matches!(
                p.file_name().and_then(|n| n.to_str()),
                Some("lib.rs" | "vocabulary.rs" | "money.rs" | "props.rs")
            )
        })
        .collect();
    files.sort();
    for file in files {
        scan(
            &fs::read_to_string(&file).expect("a src file is readable"),
            &mut names,
        );
    }
    names
}

/// Pull type and variant names out of one source file.
///
/// Deliberately a line scan rather than a parse: the crate declares its wire in one shape per type
/// and the scan is the thing that must not quietly stop matching, which is why the caller asserts a
/// floor on the count it returns.
fn scan(source: &str, names: &mut BTreeSet<String>) {
    let mut enum_name: Option<String> = None;
    let mut depth = 0usize;
    for line in source.lines() {
        let trimmed = line.trim();
        if let Some(name) = enum_name.clone() {
            if depth == 1 {
                if let Some(variant) = leading_ident(trimmed) {
                    names.insert(format!("{name}::{variant}"));
                }
            }
            depth += trimmed.matches('{').count();
            // Saturating so a stray brace in a doc comment reports as a missing variant, which
            // names itself, rather than as an arithmetic panic in an unrelated-looking test.
            depth = depth.saturating_sub(trimmed.matches('}').count());
            if depth == 0 {
                enum_name = None;
            }
            continue;
        }
        if let Some(name) = declared_after(trimmed, "pub struct ") {
            names.insert(name);
        } else if let Some(name) = declared_after(trimmed, "pub enum ") {
            enum_name = Some(name);
            depth = 1;
        } else if let Some(name) = declared_after(trimmed, "pub open enum ") {
            // The `Other` arm the vocabulary macro adds is a variant like any other.
            names.insert(format!("{name}::Other"));
            enum_name = Some(name);
            depth = 1;
        }
    }
}

fn declared_after(line: &str, keyword: &str) -> Option<String> {
    let rest = line.strip_prefix(keyword)?;
    let name: String = rest
        .chars()
        .take_while(char::is_ascii_alphanumeric)
        .collect();
    (!name.is_empty()).then_some(name)
}

/// The variant name a declaration line starts with, if it is one: an identifier beginning with an
/// uppercase letter. Doc comments, attributes and struct-variant fields all start with something
/// else.
fn leading_ident(line: &str) -> Option<String> {
    let first = line.chars().next()?;
    if !first.is_ascii_uppercase() {
        return None;
    }
    let name: String = line
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    let rest = line[name.len()..].trim_start();
    matches!(rest.chars().next(), Some(',' | '(' | '{' | '=')).then_some(name)
}
