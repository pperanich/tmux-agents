//! Guardrail: tier 3 (`tma-daemon`) must stay confined to the bin's daemon dispatch.
//!
//! The tier story says every non-daemon code path in the bin imports runtime (+ tmux) only,
//! so the `tma daemon` subcommand is the *single* place the bin reaches into `tma-daemon`. This
//! test reads the bin's own sources and fails if `tma_daemon` appears in any module other than
//! `main.rs`. It is a source-text check (no build/tmux needed): a new stray import trips it in a
//! plain `cargo test`, catching the regression the compiler cannot (the Cargo edge is legitimate;
//! *where* it is used is the invariant).

use std::fs;
use std::path::{Path, PathBuf};

/// Every `.rs` file under `dir`, recursively (the bin's modules live in `src/install/` too).
/// Traversal errors panic rather than truncating the walk: a scan that silently found nothing
/// would pass the guard below on no evidence at all.
fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display()));
    for entry in entries {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

#[test]
fn tma_daemon_is_referenced_only_from_main() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect_rs_files(&src, &mut files);

    // The walk must have actually walked: a floor on the count plus a nested module nobody would
    // reach without recursion, so a broken traversal fails here instead of reporting a clean scan.
    assert!(
        files.len() >= 12,
        "scanned only {} bin sources; the walk is not seeing the crate",
        files.len()
    );
    assert!(
        files.contains(&src.join("install/js_bridge.rs")),
        "the walk must descend into src/install/"
    );

    let sanctioned = src.join("main.rs"); // the daemon-dispatch site
    let mut offenders = Vec::new();
    for path in &files {
        if path == &sanctioned {
            continue;
        }
        let text = fs::read_to_string(path).expect("read source file");
        if text.contains("tma_daemon") {
            offenders.push(
                path.strip_prefix(&src)
                    .unwrap_or(path)
                    .display()
                    .to_string(),
            );
        }
    }
    assert!(
        offenders.is_empty(),
        "tier 3 leaked into non-daemon bin modules {offenders:?}; `tma-daemon` must be reached \
         only from the `tma daemon` dispatch in main.rs (the tier story)",
    );
}
