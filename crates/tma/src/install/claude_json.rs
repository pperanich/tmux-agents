use std::path::Path;

use super::json_value::{self, Value};

// --- Claude settings.json adapter ------------------------------------------------

/// The wrapper command an agent config invokes for one event: `<wrapper> <agent> <event>`.
pub(super) fn wrapper_command(wrapper: &Path, agent: &str, event: &str) -> String {
    format!("{} {} {}", wrapper.display(), agent, event)
}

/// The only difference between the two JSON-`hooks` editors: Claude's nested `{hooks:[{type,command}]}`
/// vs cursor's flat `{command}` (plus cursor's `version: 1`). Get-or-create, dedup-insert, and the
/// uninstall pruning (emptied array, then emptied `hooks` object) are shared by
/// [`edit_hooks_install`]/[`edit_hooks_uninstall`]; `noun` names the file in "not a JSON object" errors.
pub(super) struct HookShape {
    noun: &'static str,
    /// Build our entry for a wrapper command string.
    make_entry: fn(&str) -> Value,
    /// Whether an existing entry carries our exact wrapper command.
    entry_is_ours: fn(&Value, &str) -> bool,
    /// Run on the root object BEFORE inserting (cursor sets `version: 1` when absent).
    pre_insert: Option<fn(&mut Value)>,
    /// Run on the root AFTER the `hooks` object is pruned to empty on uninstall (cursor drops a
    /// tma-added `version: 1`).
    post_prune: Option<fn(&mut Value)>,
}

/// Claude / gemini / codex-hooks.json shape: the nested `{hooks:[{type,command}]}` entry.
const CLAUDE_SHAPE: HookShape = HookShape {
    noun: "settings",
    make_entry: command_entry,
    entry_is_ours: entry_has_command,
    pre_insert: None,
    post_prune: None,
};

/// Cursor shape: the flat `{command}` entry, with the required `version: 1` set on install and a
/// tma-added `version` dropped on a full uninstall.
pub(super) const CURSOR_SHAPE: HookShape = HookShape {
    noun: "cursor hooks.json",
    make_entry: cursor_command_entry,
    entry_is_ours: cursor_entry_has_command,
    pre_insert: Some(cursor_set_version),
    post_prune: Some(cursor_drop_version),
};

/// Insert (idempotently) the hooks block referencing the wrapper, returning the new file text. Deep
/// dedup makes re-install a no-op and the round-trip byte-identical; entry shape per [`HookShape`].
pub(super) fn edit_hooks_install(
    old: &str,
    wrapper: &Path,
    agent: &str,
    events: &[String],
    shape: &HookShape,
) -> Result<String, String> {
    let mut root = json_value::parse(old)?;
    if root.as_object_mut().is_none() {
        return Err(format!("{} root is not a JSON object", shape.noun));
    }
    if let Some(pre) = shape.pre_insert {
        pre(&mut root);
    }
    if root.get("hooks").is_none() {
        root.obj_set("hooks", Value::Obj(Vec::new()));
    }
    let hooks_obj = root
        .as_object_mut()
        .unwrap()
        .iter_mut()
        .find(|(k, _)| k == "hooks")
        .map(|(_, v)| v)
        .unwrap()
        .as_object_mut()
        .ok_or_else(|| format!("{} `hooks` is not an object", shape.noun))?;

    for event in events {
        let cmd = wrapper_command(wrapper, agent, event);
        // get-or-create the event array
        if !hooks_obj.iter().any(|(k, _)| k == event) {
            hooks_obj.push((event.clone(), Value::Arr(Vec::new())));
        }
        let arr = hooks_obj
            .iter_mut()
            .find(|(k, _)| k == event)
            .map(|(_, v)| v)
            .unwrap()
            .as_array_mut()
            .ok_or_else(|| format!("{} hooks event entry is not an array", shape.noun))?;
        if !arr.iter().any(|e| (shape.entry_is_ours)(e, &cmd)) {
            arr.push((shape.make_entry)(&cmd));
        }
    }
    Ok(json_value::to_pretty(&root))
}

/// Remove exactly our entries, pruning emptied event arrays and then the emptied `hooks` object
/// (and, per [`HookShape::post_prune`], any tma-added companion key like cursor's `version`).
pub(super) fn edit_hooks_uninstall(
    old: &str,
    wrapper: &Path,
    agent: &str,
    events: &[String],
    shape: &HookShape,
) -> Result<String, String> {
    let mut root = json_value::parse(old)?;
    let Some(root_obj) = root.as_object_mut() else {
        return Ok(old.to_string());
    };
    let Some(hooks) = root_obj
        .iter_mut()
        .find(|(k, _)| k == "hooks")
        .map(|(_, v)| v)
    else {
        return Ok(json_value::to_pretty(&root));
    };
    let Some(hooks_obj) = hooks.as_object_mut() else {
        return Ok(json_value::to_pretty(&root));
    };

    for event in events {
        let cmd = wrapper_command(wrapper, agent, event);
        if let Some(arr) = hooks_obj
            .iter_mut()
            .find(|(k, _)| k == event)
            .map(|(_, v)| v)
            .and_then(Value::as_array_mut)
        {
            arr.retain(|e| !(shape.entry_is_ours)(e, &cmd));
        }
        // drop the event key if its array is now empty
        let empty = hooks_obj
            .iter()
            .find(|(k, _)| k == event)
            .map(|(_, v)| matches!(v, Value::Arr(a) if a.is_empty()))
            .unwrap_or(false);
        if empty {
            hooks_obj.retain(|(k, _)| k != event);
        }
    }
    let hooks_empty = matches!(hooks, Value::Obj(o) if o.is_empty());
    if hooks_empty {
        root.obj_remove("hooks");
        if let Some(post) = shape.post_prune {
            post(&mut root);
        }
    }
    Ok(json_value::to_pretty(&root))
}

/// The Claude-family JSON editor (Claude/gemini `settings.json`, codex `hooks.json` — the identical
/// nested shape). A thin alias over [`edit_hooks_install`] with [`CLAUDE_SHAPE`].
pub(super) fn edit_settings_install(
    old: &str,
    wrapper: &Path,
    agent: &str,
    events: &[String],
) -> Result<String, String> {
    edit_hooks_install(old, wrapper, agent, events, &CLAUDE_SHAPE)
}

/// Uninstall counterpart to [`edit_settings_install`] (the Claude/gemini/codex nested shape).
pub(super) fn edit_settings_uninstall(
    old: &str,
    wrapper: &Path,
    agent: &str,
    events: &[String],
) -> Result<String, String> {
    edit_hooks_uninstall(old, wrapper, agent, events, &CLAUDE_SHAPE)
}

// --- Cursor hooks.json adapter ---------------------------------------------------
//
// Cursor's shape is a flat `{command}` entry (no nested `hooks` array, no `type`). These functions
// supply it to the shared editor via `CURSOR_SHAPE`, preserving unrelated user hooks.

/// One cursor hook entry for our wrapper: `{ "command": "<wrapper> cursor <event>" }`.
fn cursor_command_entry(cmd: &str) -> Value {
    Value::Obj(vec![("command".to_string(), Value::Str(cmd.to_string()))])
}

/// Whether a cursor hook entry is ours (its `command` equals our exact wrapper invocation).
pub(super) fn cursor_entry_has_command(entry: &Value, cmd: &str) -> bool {
    entry.get("command").and_then(Value::as_str) == Some(cmd)
}

/// [`HookShape::pre_insert`] for cursor: ensure the required schema `version: 1`, set only when
/// absent so a user's own `version` is never clobbered.
fn cursor_set_version(root: &mut Value) {
    if root.get("version").is_none() {
        root.obj_set("version", Value::Num("1".to_string()));
    }
}

/// [`HookShape::post_prune`] for cursor: once every tma hook is gone and the `hooks` object was
/// pruned to empty, a lone `version: 1` is the one tma added, so drop it (byte-clean uninstall from
/// a versionless file). A user's own `version` survives whenever any hook remains or it is not `1`.
fn cursor_drop_version(root: &mut Value) {
    if matches!(root.get("version"), Some(Value::Num(n)) if n == "1") {
        root.obj_remove("version");
    }
}

/// `{ "hooks": [ { "type": "command", "command": <cmd> } ] }` — one wrapper entry, no
/// matcher (fire for every occurrence; `tma event` re-applies the manifest matcher).
fn command_entry(cmd: &str) -> Value {
    Value::Obj(vec![(
        "hooks".to_string(),
        Value::Arr(vec![Value::Obj(vec![
            ("type".to_string(), Value::Str("command".to_string())),
            ("command".to_string(), Value::Str(cmd.to_string())),
        ])]),
    )])
}

/// Whether a settings hook entry contains our exact wrapper command in its `hooks` array.
pub(super) fn entry_has_command(entry: &Value, cmd: &str) -> bool {
    entry
        .get("hooks")
        .and_then(|h| match h {
            Value::Arr(a) => Some(a),
            _ => None,
        })
        .is_some_and(|a| {
            a.iter()
                .any(|c| c.get("command").and_then(Value::as_str) == Some(cmd))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifests;
    use std::path::PathBuf;
    use tma_core::Manifest;

    fn claude() -> Manifest {
        Manifest::parse(
            include_str!("../../../tma-core/manifests/claude.toml"),
            "claude.toml",
        )
        .unwrap()
    }

    fn wrapper() -> PathBuf {
        PathBuf::from("/opt/tma/tma-hook")
    }

    fn gemini() -> Manifest {
        Manifest::parse(
            include_str!("../../../tma-core/manifests/gemini.toml"),
            "gemini.toml",
        )
        .unwrap()
    }

    fn cursor() -> Manifest {
        Manifest::parse(
            include_str!("../../../tma-core/manifests/cursor.toml"),
            "cursor.toml",
        )
        .unwrap()
    }

    // ---- settings.json round-trip (byte-identical) -----------------------------

    #[test]
    fn install_then_uninstall_is_byte_identical() {
        let events = manifests::hook_events(&claude());
        // A canonical starting file with unrelated content the installer must preserve.
        let original = json_value::to_pretty(
            &json_value::parse(r#"{"model":"opus","permissions":{"allow":["Bash"]}}"#).unwrap(),
        );
        let installed = edit_settings_install(&original, &wrapper(), "claude", &events).unwrap();
        assert_ne!(installed, original, "install must change the file");
        assert!(installed.contains("/opt/tma/tma-hook claude Notification"));
        let removed = edit_settings_uninstall(&installed, &wrapper(), "claude", &events).unwrap();
        assert_eq!(removed, original, "uninstall must restore byte-for-byte");
    }

    #[test]
    fn install_is_idempotent() {
        let events = manifests::hook_events(&claude());
        let once = edit_settings_install("{}\n", &wrapper(), "claude", &events).unwrap();
        let twice = edit_settings_install(&once, &wrapper(), "claude", &events).unwrap();
        assert_eq!(once, twice, "re-install must be a no-op (deep dedup)");
    }

    #[test]
    fn install_preserves_a_users_other_hook_for_the_same_event() {
        let events = manifests::hook_events(&claude());
        // The user already has their own Stop hook; ours must be added alongside, not clobber.
        let start = r#"{
  "hooks": {
    "Stop": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "my-own-script"
          }
        ]
      }
    ]
  }
}
"#;
        let installed = edit_settings_install(start, &wrapper(), "claude", &events).unwrap();
        assert!(installed.contains("my-own-script"), "user hook preserved");
        assert!(installed.contains("/opt/tma/tma-hook claude Stop"));
        // Uninstall leaves the user's own hook intact.
        let removed = edit_settings_uninstall(&installed, &wrapper(), "claude", &events).unwrap();
        assert!(removed.contains("my-own-script"));
        assert!(!removed.contains("tma-hook claude Stop"));
    }

    // ---- Gemini settings.json adapter -------------------------------------------

    #[test]
    fn gemini_install_uninstall_is_byte_identical() {
        // Gemini's settings.json reuses the Claude JSON editor (same `hooks` shape, different
        // file), so it inherits the byte-identical round-trip + user-hook preservation.
        let events = manifests::hook_events(&gemini());
        let original = json_value::to_pretty(
            &json_value::parse(
                r#"{"security":{"auth":{"selectedType":"gemini-api-key"}},"hooks":{"AfterAgent":[{"hooks":[{"type":"command","command":"my-own-hook"}]}]}}"#,
            )
            .unwrap(),
        );
        let installed = edit_settings_install(&original, &wrapper(), "gemini", &events).unwrap();
        assert!(installed.contains("/opt/tma/tma-hook gemini SessionStart"));
        assert!(installed.contains("/opt/tma/tma-hook gemini AfterAgent"));
        assert!(installed.contains("/opt/tma/tma-hook gemini BeforeTool"));
        assert!(
            installed.contains("my-own-hook"),
            "a user's own gemini hook survives"
        );
        assert!(
            installed.contains("\"selectedType\": \"gemini-api-key\""),
            "unrelated settings preserved"
        );
        let twice = edit_settings_install(&installed, &wrapper(), "gemini", &events).unwrap();
        assert_eq!(installed, twice, "re-install must be a no-op (deep dedup)");
        let removed = edit_settings_uninstall(&installed, &wrapper(), "gemini", &events).unwrap();
        assert_eq!(removed, original, "uninstall must restore byte-for-byte");
    }

    // ---- Cursor hooks.json adapter (shared engine + version drop) ---------------

    #[test]
    fn cursor_install_uninstall_from_empty_is_byte_clean() {
        // Cursor drives the shared editor with `CURSOR_SHAPE`. Install→uninstall from a versionless
        // file must be byte-clean: the tma-added `version` is dropped with the last hook.
        let events = manifests::hook_events(&cursor());
        let empty = json_value::to_pretty(&json_value::parse("{}").unwrap());
        let installed =
            edit_hooks_install(&empty, &wrapper(), "cursor", &events, &CURSOR_SHAPE).unwrap();
        assert!(
            installed.contains("\"version\": 1"),
            "install sets the required version: {installed}"
        );
        assert!(
            installed.contains("/opt/tma/tma-hook cursor "),
            "wired the flat cursor entry: {installed}"
        );
        assert!(
            !installed.contains("\"type\": \"command\""),
            "cursor uses the flat entry, not the nested Claude shape"
        );
        // Re-install is a no-op (deep dedup, byte-identical).
        let twice =
            edit_hooks_install(&installed, &wrapper(), "cursor", &events, &CURSOR_SHAPE).unwrap();
        assert_eq!(installed, twice, "re-install must be byte-identical");
        // Uninstall drops our hooks AND the tma-added version → back to the empty object.
        let removed =
            edit_hooks_uninstall(&installed, &wrapper(), "cursor", &events, &CURSOR_SHAPE).unwrap();
        assert_eq!(
            removed, empty,
            "uninstall from a versionless file is byte-clean: {removed}"
        );
    }

    #[test]
    fn cursor_uninstall_keeps_a_users_version_and_hook() {
        // The version-drop is gated on the `hooks` object being pruned to empty: a user's own hook
        // (and the `version` they set) must survive an uninstall byte-for-byte.
        let events = manifests::hook_events(&cursor());
        let original = json_value::to_pretty(
            &json_value::parse(
                r#"{"version":1,"hooks":{"afterFileEdit":[{"command":"my-formatter.sh"}]}}"#,
            )
            .unwrap(),
        );
        let installed =
            edit_hooks_install(&original, &wrapper(), "cursor", &events, &CURSOR_SHAPE).unwrap();
        assert!(installed.contains("my-formatter.sh"), "user hook preserved");
        let removed =
            edit_hooks_uninstall(&installed, &wrapper(), "cursor", &events, &CURSOR_SHAPE).unwrap();
        assert_eq!(
            removed, original,
            "a user hook + its version survive the uninstall byte-for-byte"
        );
    }
}
