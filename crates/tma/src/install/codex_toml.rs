use std::path::Path;

use super::json_value::{self, Value};
use super::paths;
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

/// Whether a `notify` item is tma's own entry: its program (element 0) is a `tma-hook` wrapper.
/// Robust to the wrapper path moving between installs, and to the user having renamed nothing.
/// Deliberately blind to a CHAINED entry ([`chained_notify`]), whose program belongs to someone
/// else: uninstall removes what this returns true for, and a foreign argv is never tma's to delete.
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

/// A `notify` entry owned by another program that passes tma's command on to it.
///
/// Codex allows one `notify`, so a tool that wants the signal takes the key and forwards what was
/// there before. Codex Computer Use writes `["…/SkyComputerUseClient", "turn-ended",
/// "--previous-notify", "[\"tma-hook\",\"codex\",\"notify\"]"]`. tma's wiring still fires, so this
/// is a working install, not a missing one.
pub(super) struct Chain {
    /// The wrapping program's own name (`SkyComputerUseClient`), for the report.
    pub program: String,
    /// The `tma-hook` reference inside the chained command, for the hand-edit message.
    pub reference: String,
    /// Whether that reference reaches the wrapper this build writes.
    pub current: bool,
}

/// The chained tma command inside a foreign `notify` argv, `None` when there is none.
///
/// The chained command is looked for in EVERY element rather than only after `--previous-notify`:
/// that flag is one tool's spelling, and an argv that carries `tma-hook codex notify` anywhere is
/// wiring that fires either way. Both shapes seen in the wild are read: a JSON string array (Codex
/// Computer Use) and a plain command string.
pub(super) fn chained_notify(item: &toml_edit::Item, wrapper: &Path) -> Option<Chain> {
    let arr = item.as_array()?;
    let argv: Vec<&str> = arr.iter().map(|v| v.as_str()).collect::<Option<_>>()?;
    let program = Path::new(argv.first()?)
        .file_name()
        .and_then(|n| n.to_str())?
        .to_string();
    let reference = argv.iter().skip(1).find_map(|el| chained_tma_ref(el))?;
    let current = paths::same_wrapper_file(&reference, wrapper);
    Some(Chain {
        program,
        reference,
        current,
    })
}

/// The `tma-hook` reference inside one argv element that carries tma's command, `None` otherwise.
/// A JSON string array first (`["tma-hook","codex","notify"]`), else the same argv as a plain
/// whitespace-separated command string.
fn chained_tma_ref(element: &str) -> Option<String> {
    if let Ok(Value::Arr(items)) = json_value::parse(element) {
        let argv: Vec<&str> = items.iter().map(|v| v.as_str()).collect::<Option<_>>()?;
        return tma_notify_argv(&argv);
    }
    tma_notify_argv(&element.split_whitespace().collect::<Vec<_>>())
}

/// The program of an argv that is exactly tma's codex notify command, `None` for anything else.
fn tma_notify_argv(argv: &[&str]) -> Option<String> {
    let [prog, agent, event] = argv else {
        return None;
    };
    if *agent != CODEX_AGENT || *event != CODEX_NOTIFY_EVENT {
        return None;
    }
    (Path::new(prog).file_name().and_then(|n| n.to_str()) == Some(paths::WRAPPER_NAME))
        .then(|| (*prog).to_string())
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
        // Another program owns `notify` and chains ours onward. A working chain is left exactly as
        // it is; a stale one is named for a hand edit, because rewriting a foreign program's argv
        // is not tma's to do (the flag, its order, and the encoding are that program's contract).
        Some(item) => match chained_notify(item, wrapper) {
            Some(chain) if chain.current => return Ok(old.to_string()),
            Some(chain) => {
                return Err(format!(
                    "Codex config.toml `notify` runs {} and chains tma's command onward, but that \
                     chained command still names {}, which is not the wrapper this build writes \
                     ({}). tma will not rewrite another program's argv: edit the chained command in \
                     config.toml by hand, then re-run.",
                    chain.program,
                    chain.reference,
                    wrapper.display()
                ))
            }
            // A foreign notify program: never overwrite it (Codex supports only one).
            None => {
                return Err(
                    "Codex config.toml already defines a `notify` program that is not tma's. \
                     Codex allows only one notify program, so tma will not overwrite it: point \
                     your notify at `tma-hook codex notify`, or remove it, then re-run."
                        .to_string(),
                )
            }
        },
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

/// What Codex's `notify` key says about tma's wiring (used by `--check` and `tma doctor`).
pub(super) struct NotifyStatus {
    /// tma's command is reachable from that key at all, directly or through a chain.
    pub present: bool,
    /// And it reaches the wrapper this build writes.
    pub current: bool,
    /// The program that owns the key and chains ours onward, `None` for tma's own entry.
    pub chained_through: Option<String>,
}

/// Read Codex's `notify` key. Unlike [`codex_notify_matches`], which asks whether a re-install would
/// be a no-op, this asks whether the wiring WORKS: an absolute path an older tma wrote and the bare
/// name this build would write are the same file, and reporting that as stale is noise. A chained
/// entry counts as present, since tma's command still fires; whose argv it sits in is a note, not a
/// fault.
pub(super) fn notify_status(text: &str, wrapper: &Path) -> NotifyStatus {
    let absent = NotifyStatus {
        present: false,
        current: false,
        chained_through: None,
    };
    let Ok(doc) = text.parse::<toml_edit::DocumentMut>() else {
        return absent;
    };
    let Some(item) = doc.get("notify") else {
        return absent;
    };
    if codex_notify_is_ours(item) {
        let current = codex_notify_matches(item, wrapper)
            || (notify_tail_ok(item)
                && item
                    .as_array()
                    .and_then(|a| a.get(0))
                    .and_then(|v| v.as_str())
                    .is_some_and(|prog| paths::same_wrapper_file(prog, wrapper)));
        return NotifyStatus {
            present: true,
            current,
            chained_through: None,
        };
    }
    match chained_notify(item, wrapper) {
        Some(chain) => NotifyStatus {
            present: true,
            current: chain.current,
            chained_through: Some(chain.program),
        },
        None => absent,
    }
}

/// Whether a `notify` array's tail is tma's `["…", "codex", "notify"]` (the program is judged
/// separately, since two spellings can name one file).
fn notify_tail_ok(item: &toml_edit::Item) -> bool {
    item.as_array().is_some_and(|arr| {
        arr.len() == 3
            && arr.get(1).and_then(|v| v.as_str()) == Some(CODEX_AGENT)
            && arr.get(2).and_then(|v| v.as_str()) == Some(CODEX_NOTIFY_EVENT)
    })
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
hooks). Codex pins that trust to the exact command string, so an install that CHANGES it (an \
[install] wrapper_ref switch, a moved wrapper) has to be trusted again. The notify signal works \
without this step.";

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
        let status = notify_status(&text, &wrapper());
        assert!(status.present && status.current, "current wrapper ok");
        assert!(status.chained_through.is_none(), "tma's own entry");
        assert!(
            !notify_status(&text, &PathBuf::from("/other/tma-hook")).current,
            "a different wrapper path is drift"
        );
        // A config with no notify is not "ok" (not installed).
        assert!(!notify_status("model = \"x\"\n", &wrapper()).present);
    }

    /// The real shape from the field: Codex Computer Use takes `notify` for itself and passes what
    /// was there before as a JSON array in `--previous-notify`. tma's command still fires, so this
    /// is a working install reported through its chain, not a missing entry.
    #[test]
    fn a_chained_notify_reads_as_wired_and_names_the_wrapping_program() {
        let chained = |reference: &str| {
            format!(
                "notify = [\"/Apps/Codex Computer Use.app/SkyComputerUseClient\", \"turn-ended\", \
                 \"--previous-notify\", \"[\\\"{reference}\\\",\\\"codex\\\",\\\"notify\\\"]\"]\n"
            )
        };

        let current = chained("/opt/tma/tma-hook");
        let status = notify_status(&current, &wrapper());
        assert!(status.present, "a chained entry is installed wiring");
        assert!(status.current, "and it names this build's wrapper");
        assert_eq!(
            status.chained_through.as_deref(),
            Some("SkyComputerUseClient"),
            "the report names the program that owns the key"
        );

        // Chained and current: install must leave the foreign argv exactly as it found it.
        assert_eq!(
            edit_codex_install(&current, &wrapper()).unwrap(),
            current,
            "a current chain is never rewritten"
        );

        // Chained but stale: refused, naming the hand edit rather than rewriting someone's argv.
        let stale = chained("/old/path/tma-hook");
        let stale_status = notify_status(&stale, &wrapper());
        assert!(stale_status.present && !stale_status.current);
        let err = edit_codex_install(&stale, &wrapper()).unwrap_err();
        assert!(
            err.contains("SkyComputerUseClient") && err.contains("/old/path/tma-hook"),
            "the refusal names the wrapping program and the stale reference: {err}"
        );
        assert!(
            err.contains("by hand"),
            "and says the edit is the user's to make: {err}"
        );

        // A chained command spelled as a plain string, not a JSON array.
        let plain =
            "notify = [\"/usr/bin/wrapper\", \"--then\", \"/opt/tma/tma-hook codex notify\"]\n";
        let plain_status = notify_status(plain, &wrapper());
        assert!(plain_status.present && plain_status.current);
        assert_eq!(plain_status.chained_through.as_deref(), Some("wrapper"));

        // A foreign notify with no tma command anywhere in it is still foreign.
        let foreign = "notify = [\"my-notifier\", \"--previous-notify\", \"[\\\"other\\\"]\"]\n";
        let foreign_status = notify_status(foreign, &wrapper());
        assert!(!foreign_status.present && foreign_status.chained_through.is_none());
        assert!(
            edit_codex_install(foreign, &wrapper()).is_err(),
            "and install still refuses to overwrite it"
        );
        // Uninstall never touches a chain: the argv belongs to the wrapping program.
        assert_eq!(edit_codex_uninstall(&current).unwrap(), current);
    }
}
