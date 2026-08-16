use std::path::Path;

use super::json_value::{self, Value};

// --- statusline context shim -----------------------------------------------------
//
// An agent's statusLine command receives a per-turn JSON payload; the shim runs the user's existing
// statusline command (stdin passed through, its output emitted unchanged) and, fire-and-forget,
// forwards the same payload to `tma event --agent <agent> --kind context --pane "$TMUX_PANE"
// --payload -`. Chaining means installing never
// replaces or breaks a user's statusline (open question 5). Claude (`~/.claude/settings.json`) and
// Cursor (`~/.cursor/cli-config.json`) share this machinery — same `statusLine` object
// shape, only the agent name in the forward and the config file differ.

/// The substring identifying a tma context shim for `agent` inside a `statusLine.command`. `--check`
/// treats a command lacking it as clobbered (the forward was overwritten). The user may still freely
/// edit the wrapped inner command — that keeps the marker, so it stays recognized.
fn statusline_marker(agent: &str) -> String {
    format!("event --agent {agent} --kind context")
}

/// The delimiter separating the forward from the wrapped inner statusline command. Extraction on
/// uninstall splits on it to recover the original command; its absence means an empty inner.
const STATUSLINE_INNER_DELIM: &str = " & printf '%s' \"$_TMA_SL\" | ";

/// Render the statusline shim command for `agent` via `bin`, wrapping `inner` (the user's prior
/// statusline command, empty when there was none). It captures stdin once, forwards a copy
/// fire-and-forget to `tma event --kind context` with `$TMUX_PANE`, and pipes the same stdin to `inner`
/// whose stdout becomes the statusline. The binary is late-bound the way the `tma-hook` wrapper
/// binds it (install-time path first, `$PATH` after), so a moved binary drops the context lane
/// rather than the whole statusline.
fn render_statusline_shim(bin: &Path, agent: &str, inner: &str) -> String {
    let fwd = format!(
        "_TMA_SL=$(cat); _TMA_BIN='{}'; [ -x \"$_TMA_BIN\" ] || _TMA_BIN=tma; \
         printf '%s' \"$_TMA_SL\" | \"$_TMA_BIN\" event --agent {agent} --kind context \
         --pane \"${{TMUX_PANE:-}}\" --payload - >/dev/null 2>&1",
        bin.display()
    );
    if inner.is_empty() {
        fwd
    } else {
        format!("{fwd}{STATUSLINE_INNER_DELIM}{inner}")
    }
}

/// Recover the wrapped inner statusline command from a shim (empty when the shim wrapped nothing).
fn extract_statusline_inner(cmd: &str) -> String {
    cmd.split_once(STATUSLINE_INNER_DELIM)
        .map(|(_, inner)| inner.to_string())
        .unwrap_or_default()
}

/// The `statusLine.command` string from a parsed settings root, if any.
fn statusline_command(root: &Value) -> Option<&str> {
    root.get("statusLine")
        .and_then(|s| s.get("command"))
        .and_then(Value::as_str)
}

/// Install the statusline shim into a parsed-then-reserialized settings text: wrap any existing
/// command (re-wrapping our own shim so a moved binary path re-points, idempotent otherwise), else
/// create a `statusLine` object. Preserves any extra `statusLine` keys and unrelated settings.
pub(super) fn edit_statusline_install(
    old: &str,
    bin: &Path,
    agent: &str,
) -> Result<String, String> {
    let mut root = json_value::parse(old)?;
    if root.as_object_mut().is_none() {
        return Err("settings root is not a JSON object".to_string());
    }
    let marker = statusline_marker(agent);
    let inner = match statusline_command(&root) {
        Some(cmd) if cmd.contains(&marker) => extract_statusline_inner(cmd),
        Some(cmd) => cmd.to_string(),
        None => String::new(),
    };
    let shim = render_statusline_shim(bin, agent, &inner);
    if root.get("statusLine").is_none() {
        root.obj_set("statusLine", Value::Obj(Vec::new()));
    }
    let sl = root
        .as_object_mut()
        .unwrap()
        .iter_mut()
        .find(|(k, _)| k == "statusLine")
        .map(|(_, v)| v)
        .unwrap();
    if sl.as_object_mut().is_none() {
        *sl = Value::Obj(Vec::new()); // a non-object statusLine is replaced
    }
    sl.obj_set("type", Value::Str("command".to_string()));
    sl.obj_set("command", Value::Str(shim));
    Ok(json_value::to_pretty(&root))
}

/// Remove the statusline shim, restoring the wrapped inner command (or dropping the whole
/// `statusLine` object when the shim wrapped nothing). A foreign or absent command is left untouched.
pub(super) fn edit_statusline_uninstall(old: &str, agent: &str) -> Result<String, String> {
    let mut root = json_value::parse(old)?;
    let Some(cmd) = statusline_command(&root).map(str::to_string) else {
        return Ok(json_value::to_pretty(&root));
    };
    if !cmd.contains(&statusline_marker(agent)) {
        return Ok(json_value::to_pretty(&root)); // not ours — leave it
    }
    let inner = extract_statusline_inner(&cmd);
    if inner.is_empty() {
        root.obj_remove("statusLine");
    } else if let Some(sl) = root.as_object_mut().and_then(|o| {
        o.iter_mut()
            .find(|(k, _)| k == "statusLine")
            .map(|(_, v)| v)
    }) {
        sl.obj_set("command", Value::Str(inner));
    }
    Ok(json_value::to_pretty(&root))
}

/// The statusline shim's wiring state, folded into the Claude adapter's [`HookWiring`](super::HookWiring) for `--check`.
pub(super) enum StatuslineWiring {
    Wired,
    NotInstalled,
    /// Present but clobbered (the forward overwritten) or stale (a different binary path); the reason.
    Drift(String),
}

/// Classify the statusline shim (read-only): `Wired` when the current shim is exactly what we would
/// write, `Drift` when a command is present but not our shim or references a stale binary, else absent.
pub(super) fn classify_statusline(
    root: &Value,
    bin: &Path,
    settings: &Path,
    agent: &str,
) -> StatuslineWiring {
    match statusline_command(root) {
        None => StatuslineWiring::NotInstalled,
        Some(c) if !c.contains(&statusline_marker(agent)) => StatuslineWiring::Drift(format!(
            "agent {agent}: statusline command in {} is not tma's context shim (clobbered); reinstall",
            settings.display()
        )),
        Some(c) => {
            if c == render_statusline_shim(bin, agent, &extract_statusline_inner(c)) {
                StatuslineWiring::Wired
            } else {
                StatuslineWiring::Drift(format!(
                    "agent {agent}: statusline context shim in {} references a different binary; reinstall",
                    settings.display()
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // ---- statusline context shim ----------------------------------------------

    fn bin() -> PathBuf {
        PathBuf::from("/opt/tma/tma")
    }

    #[test]
    fn statusline_shim_install_from_nothing_then_uninstall_is_byte_identical() {
        let original = json_value::to_pretty(&json_value::parse(r#"{"model":"opus"}"#).unwrap());
        let installed = edit_statusline_install(&original, &bin(), "claude").unwrap();
        assert_ne!(installed, original);
        assert!(
            installed.contains(&statusline_marker("claude")),
            "the shim forwards to the context intake"
        );
        // Wrapping nothing ⇒ uninstall drops the whole statusLine object, restoring the file.
        let removed = edit_statusline_uninstall(&installed, "claude").unwrap();
        assert_eq!(removed, original);
    }

    #[test]
    fn statusline_shim_wraps_and_restores_a_user_command() {
        let inner = "~/.claude/my-statusline.sh";
        let original = json_value::to_pretty(
            &json_value::parse(&format!(
                r#"{{"statusLine":{{"type":"command","command":"{inner}"}}}}"#
            ))
            .unwrap(),
        );
        let installed = edit_statusline_install(&original, &bin(), "claude").unwrap();
        assert!(installed.contains(&statusline_marker("claude")));
        assert!(
            installed.contains(inner),
            "the user's command is chained, not replaced"
        );
        // Re-install is idempotent (re-wraps our own inner, byte-identical for the same binary).
        assert_eq!(
            edit_statusline_install(&installed, &bin(), "claude").unwrap(),
            installed
        );
        // Uninstall restores the original wrapped command.
        let removed = edit_statusline_uninstall(&installed, "claude").unwrap();
        assert_eq!(removed, original);
    }

    #[test]
    fn statusline_shim_binds_the_binary_at_fire_time() {
        // The install-time path is the fast path, but a moved/rebuilt binary must not kill the
        // forward: the shim falls back to `tma` on $PATH, the tma-hook wrapper's own order.
        let shim = render_statusline_shim(&bin(), "claude", "");
        assert!(shim.contains("_TMA_BIN='/opt/tma/tma'"), "{shim}");
        assert!(shim.contains("|| _TMA_BIN=tma"), "{shim}");
        assert!(
            !shim.contains("| /opt/tma/tma event"),
            "the binary is invoked through the resolved variable, not inlined: {shim}"
        );
    }

    #[test]
    fn statusline_check_detects_clobber_and_stale_binary() {
        let installed = edit_statusline_install("{}\n", &bin(), "claude").unwrap();
        let root = json_value::parse(&installed).unwrap();
        let settings = PathBuf::from("/home/u/.claude/settings.json");
        assert!(matches!(
            classify_statusline(&root, &bin(), &settings, "claude"),
            StatuslineWiring::Wired
        ));
        // A user overwriting the whole command (removing our forward) is a clobber.
        let clobbered =
            json_value::parse(r#"{"statusLine":{"type":"command","command":"my-line.sh"}}"#)
                .unwrap();
        assert!(matches!(
            classify_statusline(&clobbered, &bin(), &settings, "claude"),
            StatuslineWiring::Drift(_)
        ));
        // A moved binary path is stale (still ours, but not what we would write now).
        let stale = json_value::parse(&installed).unwrap();
        assert!(matches!(
            classify_statusline(&stale, &PathBuf::from("/new/tma"), &settings, "claude"),
            StatuslineWiring::Drift(_)
        ));
        // No statusLine at all is a clean not-installed.
        assert!(matches!(
            classify_statusline(
                &json_value::parse("{}").unwrap(),
                &bin(),
                &settings,
                "claude"
            ),
            StatuslineWiring::NotInstalled
        ));
    }

    #[test]
    fn statusline_check_survives_the_user_editing_the_inner_command() {
        // Wrapping a user command, then the user edits THEIR statusline: the forward is intact, so the
        // shim stays recognized (open question 5) — not a clobber.
        let installed = edit_statusline_install(
            r#"{"statusLine":{"type":"command","command":"old.sh"}}"#,
            &bin(),
            "claude",
        )
        .unwrap();
        let edited = installed.replace("old.sh", "new.sh");
        let root = json_value::parse(&edited).unwrap();
        assert!(matches!(
            classify_statusline(&root, &bin(), &PathBuf::from("s.json"), "claude"),
            StatuslineWiring::Wired
        ));
        assert_eq!(
            extract_statusline_inner(statusline_command(&root).unwrap()),
            "new.sh"
        );
    }

    #[test]
    fn statusline_shim_cursor_preserves_padding_and_uses_a_cursor_marker() {
        // Cursor's cli-config.json statusLine carries a sibling `padding` key the round-trip must keep
        // byte-faithfully, and the shim's forward must name `cursor`, not `claude`.
        let original = json_value::to_pretty(
            &json_value::parse(
                r#"{"statusLine":{"type":"command","command":"my-line.sh","padding":2}}"#,
            )
            .unwrap(),
        );
        let installed = edit_statusline_install(&original, &bin(), "cursor").unwrap();
        assert!(
            installed.contains(&statusline_marker("cursor")),
            "the forward targets the cursor context intake"
        );
        assert!(
            !installed.contains(&statusline_marker("claude")),
            "a cursor shim is not a claude shim"
        );
        assert!(
            installed.contains("\"padding\": 2"),
            "the unknown padding key survives install: {installed}"
        );
        assert!(
            installed.contains("my-line.sh"),
            "the user command is chained"
        );

        // `--check`: our own shim is Wired; a claude marker would misclassify it as a clobber, so the
        // agent-parameterized classify is what keeps the two shims distinct.
        let root = json_value::parse(&installed).unwrap();
        let cfg = PathBuf::from("/home/u/.cursor/cli-config.json");
        assert!(matches!(
            classify_statusline(&root, &bin(), &cfg, "cursor"),
            StatuslineWiring::Wired
        ));
        assert!(matches!(
            classify_statusline(&root, &bin(), &cfg, "claude"),
            StatuslineWiring::Drift(_)
        ));

        // Uninstall restores the original, padding intact.
        let removed = edit_statusline_uninstall(&installed, "cursor").unwrap();
        assert_eq!(removed, original);
    }
}
