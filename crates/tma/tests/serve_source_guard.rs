//! A source-text guard over the serve module: `force` is unreachable from a device, and no
//! exec action is named there.
//!
//! Two properties the compiler cannot state. `FireArgs::force` is a plain `bool`, so a `true`
//! anywhere on this path would compile and would skip the `when` gate for a caller that is not in
//! the room; and an exec action is the one class that sends no keystrokes and therefore has no
//! freshness guard at all, so serve refuses it by kind and never by naming the variant.
//!
//! The anti-vacuity floor matters more than either check: a guard that scanned nothing would pass.
//! It asserts the walk found the serve module, its submodules, and a real `FireArgs` construction.

use std::fs;
use std::path::{Path, PathBuf};

/// Every `.rs` file of the serve module: `src/serve.rs` plus everything under `src/serve/`.
fn serve_sources() -> Vec<PathBuf> {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = vec![src.join("serve.rs")];
    let dir = src.join("serve");
    let entries = fs::read_dir(&dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display()));
    for entry in entries {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            files.push(path);
        }
    }
    files.sort();
    files
}

/// The scanned text of every serve source, with its path, and the anti-vacuity assertions that
/// make a finding meaningful.
fn scanned() -> Vec<(PathBuf, String)> {
    let files = serve_sources();
    assert!(
        files.len() >= 4,
        "scanned only {} serve sources; the walk is not seeing the module",
        files.len()
    );
    let read: Vec<(PathBuf, String)> = files
        .into_iter()
        .map(|path| {
            let text = fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            (path, text)
        })
        .collect();
    assert!(
        read.iter().any(|(_, text)| text.contains("FireArgs {")),
        "the guard found no FireArgs construction at all, so it is asserting nothing"
    );
    read
}

/// the force rule. `force` skips the `when` gate, and a device is never in the room to have decided that.
#[test]
fn serve_never_constructs_a_forced_fire() {
    for (path, text) in scanned() {
        for (n, line) in text.lines().enumerate() {
            let code = line.split("//").next().unwrap_or(line);
            assert!(
                !code.contains("force: true"),
                "{}:{}: serve must never force a fire: {line}",
                path.display(),
                n + 1
            );
        }
    }
}

/// An exec action sends no keystrokes, so it is the one class with no freshness re-verify.
/// Serve refuses it through `ActionKind::sends_keystrokes`, which makes a new kind answer the
/// question rather than inherit a silent "no"; naming the variant here would be the equality test
/// that method exists to replace.
#[test]
fn serve_never_names_an_exec_action_kind() {
    let mut refuses_by_kind = false;
    for (path, text) in scanned() {
        refuses_by_kind |= text.contains("sends_keystrokes()");
        for (n, line) in text.lines().enumerate() {
            let code = line.split("//").next().unwrap_or(line);
            assert!(
                !code.contains("ActionKind::Exec"),
                "{}:{}: serve gates on the kind's own method, never on the variant: {line}",
                path.display(),
                n + 1
            );
        }
    }
    assert!(
        refuses_by_kind,
        "serve must refuse exec actions somewhere; nothing calls sends_keystrokes()"
    );
}
