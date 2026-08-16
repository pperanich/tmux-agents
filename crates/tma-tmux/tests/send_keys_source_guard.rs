//! Guardrail: `send-keys` is constructed only inside `tma-tmux`.
//!
//! The write path is pinned — "No other crate constructs `send-keys`" — so the keys action's
//! guarded delivery cannot be bypassed by a hand-rolled shell-out elsewhere. This is a source-text
//! check (no build/tmux needed): it scans every crate's `src/` for the quoted `"send-keys"` argv
//! token and fails if any crate other than `tma-tmux` carries one. Integration tests under `tests/`
//! legitimately drive panes with raw `send-keys`, so only production `src/` is scanned. A doc
//! comment mentioning send-keys uses backticks, not the double-quoted argv form, so it never trips.

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
