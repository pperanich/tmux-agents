//! Action manifest discovery: the bundled corpus embedded at compile time from
//! `tma-core/actions/`, overlaid with user actions from `~/.config/tma/actions/` shadowing by
//! filename stem — exactly the discipline [`crate::manifests`] uses for agent manifests. The schema,
//! validation, and gate evaluation live in [`tma_core::action`]; this module only finds, loads, and
//! resolves the effective set. Parse errors are loud (they name the file), never swallowed.
//!
//! The `tma act` path loads the set fresh on every invocation (a one-shot CLI), so a dropped or
//! edited action file takes effect immediately — hot-reload by construction, alongside the agent
//! manifest reload the same discovery discipline drives. The daemon has no action path, so
//! there is nothing to hot-swap in a long-lived process.

use std::path::{Path, PathBuf};

use tma_core::{ActionError, ActionManifest};

/// The bundled action corpus, embedded at compile time (the safety-critical guarded
/// keystrokes ship as manifests). Each entry is `(stem, toml)`; the stem is the load identity the
/// `name` must equal and the key user files shadow on.
const BUNDLED: &[(&str, &str)] = &[
    (
        "approve",
        include_str!("../../tma-core/actions/approve.toml"),
    ),
    ("deny", include_str!("../../tma-core/actions/deny.toml")),
    (
        "interrupt",
        include_str!("../../tma-core/actions/interrupt.toml"),
    ),
    (
        "compact",
        include_str!("../../tma-core/actions/compact.toml"),
    ),
];

/// Errors loading the action set. Every variant names the offending file (an `ActionError` already
/// carries its file; the directory-read failure names the dir).
#[derive(Debug, thiserror::Error)]
pub enum ActionLoadError {
    #[error(transparent)]
    Action(#[from] ActionError),
    #[error("cannot read actions dir {dir}: {source}")]
    Dir {
        dir: String,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot read action {file}: {source}")]
    Read {
        file: String,
        #[source]
        source: std::io::Error,
    },
}

/// Load the effective action set. `action_dir` loads exactly that dir (test isolation); otherwise
/// the bundled corpus overlaid with user overrides from `~/.config/tma/actions/`, shadowing by stem.
pub fn load(action_dir: Option<&Path>) -> Result<Vec<ActionManifest>, ActionLoadError> {
    if let Some(dir) = action_dir {
        return load_dir(dir);
    }

    let mut loaded: Vec<ActionManifest> = Vec::with_capacity(BUNDLED.len());
    for (stem, toml_src) in BUNDLED {
        let file = format!("<bundled>/{stem}.toml");
        loaded.push(ActionManifest::parse(toml_src, stem, &file)?);
    }

    if let Some(user_dir) = user_actions_dir() {
        if user_dir.is_dir() {
            for a in load_dir(&user_dir)? {
                match loaded.iter_mut().find(|e| e.name == a.name) {
                    Some(existing) => *existing = a, // user override shadows bundled
                    None => loaded.push(a),
                }
            }
        }
    }
    Ok(loaded)
}

/// Find a loaded action by name (the invocation key, equal to its stem).
pub fn find<'a>(actions: &'a [ActionManifest], name: &str) -> Option<&'a ActionManifest> {
    actions.iter().find(|a| a.name == name)
}

/// One action file's parse outcome, for `tma doctor`: the source file plus either the
/// parsed manifest or its load-error message (parse error, stem/name mismatch, unknown `requires`
/// token, all surfaced by [`ActionManifest::parse`]). The error is a rendered string because doctor
/// only reports it; it never re-dispatches on the variant.
pub struct ActionDiag {
    pub file: String,
    pub result: Result<ActionManifest, String>,
}

/// Parse every action file (bundled, then the user dir) individually for `tma doctor`, collecting a
/// per-file result rather than stopping at the first error (as [`load`] does) — doctor reports every
/// issue at once. A user file shadows a bundled one by stem, but doctor keeps both diagnoses so a
/// broken user override still surfaces; a user-dir read failure becomes one error entry naming the dir.
pub fn diagnose() -> Vec<ActionDiag> {
    let mut out: Vec<ActionDiag> = BUNDLED
        .iter()
        .map(|(stem, toml_src)| {
            let file = format!("<bundled>/{stem}.toml");
            let result = ActionManifest::parse(toml_src, stem, &file).map_err(|e| e.to_string());
            ActionDiag { file, result }
        })
        .collect();

    let Some(user_dir) = user_actions_dir().filter(|d| d.is_dir()) else {
        return out;
    };
    let rd = match std::fs::read_dir(&user_dir) {
        Ok(rd) => rd,
        Err(source) => {
            out.push(ActionDiag {
                file: user_dir.display().to_string(),
                result: Err(format!("cannot read actions dir: {source}")),
            });
            return out;
        }
    };
    let mut paths: Vec<PathBuf> = rd
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "toml"))
        .collect();
    paths.sort();
    for path in &paths {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown");
        let file = path.display().to_string();
        let result = match std::fs::read_to_string(path) {
            Ok(src) => ActionManifest::parse(&src, stem, &file).map_err(|e| e.to_string()),
            Err(source) => Err(format!("cannot read file: {source}")),
        };
        out.push(ActionDiag { file, result });
    }
    out
}

fn load_dir(dir: &Path) -> Result<Vec<ActionManifest>, ActionLoadError> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|source| ActionLoadError::Dir {
            dir: dir.display().to_string(),
            source,
        })?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "toml"))
        .collect();
    entries.sort();

    let mut out = Vec::with_capacity(entries.len());
    for path in &entries {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown");
        let file = path.display().to_string();
        let toml_src = std::fs::read_to_string(path).map_err(|source| ActionLoadError::Read {
            file: file.clone(),
            source,
        })?;
        out.push(ActionManifest::parse(&toml_src, stem, &file)?);
    }
    Ok(out)
}

fn user_actions_dir() -> Option<PathBuf> {
    // XDG_CONFIG_HOME, else ~/.config — the same resolution the agent manifest loader uses.
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg).join("tma/actions"));
        }
    }
    std::env::var("HOME")
        .ok()
        .filter(|h| !h.is_empty())
        .map(|home| PathBuf::from(home).join(".config/tma/actions"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tma_core::ActionKind;

    #[test]
    fn bundled_actions_load_and_are_named_by_stem() {
        let set = load(Some(Path::new("does-not-exist-force-bundled-only")))
            .err()
            .map(|_| ()); // load_dir on a missing dir errors; assert bundled via the real path below.
        assert!(set.is_some());

        // The real bundled path: no override dir, but isolate from a developer's own ~/.config by
        // pointing HOME/XDG at an empty temp dir so only the compiled-in corpus loads.
        let tmp = std::env::temp_dir().join(format!("tma-actions-empty-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let actions = with_env(&tmp, || load(None).unwrap());
        let names: Vec<&str> = actions.iter().map(|a| a.name.as_str()).collect();
        for want in ["approve", "deny", "interrupt", "compact"] {
            assert!(
                names.contains(&want),
                "bundled {want} missing from {names:?}"
            );
        }
        // Every bundled action is a keys action (the safety-critical class).
        assert!(actions.iter().all(|a| a.kind == ActionKind::Keys));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn user_dir_shadows_bundled_by_stem() {
        let home = std::env::temp_dir().join(format!("tma-actions-home-{}", std::process::id()));
        let actions_dir = home.join(".config/tma/actions");
        std::fs::create_dir_all(&actions_dir).unwrap();
        // Shadow the bundled `approve` with a user file carrying a different label.
        std::fs::write(
            actions_dir.join("approve.toml"),
            "min_engine_version = \"0.1\"\nname = \"approve\"\nlabel = \"Approve (mine)\"\nkind = \"keys\"\n[keys]\nclaude = [\"y\", \"Enter\"]\n",
        )
        .unwrap();
        let actions = with_env(&home, || load(None).unwrap());
        let approve = find(&actions, "approve").expect("approve present");
        assert_eq!(approve.label, "Approve (mine)", "user file shadows bundled");
        assert_eq!(
            approve.keys_for("claude"),
            Some(["y".to_string(), "Enter".to_string()].as_slice())
        );
        // The other bundled actions still load (shadow is per-stem, not a replacement of the set).
        assert!(find(&actions, "deny").is_some());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn parse_error_in_user_dir_is_loud() {
        let dir = std::env::temp_dir().join(format!("tma-actions-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Stem/name mismatch is a load error naming the file (a manifest's `name` is its stem).
        std::fs::write(
            dir.join("mine.toml"),
            "min_engine_version = \"0.1\"\nname = \"other\"\nlabel = \"X\"\nkind = \"keys\"\n[keys]\nclaude = [\"1\"]\n",
        )
        .unwrap();
        let err = load(Some(&dir)).unwrap_err();
        assert!(
            matches!(
                err,
                ActionLoadError::Action(ActionError::NameMismatch { .. })
            ),
            "expected a loud NameMismatch, got {err:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Run `f` with `HOME`/`XDG_CONFIG_HOME` pointed at `home` so discovery sees a controlled user
    /// dir. Serialized process-wide because it mutates process env (`std::env::set_var`).
    fn with_env<T>(home: &Path, f: impl FnOnce() -> T) -> T {
        use std::sync::Mutex;
        static ENV_LOCK: Mutex<()> = Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev_home = std::env::var_os("HOME");
        let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("HOME", home);
        std::env::remove_var("XDG_CONFIG_HOME");
        let out = f();
        match prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        match prev_xdg {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        out
    }
}
