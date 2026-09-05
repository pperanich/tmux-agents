//! Guardrail: `send-keys` is constructed only inside `tma-tmux`.
//!
//! The write path is pinned — "No other crate constructs `send-keys`" — so the keys action's
//! guarded delivery cannot be bypassed by a hand-rolled shell-out elsewhere. This is a source-text
//! check (no build/tmux needed): it scans every crate's `src/` for the quoted `"send-keys"` argv
//! token and fails if any crate other than `tma-tmux` carries one. Integration tests under `tests/`
//! legitimately drive panes with raw `send-keys`, so only production `src/` is scanned. A doc
//! comment mentioning send-keys uses backticks, not the double-quoted argv form, so it never trips.
//!
//! The second test covers the literal-text path the same way: `send_text` is the one construction
//! that carries a caller's arbitrary string, so its argv must keep `-l` (or the word `Enter` in a
//! message presses Return) and `--` (or a message starting with `-` is read by tmux as a flag).

use std::fs;
use std::path::Path;

/// The quoted argv token an actual `send-keys` construction carries; a backticked prose mention of
/// `send-keys` (the only non-tmux occurrence) does not match this.
const TOKEN: &str = "\"send-keys\"";

fn collect_offenders(dir: &Path, offenders: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_offenders(&path, offenders);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            if let Ok(text) = fs::read_to_string(&path) {
                if text.contains(TOKEN) {
                    offenders.push(path.display().to_string());
                }
            }
        }
    }
}

#[test]
fn send_keys_is_constructed_only_in_tma_tmux() {
    // CARGO_MANIFEST_DIR is crates/tma-tmux; its parent is the workspace `crates/` dir.
    let crates_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates dir")
        .to_path_buf();

    let mut offenders = Vec::new();
    for entry in fs::read_dir(&crates_dir).expect("read crates dir") {
        let crate_dir = entry.expect("dir entry").path();
        let name = crate_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if name == "tma-tmux" {
            continue; // the sanctioned choke point
        }
        collect_offenders(&crate_dir.join("src"), &mut offenders);
    }

    assert!(
        offenders.is_empty(),
        "`send-keys` is constructed outside tma-tmux in {offenders:?}; the keys write path must \
         route through `tma_tmux::tmux::Tmux::send_keys`",
    );
}

/// The literal-send argv, verbatim. Drop `-l` and a steered `Enter` presses Return; drop `--` and a
/// steered `-foo` is read as a flag. Both live in one source line, so both are pinned by one needle.
const LITERAL_ARGV: &str = r#"&["send-keys", "-t", pane_id, "-l", "--", text]"#;

#[test]
fn the_literal_send_keeps_l_and_the_argv_terminator() {
    let display = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/tmux/display.rs");
    let text = fs::read_to_string(&display).expect("read display.rs");
    // Anti-vacuity: the needle is worthless if the function it guards has been renamed away.
    assert!(
        text.contains("pub fn send_text("),
        "send_text has moved out of {}; move this guard with it",
        display.display()
    );
    assert!(
        text.contains(LITERAL_ARGV),
        "the literal send must construct {LITERAL_ARGV} in {}: `-l` keeps a caller's `Enter` five \
         characters of text, and `--` keeps a leading `-` from being read as a flag",
        display.display()
    );
}
