//! `tma install-hooks`: wire the `tma-hook` wrapper into an agent's config and install the tmux
//! attention-clear server hooks. Two symmetric, idempotent, diff-before-write, byte-identical
//! write sites: the agent config (an "honest split", one [`adapters::AgentAdapter`] arm per agent, since each
//! config format differs) and the tmux server hooks (`after-select-*` running `tma clear-attention
//! #{hook_pane}`, their assigned indexes recorded in a PER-SERVER `hooks-state-<server>.toml`,
//! keyed because tmux `set-hook -g` indexes are per-server: a shared file would let one server's
//! uninstall strip another's). `--check` verifies both and detects a config-reload hook wipe. The
//! wired event set is [`crate::manifests::hook_events`], the docs-drift source of truth.

use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde::{Deserialize, Serialize};

use crate::cli_support;
use crate::manifests::LoadedManifest;
use crate::tmux::Tmux;
use json_value::Value;

mod adapters;
mod claude_json;
mod codex_toml;
mod diff;
mod js_bridge;
mod json_value;
mod paths;
mod statusline;

pub(crate) use paths::{resolve_config_dir, resolve_tmux_conf};

use adapters::{adapter_for, resolve_adapter};
use paths::{tma_bin, ConfigPaths, PathOverrides, Wrapper, WRAPPER_NAME};
pub(crate) use statusline::Statusline;

/// The wrapper script shipped in the crate, embedded so `install-hooks` can write it out
/// alongside the binary at install time.
const WRAPPER_SRC: &str = include_str!("../assets/tma-hook");

/// The Codex agent name (manifest stem) and its `notify` event token: `notify = ["<tma-hook>",
/// "codex", "notify"]`. Shared with the drift test so config and manifest cannot diverge.
const CODEX_AGENT: &str = "codex";
pub(crate) const CODEX_NOTIFY_EVENT: &str = "notify";

/// The always-installed tmux attention-clear hooks. `after-select-*` fire
/// unconditionally, so they are the zero-config posture.
const TMUX_HOOKS: &[&str] = &["after-select-pane", "after-select-window"];

/// The extra hook installed only when `[focus] events = true`. `pane-focus-in` fires only under the
/// user's tmux `focus-events` (default off), so it is off by default and config-gated here.
const FOCUS_HOOK: &str = "pane-focus-in";

/// The superset tma may have installed (base + opt-in focus hook), used by `--uninstall` and
/// removal-by-content so a focus hook is still removed after `focus_events` is turned back off.
const ALL_TMUX_HOOKS: &[&str] = &["after-select-pane", "after-select-window", "pane-focus-in"];

/// The hooks that SHOULD be installed for the `[focus] events` posture (base + `pane-focus-in` when
/// opted in). Drives install and `--check`.
fn desired_hooks(focus_events: bool) -> Vec<&'static str> {
    let mut hooks: Vec<&'static str> = TMUX_HOOKS.to_vec();
    if focus_events {
        hooks.push(FOCUS_HOOK);
    }
    hooks
}

/// Options for `tma install-hooks` (parsed from the CLI in `main`).
pub(crate) struct InstallOpts {
    /// The agent to (un)install; `None` is only valid with `--check` (checks all agents) or
    /// [`Self::all`].
    pub agent: Option<String>,
    /// `--all`: act on every agent that ALREADY carries tma wiring, instead of one named agent.
    pub all: bool,
    pub uninstall: bool,
    pub check: bool,
    /// What to do about the statusline context shim (`--statusline` / `--no-statusline`); see
    /// [`Statusline`]. Only Claude and cursor have one.
    pub statusline: Statusline,
    /// Skip the interactive diff confirmation (tests, scripted installs).
    pub assume_yes: bool,
    pub server: tma_tmux::tmux::Server,
    pub manifest_dir: Option<PathBuf>,
    /// Override the agent settings path (default `~/.claude/settings.json`; env
    /// `TMA_CLAUDE_SETTINGS`). Every override keeps tests off the real config.
    pub settings: Option<PathBuf>,
    /// Override Gemini's `settings.json` (default `~/.gemini/settings.json`; env `TMA_GEMINI_SETTINGS`).
    pub gemini_settings: Option<PathBuf>,
    /// Override the tma config dir holding `hooks-state-<server>.toml` (default `~/.config/tma`;
    /// env `TMA_CONFIG_DIR`).
    pub config_dir: Option<PathBuf>,
    /// Override where the wrapper is written (default: sibling `tma-hook` of the binary; env
    /// `TMA_WRAPPER_PATH`).
    pub wrapper_path: Option<PathBuf>,
    /// `[install] wrapper_ref` (or `--wrapper-ref`): how the agent configs NAME the wrapper.
    pub wrapper_ref: crate::config::WrapperRef,
    /// Override the OpenCode plugin path (default `~/.config/opencode/plugin/tma.js`; env
    /// `TMA_OPENCODE_PLUGIN`).
    pub opencode_plugin: Option<PathBuf>,
    /// Override Codex's `config.toml` (default `$CODEX_HOME/config.toml`, else `~/.codex/config.toml`;
    /// env `TMA_CODEX_CONFIG`). REQUIRED for test safety: the real one is often a dotfiles symlink.
    pub codex_config: Option<PathBuf>,
    /// Override Codex's `hooks.json` (default `$CODEX_HOME/hooks.json`, else `~/.codex/hooks.json`;
    /// env `TMA_CODEX_HOOKS`).
    pub codex_hooks: Option<PathBuf>,
    /// Override Cursor's `hooks.json` (default `~/.cursor/hooks.json`; env `TMA_CURSOR_HOOKS`).
    pub cursor_hooks: Option<PathBuf>,
    /// Override Cursor's `cli-config.json` holding the statusLine context shim (default
    /// `~/.cursor/cli-config.json`; env `TMA_CURSOR_CLI_CONFIG`).
    pub cursor_cli_config: Option<PathBuf>,
    /// Override pi's extension file (default `~/.pi/agent/extensions/tma.js`, or
    /// `$PI_CODING_AGENT_DIR/extensions/tma.js`; env `TMA_PI_EXTENSION`).
    pub pi_extension: Option<PathBuf>,
    /// `[focus] events`: also install the `pane-focus-in` clear hook (default off; it fires only
    /// under tmux `focus-events on`).
    pub focus_events: bool,
    /// `[[agent]]` config: enable/disable + custom process-name maps.
    pub agents: Vec<crate::config::AgentConfig>,
}

/// Recorded tmux-hook install metadata, persisted to `hooks-state.toml`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct HooksState {
    #[serde(default)]
    tmux_hooks: Vec<TmuxHookRecord>,
}

#[derive(Debug, Serialize, Deserialize)]
struct TmuxHookRecord {
    hook: String,
    index: usize,
}

pub(crate) fn run(opts: InstallOpts) -> ExitCode {
    let manifests =
        match cli_support::load_manifests_or_exit(opts.manifest_dir.as_deref(), &opts.agents) {
            Ok(m) => m,
            Err(code) => return code,
        };
    let paths = ConfigPaths::resolve(PathOverrides {
        settings: opts.settings.as_deref(),
        gemini_settings: opts.gemini_settings.as_deref(),
        opencode_plugin: opts.opencode_plugin.as_deref(),
        codex_config: opts.codex_config.as_deref(),
        codex_hooks: opts.codex_hooks.as_deref(),
        cursor_hooks: opts.cursor_hooks.as_deref(),
        cursor_cli_config: opts.cursor_cli_config.as_deref(),
        pi_extension: opts.pi_extension.as_deref(),
    });
    let config_dir = resolve_config_dir(opts.config_dir.as_deref());
    let wrapper = Wrapper::resolve(opts.wrapper_path.as_deref(), opts.wrapper_ref);
    let tmux = Tmux::connect(&opts.server);

    // `--all` names its own set, so naming an agent too says two different things at once.
    if opts.all && opts.agent.is_some() {
        eprintln!("tma: --all covers every wired agent; drop the agent name, or drop --all");
        return ExitCode::FAILURE;
    }

    if opts.check {
        // A named agent that run_check would silently skip deserves the same signal the
        // install path gives loudly: not tma-installable, wire manually.
        if let Some(agent) = opts.agent.as_deref() {
            match manifests.iter().find(|m| m.name == agent) {
                None => {
                    eprintln!("tma: no manifest for agent {agent:?}");
                    return ExitCode::FAILURE;
                }
                Some(lm) if lm.manifest.hooks.is_none() || adapter_for(&lm.name).is_none() => {
                    eprintln!(
                        "tma: {agent} has no hook installer (hookless or no adapter); \
                         nothing to check — wire manually via `tma-hook {agent} <event>`"
                    );
                }
                Some(_) => {}
            }
        }
        return run_check(
            &manifests,
            &paths,
            &config_dir,
            &wrapper,
            &tmux,
            opts.focus_events,
            opts.agent.as_deref(),
            opts.statusline,
        );
    }

    if opts.all {
        return run_all(&manifests, &paths, &config_dir, &wrapper, &tmux, &opts);
    }

    let Some(agent) = opts.agent.as_deref() else {
        eprintln!(
            "tma: install-hooks needs an agent (e.g. `tma install-hooks claude`), or --all for \
             every agent already wired"
        );
        return ExitCode::FAILURE;
    };
    let Some(lm) = manifests.iter().find(|m| m.name == agent) else {
        eprintln!("tma: no manifest for agent {agent:?}");
        return ExitCode::FAILURE;
    };

    if opts.uninstall {
        uninstall(
            lm,
            &manifests,
            &paths,
            &config_dir,
            &wrapper,
            &tmux,
            opts.assume_yes,
        )
    } else {
        install(
            lm,
            &paths,
            &config_dir,
            &wrapper,
            &tmux,
            opts.assume_yes,
            opts.focus_events,
            opts.statusline,
        )
    }
}

/// `--all`: (re)install or uninstall every agent that ALREADY carries tma wiring — the set bare
/// `--check` reports on, NOT every agent tma ships an adapter for.
///
/// The distinction is the whole design: installing every installable agent would create a
/// `~/.gemini/settings.json` for someone who has never run Gemini. This only rewrites config files
/// that already hold tma's wiring, which is what a changed `[install] wrapper_ref` or a moved binary
/// needs — including the agents `tma init` skips, since it wires only what it finds on `$PATH`.
fn run_all(
    manifests: &[LoadedManifest],
    paths: &ConfigPaths,
    config_dir: &Path,
    wrapper: &Wrapper,
    tmux: &Tmux,
    opts: &InstallOpts,
) -> ExitCode {
    // Statusline::Keep: this asks "is there wiring here", a question no statusline flag changes.
    // Whether each agent's shim is installed stays that agent's recorded choice, applied below.
    let wired: Vec<&LoadedManifest> = manifests
        .iter()
        .filter(|lm| {
            matches!(
                classify_agent(lm, paths, wrapper.reference(), Statusline::Keep),
                HookWiring::Wired | HookWiring::Incomplete(_)
            )
        })
        .collect();

    let verb = if opts.uninstall {
        "uninstalling"
    } else {
        "wiring"
    };
    if wired.is_empty() {
        println!(
            "tma: no agent is wired, so there is nothing to change; name one \
             (`tma install-hooks claude`) or run `tma init`"
        );
        return ExitCode::SUCCESS;
    }

    // One agent's failure must not strand the rest half-repointed, so every agent is attempted and
    // the failures are named at the end.
    let mut failed: Vec<&str> = Vec::new();
    for lm in wired {
        println!("\ntma: {verb} {} ...", lm.name);
        let code = if opts.uninstall {
            uninstall(
                lm,
                manifests,
                paths,
                config_dir,
                wrapper,
                tmux,
                opts.assume_yes,
            )
        } else {
            install(
                lm,
                paths,
                config_dir,
                wrapper,
                tmux,
                opts.assume_yes,
                opts.focus_events,
                opts.statusline,
            )
        };
        if !matches!(code, ExitCode::SUCCESS) {
            failed.push(&lm.name);
        }
    }

    if failed.is_empty() {
        return ExitCode::SUCCESS;
    }
    eprintln!("tma: {} did not complete: {}", verb, failed.join(", "));
    ExitCode::FAILURE
}

// --- install / uninstall ---------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn install(
    lm: &LoadedManifest,
    paths: &ConfigPaths,
    config_dir: &Path,
    wrapper: &Wrapper,
    tmux: &Tmux,
    assume_yes: bool,
    focus_events: bool,
    statusline: Statusline,
) -> ExitCode {
    // Refuse an un-installable agent before writing anything: a hookless or adapter-less
    // agent must never fall back to Claude's JSON and contaminate its config.
    let adapter = match resolve_adapter(lm) {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("tma: {msg}");
            return ExitCode::FAILURE;
        }
    };

    // 1. Write the wrapper alongside the binary so the config entries resolve.
    if let Err(err) = write_wrapper(wrapper.write_path()) {
        eprintln!(
            "tma: cannot write wrapper {}: {err}",
            wrapper.write_path().display()
        );
        if matches!(
            err.kind(),
            io::ErrorKind::PermissionDenied | io::ErrorKind::ReadOnlyFilesystem
        ) {
            eprintln!(
                "      That directory is read-only, so the wrapper has to live elsewhere: name a \
                 writable one with `--wrapper-path <PATH>` (env TMA_WRAPPER_PATH), e.g. \
                 ~/.local/bin/tma-hook."
            );
        }
        return ExitCode::FAILURE;
    }

    // 2. A bare reference is only as good as the `$PATH` the agent inherits, and a wrapper the agent
    // cannot find fails SILENTLY (the wrapper's own contract). Verify it now, while there is
    // someone to tell, rather than shipping hooks that never fire.
    if !bare_reference_ok(wrapper) {
        return ExitCode::FAILURE;
    }
    warn_store_reference(wrapper);

    // 3. Agent config: wire the wrapper via the agent's own mechanism (the honest split). With
    // neither statusline flag, a previous opt-in stands, so a plain re-install keeps an asked-for
    // shim current rather than leaving it to rot behind a moved binary.
    let statusline = effective_statusline(statusline, &lm.name, config_dir);
    if !adapter.install(lm, paths, wrapper.reference(), assume_yes, statusline) {
        return ExitCode::FAILURE;
    }
    // Record the choice only once the write succeeded, and only when the flags stated one: `Keep`
    // is "no opinion", which must not overwrite the opinion already on file.
    match statusline {
        Statusline::Install => record_statusline(config_dir, &lm.name, true),
        Statusline::Remove => record_statusline(config_dir, &lm.name, false),
        Statusline::Keep => {}
    }

    // 4. tmux server hooks + record the assigned indexes. The hook set depends on the
    // `[focus] events` posture (base always; `pane-focus-in` only when opted in).
    match install_tmux_hooks(tmux, &desired_hooks(focus_events)) {
        Ok(records) => {
            if let Err(err) = write_hooks_state(
                config_dir,
                tmux,
                &HooksState {
                    tmux_hooks: records,
                },
            ) {
                eprintln!("tma: cannot record hooks state: {err}");
                return ExitCode::FAILURE;
            }
        }
        Err(err) => {
            eprintln!("tma: cannot install tmux hooks: {err}");
            return ExitCode::FAILURE;
        }
    }

    println!("tma: installed hooks for {}", lm.name);
    ExitCode::SUCCESS
}

#[allow(clippy::too_many_arguments)]
fn uninstall(
    lm: &LoadedManifest,
    manifests: &[LoadedManifest],
    paths: &ConfigPaths,
    config_dir: &Path,
    wrapper: &Wrapper,
    tmux: &Tmux,
    assume_yes: bool,
) -> ExitCode {
    // Refuse an un-installable agent up front (symmetric to install): with no adapter or no
    // [hooks] block there is no wiring tma could have written, so touch no config file.
    let adapter = match resolve_adapter(lm) {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("tma: {msg}");
            return ExitCode::FAILURE;
        }
    };

    // Agent config: undo the wrapper wiring via the agent's own mechanism (symmetric to
    // install). A no-op change (already absent) is fine — do not fail.
    if !adapter.uninstall(lm, paths, assume_yes) {
        return ExitCode::FAILURE;
    }

    // Remove exactly the recorded tmux-hook indexes (content-match fallback when no state file).
    let read = read_hooks_state(config_dir, tmux);
    let state = read.as_ref().map(|(s, _)| s);
    let from_legacy = read.as_ref().is_some_and(|(_, l)| *l);
    if let Err(err) = uninstall_tmux_hooks(tmux, state) {
        eprintln!("tma: cannot remove tmux hooks: {err}");
        return ExitCode::FAILURE;
    }
    let keyed = hooks_state_path(config_dir, tmux);
    let _ = std::fs::remove_file(&keyed);
    // Clear the legacy unkeyed record only when this uninstall consumed it: another pre-keying
    // server may still own that file, and deleting it would orphan its recorded indexes.
    if from_legacy {
        let legacy = config_dir.join(LEGACY_HOOKS_STATE);
        if keyed != legacy {
            let _ = std::fs::remove_file(legacy);
        }
    }

    println!("tma: uninstalled hooks for {}", lm.name);
    // With the last agent's wiring gone nothing refreshes a stamp again, so leaving them would
    // freeze every `#{@agent_state}` in a user's format at whatever it last read.
    if !any_agent_still_wired(manifests, paths, wrapper.reference(), &lm.name) {
        sweep_pane_stamps(tmux);
    }
    ExitCode::SUCCESS
}

/// Whether any agent OTHER than `except` still carries tma wiring, read with the same predicate
/// `--check` uses. Partial wiring counts: it still fires hooks, so it still stamps panes.
fn any_agent_still_wired(
    manifests: &[LoadedManifest],
    paths: &ConfigPaths,
    wrapper: &Path,
    except: &str,
) -> bool {
    manifests.iter().filter(|lm| lm.name != except).any(|lm| {
        matches!(
            classify_agent(lm, paths, wrapper, Statusline::Keep),
            HookWiring::Wired | HookWiring::Incomplete(_)
        )
    })
}

/// Clear tma's pane options from every pane, then name the one thing tma must not clean up itself:
/// a status-line entry the user added by hand to their own tmux config.
fn sweep_pane_stamps(tmux: &Tmux) {
    match tmux.clear_all_pane_stamps() {
        Ok(0) => {}
        Ok(panes) => println!("tma: cleared tma's options from {panes} pane(s)"),
        // No server, no panes: the options died with it, so there is nothing to report.
        Err(crate::tmux::TmuxError::ServerGone) => {}
        Err(err) => {
            eprintln!("tma: cannot clear the pane options ({err}); they will read stale state");
            return;
        }
    }
    println!(
        "tma: if you added a tma segment to your tmux config, remove it by hand \
         (tma never wrote it), e.g.:\n      set -g status-right '#(tma status)'"
    );
}

// --- tmux server hooks -----------------------------------------------------------

/// The tmux-hook command clearing attention on focus change. `#{hook_pane}` (never `$TMUX_PANE`)
/// binds the pane at hook time. The binary is LATE-BOUND like the `tma-hook` wrapper: the install-time
/// absolute path when it is still executable, else plain `tma` off `$PATH`, so a rebuilt, moved, or
/// re-installed binary keeps the hook working instead of leaving a dead command behind. The
/// middle-tier nudge lives inside the same `clear-attention` subcommand.
fn clear_attention_command(bin: &Path) -> String {
    // Single quotes only: the whole string is a tmux double-quoted argument, where tmux expands
    // `#{...}` (wanted) and `$name` (not wanted), so the shell side stays `$`-free. `#{hook_pane}` is
    // quoted so an empty expansion still passes an argument (`clear-attention ''` no-ops). The PATH
    // fallback swallows its own failure: with no `tma` anywhere, sh exits 127 and tmux would flash
    // "returned 127" on every pane switch, so that branch stays silent like the tma-hook wrapper.
    format!(
        "run-shell \"if [ -x '{0}' ]; then '{0}' clear-attention '#{{hook_pane}}'; \
         else tma clear-attention '#{{hook_pane}}' 2>/dev/null || true; fi\"",
        bin.display()
    )
}

/// Whether an installed hook command is what install would write now. Compared modulo whitespace and
/// quoting: tmux re-serializes the stored command when printing it back, so quote style is its
/// choice, while a changed binary path or command shape (the drift this detects) survives normalization.
fn hook_command_current(installed: &str, expected: &str) -> bool {
    fn normalize(s: &str) -> String {
        s.chars()
            .filter(|c| !c.is_whitespace() && !matches!(c, '"' | '\'' | '\\'))
            .collect()
    }
    normalize(installed) == normalize(expected)
}

/// Install the attention-clear hooks idempotently and return their recorded `(hook, index)` entries.
/// An existing entry of ours is reused when it matches what we would write now and rewritten in
/// place (keeping its index) when it has drifted: a stale binary path or an older command shape.
fn install_tmux_hooks(tmux: &Tmux, hooks: &[&str]) -> Result<Vec<TmuxHookRecord>, String> {
    let command = clear_attention_command(&tma_bin());
    let mut records = Vec::new();
    for &hook in hooks {
        let existing = tmux.show_global_hook(hook).map_err(|e| e.to_string())?;
        match existing.iter().find(|(_, c)| is_ours(c)) {
            None => tmux
                .append_global_hook(hook, &command)
                .map_err(|e| e.to_string())?,
            Some((idx, c)) if !hook_command_current(c, &command) => tmux
                .set_global_hook_index(hook, *idx, &command)
                .map_err(|e| e.to_string())?,
            Some(_) => {}
        }
        // Read back the assigned index (record the actual, tmux-chosen index).
        let after = tmux.show_global_hook(hook).map_err(|e| e.to_string())?;
        if let Some((idx, _)) = after.iter().find(|(_, c)| is_ours(c)) {
            records.push(TmuxHookRecord {
                hook: hook.to_string(),
                index: *idx,
            });
        }
    }
    Ok(records)
}

/// Remove our clear-attention entries from each hook. Primary: remove the `hooks-state.toml`
/// indexes, but only where the entry still living at that index is ours (a config reload can shuffle
/// the array). Fallback with no recorded state: match by the clear-attention substring.
fn uninstall_tmux_hooks(tmux: &Tmux, state: Option<&HooksState>) -> Result<(), String> {
    let recorded = state.map(|s| s.tmux_hooks.as_slice()).unwrap_or(&[]);
    if recorded.is_empty() {
        return uninstall_tmux_hooks_by_content(tmux);
    }
    // Iterate the superset so a `pane-focus-in` hook installed under `focus_events = true` is
    // still removed after the config is turned back off.
    for &hook in ALL_TMUX_HOOKS {
        let entries = tmux.show_global_hook(hook).map_err(|e| e.to_string())?;
        // The recorded indexes for this hook whose current occupant is still ours.
        let mut indexes: Vec<usize> = recorded
            .iter()
            .filter(|r| r.hook == hook)
            .map(|r| r.index)
            .filter(|idx| entries.iter().any(|(i, c)| i == idx && is_ours(c)))
            .collect();
        indexes.sort_unstable();
        indexes.dedup();
        // Remove high-to-low; tmux keeps the other indexes stable regardless, but this is
        // robust if that ever changes.
        for idx in indexes.into_iter().rev() {
            tmux.remove_global_hook_index(hook, idx)
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// Fallback removal by content when no recorded state exists (index-stable).
fn uninstall_tmux_hooks_by_content(tmux: &Tmux) -> Result<(), String> {
    for &hook in ALL_TMUX_HOOKS {
        let entries = tmux.show_global_hook(hook).map_err(|e| e.to_string())?;
        let mut ours: Vec<usize> = entries
            .iter()
            .filter(|(_, c)| is_ours(c))
            .map(|(i, _)| *i)
            .collect();
        ours.sort_unstable();
        for idx in ours.into_iter().rev() {
            tmux.remove_global_hook_index(hook, idx)
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// A hook command is ours iff it invokes `clear-attention` — the OWNERSHIP test (what uninstall may
/// remove), deliberately path-blind. Whether an owned entry is still CURRENT is
/// [`hook_command_current`]'s job; the two questions have different answers after a binary moves.
fn is_ours(command: &str) -> bool {
    command.contains("clear-attention")
}

/// One tmux server hook's state, as `--check` and `doctor` report it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TmuxHookState {
    /// Our entry is there and matches the command install would write now.
    Present,
    /// Our entry is there but differs from that command: a moved binary or an older shape.
    Drifted,
    /// This server has no tma entry at all, yet the install record says it should — tmux hooks are
    /// runtime server state, so a `kill-server`/reboot wiped them.
    Wiped,
    /// No entry of ours and nothing recorded: never installed against this server.
    Missing,
}

impl TmuxHookState {
    /// Stable token for `doctor --json`.
    pub(crate) fn token(self) -> &'static str {
        match self {
            TmuxHookState::Present => "present",
            TmuxHookState::Drifted => "drifted",
            TmuxHookState::Wiped => "wiped",
            TmuxHookState::Missing => "missing",
        }
    }

    pub(crate) fn is_present(self) -> bool {
        matches!(self, TmuxHookState::Present)
    }

    /// The drift line `--check` prints and `doctor` shows, `None` when the hook is fine.
    pub(crate) fn reason(self, hook: &str) -> Option<String> {
        match self {
            TmuxHookState::Present => None,
            TmuxHookState::Drifted => Some(format!(
                "tmux hook {hook} is stale (it runs a different command than this build installs, \
                 e.g. a tma binary that has moved); run `tma install-hooks <agent>` to repoint it"
            )),
            TmuxHookState::Wiped => Some(format!(
                "tmux hook {hook} installed but not present on this server, likely restarted \
                 (tmux hooks are runtime state); run `tma install-hooks <agent>`, or add the \
                 `set-hook -ga` lines to whichever tmux config you use (~/.tmux.conf or \
                 ~/.config/tmux/tmux.conf) to make them durable"
            )),
            TmuxHookState::Missing => Some(format!("tmux hook {hook} missing (config reload?)")),
        }
    }
}

// --- read-only hook diagnosis (shared by --check and `tma doctor`) ---------------

/// One agent's hook-wiring category, the structured form of what `install-hooks --check` inspects
/// (read-only). Shared by `doctor` and `run_check` so the two commands can never disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HookWiring {
    /// Hook-capable, an adapter exists, and every declared event is wired to the wrapper.
    Wired,
    /// Hook-capable and partly wired, but with drift (a missing event or a stale wrapper path);
    /// carries the human-readable reasons `--check` prints.
    Incomplete(Vec<String>),
    /// Hook-capable with an adapter, but no wiring present (never installed for this agent).
    NotInstalled,
    /// The manifest declares no `[hooks]` block — detected by screen rules only.
    Hookless,
    /// Hook-capable manifest but tma ships no installer adapter (wire by hand).
    NoAdapter,
}

/// One agent's hook-wiring diagnosis.
#[derive(Debug, Clone)]
pub(crate) struct AgentHooks {
    pub agent: String,
    pub wiring: HookWiring,
}

/// The full read-only hook diagnosis: per-agent wiring, the global wrapper presence, and the tmux
/// server hooks. Same predicates as `install-hooks --check`, never writing.
#[derive(Debug, Clone)]
pub(crate) struct HookDiagnosis {
    /// Where the wrapper script is written on disk.
    pub wrapper_path: PathBuf,
    /// What the agent configs name it by: the bare `tma-hook` under `[install] wrapper_ref =
    /// "bare"`, else an absolute path — [`Self::wrapper_path`], or the stable alias standing in for
    /// it when that is a store path.
    pub wrapper_ref: PathBuf,
    /// Whether [`Self::wrapper_ref`] is the bare name. Not inferable from the two paths differing,
    /// which is also what a store path's stable alias looks like.
    pub wrapper_bare: bool,
    /// Whether that reference resolves — the file exists, or the bare name is on `$PATH`.
    pub wrapper_present: bool,
    pub agents: Vec<AgentHooks>,
    /// `(hook name, state)` for the desired tmux server hooks. Empty if the server
    /// could not be read.
    pub tmux_hooks: Vec<(String, TmuxHookState)>,
    /// Why the server read failed, when it did. An unreadable server is not a clean bill of
    /// health: `--check` reports it instead of passing on the empty list above.
    pub tmux_hooks_error: Option<String>,
}

/// Diagnose hook wiring read-only for `tma doctor`, resolving paths by the same env/default rules as
/// install (all `TMA_*` overrides, so tests stay isolated). Shares [`build_diagnosis`] with `--check`.
pub(crate) fn diagnose_hooks(
    manifests: &[LoadedManifest],
    tmux: &Tmux,
    focus_events: bool,
    statusline: Statusline,
    wrapper_ref: crate::config::WrapperRef,
) -> HookDiagnosis {
    let paths = ConfigPaths::resolve(PathOverrides::default());
    let config_dir = resolve_config_dir(None);
    let wrapper = Wrapper::resolve(None, wrapper_ref);
    build_diagnosis(
        manifests,
        &paths,
        &config_dir,
        &wrapper,
        tmux,
        focus_events,
        statusline,
    )
}

/// The shared read-only core behind both `--check` and [`diagnose_hooks`]. Reads the agent
/// config files, the wrapper path, and the tmux server hooks; writes nothing.
#[allow(clippy::too_many_arguments)]
fn build_diagnosis(
    manifests: &[LoadedManifest],
    paths: &ConfigPaths,
    config_dir: &Path,
    wrapper: &Wrapper,
    tmux: &Tmux,
    focus_events: bool,
    statusline: Statusline,
) -> HookDiagnosis {
    // The config entries invoke the wrapper by the reference below and its death is silent, so
    // whether that reference still resolves is part of the diagnosis (a moved or deleted wrapper —
    // or, for a bare reference, a `$PATH` that no longer holds it — breaks every wired hook).
    let wrapper_present = wrapper.resolves();

    let agents = manifests
        .iter()
        .map(|lm| AgentHooks {
            agent: lm.name.clone(),
            // Per agent, because the opt-in is per agent: a `--check`/doctor with no statusline flag
            // asks the record what this agent wanted, so an opt-in is not re-reported forever.
            wiring: classify_agent(
                lm,
                paths,
                wrapper.reference(),
                effective_statusline(statusline, &lm.name, config_dir),
            ),
        })
        .collect();

    // tmux hooks: a config reload with an unindexed `set-hook -g` wipes the array — detect by our
    // entry's absence at its recorded index. A legacy-fallback record can yield a false "missing"
    // here; install rewrites the keyed file and clears the ambiguity.
    let state = read_hooks_state(config_dir, tmux).map(|(s, _)| s);
    let (tmux_hooks, tmux_hooks_error) =
        match tmux_hook_states(tmux, state.as_ref(), &desired_hooks(focus_events)) {
            Ok(hooks) => (hooks, None),
            Err(err) => (Vec::new(), Some(err)),
        };

    HookDiagnosis {
        wrapper_path: wrapper.write_path().to_path_buf(),
        wrapper_ref: wrapper.reference().to_path_buf(),
        wrapper_bare: wrapper.is_bare(),
        wrapper_present,
        agents,
        tmux_hooks,
        tmux_hooks_error,
    }
}

/// Whether `install-hooks` can wire this agent at all: its manifest declares `[hooks]` AND tma
/// ships an adapter for its config format. The predicate `tma init` offers an agent on, so the
/// wizard can never propose a wiring [`resolve_adapter`] would refuse.
pub(crate) fn is_installable(lm: &LoadedManifest) -> bool {
    lm.manifest.hooks.is_some() && adapter_for(&lm.name).is_some()
}

/// Classify one agent's config wiring (read-only). A hookless or adapter-less manifest is reported
/// as such; otherwise [`adapters::AgentAdapter::classify`] inspects its own config.
fn classify_agent(
    lm: &LoadedManifest,
    paths: &ConfigPaths,
    wrapper: &Path,
    statusline: Statusline,
) -> HookWiring {
    if lm.manifest.hooks.is_none() {
        return HookWiring::Hookless;
    }
    let Some(adapter) = adapter_for(&lm.name) else {
        return HookWiring::NoAdapter;
    };
    adapter.classify(lm, paths, wrapper, statusline)
}

// --- --check ---------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn run_check(
    manifests: &[LoadedManifest],
    paths: &ConfigPaths,
    config_dir: &Path,
    wrapper: &Wrapper,
    tmux: &Tmux,
    focus_events: bool,
    agent_filter: Option<&str>,
    statusline: Statusline,
) -> ExitCode {
    let diag = build_diagnosis(
        manifests,
        paths,
        config_dir,
        wrapper,
        tmux,
        focus_events,
        statusline,
    );
    let mut missing = Vec::new();

    if !diag.wrapper_present {
        missing.push(if wrapper.is_bare() {
            // A bare reference can go missing without the file going anywhere: the configs name it,
            // so a `$PATH` that dropped its directory is the same outage as a deleted wrapper.
            format!(
                "wrapper `{WRAPPER_NAME}` not on $PATH (config entries name it; add {} to $PATH)",
                diag.wrapper_path
                    .parent()
                    .unwrap_or(Path::new("."))
                    .display()
            )
        } else {
            format!(
                "wrapper {} missing (config entries reference it; reinstall to restore)",
                diag.wrapper_path.display()
            )
        });
    }

    // Bare `--check` inspects every bundled agent but only reports `Incomplete` (partial/stale)
    // wiring, so an agent the user never installed is a clean skip. A NAMED agent scopes the report
    // (and the exit code) to itself: a sibling's drift must not fail its check. The shared wrapper +
    // tmux hooks below stay global — prerequisites the named agent depends on too.
    for a in &diag.agents {
        if agent_filter.is_some_and(|want| a.agent != want) {
            continue;
        }
        if let HookWiring::Incomplete(reasons) = &a.wiring {
            missing.extend(reasons.iter().cloned());
        }
    }

    // An unreadable server yields no hook states at all; reporting that is the only honest
    // answer, since "no drift found" would be drawn from evidence `--check` never got.
    if let Some(err) = &diag.tmux_hooks_error {
        missing.push(format!("cannot verify tmux hooks: {err}"));
    }
    for (hook, state) in &diag.tmux_hooks {
        missing.extend(state.reason(hook));
    }

    if missing.is_empty() {
        println!("tma: hooks OK");
        ExitCode::SUCCESS
    } else {
        eprintln!("tma: hook wiring incomplete:");
        for m in &missing {
            eprintln!("  - {m}");
        }
        eprintln!("run `tma install-hooks <agent>` to reinstall");
        ExitCode::FAILURE
    }
}

/// Classify each desired tmux hook. Our entry is looked up at its `hooks-state.toml` index when one
/// is recorded (a config reload can shuffle the array), else by content; a found entry is then
/// compared against the freshly rendered command, so a stale binary path reads as drift rather than
/// as present. A recorded hook with no tma entry anywhere on the server is the restart signature.
fn tmux_hook_states(
    tmux: &Tmux,
    state: Option<&HooksState>,
    hooks: &[&str],
) -> Result<Vec<(String, TmuxHookState)>, String> {
    let expected = clear_attention_command(&tma_bin());
    let recorded = state.map(|s| s.tmux_hooks.as_slice()).unwrap_or(&[]);
    let mut out = Vec::new();
    for &hook in hooks {
        let entries = tmux.show_global_hook(hook).map_err(|e| e.to_string())?;
        let has_record = recorded.iter().any(|r| r.hook == hook);
        let ours = if has_record {
            recorded
                .iter()
                .filter(|r| r.hook == hook)
                .find_map(|r| entries.iter().find(|(i, c)| *i == r.index && is_ours(c)))
        } else {
            entries.iter().find(|(_, c)| is_ours(c))
        };
        let state = match ours {
            Some((_, c)) if hook_command_current(c, &expected) => TmuxHookState::Present,
            Some(_) => TmuxHookState::Drifted,
            None if has_record && !entries.iter().any(|(_, c)| is_ours(c)) => TmuxHookState::Wiped,
            None => TmuxHookState::Missing,
        };
        out.push((hook.to_string(), state));
    }
    Ok(out)
}

/// The legacy, pre-per-server-keying state filename. Kept as the migration source (and the
/// server-gone fallback) so a single-server install written before keying is still honored.
const LEGACY_HOOKS_STATE: &str = "hooks-state.toml";

/// The per-server hooks-state path `hooks-state-<key>.toml`, keyed by a hash of `#{socket_path}`
/// ([`tma_runtime::ipc::socket_key`]) since tmux `set-hook -g` indexes are per-server. Falls back to
/// the legacy unkeyed name when the server is unreachable.
fn hooks_state_path(config_dir: &Path, tmux: &Tmux) -> PathBuf {
    match tma_runtime::ipc::resolve_socket_path(tmux) {
        Some(socket_path) => config_dir.join(format!(
            "hooks-state-{}.toml",
            tma_runtime::ipc::socket_key(&socket_path)
        )),
        None => config_dir.join(LEGACY_HOOKS_STATE),
    }
}

/// Read the target server's tmux-hook metadata, `None` when absent/unparseable. Primary: the keyed
/// file. Migration: when absent, fall back to the legacy unkeyed `hooks-state.toml` (this server's in
/// the common single-server setup). The returned flag is `true` on the legacy source, so uninstall
/// removes it only when consumed; `is_ours` content matching bounds the damage if it was another
/// server's record.
fn read_hooks_state(config_dir: &Path, tmux: &Tmux) -> Option<(HooksState, bool)> {
    let keyed = hooks_state_path(config_dir, tmux);
    let (text, from_legacy) = match std::fs::read_to_string(&keyed) {
        Ok(text) => (text, false),
        // Fall back to the legacy unkeyed record (single-server installs pre-dating keying).
        // When `keyed` already IS the legacy path (server gone), that read already happened.
        Err(_) => {
            let legacy = config_dir.join(LEGACY_HOOKS_STATE);
            if keyed == legacy {
                return None;
            }
            (std::fs::read_to_string(legacy).ok()?, true)
        }
    };
    Some((toml::from_str(&text).ok()?, from_legacy))
}

// --- file plumbing ---------------------------------------------------------------

/// Read a config file tma edits: an ABSENT file reads as `absent` (the shape the installer builds
/// on), every other error is reported. Only `NotFound` may yield the empty document: a present but
/// unreadable file (permissions, non-UTF-8, a dangling symlink) would otherwise be diffed against
/// fresh wiring and silently overwritten under `--yes`.
fn read_existing(path: &Path, absent: &str) -> Result<String, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(absent.to_string()),
        Err(err) => Err(format!("cannot read {}: {err}", path.display())),
    }
}

/// [`read_existing`] for a JSON settings file (absent ⇒ `{}`).
fn read_or_empty_object(path: &Path) -> Result<String, String> {
    read_existing(path, "{}\n")
}

/// Print a read failure and turn it into `None`, so an install/uninstall aborts on the spot rather
/// than rewriting a file it could not read.
fn reported(read: Result<String, String>) -> Option<String> {
    read.map_err(|err| eprintln!("tma: {err}")).ok()
}

/// One `classify` read: the parsed JSON root, or the drift line `--check` prints. An unreadable
/// config is a diagnosis tma cannot make, not an agent that was never installed.
fn classify_root(agent: &str, path: &Path) -> Result<Value, HookWiring> {
    match read_or_empty_object(path) {
        Ok(text) => Ok(json_value::parse(&text).unwrap_or(Value::Obj(Vec::new()))),
        Err(err) => Err(HookWiring::Incomplete(vec![format!(
            "agent {agent}: {err}"
        )])),
    }
}

/// Show a diff, confirm (unless `assume_yes`), and write. Returns `true` on success (written
/// or no change needed), `false` on a declined/failed write.
pub(crate) fn apply_file(path: &Path, old: &str, new: &str, assume_yes: bool, label: &str) -> bool {
    if old == new {
        println!("tma: {label} already up to date ({})", path.display());
        return true;
    }
    println!("tma: proposed change to {} ({label}):", path.display());
    print_diff(old, new);
    if !assume_yes && !confirm() {
        println!("tma: aborted; no changes written");
        return false;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::write(path, new) {
        Ok(()) => true,
        Err(err) => {
            eprintln!("tma: cannot write {}: {err}", path.display());
            false
        }
    }
}

/// The uninstall-only write rule (gemini/cursor/codex): write `new` iff it differs from `old`, else
/// print `<noun> already absent`, so an absent config is never created to normalize it. `label`
/// names the diff line; `noun` names the already-absent note (they differ only for gemini).
fn apply_if_changed(path: &Path, old: &str, new: &str, assume_yes: bool, label: &str, noun: &str) {
    if new != old {
        let _ = apply_file(path, old, new, assume_yes, label);
    } else {
        println!("tma: {noun} already absent ({})", path.display());
    }
}

/// Print a unified diff of `old` vs `new` (3 context lines per hunk) so a config change can be
/// reviewed before applying. Colorizes only on a real terminal with `NO_COLOR` unset, so piped
/// output stays ANSI-free.
pub(crate) fn print_diff(old: &str, new: &str) {
    let color = io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    print!("{}", diff::render_diff(old, new, color));
}

pub(crate) fn confirm() -> bool {
    print!("Apply this change? [y/N] ");
    let _ = io::stdout().flush();
    let mut line = String::new();
    if io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn write_wrapper(wrapper: &Path) -> io::Result<()> {
    // A wrapper already in place and byte-current needs no write at all, which is what lets
    // `install-hooks` run when the binary's own directory is read-only and the package installed
    // the wrapper beside it (the Nix store, a system bindir).
    if wrapper_is_current(wrapper) {
        return Ok(());
    }
    if let Some(parent) = wrapper.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(wrapper, WRAPPER_SRC)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(wrapper)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(wrapper, perms)?;
    }
    Ok(())
}

/// Whether the file at `wrapper` is already the wrapper this build would write, executable and
/// byte-identical. False for anything else, including a stale copy from an older tma, so an
/// upgrade still rewrites (and still fails loudly if it cannot).
fn wrapper_is_current(wrapper: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if !std::fs::metadata(wrapper).is_ok_and(|m| m.permissions().mode() & 0o111 != 0) {
            return false;
        }
    }
    std::fs::read(wrapper).is_ok_and(|on_disk| on_disk == WRAPPER_SRC.as_bytes())
}

/// Vet a bare wrapper reference before it goes into a config, returning false to abort the install.
///
/// An absolute reference needs no vetting: install just wrote that exact file. A bare one is
/// resolved by the agent off whatever `$PATH` it inherited, and the wrapper's exit-0-when-missing
/// contract means a miss produces no error anywhere — hooks simply never fire. So the miss is
/// caught here, at the one moment there is a person to tell.
fn bare_reference_ok(wrapper: &Wrapper) -> bool {
    if !wrapper.is_bare() {
        return true;
    }
    let dir = wrapper.write_path().parent().unwrap_or(Path::new("."));
    if !wrapper.resolves() {
        eprintln!(
            "tma: `{WRAPPER_NAME}` is not on $PATH, and [install] wrapper_ref = \"bare\" writes \
             that bare name into every agent config.\n      A wrapper an agent cannot find fails \
             silently, so nothing was wired. Put {} on your $PATH, or drop wrapper_ref to wire the \
             absolute path.",
            dir.display()
        );
        return false;
    }
    // Resolvable, but by someone else's copy: still functional (any tma-hook wrapper runs), yet the
    // binary it finds may not be the one just installed, so say which copy actually answers.
    if let Some(other) = wrapper.shadowed_by() {
        eprintln!(
            "tma: warning: $PATH answers `{WRAPPER_NAME}` with {}, not the copy written to {}",
            other.display(),
            dir.display()
        );
    }
    true
}

/// Warn when the reference about to be written is a store path with no stable alias — an install
/// run straight from the store (`nix run`, a `nix build` result) rather than through a profile.
/// The wiring works now and stops working when that path is collected, which is worth saying at the
/// moment it is written rather than leaving to be discovered as broken hooks after an upgrade.
fn warn_store_reference(wrapper: &Wrapper) {
    if !wrapper.references_store_path() {
        return;
    }
    eprintln!(
        "tma: warning: the agent configs now name {}, which is a store path: it belongs to this \
         exact build and is deleted when that build is collected.\n      Install tma into a profile \
         so a stable path points at it, or pass `--wrapper-path <PATH>` to keep the wrapper \
         somewhere of your own.",
        wrapper.reference().display()
    );
}

/// The agents that asked for the statusline shim, persisted to `statusline-state.toml`.
///
/// Not keyed per server, unlike [`HooksState`]: the shim lives in the agent's own config file
/// (`~/.claude/settings.json`), which every tmux server on the machine shares.
#[derive(Debug, Default, Serialize, Deserialize)]
struct StatuslineState {
    #[serde(default)]
    agents: Vec<String>,
}

fn statusline_state_path(config_dir: &Path) -> PathBuf {
    config_dir.join("statusline-state.toml")
}

/// Whether `agent` has asked for the statusline shim before. An absent or unparseable file reads as
/// "never asked", which is the safe direction: the shim is reported rather than silently kept.
fn statusline_opted_in(config_dir: &Path, agent: &str) -> bool {
    std::fs::read_to_string(statusline_state_path(config_dir))
        .ok()
        .and_then(|text| toml::from_str::<StatuslineState>(&text).ok())
        .is_some_and(|state| state.agents.iter().any(|a| a == agent))
}

/// Record (or clear) `agent`'s statusline opt-in. Best-effort: a write failure costs a re-run of the
/// flag, never the install itself, so it warns rather than aborting.
fn record_statusline(config_dir: &Path, agent: &str, opted_in: bool) {
    let mut state = std::fs::read_to_string(statusline_state_path(config_dir))
        .ok()
        .and_then(|text| toml::from_str::<StatuslineState>(&text).ok())
        .unwrap_or_default();
    let present = state.agents.iter().any(|a| a == agent);
    if opted_in == present {
        return;
    }
    if opted_in {
        state.agents.push(agent.to_string());
        state.agents.sort();
    } else {
        state.agents.retain(|a| a != agent);
    }
    let header = "# tma statusline-shim opt-in record: the agents that asked for the context shim\n\
                  # (`install-hooks <agent> --statusline`), so a later plain install keeps it current\n\
                  # instead of reporting it. `--no-statusline` clears an entry.\n";
    let written = std::fs::create_dir_all(config_dir).and_then(|()| {
        let toml = toml::to_string(&state)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        std::fs::write(statusline_state_path(config_dir), format!("{header}{toml}"))
    });
    if let Err(err) = written {
        eprintln!(
            "tma: could not record the statusline choice in {}: {err}",
            statusline_state_path(config_dir).display()
        );
    }
}

/// The statusline intent this run acts on: what the flags said, or — with neither flag — what the
/// agent asked for last time. This is what keeps an opt-in from being re-litigated on every later
/// `install-hooks`, `--check` and `doctor`, while a shim nobody ever asked for still gets reported.
fn effective_statusline(requested: Statusline, agent: &str, config_dir: &Path) -> Statusline {
    match requested {
        Statusline::Keep if statusline_opted_in(config_dir, agent) => Statusline::Install,
        other => other,
    }
}

fn write_hooks_state(config_dir: &Path, tmux: &Tmux, state: &HooksState) -> io::Result<()> {
    std::fs::create_dir_all(config_dir)?;
    let toml = toml::to_string(state)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    let header = "# tma tmux-hook install record, keyed per server (tmux -g hook indexes\n\
                  # are per-server). Install metadata — exempt from the no-files rule. Do not\n\
                  # hand-edit; `tma install-hooks --uninstall` clears it.\n";
    std::fs::write(
        hooks_state_path(config_dir, tmux),
        format!("{header}{toml}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- the statusline opt-in record -------------------------------------------

    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tma_sl_state_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The record is what stops an opt-in being re-litigated: with neither flag, an agent that asked
    /// for the shim resolves to `Install` (keep it current, and require it in `--check`), while an
    /// agent that never asked stays `Keep` — the state where a shim nobody requested is reported.
    /// An explicit flag always wins over the record, since it is the user restating the choice.
    #[test]
    fn a_recorded_opt_in_survives_a_later_flagless_run() {
        let dir = scratch_dir("optin");

        assert!(!statusline_opted_in(&dir, "claude"), "nothing on file yet");
        assert_eq!(
            effective_statusline(Statusline::Keep, "claude", &dir),
            Statusline::Keep
        );

        record_statusline(&dir, "claude", true);
        assert!(statusline_opted_in(&dir, "claude"));
        assert_eq!(
            effective_statusline(Statusline::Keep, "claude", &dir),
            Statusline::Install,
            "a flagless run honors the recorded opt-in rather than reporting the shim"
        );
        assert_eq!(
            effective_statusline(Statusline::Remove, "claude", &dir),
            Statusline::Remove,
            "an explicit --no-statusline still wins over the record"
        );
        assert_eq!(
            effective_statusline(Statusline::Keep, "cursor", &dir),
            Statusline::Keep,
            "the record is per agent"
        );

        record_statusline(&dir, "claude", false);
        assert!(
            !statusline_opted_in(&dir, "claude"),
            "--no-statusline clears it"
        );
        assert_eq!(
            effective_statusline(Statusline::Keep, "claude", &dir),
            Statusline::Keep
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Recording is idempotent and additive across agents, and an unreadable/absent file reads as
    /// "never asked" rather than erroring — the safe direction, since it only costs a re-run of the
    /// flag whereas the opposite would silently keep a shim.
    #[test]
    fn the_record_holds_several_agents_and_tolerates_a_missing_file() {
        let dir = scratch_dir("multi");
        assert!(!statusline_opted_in(&dir.join("nope"), "claude"));

        record_statusline(&dir, "cursor", true);
        record_statusline(&dir, "claude", true);
        record_statusline(&dir, "claude", true); // idempotent
        let text = std::fs::read_to_string(statusline_state_path(&dir)).unwrap();
        let state: StatuslineState = toml::from_str(&text).unwrap();
        assert_eq!(
            state.agents,
            vec!["claude".to_string(), "cursor".to_string()]
        );

        record_statusline(&dir, "claude", false);
        assert!(
            statusline_opted_in(&dir, "cursor"),
            "one agent's opt-out leaves the other"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- tmux hook drift + late binding -----------------------------------------

    #[test]
    fn the_tmux_hook_command_is_late_bound() {
        let cmd = clear_attention_command(Path::new("/opt/tma/tma"));
        assert!(
            cmd.contains("if [ -x '/opt/tma/tma' ]"),
            "the install-time path is the fast path: {cmd}"
        );
        assert!(
            cmd.contains("else tma clear-attention"),
            "a moved binary falls back to $PATH: {cmd}"
        );
        assert!(
            cmd.contains("'#{hook_pane}'"),
            "the pane stays bound at hook time: {cmd}"
        );
        assert!(
            !cmd.contains('$'),
            "no `$name`: tmux expands those inside the double-quoted argument: {cmd}"
        );
    }

    #[test]
    fn hook_drift_is_path_aware_not_substring() {
        let expected = "run-shell \"if [ -x '/opt/tma/tma' ]; then '/opt/tma/tma' \
                        clear-attention '#{hook_pane}'; else tma clear-attention \
                        '#{hook_pane}' 2>/dev/null; fi\"";
        assert!(hook_command_current(expected, expected), "same command");
        // tmux re-serializes the stored command when printing it back, so quoting and spacing are
        // its choice: normalization must absorb that without hiding a real change.
        let requoted = expected.replace('\'', "\"").replace("; then", ";  then");
        assert!(hook_command_current(&requoted, expected), "{requoted}");

        // A moved binary: still ours (removable), no longer current (rewritable).
        let moved = expected.replace("/opt/tma/tma", "/usr/local/bin/tma");
        assert!(is_ours(&moved), "ownership stays path-blind");
        assert!(
            !hook_command_current(&moved, expected),
            "a different path is drift"
        );
        // The pre-late-binding shape a prior release installed is drift too.
        let old_shape = "run-shell \"/opt/tma/tma clear-attention '#{hook_pane}'\"";
        assert!(is_ours(old_shape));
        assert!(!hook_command_current(old_shape, expected));
    }

    #[test]
    fn hooks_state_serializes_and_parses() {
        let state = HooksState {
            tmux_hooks: vec![
                TmuxHookRecord {
                    hook: "after-select-pane".to_string(),
                    index: 0,
                },
                TmuxHookRecord {
                    hook: "after-select-window".to_string(),
                    index: 2,
                },
            ],
        };
        let toml = toml::to_string(&state).unwrap();
        let back: HooksState = toml::from_str(&toml).unwrap();
        assert_eq!(back.tmux_hooks.len(), 2);
        assert_eq!(back.tmux_hooks[1].index, 2);
    }

    // ---- the wrapper write ------------------------------------------------------

    /// The write is skipped when the wrapper is already current, and that is the whole reason
    /// `install-hooks` works from a read-only prefix: a Nix store path ships the wrapper next to
    /// the binary at mode 555, and asking to write it there is `EACCES`. The read-only file is not
    /// the assertion (a suite running as root could write it anyway); the assertion is that install
    /// succeeds without needing to.
    #[test]
    fn an_already_current_wrapper_is_not_rewritten() {
        use std::os::unix::fs::PermissionsExt;

        let dir = scratch_dir("wrapper_ro");
        let wrapper = dir.join("tma-hook");
        std::fs::write(&wrapper, WRAPPER_SRC).unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o555)).unwrap();

        assert!(wrapper_is_current(&wrapper));
        assert!(
            write_wrapper(&wrapper).is_ok(),
            "a current wrapper needs no write, so its mode cannot fail the install"
        );

        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The skip is content-keyed, so an upgrade whose wrapper changed is still written out, and a
    /// wrapper that is not executable is repaired rather than trusted.
    #[test]
    fn a_stale_or_unexecutable_wrapper_is_rewritten() {
        use std::os::unix::fs::PermissionsExt;

        let dir = scratch_dir("wrapper_stale");
        let wrapper = dir.join("tma-hook");

        std::fs::write(&wrapper, "#!/bin/sh\n# an older tma wrote this\n").unwrap();
        assert!(!wrapper_is_current(&wrapper));
        write_wrapper(&wrapper).unwrap();
        assert_eq!(std::fs::read_to_string(&wrapper).unwrap(), WRAPPER_SRC);
        assert!(std::fs::metadata(&wrapper).unwrap().permissions().mode() & 0o111 != 0);

        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            !wrapper_is_current(&wrapper),
            "right bytes, but an agent cannot exec it"
        );
        write_wrapper(&wrapper).unwrap();
        assert!(std::fs::metadata(&wrapper).unwrap().permissions().mode() & 0o111 != 0);

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
