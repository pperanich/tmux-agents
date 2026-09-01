use std::path::Path;

use super::{apply_file, confirm, print_diff};

/// The OpenCode plugin bridge, embedded for `install-hooks opencode`. Its `@@TMA_HOOK@@` token is
/// replaced with the resolved wrapper path at install time (see [`render_js_bridge`]).
pub(super) const OPENCODE_PLUGIN_SRC: &str = include_str!("../../assets/opencode-plugin.js");

/// The placeholder in [`OPENCODE_PLUGIN_SRC`] substituted with the `tma-hook` path.
const OPENCODE_HOOK_TOKEN: &str = "@@TMA_HOOK@@";

/// The banner marker `install-hooks opencode` uses to recognize its own plugin file (idempotency +
/// safe uninstall). A file carrying it is one tma wrote.
pub(super) const OPENCODE_PLUGIN_MARKER: &str = "tma OpenCode bridge";

/// The pi extension bridge, embedded for `install-hooks pi`. pi auto-discovers modules, so pi's
/// write site is a dropped JS file like OpenCode's plugin; `@@TMA_HOOK@@` becomes the wrapper path.
pub(super) const PI_EXTENSION_SRC: &str = include_str!("../../assets/pi-extension.js");

/// The `@@TMA_HOOK@@` placeholder in [`PI_EXTENSION_SRC`] (same literal OpenCode uses); kept a
/// separate named const so the two adapters stay textually independent.
pub(super) const PI_HOOK_TOKEN: &str = "@@TMA_HOOK@@";

/// The banner marker `tma install-hooks pi` looks for to recognize its own extension file
/// (idempotency + safe uninstall). A file carrying it is one tma wrote.
pub(super) const PI_EXTENSION_MARKER: &str = "tma pi bridge";

// --- JS-bridge adapters (OpenCode plugin, pi extension) --------------------------
//
// OpenCode and pi both wire tma by dropping a rendered JS module (referencing the wrapper, carrying
// a banner marker); the identical mechanics live in these generic helpers.

/// Render a JS bridge asset with the wrapper path substituted for its token. Deterministic, so a
/// re-install with the same path is byte-identical (idempotent).
fn render_js_bridge(src: &str, token: &str, wrapper: &Path) -> String {
    src.replace(token, &wrapper.display().to_string())
}

/// Install a JS bridge: write the rendered module into `path` (diff + confirm, idempotent).
/// Refuses a pre-existing file that does not carry `marker`, the same rule uninstall applies, since
/// the write replaces the whole file and would otherwise eat a hand-written module under `--yes`.
/// Returns `true` on success or no-op.
pub(super) fn install_js_bridge(
    path: &Path,
    src: &str,
    token: &str,
    marker: &str,
    wrapper: &Path,
    assume_yes: bool,
    label: &str,
) -> bool {
    let old = match super::read_existing(path, "") {
        Ok(text) => text,
        Err(err) => {
            eprintln!("tma: {err}");
            return false;
        }
    };
    if !old.is_empty() && !old.contains(marker) {
        eprintln!(
            "tma: {} is not a tma {label} (no marker); remove it and re-run to install",
            path.display()
        );
        return false;
    }
    let new = render_js_bridge(src, token, wrapper);
    apply_file(path, &old, &new, assume_yes, label)
}

/// Uninstall a JS bridge: remove the file iff it carries `marker` (one tma wrote), never clobbering
/// a hand-written user file. A missing file is a clean no-op.
pub(super) fn uninstall_js_bridge(
    path: &Path,
    marker: &str,
    assume_yes: bool,
    label: &str,
) -> bool {
    let Ok(existing) = std::fs::read_to_string(path) else {
        println!("tma: {label} already absent ({})", path.display());
        return true;
    };
    if !existing.contains(marker) {
        eprintln!(
            "tma: {} is not a tma {label} (no marker); leaving it untouched",
            path.display()
        );
        return true;
    }
    println!("tma: proposed change to {} (remove):", path.display());
    print_diff(&existing, "");
    if !assume_yes && !confirm() {
        println!("tma: aborted; no changes written");
        return false;
    }
    match std::fs::remove_file(path) {
        Ok(()) => true,
        Err(err) => {
            eprintln!("tma: cannot remove {}: {err}", path.display());
            false
        }
    }
}

/// Whether a JS bridge is installed AND reaches the current wrapper (used by `--check`). A missing
/// file, or one whose reference resolves to a different file (or to nothing), is drift. A rendered
/// module written by an older tma spells the wrapper as an absolute path where this build would
/// write the bare name; that is the same file, so it is not drift.
pub(super) fn js_bridge_ok(path: &Path, wrapper: &Path, marker: &str) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    if !text.contains(marker) {
        return false;
    }
    text.contains(&wrapper.display().to_string())
        || embedded_wrapper(&text).is_some_and(|r| super::paths::same_wrapper_file(&r, wrapper))
}

/// The wrapper reference a rendered bridge carries, read back out of its `const TMA_HOOK = "…";`
/// line (the one place [`render_js_bridge`] substitutes).
fn embedded_wrapper(text: &str) -> Option<String> {
    let rest = text.split_once("const TMA_HOOK = \"")?.1;
    let (reference, _) = rest.split_once('"')?;
    Some(reference.to_string())
}

/// Install the OpenCode plugin into its plugin dir.
pub(super) fn install_opencode_plugin(plugin: &Path, wrapper: &Path, assume_yes: bool) -> bool {
    install_js_bridge(
        plugin,
        OPENCODE_PLUGIN_SRC,
        OPENCODE_HOOK_TOKEN,
        OPENCODE_PLUGIN_MARKER,
        wrapper,
        assume_yes,
        "opencode plugin",
    )
}

/// Uninstall the OpenCode plugin (remove iff it carries the banner marker).
pub(super) fn uninstall_opencode_plugin(plugin: &Path, assume_yes: bool) -> bool {
    uninstall_js_bridge(
        plugin,
        OPENCODE_PLUGIN_MARKER,
        assume_yes,
        "opencode plugin",
    )
}

/// Whether the OpenCode plugin is installed and references the current wrapper (`--check`).
pub(super) fn opencode_plugin_ok(plugin: &Path, wrapper: &Path) -> bool {
    js_bridge_ok(plugin, wrapper, OPENCODE_PLUGIN_MARKER)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn wrapper() -> PathBuf {
        PathBuf::from("/opt/tma/tma-hook")
    }

    #[test]
    fn opencode_plugin_renders_with_wrapper_and_all_tokens() {
        let js = render_js_bridge(
            OPENCODE_PLUGIN_SRC,
            OPENCODE_HOOK_TOKEN,
            &PathBuf::from("/opt/tma/tma-hook"),
        );
        assert!(js.contains("/opt/tma/tma-hook"), "wrapper path baked in");
        assert!(
            !js.contains(OPENCODE_HOOK_TOKEN),
            "placeholder fully substituted"
        );
        assert!(js.contains(OPENCODE_PLUGIN_MARKER), "banner marker present");
        for t in [
            "session-start",
            "user-prompt-submit",
            "stop",
            "permission-required",
        ] {
            assert!(js.contains(t), "plugin emits {t}");
        }
    }

    #[test]
    fn opencode_install_uninstall_check_roundtrip() {
        let dir = std::env::temp_dir().join(format!(
            "tma_oc_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let plugin = dir.join("opencode/plugin/tma.js");
        let wrapper = PathBuf::from("/opt/tma/tma-hook");

        // Absent → install writes a plugin that check accepts.
        assert!(!plugin.exists());
        assert!(install_opencode_plugin(&plugin, &wrapper, true));
        assert!(plugin.exists());
        assert!(opencode_plugin_ok(&plugin, &wrapper));

        // A different wrapper path is drift (stale plugin).
        assert!(!opencode_plugin_ok(
            &plugin,
            &PathBuf::from("/other/tma-hook")
        ));

        // Re-install is byte-identical (idempotent).
        let before = std::fs::read_to_string(&plugin).unwrap();
        assert!(install_opencode_plugin(&plugin, &wrapper, true));
        assert_eq!(before, std::fs::read_to_string(&plugin).unwrap());

        // Uninstall removes exactly our file.
        assert!(uninstall_opencode_plugin(&plugin, true));
        assert!(!plugin.exists(), "uninstall removes the plugin");

        // A foreign plugin at the same path is never clobbered — by uninstall or by install.
        std::fs::create_dir_all(plugin.parent().unwrap()).unwrap();
        let foreign = "// a user's own plugin\n";
        std::fs::write(&plugin, foreign).unwrap();
        assert!(uninstall_opencode_plugin(&plugin, true));
        assert!(plugin.exists(), "foreign plugin preserved");
        assert!(
            !install_opencode_plugin(&plugin, &wrapper, true),
            "install must refuse an unmarked file rather than overwrite it"
        );
        assert_eq!(
            std::fs::read_to_string(&plugin).unwrap(),
            foreign,
            "the refused install left the user's file untouched"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pi_extension_renders_with_wrapper_and_all_tokens() {
        let js = render_js_bridge(PI_EXTENSION_SRC, PI_HOOK_TOKEN, &wrapper());
        assert!(js.contains("/opt/tma/tma-hook"), "wrapper path baked in");
        assert!(!js.contains(PI_HOOK_TOKEN), "placeholder fully substituted");
        assert!(js.contains(PI_EXTENSION_MARKER), "banner marker present");
        for t in [
            "session_start",
            "before_agent_start",
            "tool_execution_start",
            "agent_settled",
            "session_shutdown",
        ] {
            assert!(js.contains(t), "extension fires {t}");
        }
    }

    #[test]
    fn pi_install_uninstall_check_roundtrip() {
        let dir = std::env::temp_dir().join(format!(
            "tma_pi_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let ext = dir.join(".pi/agent/extensions/tma.js");
        let wrapper = PathBuf::from("/opt/tma/tma-hook");

        // Absent → install writes an extension that check accepts.
        assert!(!ext.exists());
        assert!(install_js_bridge(
            &ext,
            PI_EXTENSION_SRC,
            PI_HOOK_TOKEN,
            PI_EXTENSION_MARKER,
            &wrapper,
            true,
            "pi extension"
        ));
        assert!(ext.exists());
        assert!(js_bridge_ok(&ext, &wrapper, PI_EXTENSION_MARKER));

        // A different wrapper path is drift (stale extension).
        assert!(!js_bridge_ok(
            &ext,
            &PathBuf::from("/other/tma-hook"),
            PI_EXTENSION_MARKER
        ));

        // Re-install is byte-identical (idempotent).
        let before = std::fs::read_to_string(&ext).unwrap();
        assert!(install_js_bridge(
            &ext,
            PI_EXTENSION_SRC,
            PI_HOOK_TOKEN,
            PI_EXTENSION_MARKER,
            &wrapper,
            true,
            "pi extension"
        ));
        assert_eq!(before, std::fs::read_to_string(&ext).unwrap());

        // Uninstall removes exactly our file.
        assert!(uninstall_js_bridge(
            &ext,
            PI_EXTENSION_MARKER,
            true,
            "pi extension"
        ));
        assert!(!ext.exists(), "uninstall removes the extension");

        // A foreign extension at the same path is never clobbered — by uninstall or by install.
        std::fs::create_dir_all(ext.parent().unwrap()).unwrap();
        let foreign = "// a user's own pi extension\n";
        std::fs::write(&ext, foreign).unwrap();
        assert!(uninstall_js_bridge(
            &ext,
            PI_EXTENSION_MARKER,
            true,
            "pi extension"
        ));
        assert!(ext.exists(), "foreign extension preserved");
        assert!(
            !install_js_bridge(
                &ext,
                PI_EXTENSION_SRC,
                PI_HOOK_TOKEN,
                PI_EXTENSION_MARKER,
                &wrapper,
                true,
                "pi extension"
            ),
            "install must refuse an unmarked file rather than overwrite it"
        );
        assert_eq!(
            std::fs::read_to_string(&ext).unwrap(),
            foreign,
            "the refused install left the user's file untouched"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
