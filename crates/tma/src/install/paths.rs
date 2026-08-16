//! Config-path resolution for `install-hooks`: the `override → env → default` ladders and the
//! `ConfigPaths`/`PathOverrides` bundle resolved once so `run`, `--check`, and `diagnose_hooks`
//! cannot drift on how a path is found.

use std::path::{Path, PathBuf};

/// The agent-config write targets (the honest split), resolved up front so `--check`, install, and
/// uninstall share one set of paths.
pub(super) struct ConfigPaths {
    pub(super) settings: PathBuf,
    /// Gemini's `settings.json` — the same Claude-shape `hooks` object at a different path, so the
    /// Claude JSON editor is reused over it.
    pub(super) gemini_settings: PathBuf,
    pub(super) opencode_plugin: PathBuf,
    pub(super) codex_config: PathBuf,
    /// Codex's `hooks.json` (its second mechanism; see agent-coverage.md "Codex mapping").
    pub(super) codex_hooks: PathBuf,
    /// Cursor's `hooks.json` (its OWN JSON shape), read/written by `CursorAdapter`.
    pub(super) cursor_hooks: PathBuf,
    /// Cursor's `cli-config.json`, holding the `statusLine` context shim; a
    /// different file from its `hooks.json`, so `CursorAdapter` edits both.
    pub(super) cursor_cli_config: PathBuf,
    /// pi's extension file (`~/.pi/agent/extensions/tma.js`, or under `$PI_CODING_AGENT_DIR`),
    /// written/removed by `PiAdapter`.
    pub(super) pi_extension: PathBuf,
}

/// The CLI/env path overrides feeding [`ConfigPaths::resolve`]. A `None` field falls through to its
/// env var then its default; `doctor` passes all-`None` (env/default only).
#[derive(Default)]
pub(super) struct PathOverrides<'a> {
    pub(super) settings: Option<&'a Path>,
    pub(super) gemini_settings: Option<&'a Path>,
    pub(super) opencode_plugin: Option<&'a Path>,
    pub(super) codex_config: Option<&'a Path>,
    pub(super) codex_hooks: Option<&'a Path>,
    pub(super) cursor_hooks: Option<&'a Path>,
    pub(super) cursor_cli_config: Option<&'a Path>,
    pub(super) pi_extension: Option<&'a Path>,
}

impl ConfigPaths {
    /// Resolve every agent-config path once — the single site wiring the `resolve_*` ladders,
    /// so `run`, `--check`, and `diagnose_hooks` cannot drift on how a path is found.
    pub(super) fn resolve(o: PathOverrides) -> ConfigPaths {
        ConfigPaths {
            settings: resolve_settings(o.settings),
            gemini_settings: resolve_gemini_settings(o.gemini_settings),
            opencode_plugin: resolve_opencode_plugin(o.opencode_plugin),
            codex_config: resolve_codex_config(o.codex_config),
            codex_hooks: resolve_codex_hooks(o.codex_hooks),
            cursor_hooks: resolve_cursor_hooks(o.cursor_hooks),
            cursor_cli_config: resolve_cursor_cli_config(o.cursor_cli_config),
            pi_extension: resolve_pi_extension(o.pi_extension),
        }
    }
}

/// The `override → env var → default` ladder shared by the simple single-source resolvers (no
/// intermediate `$CODEX_HOME`/`$XDG_CONFIG_HOME` branch). Tests pass the override. An empty value
/// is treated as unset throughout this file: `VAR=` means "I did not set this", and honoring it
/// would write into the current directory instead.
fn resolve(
    override_path: Option<&Path>,
    env_key: &str,
    default: impl FnOnce() -> PathBuf,
) -> PathBuf {
    if let Some(p) = override_path {
        return p.to_path_buf();
    }
    if let Some(p) = std::env::var_os(env_key).filter(|v| !v.is_empty()) {
        return PathBuf::from(p);
    }
    default()
}

fn resolve_settings(override_path: Option<&Path>) -> PathBuf {
    resolve(override_path, "TMA_CLAUDE_SETTINGS", || {
        home_join(".claude/settings.json")
    })
}

/// Resolve Gemini's user-level `settings.json` (override, env `TMA_GEMINI_SETTINGS`, else
/// `~/.gemini/settings.json`); the project `.gemini/settings.json` override is not written.
fn resolve_gemini_settings(override_path: Option<&Path>) -> PathBuf {
    resolve(override_path, "TMA_GEMINI_SETTINGS", || {
        home_join(".gemini/settings.json")
    })
}

pub(crate) fn resolve_config_dir(override_path: Option<&Path>) -> PathBuf {
    if let Some(p) = override_path {
        return p.to_path_buf();
    }
    if let Some(p) = std::env::var_os("TMA_CONFIG_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(p);
    }
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("tma");
        }
    }
    home_join(".config/tma")
}

/// Resolve the OpenCode plugin path (override, env `TMA_OPENCODE_PLUGIN`, else the global plugin dir
/// `$XDG_CONFIG_HOME/opencode/plugin/tma.js`, falling back to `~/.config/opencode/plugin/tma.js`).
fn resolve_opencode_plugin(override_path: Option<&Path>) -> PathBuf {
    if let Some(p) = override_path {
        return p.to_path_buf();
    }
    if let Some(p) = std::env::var_os("TMA_OPENCODE_PLUGIN").filter(|v| !v.is_empty()) {
        return PathBuf::from(p);
    }
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("opencode/plugin/tma.js");
        }
    }
    home_join(".config/opencode/plugin/tma.js")
}

/// Resolve Codex's `config.toml` (override, env `TMA_CODEX_CONFIG`, `$CODEX_HOME/config.toml`, else
/// `~/.codex/config.toml`). Tests pass the override so the (often dotfiles-symlinked) real config is safe.
fn resolve_codex_config(override_path: Option<&Path>) -> PathBuf {
    if let Some(p) = override_path {
        return p.to_path_buf();
    }
    if let Some(p) = std::env::var_os("TMA_CODEX_CONFIG").filter(|v| !v.is_empty()) {
        return PathBuf::from(p);
    }
    if let Some(home) = std::env::var_os("CODEX_HOME") {
        if !home.is_empty() {
            return PathBuf::from(home).join("config.toml");
        }
    }
    home_join(".codex/config.toml")
}

fn resolve_codex_hooks(override_path: Option<&Path>) -> PathBuf {
    if let Some(p) = override_path {
        return p.to_path_buf();
    }
    if let Some(p) = std::env::var_os("TMA_CODEX_HOOKS").filter(|v| !v.is_empty()) {
        return PathBuf::from(p);
    }
    if let Some(home) = std::env::var_os("CODEX_HOME") {
        if !home.is_empty() {
            return PathBuf::from(home).join("hooks.json");
        }
    }
    home_join(".codex/hooks.json")
}

/// Resolve Cursor's user-level `hooks.json` (override, env `TMA_CURSOR_HOOKS`, else
/// `~/.cursor/hooks.json`); the project `.cursor/hooks.json` override is not written.
fn resolve_cursor_hooks(override_path: Option<&Path>) -> PathBuf {
    resolve(override_path, "TMA_CURSOR_HOOKS", || {
        home_join(".cursor/hooks.json")
    })
}

/// Resolve Cursor's user-level `cli-config.json` holding the statusLine context shim (override, env
/// `TMA_CURSOR_CLI_CONFIG`, else `~/.cursor/cli-config.json`).
fn resolve_cursor_cli_config(override_path: Option<&Path>) -> PathBuf {
    resolve(override_path, "TMA_CURSOR_CLI_CONFIG", || {
        home_join(".cursor/cli-config.json")
    })
}

/// Resolve pi's extension file (override, env `TMA_PI_EXTENSION`, `$PI_CODING_AGENT_DIR/extensions/tma.js`,
/// else the auto-discovered `~/.pi/agent/extensions/tma.js`); the project `.pi/extensions/` is not written.
fn resolve_pi_extension(override_path: Option<&Path>) -> PathBuf {
    if let Some(p) = override_path {
        return p.to_path_buf();
    }
    if let Some(p) = std::env::var_os("TMA_PI_EXTENSION").filter(|v| !v.is_empty()) {
        return PathBuf::from(p);
    }
    if let Some(dir) = std::env::var_os("PI_CODING_AGENT_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir).join("extensions/tma.js");
        }
    }
    home_join(".pi/agent/extensions/tma.js")
}

pub(super) fn resolve_wrapper(override_path: Option<&Path>) -> PathBuf {
    resolve(override_path, "TMA_WRAPPER_PATH", || {
        tma_bin()
            .parent()
            .map(|d| d.join("tma-hook"))
            .unwrap_or_else(|| PathBuf::from("tma-hook"))
    })
}

/// The tma binary path referenced by the tmux hook command (`$TMA_BIN`, else the running
/// exe). Tests point `$TMA_BIN` at the built binary.
pub(super) fn tma_bin() -> PathBuf {
    if let Some(p) = std::env::var_os("TMA_BIN").filter(|v| !v.is_empty()) {
        return PathBuf::from(p);
    }
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("tma"))
}

pub(crate) fn home_join(rel: &str) -> PathBuf {
    home_dir().join(rel)
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// `$XDG_CONFIG_HOME` when set and non-empty. Empty is treated as unset, like [`resolve_config_dir`].
fn xdg_config_home() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Resolve the user tmux config `install-keys` marks with its `source-file` line: the override
/// (`--conf`) wins, else the first of tmux's own user config files that exists.
pub(crate) fn resolve_tmux_conf(override_path: Option<&Path>) -> PathBuf {
    if let Some(p) = override_path {
        return p.to_path_buf();
    }
    tmux_conf_in(&home_dir(), xdg_config_home().as_deref())
}

/// The tmux config to mark, given a home and an optional `$XDG_CONFIG_HOME`.
///
/// The candidate order is tmux's own (verified on 3.6a via `man tmux` FILES and a scratch server
/// with a fake HOME/XDG): `~/.tmux.conf`, then `$XDG_CONFIG_HOME/tmux/tmux.conf`, then
/// `~/.config/tmux/tmux.conf`. 3.6a loads every one that exists, earlier tmux only the first, so
/// marking the first that exists lands in a file tmux loads either way.
///
/// Invariant: a path that does not exist is only ever returned when NO candidate exists, so the
/// file tma creates can never shadow a config the user already has.
fn tmux_conf_in(home: &Path, xdg: Option<&Path>) -> PathBuf {
    let dot = home.join(".tmux.conf");
    let xdg_conf = xdg.map(|x| x.join("tmux/tmux.conf"));
    let home_config = home.join(".config/tmux/tmux.conf");

    let existing = [Some(&dot), xdg_conf.as_ref(), Some(&home_config)]
        .into_iter()
        .flatten()
        .find(|p| p.is_file());
    if let Some(found) = existing {
        return found.clone();
    }

    // Nothing to shadow: create the modern location when the user's config tree says XDG, else
    // fall back to the historic dotfile.
    match xdg_conf {
        Some(p) => p,
        None if home.join(".config").is_dir() => home_config,
        None => dot,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A private temp dir for one test (no env mutation: the suite runs in parallel).
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tma_paths_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// `VAR=` means "I did not set this", not "resolve to the current directory". A shell that
    /// exports an unset variable (or a `env VAR= tma ...`) would otherwise make every `install-hooks`
    /// write land relative to wherever tma happened to be run from.
    #[test]
    fn an_empty_env_value_falls_through_to_the_default() {
        // A key unique to this test, so setting it cannot perturb the parallel suite.
        let key = format!("TMA_TEST_EMPTY_{}", std::process::id());
        let default = scratch("empty_env").join("fallback.json");
        let d = || default.clone();

        assert_eq!(resolve(None, &key, d), default, "unset takes the default");

        std::env::set_var(&key, "");
        assert_eq!(
            resolve(None, &key, d),
            default,
            "an empty value is unset, not a relative path"
        );

        std::env::set_var(&key, "/etc/somewhere.json");
        assert_eq!(resolve(None, &key, d), PathBuf::from("/etc/somewhere.json"));

        // The override still wins over a set value.
        let over = PathBuf::from("/tmp/override.json");
        assert_eq!(resolve(Some(&over), &key, d), over);
        std::env::remove_var(&key);
    }

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "set -g mouse on\n").unwrap();
    }

    /// The first existing candidate wins, in tmux's own order. The `~/.tmux.conf` arm doubles as
    /// the upgrade path: someone whose line landed there under the old unconditional default keeps
    /// being resolved to it, so `--check` and `--uninstall` still find the marked line.
    #[test]
    fn tmux_conf_picks_the_first_existing_candidate() {
        let dir = scratch("first");
        let home = dir.join("home");
        let xdg = dir.join("xdg");
        touch(&home.join(".config/tmux/tmux.conf"));
        assert_eq!(
            tmux_conf_in(&home, Some(&xdg)),
            home.join(".config/tmux/tmux.conf"),
            "only ~/.config exists"
        );

        touch(&xdg.join("tmux/tmux.conf"));
        assert_eq!(
            tmux_conf_in(&home, Some(&xdg)),
            xdg.join("tmux/tmux.conf"),
            "XDG outranks ~/.config"
        );
        assert_eq!(
            tmux_conf_in(&home, None),
            home.join(".config/tmux/tmux.conf"),
            "an unset XDG_CONFIG_HOME drops that candidate"
        );

        touch(&home.join(".tmux.conf"));
        assert_eq!(
            tmux_conf_in(&home, Some(&xdg)),
            home.join(".tmux.conf"),
            "the dotfile outranks both"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// With no config anywhere, tma creates the modern location and never a shadowing file: the
    /// returned path is always one that does not exist, and no other candidate exists either.
    #[test]
    fn tmux_conf_creates_the_modern_location_only_when_nothing_exists() {
        let dir = scratch("create");
        let home = dir.join("home");
        let xdg = dir.join("xdg");
        std::fs::create_dir_all(&home).unwrap();

        let with_xdg = tmux_conf_in(&home, Some(&xdg));
        assert_eq!(with_xdg, xdg.join("tmux/tmux.conf"));
        assert!(
            !with_xdg.exists(),
            "nothing existed, so nothing is shadowed"
        );

        assert_eq!(
            tmux_conf_in(&home, None),
            home.join(".tmux.conf"),
            "no XDG_CONFIG_HOME and no ~/.config: the historic dotfile"
        );

        std::fs::create_dir_all(home.join(".config")).unwrap();
        assert_eq!(
            tmux_conf_in(&home, None),
            home.join(".config/tmux/tmux.conf"),
            "an existing ~/.config means the user is on the XDG layout"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A directory (or a dangling symlink) at a candidate path is not a config: it must not be
    /// picked, and it must not stop a real config later in the order from being found.
    #[test]
    fn tmux_conf_ignores_a_non_file_candidate() {
        let dir = scratch("nonfile");
        let home = dir.join("home");
        std::fs::create_dir_all(home.join(".tmux.conf")).unwrap();
        touch(&home.join(".config/tmux/tmux.conf"));
        assert_eq!(
            tmux_conf_in(&home, None),
            home.join(".config/tmux/tmux.conf")
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `--conf` keeps absolute precedence: no probing, no fallback.
    #[test]
    fn explicit_conf_override_wins() {
        let explicit = Path::new("/nowhere/custom.conf");
        assert_eq!(resolve_tmux_conf(Some(explicit)), explicit);
    }
}
