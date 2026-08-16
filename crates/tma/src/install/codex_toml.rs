use std::path::Path;

use super::{CODEX_AGENT, CODEX_NOTIFY_EVENT};
use crate::manifests;

// --- Codex config.toml adapter ---------------------------------------------------

/// The `notify` array tma writes: `["<tma-hook>", "codex", "notify"]`. Codex spawns it on a
/// notification with the JSON appended as a trailing argv arg; the wrapper forwards to `tma event`.
fn codex_notify_array(wrapper: &Path) -> toml_edit::Array {
    let mut arr = toml_edit::Array::new();
    arr.push(wrapper.display().to_string());
    arr.push(CODEX_AGENT);
    arr.push(CODEX_NOTIFY_EVENT);
    arr
}

/// Whether a `notify` item is tma's — its program (element 0) is a `tma-hook` wrapper.
/// Robust to the wrapper path moving between installs, and to the user having renamed nothing.
pub(super) fn codex_notify_is_ours(item: &toml_edit::Item) -> bool {
    item.as_array()
        .and_then(|a| a.get(0))
        .and_then(|v| v.as_str())
        .map(|prog| Path::new(prog).file_name().and_then(|n| n.to_str()) == Some("tma-hook"))
        .unwrap_or(false)
}

/// Whether a `notify` item is EXACTLY the array we would write for `wrapper` (used to keep a
/// re-install byte-identical and to detect a stale wrapper path in `--check`).
fn codex_notify_matches(item: &toml_edit::Item, wrapper: &Path) -> bool {
    let Some(arr) = item.as_array() else {
        return false;
    };
    let want = [
        wrapper.display().to_string(),
        CODEX_AGENT.to_string(),
        CODEX_NOTIFY_EVENT.to_string(),
    ];
    arr.len() == want.len()
        && arr
            .iter()
            .zip(&want)
            .all(|(v, w)| v.as_str() == Some(w.as_str()))
}

/// Insert (idempotently) tma's `notify` program into Codex's `config.toml`. Format-preserving
/// (toml_edit): comments and unrelated keys survive. Codex allows only ONE `notify`, so a foreign
/// one is never clobbered — the install refuses instead.
pub(super) fn edit_codex_install(old: &str, wrapper: &Path) -> Result<String, String> {
    let mut doc: toml_edit::DocumentMut = old
        .parse()
        .map_err(|e| format!("cannot parse Codex config.toml: {e}"))?;
    match doc.get("notify") {
        // Already exactly ours: no-op, byte-identical re-install (do not reformat the line).
        Some(item) if codex_notify_matches(item, wrapper) => return Ok(old.to_string()),
        // Ours but pointing at a different wrapper path: re-point it.
        Some(item) if codex_notify_is_ours(item) => {}
        // A foreign notify program: never overwrite it (Codex supports only one).
        Some(_) => {
            return Err(
                "Codex config.toml already defines a `notify` program that is not tma's. \
                 Codex allows only one notify program, so tma will not overwrite it — point \
                 your notify at `tma-hook codex notify`, or remove it, then re-run."
                    .to_string(),
            )
        }
        None => {}
    }
    doc["notify"] = toml_edit::Item::Value(toml_edit::Value::Array(codex_notify_array(wrapper)));
    Ok(doc.to_string())
}

/// Remove exactly tma's `notify` entry, leaving a foreign or absent one untouched (symmetric to
/// install). Format-preserving: everything else in `config.toml` survives byte-for-byte.
pub(super) fn edit_codex_uninstall(old: &str) -> Result<String, String> {
    let mut doc: toml_edit::DocumentMut = old
        .parse()
        .map_err(|e| format!("cannot parse Codex config.toml: {e}"))?;
    let ours = doc.get("notify").is_some_and(codex_notify_is_ours);
    if ours {
        doc.remove("notify");
    }
    Ok(doc.to_string())
}

/// Whether Codex's `notify` is installed AND references the current wrapper (used by `--check`).
/// A missing key, a foreign program, or a stale wrapper path is drift.
pub(super) fn codex_notify_ok(text: &str, wrapper: &Path) -> bool {
    text.parse::<toml_edit::DocumentMut>()
        .ok()
        .and_then(|doc| doc.get("notify").map(|i| codex_notify_matches(i, wrapper)))
        .unwrap_or(false)
}

/// Read Codex's `config.toml`: absent ⇒ empty text (an empty TOML document), unreadable ⇒ an error
/// the caller reports — never an empty document tma would then write over the user's file.
pub(super) fn read_codex_config(path: &Path) -> Result<String, String> {
    super::read_existing(path, "")
}

/// The events wired into Codex's `hooks.json`: everything the manifest declares except `notify`
/// (which goes through `config.toml`). Verified live on 0.145.0 that hooks.json takes the exact JSON
/// shape [`edit_settings_install`](super::claude_json::edit_settings_install) writes, so the Claude editor is reused as-is.
pub(super) fn codex_hooks_events(manifest: &tma_core::Manifest) -> Vec<String> {
    manifests::hook_events(manifest)
        .into_iter()
        .filter(|e| e != CODEX_NOTIFY_EVENT)
        .collect()
}

/// The one-time manual step hooks.json wiring needs (agent-coverage.md "Codex mapping", trust gate):
/// codex silently skips a hook definition until the user reviews and trusts it in the TUI.
pub(super) const CODEX_TRUST_NOTICE: &str =
    "codex trust gate: the hooks.json entries stay INERT until \
you open codex, run /hooks, and trust the tma-hook entries (codex silently skips untrusted \
hooks). The notify signal works without this step.";

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn wrapper() -> PathBuf {
        PathBuf::from("/opt/tma/tma-hook")
    }

    #[test]
    fn codex_install_then_uninstall_is_byte_identical() {
        // A config with a comment + unrelated keys the installer must preserve.
        let original = "# my codex config\nmodel = \"gpt-5.2\"\n";
        let installed = edit_codex_install(original, &wrapper()).unwrap();
        assert_ne!(installed, original, "install must add the notify key");
        assert!(installed.contains("# my codex config"), "comment preserved");
        assert!(installed.contains("model = \"gpt-5.2\""), "key preserved");
        assert!(
            installed.contains("\"/opt/tma/tma-hook\"")
                && installed.contains("\"codex\"")
                && installed.contains("\"notify\""),
            "notify array written: {installed}"
        );
        let removed = edit_codex_uninstall(&installed).unwrap();
        assert_eq!(removed, original, "uninstall restores byte-for-byte");
    }

    #[test]
    fn codex_install_is_idempotent() {
        let once = edit_codex_install("model = \"x\"\n", &wrapper()).unwrap();
        let twice = edit_codex_install(&once, &wrapper()).unwrap();
        assert_eq!(once, twice, "re-install must be byte-identical (no-op)");
    }

    #[test]
    fn codex_install_repoints_a_stale_wrapper_but_keeps_a_foreign_one() {
        // Ours-but-stale (different wrapper path): re-pointed to the current wrapper.
        let stale = "notify = [\"/old/path/tma-hook\", \"codex\", \"notify\"]\n";
        let fixed = edit_codex_install(stale, &wrapper()).unwrap();
        assert!(fixed.contains("/opt/tma/tma-hook"), "re-pointed: {fixed}");
        assert!(!fixed.contains("/old/path"), "stale path replaced");

        // A user's own notify program is never clobbered — install refuses.
        let foreign = "notify = [\"my-notifier\"]\n";
        assert!(
            edit_codex_install(foreign, &wrapper()).is_err(),
            "must refuse to overwrite a foreign notify"
        );
        // Uninstall leaves a foreign notify untouched.
        assert_eq!(edit_codex_uninstall(foreign).unwrap(), foreign);
    }

    #[test]
    fn codex_notify_ok_detects_stale_wrapper() {
        let text = edit_codex_install("", &wrapper()).unwrap();
        assert!(codex_notify_ok(&text, &wrapper()), "current wrapper ok");
        assert!(
            !codex_notify_ok(&text, &PathBuf::from("/other/tma-hook")),
            "a different wrapper path is drift"
        );
        // A config with no notify is not "ok" (not installed).
        assert!(!codex_notify_ok("model = \"x\"\n", &wrapper()));
    }
}
