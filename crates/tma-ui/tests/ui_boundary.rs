//! Guardrail: `tma-ui` never calls a `Tmux` method directly.
//!
//! The UI-snapshot rule (RD4) says display code touches tmux only through the named helpers in
//! `tma_runtime::ui` (capture_preview, focus_pane, clear_attention, the jump trail, the watch-pid
//! advertisement). `tma-ui` has a runtime-only Cargo edge, so `Tmux`/`TmuxError` are *nameable*
//! here through runtime's re-export and a future `tmux.set_option(...)` would compile clean — the
//! compiler cannot forbid what the type system lets you name. This source-text check (no build/tmux
//! needed) scans every crate `src/` file and fails on a direct `tmux.<method>(` touch, the same
//! companion a source guard gives the tier boundary (`crates/tma/tests/tier_boundary.rs`). Comment
//! lines are skipped, so a doc comment writing `tmux.set_option(...)` to explain the rule never
//! trips; only live code does.

use std::fs;
use std::path::{Path, PathBuf};

/// True if `line` carries a direct `tmux.<method>(` call: a `tmux` receiver on a word boundary
/// (so `self.tmux.foo(` counts but `mytmux.foo(` does not), then `.`, a lowercase/underscore
/// method name, and an open paren. Mirrors the `\btmux\.[a-z_]+\(` shape without a regex dep.
fn is_direct_tmux_call(line: &str) -> bool {
    let bytes = line.as_bytes();
    let ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut i = 0;
    while let Some(off) = line[i..].find("tmux.") {
        let start = i + off;
        let boundary = start == 0 || !ident(bytes[start - 1]);
        if boundary {
            let name_start = start + "tmux.".len();
            let mut j = name_start;
            while j < bytes.len() && (bytes[j].is_ascii_lowercase() || bytes[j] == b'_') {
                j += 1;
            }
            if j > name_start && bytes.get(j) == Some(&b'(') {
                return true;
            }
        }
        i = start + "tmux.".len();
    }
    false
}

/// Scan `dir` recursively, recording offending lines and every file visited. Traversal and read
/// errors panic rather than being skipped: a file this walk cannot open is a file it cannot clear,
/// and an empty offender list drawn from an empty walk would pass the guard on no evidence.
fn collect_offenders(dir: &Path, offenders: &mut Vec<String>, scanned: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display()));
    for entry in entries {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            collect_offenders(&path, offenders, scanned);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            let text = fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            scanned.push(path.clone());
            for (n, line) in text.lines().enumerate() {
                if line.trim_start().starts_with("//") {
                    continue; // prose explaining the rule is not a call site
                }
                if is_direct_tmux_call(line) {
                    offenders.push(format!("{}:{}", path.display(), n + 1));
                }
            }
        }
    }
}

#[test]
fn ui_never_calls_tmux_directly() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders = Vec::new();
    let mut scanned = Vec::new();
    collect_offenders(&src, &mut offenders, &mut scanned);

    // The walk must have actually read the crate: a floor on the count plus the surfaces this rule
    // exists for, so a scan that saw nothing fails here rather than reporting a clean crate.
    assert!(
        scanned.len() >= 8,
        "scanned only {} UI sources; the walk is not seeing the crate",
        scanned.len()
    );
    for name in ["picker.rs", "watch.rs", "runner.rs"] {
        assert!(
            scanned.contains(&src.join(name)),
            "the walk must cover src/{name}"
        );
    }

    assert!(
        offenders.is_empty(),
        "`tma-ui` calls a `Tmux` method directly at {offenders:?}; every tmux touchpoint must \
         route through a named helper in `tma_runtime::ui` (RD4, the UI-snapshot rule)",
    );
}
