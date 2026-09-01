use std::path::Path;

use super::claude_json::{
    edit_hooks_install, edit_hooks_uninstall, edit_settings_install, edit_settings_uninstall,
    flat_commands, is_wrapper_command, nested_commands, wrapper_command, CURSOR_SHAPE,
};
use super::codex_toml::{
    codex_hooks_events, codex_notify_is_ours, codex_notify_ok, edit_codex_install,
    edit_codex_uninstall, read_codex_config, CODEX_TRUST_NOTICE,
};
use super::js_bridge::{
    install_js_bridge, install_opencode_plugin, js_bridge_ok, opencode_plugin_ok,
    uninstall_js_bridge, uninstall_opencode_plugin, OPENCODE_PLUGIN_MARKER, PI_EXTENSION_MARKER,
    PI_EXTENSION_SRC, PI_HOOK_TOKEN,
};
use super::json_value::Value;
use super::paths::{same_wrapper_file, tma_bin, ConfigPaths};
use super::statusline::{
    classify_statusline, edit_statusline_install, edit_statusline_uninstall, Statusline,
    StatuslineWiring,
};
use super::{
    apply_file, apply_if_changed, classify_root, read_or_empty_object, reported, HookWiring,
};
use crate::manifests::{self, LoadedManifest};

/// The per-agent installer adapter (the honest split): one impl per agent owns the config
/// `install`/`uninstall` writes and the read-only `classify` for `--check`/`doctor`, since formats
/// differ (Claude a JSON `hooks` block, OpenCode a plugin, Codex two channels). New agents add an
/// impl and an [`adapter_for`] arm. The agent-independent tmux hooks stay in [`super::install`]/[`super::uninstall`].
pub(super) trait AgentAdapter {
    /// Write the agent-config wiring (step 2 of install). `true` on success or a clean no-op;
    /// `false` (after printing the reason) aborts the install.
    fn install(
        &self,
        lm: &LoadedManifest,
        paths: &ConfigPaths,
        wrapper: &Path,
        assume_yes: bool,
        statusline: Statusline,
    ) -> bool;

    /// Undo the agent-config wiring, symmetric to [`AgentAdapter::install`]. `true` on success
    /// or an already-absent no-op; `false` (after printing the reason) aborts the uninstall.
    ///
    /// Takes no wrapper path, unlike `install`: what makes wiring tma's is its shape, so uninstall
    /// removes every entry of ours rather than only the one this build would have written.
    fn uninstall(&self, lm: &LoadedManifest, paths: &ConfigPaths, assume_yes: bool) -> bool;

    /// Classify this agent's config wiring read-only (the per-agent half of `--check`/`doctor`).
    /// Each adapter parses its own config file, so no pre-parsed root is threaded in.
    fn classify(
        &self,
        lm: &LoadedManifest,
        paths: &ConfigPaths,
        wrapper: &Path,
        statusline: Statusline,
    ) -> HookWiring;
}

/// The installer adapter for an agent, or `None` when tma has none. Returning `None` (never a
/// silent Claude default) keeps an unknown agent from contaminating `~/.claude/settings.json`.
pub(super) fn adapter_for(agent: &str) -> Option<&'static dyn AgentAdapter> {
    match agent {
        "claude" => Some(&ClaudeAdapter),
        "gemini" => Some(&GeminiAdapter),
        "cursor" => Some(&CursorAdapter),
        "opencode" => Some(&OpenCodeAdapter),
        "codex" => Some(&CodexAdapter),
        "pi" => Some(&PiAdapter),
        _ => None,
    }
}

/// Resolve the installer adapter, refusing (user-facing message, no config touched) when the agent
/// is not auto-installable: no adapter, or a hookless manifest. Both are wired by hand instead.
pub(super) fn resolve_adapter(lm: &LoadedManifest) -> Result<&'static dyn AgentAdapter, String> {
    if lm.manifest.hooks.is_none() {
        return Err(format!(
            "agent {:?} is hookless (its manifest has no [hooks] block), so there is nothing \
             to install. Hookless agents are detected by screen rules, not hooks.",
            lm.name
        ));
    }
    adapter_for(&lm.name).ok_or_else(|| {
        format!(
            "no install-hooks adapter for agent {0:?}: tma cannot write its config format. \
             Wire it by hand — configure {0} to run `tma-hook {0} <event>` for each event \
             (see docs/reference/agent-coverage.md).",
            lm.name
        )
    })
}

/// Whether each of `events` is wired to the wrapper in a parsed JSON `hooks` root. Shared across
/// every JSON-hooks adapter (Claude/gemini/codex nested, cursor flat), differing only in the
/// `entry_is_ours` predicate, so the `hooks.<event>[]` scan exists exactly once.
fn events_wired(
    root: &Value,
    agent: &str,
    events: &[String],
    wrapper: &Path,
    entry_commands: fn(&Value) -> Vec<String>,
) -> Vec<EventWiring> {
    events
        .iter()
        .map(|event| {
            let cmd = wrapper_command(wrapper, agent, event);
            let commands: Vec<String> = root
                .get("hooks")
                .and_then(|h| h.get(event))
                .and_then(|arr| match arr {
                    Value::Arr(a) => Some(a),
                    _ => None,
                })
                .map(|a| a.iter().flat_map(entry_commands).collect())
                .unwrap_or_default();
            let ours: Vec<&String> = commands
                .iter()
                .filter(|c| is_wrapper_command(c, agent, event))
                .collect();
            if commands.contains(&cmd)
                || ours
                    .iter()
                    .any(|c| entry_names_the_same_wrapper(c, wrapper, agent, event))
            {
                EventWiring::Current
            } else if !ours.is_empty() {
                EventWiring::Stale
            } else {
                EventWiring::Absent
            }
        })
        .collect()
}

/// Whether a wired command `<reference> <agent> <event>` reaches the same wrapper file `expected`
/// does. The suffix is already known to match ([`is_wrapper_command`] gated the caller), so this
/// only has to judge the program part.
fn entry_names_the_same_wrapper(cmd: &str, expected: &Path, agent: &str, event: &str) -> bool {
    cmd.strip_suffix(&format!(" {agent} {event}"))
        .is_some_and(|reference| same_wrapper_file(reference, expected))
}

/// One declared event's wiring state. `Stale` is the distinction that matters: an entry that is
/// tma's but names a different wrapper is wiring that exists and is wrong, which reads very
/// differently from wiring that was never installed — and, collapsed into `Absent`, made a wholly
/// repointed config report as simply not installed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum EventWiring {
    Current,
    Stale,
    Absent,
}

/// One channel of an agent's wiring (its hook events, a statusline shim, codex's `notify` key):
/// whether any of it is present at all, whether it is fully current, and the drift lines to report
/// when it is not. A current channel carries no reasons.
struct Channel {
    present: bool,
    current: bool,
    reasons: Vec<String>,
}

/// Reduce every channel of one agent to a [`HookWiring`]: nothing present ⇒ `NotInstalled`,
/// everything current ⇒ `Wired`, anything else ⇒ `Incomplete` carrying the channels' reasons in the
/// order given. The single reduction behind all three classify paths.
fn classify_channels(channels: &[Channel]) -> HookWiring {
    if channels.iter().all(|c| !c.present) {
        HookWiring::NotInstalled
    } else if channels.iter().all(|c| c.current) {
        HookWiring::Wired
    } else {
        HookWiring::Incomplete(
            channels
                .iter()
                .flat_map(|c| c.reasons.iter().cloned())
                .collect(),
        )
    }
}

/// The hook-events channel, from the per-event mask [`events_wired`] returns.
///
/// Zero declared events is drift, never a pass: `iter().all()` is vacuously true on both the wired
/// and the unwired branch, so an empty set would otherwise classify as fully wired.
fn hook_channel(agent: &str, events: &[String], wired: &[EventWiring]) -> Channel {
    if events.is_empty() {
        return Channel {
            present: true,
            current: false,
            reasons: vec![format!(
                "agent {agent}: declares no hook events, so no wiring could be verified"
            )],
        };
    }
    Channel {
        present: wired.iter().any(|w| *w != EventWiring::Absent),
        current: wired.iter().all(|w| *w == EventWiring::Current),
        reasons: unwired_reasons(agent, events, wired),
    }
}

/// The statusline-shim channel (Claude and cursor), naming the file the shim belongs in. The shim is
/// opt-in, so what counts as drift depends on what this run asked for: an absent shim is only a
/// problem under `--statusline`, and a present one is a problem under `--no-statusline` — or under
/// neither flag, where it means a shim is sitting in the user's settings that nothing requested.
fn statusline_channel(
    agent: &str,
    sl: StatuslineWiring,
    settings: &Path,
    want: Statusline,
) -> Channel {
    let ok = |present| Channel {
        present,
        current: true,
        reasons: Vec::new(),
    };
    match (sl, want) {
        (StatuslineWiring::Wired, Statusline::Install) => ok(true),
        // Nothing of tma's is in that file: absent, or the user's own command sitting where the shim
        // would go. Neither is drift for a run that did not ask for the shim.
        (
            StatuslineWiring::NotInstalled | StatuslineWiring::Foreign,
            Statusline::Remove | Statusline::Keep,
        ) => ok(false),
        // Our shim, but stale (a moved binary): a repair under `--statusline`, and still a shim
        // nobody asked for otherwise, so it is reported either way.
        (StatuslineWiring::Stale(reason), Statusline::Install) => Channel {
            present: true,
            current: false,
            reasons: vec![reason],
        },
        (StatuslineWiring::Wired | StatuslineWiring::Stale(_), Statusline::Keep) => Channel {
            present: true,
            current: false,
            reasons: vec![format!(
                "agent {agent}: statusline context shim is installed in {} but was not asked for; \
                 keep it with `--statusline` or remove it with `--no-statusline`",
                settings.display()
            )],
        },
        (StatuslineWiring::Wired | StatuslineWiring::Stale(_), Statusline::Remove) => Channel {
            present: true,
            current: false,
            reasons: vec![format!(
                "agent {agent}: statusline context shim is still installed in {}; re-run \
                 `tma install-hooks {agent} --no-statusline` to remove it",
                settings.display()
            )],
        },
        // Asked for the shim and something else holds the slot: the forward was overwritten.
        (StatuslineWiring::Foreign, Statusline::Install) => Channel {
            present: true,
            current: false,
            reasons: vec![format!(
                "agent {agent}: statusline command in {} is not tma's context shim (clobbered); \
                 re-run with `--statusline` to re-wrap it",
                settings.display()
            )],
        },
        (StatuslineWiring::NotInstalled, Statusline::Install) => Channel {
            present: false,
            current: false,
            reasons: vec![format!(
                "agent {agent}: statusline context shim not installed in {}",
                settings.display()
            )],
        },
    }
}

/// Apply this run's statusline intent to a settings text, returning it with the label for the diff
/// the user confirms — so the prompt names what the change actually contains. `Keep` returns the
/// text untouched, which is what makes an install without either flag leave a user's statusline
/// exactly as they left it. `None` after printing the reason: the caller aborts.
fn apply_statusline(
    text: String,
    agent: &str,
    intent: Statusline,
    file: &Path,
) -> Option<(String, &'static str)> {
    let edited = match intent {
        Statusline::Install => edit_statusline_install(&text, &tma_bin(), agent)
            .map(|new| (new, "agent hooks + statusline context shim")),
        Statusline::Remove => edit_statusline_uninstall(&text, agent)
            .map(|new| (new, "agent hooks (statusline context shim removed)")),
        Statusline::Keep => return Some((text, "agent hooks")),
    };
    match edited {
        Ok(pair) => Some(pair),
        Err(err) => {
            eprintln!("tma: cannot edit {}: {err}", file.display());
            None
        }
    }
}

/// One "hook `<event>` not wired" reason per unwired event (the drift-report lines `--check`
/// prints).
fn unwired_reasons(agent: &str, events: &[String], wired: &[EventWiring]) -> Vec<String> {
    events
        .iter()
        .zip(wired)
        .filter_map(|(event, state)| match state {
            EventWiring::Current => None,
            EventWiring::Absent => Some(format!("agent {agent}: hook {event} not wired")),
            EventWiring::Stale => Some(format!(
                "agent {agent}: hook {event} names a different tma-hook (stale); re-run \
                 `tma install-hooks {agent}` to repoint it"
            )),
        })
        .collect()
}

/// Claude Code: a `hooks` block plus a statusline context shim in `~/.claude/settings.json`.
struct ClaudeAdapter;

impl AgentAdapter for ClaudeAdapter {
    fn install(
        &self,
        lm: &LoadedManifest,
        paths: &ConfigPaths,
        wrapper: &Path,
        assume_yes: bool,
        statusline: Statusline,
    ) -> bool {
        let events = manifests::hook_events(&lm.manifest);
        let settings = &paths.settings;
        let Some(old) = reported(read_or_empty_object(settings)) else {
            return false;
        };
        // Chain both edits (hooks, then whatever the run asked for on the statusline) so the whole
        // settings.json change is one diff + one confirm.
        let with_hooks = match edit_settings_install(&old, wrapper, &lm.name, &events) {
            Ok(new) => new,
            Err(err) => {
                eprintln!("tma: cannot edit {}: {err}", settings.display());
                return false;
            }
        };
        let Some((new, label)) = apply_statusline(with_hooks, &lm.name, statusline, settings)
        else {
            return false;
        };
        apply_file(settings, &old, &new, assume_yes, label)
    }

    fn uninstall(&self, lm: &LoadedManifest, paths: &ConfigPaths, assume_yes: bool) -> bool {
        let events = manifests::hook_events(&lm.manifest);
        let settings = &paths.settings;
        let Some(old) = reported(read_or_empty_object(settings)) else {
            return false;
        };
        let without_hooks = match edit_settings_uninstall(&old, &lm.name, &events) {
            Ok(new) => new,
            Err(err) => {
                eprintln!("tma: cannot edit {}: {err}", settings.display());
                return false;
            }
        };
        match edit_statusline_uninstall(&without_hooks, &lm.name) {
            Ok(new) => {
                let _ = apply_file(
                    settings,
                    &old,
                    &new,
                    assume_yes,
                    "agent hooks + statusline shim",
                );
                true
            }
            Err(err) => {
                eprintln!("tma: cannot edit {}: {err}", settings.display());
                false
            }
        }
    }

    fn classify(
        &self,
        lm: &LoadedManifest,
        paths: &ConfigPaths,
        wrapper: &Path,
        statusline: Statusline,
    ) -> HookWiring {
        let events = manifests::hook_events(&lm.manifest);
        let root = match classify_root(&lm.name, &paths.settings) {
            Ok(root) => root,
            Err(wiring) => return wiring,
        };
        let wired = events_wired(&root, &lm.name, &events, wrapper, nested_commands);
        let sl = classify_statusline(&root, &tma_bin(), &paths.settings, &lm.name);
        classify_channels(&[
            hook_channel(&lm.name, &events, &wired),
            statusline_channel(&lm.name, sl, &paths.settings, statusline),
        ])
    }
}

/// Gemini CLI: the same Claude-shape `hooks` JSON at `~/.gemini/settings.json`, so this adapter
/// reuses the Claude JSON editor verbatim, differing only in the file it edits.
struct GeminiAdapter;

/// The one-time manual step gemini needs: it gates local hooks behind a per-FOLDER trust prompt
/// (no per-hook gate like codex). Printed after install.
const GEMINI_TRUST_NOTICE: &str = "gemini folder-trust gate: the settings.json hooks load only \
after you trust the working folder in gemini (it prompts \"Trusting a folder allows Gemini CLI \
to load its local configurations, including … hooks …\" on first run there). Once the folder is \
trusted the hooks fire; there is no separate per-hook trust step.";

impl AgentAdapter for GeminiAdapter {
    fn install(
        &self,
        lm: &LoadedManifest,
        paths: &ConfigPaths,
        wrapper: &Path,
        assume_yes: bool,
        _statusline: Statusline,
    ) -> bool {
        let events = manifests::hook_events(&lm.manifest);
        let settings = &paths.gemini_settings;
        let Some(old) = reported(read_or_empty_object(settings)) else {
            return false;
        };
        match edit_settings_install(&old, wrapper, &lm.name, &events) {
            Ok(new) => {
                let ok = apply_file(settings, &old, &new, assume_yes, "agent hooks");
                if ok {
                    println!("tma: {GEMINI_TRUST_NOTICE}");
                }
                ok
            }
            Err(err) => {
                eprintln!("tma: cannot edit {}: {err}", settings.display());
                false
            }
        }
    }

    fn uninstall(&self, lm: &LoadedManifest, paths: &ConfigPaths, assume_yes: bool) -> bool {
        let events = manifests::hook_events(&lm.manifest);
        let settings = &paths.gemini_settings;
        let Some(old) = reported(read_or_empty_object(settings)) else {
            return false;
        };
        match edit_settings_uninstall(&old, &lm.name, &events) {
            // Only write when our entry was actually present, so an absent file is never created
            // by uninstall (symmetric to the codex adapter's only-write-on-change).
            Ok(new) => {
                apply_if_changed(
                    settings,
                    &old,
                    &new,
                    assume_yes,
                    "agent hooks",
                    "gemini hooks",
                );
                true
            }
            Err(err) => {
                eprintln!("tma: cannot edit {}: {err}", settings.display());
                false
            }
        }
    }

    fn classify(
        &self,
        lm: &LoadedManifest,
        paths: &ConfigPaths,
        wrapper: &Path,
        _statusline: Statusline,
    ) -> HookWiring {
        let events = manifests::hook_events(&lm.manifest);
        let root = match classify_root(&lm.name, &paths.gemini_settings) {
            Ok(root) => root,
            Err(wiring) => return wiring,
        };
        let wired = events_wired(&root, &lm.name, &events, wrapper, nested_commands);
        classify_channels(&[hook_channel(&lm.name, &events, &wired)])
    }
}

/// Cursor CLI: two files. The `hooks` block in `~/.cursor/hooks.json`, a flat `{command}` entry
/// (`{"version":1,"hooks":{"<event>":[{"command":"<string>"}]}}`) driving the shared editor with
/// [`CURSOR_SHAPE`] (only the entry shape and `version: 1` differ), plus a statusLine context shim in
/// `~/.cursor/cli-config.json` sharing the Claude shim machinery. Both parse their
/// own file, preserving unrelated user keys; a project `.cursor/hooks.json` is the override tma leaves.
struct CursorAdapter;

impl AgentAdapter for CursorAdapter {
    fn install(
        &self,
        lm: &LoadedManifest,
        paths: &ConfigPaths,
        wrapper: &Path,
        assume_yes: bool,
        statusline: Statusline,
    ) -> bool {
        let events = manifests::hook_events(&lm.manifest);
        let hooks = &paths.cursor_hooks;
        let Some(old) = reported(read_or_empty_object(hooks)) else {
            return false;
        };
        let hooks_ok = match edit_hooks_install(&old, wrapper, &lm.name, &events, &CURSOR_SHAPE) {
            Ok(new) => apply_file(hooks, &old, &new, assume_yes, "cursor hooks"),
            Err(err) => {
                eprintln!("tma: cannot edit {}: {err}", hooks.display());
                false
            }
        };
        if !hooks_ok {
            return false;
        }
        // The statusLine context shim in the separate cli-config.json. `Keep` never opens that file
        // at all: an install that was not asked for a shim has no business rewriting it.
        if statusline == Statusline::Keep {
            return true;
        }
        let cfg = &paths.cursor_cli_config;
        let Some(old_cfg) = reported(read_or_empty_object(cfg)) else {
            return false;
        };
        let Some((new, _)) = apply_statusline(old_cfg.clone(), &lm.name, statusline, cfg) else {
            return false;
        };
        apply_file(
            cfg,
            &old_cfg,
            &new,
            assume_yes,
            "cursor statusline context shim",
        )
    }

    fn uninstall(&self, lm: &LoadedManifest, paths: &ConfigPaths, assume_yes: bool) -> bool {
        let events = manifests::hook_events(&lm.manifest);
        let hooks = &paths.cursor_hooks;
        let Some(old) = reported(read_or_empty_object(hooks)) else {
            return false;
        };
        match edit_hooks_uninstall(&old, &lm.name, &events, &CURSOR_SHAPE) {
            // Only write when our entry was actually present, so an absent file is never created
            // by uninstall (symmetric to the gemini/codex adapters).
            Ok(new) => {
                apply_if_changed(
                    hooks,
                    &old,
                    &new,
                    assume_yes,
                    "cursor hooks",
                    "cursor hooks",
                );
            }
            Err(err) => {
                eprintln!("tma: cannot edit {}: {err}", hooks.display());
                return false;
            }
        }
        // The statusLine shim in cli-config.json; same only-write-on-change rule so an absent
        // cli-config.json is never created by uninstall.
        let cfg = &paths.cursor_cli_config;
        let Some(old_cfg) = reported(read_or_empty_object(cfg)) else {
            return false;
        };
        match edit_statusline_uninstall(&old_cfg, &lm.name) {
            Ok(new) => {
                apply_if_changed(
                    cfg,
                    &old_cfg,
                    &new,
                    assume_yes,
                    "cursor statusline shim",
                    "cursor statusline shim",
                );
                true
            }
            Err(err) => {
                eprintln!("tma: cannot edit {}: {err}", cfg.display());
                false
            }
        }
    }

    fn classify(
        &self,
        lm: &LoadedManifest,
        paths: &ConfigPaths,
        wrapper: &Path,
        statusline: Statusline,
    ) -> HookWiring {
        let events = manifests::hook_events(&lm.manifest);
        let root = match classify_root(&lm.name, &paths.cursor_hooks) {
            Ok(root) => root,
            Err(wiring) => return wiring,
        };
        let wired = events_wired(&root, &lm.name, &events, wrapper, flat_commands);
        let cfg_root = match classify_root(&lm.name, &paths.cursor_cli_config) {
            Ok(root) => root,
            Err(wiring) => return wiring,
        };
        let sl = classify_statusline(&cfg_root, &tma_bin(), &paths.cursor_cli_config, &lm.name);
        classify_channels(&[
            hook_channel(&lm.name, &events, &wired),
            statusline_channel(&lm.name, sl, &paths.cursor_cli_config, statusline),
        ])
    }
}

/// OpenCode: a JS plugin module dropped into its plugin dir.
struct OpenCodeAdapter;

impl AgentAdapter for OpenCodeAdapter {
    fn install(
        &self,
        _lm: &LoadedManifest,
        paths: &ConfigPaths,
        wrapper: &Path,
        assume_yes: bool,
        _statusline: Statusline,
    ) -> bool {
        install_opencode_plugin(&paths.opencode_plugin, wrapper, assume_yes)
    }

    fn uninstall(&self, _lm: &LoadedManifest, paths: &ConfigPaths, assume_yes: bool) -> bool {
        uninstall_opencode_plugin(&paths.opencode_plugin, assume_yes)
    }

    fn classify(
        &self,
        lm: &LoadedManifest,
        paths: &ConfigPaths,
        wrapper: &Path,
        _statusline: Statusline,
    ) -> HookWiring {
        let installed = std::fs::read_to_string(&paths.opencode_plugin)
            .map(|t| t.contains(OPENCODE_PLUGIN_MARKER))
            .unwrap_or(false);
        if !installed {
            HookWiring::NotInstalled
        } else if opencode_plugin_ok(&paths.opencode_plugin, wrapper) {
            HookWiring::Wired
        } else {
            HookWiring::Incomplete(vec![format!(
                "agent {}: plugin {} is stale (references a different wrapper); reinstall",
                lm.name,
                paths.opencode_plugin.display()
            )])
        }
    }
}

/// pi: a JS extension module dropped into `~/.pi/agent/extensions/` (the pi analog of OpenCode's
/// plugin). It shells out to `tma-hook pi <event>`, injecting the session id from `ctx.sessionManager`.
struct PiAdapter;

impl AgentAdapter for PiAdapter {
    fn install(
        &self,
        _lm: &LoadedManifest,
        paths: &ConfigPaths,
        wrapper: &Path,
        assume_yes: bool,
        _statusline: Statusline,
    ) -> bool {
        install_js_bridge(
            &paths.pi_extension,
            PI_EXTENSION_SRC,
            PI_HOOK_TOKEN,
            PI_EXTENSION_MARKER,
            wrapper,
            assume_yes,
            "pi extension",
        )
    }

    fn uninstall(&self, _lm: &LoadedManifest, paths: &ConfigPaths, assume_yes: bool) -> bool {
        uninstall_js_bridge(
            &paths.pi_extension,
            PI_EXTENSION_MARKER,
            assume_yes,
            "pi extension",
        )
    }

    fn classify(
        &self,
        lm: &LoadedManifest,
        paths: &ConfigPaths,
        wrapper: &Path,
        _statusline: Statusline,
    ) -> HookWiring {
        let installed = std::fs::read_to_string(&paths.pi_extension)
            .map(|t| t.contains(PI_EXTENSION_MARKER))
            .unwrap_or(false);
        if !installed {
            HookWiring::NotInstalled
        } else if js_bridge_ok(&paths.pi_extension, wrapper, PI_EXTENSION_MARKER) {
            HookWiring::Wired
        } else {
            HookWiring::Incomplete(vec![format!(
                "agent {}: extension {} is stale (references a different wrapper); reinstall",
                lm.name,
                paths.pi_extension.display()
            )])
        }
    }
}

/// Codex: two channels — the `notify` key in `config.toml` (idle) plus a `hooks.json`
/// (working/lifecycle), both owned by this one adapter (agent-coverage.md "Codex mapping").
struct CodexAdapter;

impl AgentAdapter for CodexAdapter {
    fn install(
        &self,
        lm: &LoadedManifest,
        paths: &ConfigPaths,
        wrapper: &Path,
        assume_yes: bool,
        _statusline: Statusline,
    ) -> bool {
        // Two mechanisms, both wired: the notify key in config.toml (idle), then the verified
        // hooks.json events (working/lifecycle).
        let cfg = &paths.codex_config;
        let Some(old) = reported(read_codex_config(cfg)) else {
            return false;
        };
        let notify_ok = match edit_codex_install(&old, wrapper) {
            Ok(new) => apply_file(cfg, &old, &new, assume_yes, "codex notify"),
            Err(err) => {
                eprintln!("tma: {err}");
                false
            }
        };
        if !notify_ok {
            return false;
        }
        let events = codex_hooks_events(&lm.manifest);
        let hooks_path = &paths.codex_hooks;
        let Some(old_hooks) = reported(read_or_empty_object(hooks_path)) else {
            return false;
        };
        match edit_settings_install(&old_hooks, wrapper, &lm.name, &events) {
            Ok(new) => {
                let ok = apply_file(hooks_path, &old_hooks, &new, assume_yes, "codex hooks");
                if ok {
                    println!("tma: {CODEX_TRUST_NOTICE}");
                }
                ok
            }
            Err(err) => {
                eprintln!("tma: cannot edit {}: {err}", hooks_path.display());
                false
            }
        }
    }

    fn uninstall(&self, lm: &LoadedManifest, paths: &ConfigPaths, assume_yes: bool) -> bool {
        let cfg = &paths.codex_config;
        let Some(old) = reported(read_codex_config(cfg)) else {
            return false;
        };
        match edit_codex_uninstall(&old) {
            // Only write when our entry was actually present (avoid rewriting/creating a config
            // just to normalize it).
            Ok(new) => {
                apply_if_changed(cfg, &old, &new, assume_yes, "codex notify", "codex notify")
            }
            Err(err) => {
                eprintln!("tma: {err}");
                return false;
            }
        }
        // hooks.json: remove exactly our entries, symmetric to install. Same only-write-on-change
        // rule so an absent file is never created by uninstall.
        let events = codex_hooks_events(&lm.manifest);
        let hooks_path = &paths.codex_hooks;
        let Some(old_hooks) = reported(read_or_empty_object(hooks_path)) else {
            return false;
        };
        match edit_settings_uninstall(&old_hooks, &lm.name, &events) {
            Ok(new) => {
                apply_if_changed(
                    hooks_path,
                    &old_hooks,
                    &new,
                    assume_yes,
                    "codex hooks",
                    "codex hooks",
                );
                true
            }
            Err(err) => {
                eprintln!("tma: cannot edit {}: {err}", hooks_path.display());
                false
            }
        }
    }

    fn classify(
        &self,
        lm: &LoadedManifest,
        paths: &ConfigPaths,
        wrapper: &Path,
        _statusline: Statusline,
    ) -> HookWiring {
        // Both channels diagnosed together (notify + hooks.json). Only wiring is observable: whether
        // the user has trusted the hooks.json entries lives in codex's internal store, not here.
        let text = match read_codex_config(&paths.codex_config) {
            Ok(text) => text,
            Err(err) => {
                return HookWiring::Incomplete(vec![format!("agent {}: {err}", lm.name)]);
            }
        };
        let doc = text.parse::<toml_edit::DocumentMut>().ok();
        let notify = doc.as_ref().and_then(|d| d.get("notify"));
        let notify_ours = notify.is_some_and(codex_notify_is_ours);
        let notify_current = notify_ours && codex_notify_ok(&text, wrapper);

        let events = codex_hooks_events(&lm.manifest);
        let hooks_root = match classify_root(&lm.name, &paths.codex_hooks) {
            Ok(root) => root,
            Err(wiring) => return wiring,
        };
        let wired = events_wired(&hooks_root, &lm.name, &events, wrapper, nested_commands);

        // The notify channel first, so a merged drift report reads config.toml then hooks.json.
        let notify_channel = Channel {
            present: notify_ours,
            current: notify_current,
            reasons: if !notify_ours {
                vec![format!(
                    "agent {}: config {} has no tma `notify` entry",
                    lm.name,
                    paths.codex_config.display()
                )]
            } else if !notify_current {
                vec![format!(
                    "agent {}: config {} `notify` references a different wrapper; reinstall",
                    lm.name,
                    paths.codex_config.display()
                )]
            } else {
                Vec::new()
            },
        };
        classify_channels(&[notify_channel, hook_channel(&lm.name, &events, &wired)])
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::install::js_bridge::OPENCODE_PLUGIN_SRC;
    use crate::install::{json_value, CODEX_NOTIFY_EVENT};
    use tma_core::Manifest;
    // Test-only cross-crate reference: the drift assertions exercise the `event` bridge's parser.
    // Production `install` imports no event vocabulary beyond the manifest layer.
    use tma_runtime::event;

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

    fn events(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    /// An adapter declaring zero hook events reports drift, never a clean pass: `all()` over an
    /// empty mask is true on both branches, so the empty set used to classify as fully wired.
    #[test]
    fn zero_hook_events_is_never_wired() {
        let empty = || hook_channel("ghost", &[], &[]);
        let HookWiring::Incomplete(reasons) = classify_channels(&[empty()]) else {
            panic!("an adapter with no hook events must not classify as wired or not-installed");
        };
        assert_eq!(reasons.len(), 1);
        assert!(
            reasons[0].contains("declares no hook events"),
            "the reason says what is missing: {reasons:?}"
        );

        // A second channel that IS fully wired cannot rescue it.
        let current = Channel {
            present: true,
            current: true,
            reasons: Vec::new(),
        };
        assert!(matches!(
            classify_channels(&[empty(), current]),
            HookWiring::Incomplete(_)
        ));
    }

    /// The three reductions over a non-empty event set, and the reason order: channels report in
    /// the order the adapter lists them (codex puts its `notify` channel first).
    #[test]
    fn classify_channels_reduces_hooks_and_a_second_channel() {
        let ev = events(&["SessionStart", "Stop"]);
        let installed = |current: bool, reason: &str| Channel {
            present: true,
            current,
            reasons: if current {
                Vec::new()
            } else {
                vec![reason.to_string()]
            },
        };

        assert!(matches!(
            classify_channels(&[
                hook_channel("codex", &ev, &[EventWiring::Current, EventWiring::Current]),
                installed(true, "")
            ]),
            HookWiring::Wired
        ));
        assert!(matches!(
            classify_channels(&[
                hook_channel("codex", &ev, &[EventWiring::Absent, EventWiring::Absent]),
                Channel {
                    present: false,
                    current: false,
                    reasons: vec!["notify absent".to_string()],
                },
            ]),
            HookWiring::NotInstalled
        ));

        let HookWiring::Incomplete(reasons) = classify_channels(&[
            installed(false, "notify stale"),
            hook_channel("codex", &ev, &[EventWiring::Current, EventWiring::Absent]),
        ]) else {
            panic!("a half-wired agent is Incomplete");
        };
        assert_eq!(
            reasons,
            vec![
                "notify stale".to_string(),
                "agent codex: hook Stop not wired".to_string()
            ]
        );
    }

    /// The statusline channel under `--statusline`: the three verdicts of a run that wants the shim,
    /// including the file the missing-shim line names.
    #[test]
    fn statusline_channel_carries_each_verdict() {
        let settings = PathBuf::from("/home/u/.claude/settings.json");
        let wired = statusline_channel(
            "claude",
            StatuslineWiring::Wired,
            &settings,
            Statusline::Install,
        );
        assert!(wired.present && wired.current && wired.reasons.is_empty());

        let drift = statusline_channel(
            "claude",
            StatuslineWiring::Stale("shim references a different binary".to_string()),
            &settings,
            Statusline::Install,
        );
        assert!(drift.present && !drift.current);
        assert_eq!(
            drift.reasons,
            vec!["shim references a different binary".to_string()]
        );

        let absent = statusline_channel(
            "claude",
            StatuslineWiring::NotInstalled,
            &settings,
            Statusline::Install,
        );
        assert!(!absent.present && !absent.current);
        assert!(absent.reasons[0].contains("/home/u/.claude/settings.json"));
    }

    /// The shim is opt-in, so the same on-disk state reads differently per intent: absent is only a
    /// problem when asked for, present is a problem when asked to remove — and, with neither flag,
    /// present is reported too, so a shim nobody requested does not sit in a user's settings unseen.
    #[test]
    fn statusline_channel_reads_the_same_state_against_what_was_asked_for() {
        let settings = PathBuf::from("/home/u/.claude/settings.json");
        let ch = |sl, want| statusline_channel("claude", sl, &settings, want);

        let absent_unasked = ch(StatuslineWiring::NotInstalled, Statusline::Keep);
        assert!(
            absent_unasked.current && absent_unasked.reasons.is_empty(),
            "no shim and none asked for is not drift"
        );
        assert!(
            ch(StatuslineWiring::NotInstalled, Statusline::Remove).current,
            "--no-statusline is satisfied by an absent shim"
        );

        let present_unasked = ch(StatuslineWiring::Wired, Statusline::Keep);
        assert!(!present_unasked.current, "an unrequested shim is reported");
        assert!(
            present_unasked.reasons[0].contains("--no-statusline"),
            "and names how to remove it: {:?}",
            present_unasked.reasons
        );

        let present_removing = ch(StatuslineWiring::Wired, Statusline::Remove);
        assert!(!present_removing.current);
        assert!(present_removing.reasons[0].contains("--no-statusline"));
    }

    /// The one doc-drift scanner: the deduped backtick tokens in agent-coverage.md's "### `<heading>`"
    /// section. `table_only` restricts to table rows (so prose can't mask a dropped row); `event_shaped`
    /// keeps only hook-event-shaped tokens ([`is_event_name`]). Every per-agent guard is this one function.
    fn doc_tokens(md: &str, heading: &str, table_only: bool, event_shaped: bool) -> Vec<String> {
        let mut in_section = false;
        let mut out: Vec<String> = Vec::new();
        for line in md.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("### ") {
                in_section = rest.contains(heading);
                continue;
            }
            if !in_section || (table_only && !trimmed.starts_with('|')) {
                continue;
            }
            for tok in backtick_tokens(trimmed) {
                if (!event_shaped || is_event_name(&tok)) && !out.contains(&tok) {
                    out.push(tok);
                }
            }
        }
        out
    }

    /// Every backtick-delimited token in a line (empty tokens dropped).
    fn backtick_tokens(s: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '`' {
                let mut tok = String::new();
                for c2 in chars.by_ref() {
                    if c2 == '`' {
                        break;
                    }
                    tok.push(c2);
                }
                if !tok.is_empty() {
                    out.push(tok);
                }
            }
        }
        out
    }

    /// A hook-event-shaped token: leading uppercase, then ASCII alphanumerics only. Excludes
    /// prose tokens like `permission_prompt|elicitation_dialog`, `$TMUX_PANE`, `hooks`.
    fn is_event_name(s: &str) -> bool {
        s.chars().next().is_some_and(|c| c.is_ascii_uppercase())
            && s.chars().all(|c| c.is_ascii_alphanumeric())
    }

    // ---- docs-drift: manifest ⇔ installer/parser event set -----------------------

    #[test]
    fn drift_manifest_installer_parser_consistent() {
        let claude = claude();

        // The installer wires exactly the manifest's declared events + normative subagents,
        // and the parser's hand-maintained coverage declaration equals that set.
        let mut wired = manifests::hook_events(&claude);
        wired.sort();
        let mut coverage: Vec<String> = manifests::CLAUDE_PARSER_COVERAGE
            .iter()
            .map(|s| s.to_string())
            .collect();
        coverage.sort();
        assert_eq!(
            wired, coverage,
            "installer event set must equal parser coverage"
        );

        // Every wired event has a parser arm (maps to something, or is a subagent event).
        for e in &wired {
            let payload = r#"{"session_id":"s","notification_type":"permission_prompt"}"#;
            assert_ne!(
                event::map_event(e, payload, &claude),
                event::Mapped::Unmapped,
                "parser has no arm for {e}"
            );
        }

        // Docs leg: the agent-coverage.md "Claude Code mapping" table must document exactly the
        // wired event set — no doc event the installer omits, no wired event the docs omit.
        let mut documented = doc_tokens(
            include_str!("../../../../docs/reference/agent-coverage.md"),
            "Claude Code mapping",
            true,
            true,
        );
        documented.sort();
        assert_eq!(
            wired, documented,
            "agent-coverage.md mapping table must document exactly the wired hook events"
        );

        // Failability of the docs leg: dropping the subagent row from the table (a stand-in
        // for the docs drifting out of sync) must change the extracted set.
        let dropped: String = include_str!("../../../../docs/reference/agent-coverage.md")
            .lines()
            .filter(|l| !l.contains("`SubagentStart`"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut mutated_docs = doc_tokens(&dropped, "Claude Code mapping", true, true);
        mutated_docs.sort();
        assert_ne!(
            mutated_docs, documented,
            "dropping an event row from the agent-coverage.md table must be caught"
        );

        // Failability (the point of the drift test): a manifest that maps an event the
        // parser coverage omits breaks the equality. Prove it here with a temporary mutation.
        let mutated_src = include_str!("../../../tma-core/manifests/claude.toml").to_string()
            + "\n[[hooks.map]]\nevent = \"PreCompact\"\nclaim = { state = \"working\" }\n";
        let mutated = Manifest::parse(&mutated_src, "mutated.toml").unwrap();
        let mut mutated_events = manifests::hook_events(&mutated);
        mutated_events.sort();
        assert_ne!(
            mutated_events, coverage,
            "adding a manifest event without extending CLAUDE_PARSER_COVERAGE must be caught"
        );
    }

    // ---- Codex hooks.json adapter (the `codex()` manifest helper sits with the
    // notify adapter tests below) --------------------------------------------------

    #[test]
    fn codex_hooks_json_install_uninstall_round_trip() {
        // hooks.json reuses the Claude JSON editor (verified on 0.145.0), inheriting the
        // byte-identical round-trip and idempotency guarantees.
        let events = codex_hooks_events(&codex());
        let original = json_value::to_pretty(
            &json_value::parse(r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"my-own-stop-hook"}]}]}}"#)
                .unwrap(),
        );
        let installed = edit_settings_install(&original, &wrapper(), "codex", &events).unwrap();
        assert!(installed.contains("/opt/tma/tma-hook codex SessionStart"));
        assert!(installed.contains("/opt/tma/tma-hook codex UserPromptSubmit"));
        assert!(installed.contains("/opt/tma/tma-hook codex SessionEnd"));
        assert!(
            !installed.contains("tma-hook codex notify"),
            "notify goes through config.toml, never hooks.json"
        );
        assert!(
            installed.contains("my-own-stop-hook"),
            "a user's own hook survives"
        );
        let twice = edit_settings_install(&installed, &wrapper(), "codex", &events).unwrap();
        assert_eq!(installed, twice, "re-install must be a no-op (deep dedup)");
        let removed = edit_settings_uninstall(&installed, "codex", &events).unwrap();
        assert_eq!(removed, original, "uninstall must restore byte-for-byte");
    }

    #[test]
    fn drift_codex_manifest_installer_parser_docs_consistent() {
        let codex = codex();

        // The hooks.json event set is the manifest's declared events + normative subagents,
        // minus notify (which is config.toml's channel, not a hooks.json event).
        let mut wired = codex_hooks_events(&codex);
        wired.sort();
        let mut full = manifests::hook_events(&codex);
        full.sort();
        let mut expected_full = wired.clone();
        expected_full.push(CODEX_NOTIFY_EVENT.to_string());
        expected_full.sort();
        assert_eq!(
            full, expected_full,
            "codex wires exactly notify + the hooks.json events"
        );

        // Every hooks.json-wired event has a parser arm.
        for e in &wired {
            let payload = r#"{"session_id":"s","hook_event_name":"x"}"#;
            assert_ne!(
                event::map_event(e, payload, &codex),
                event::Mapped::Unmapped,
                "parser has no arm for {e}"
            );
        }

        // Docs leg: the agent-coverage.md "Codex mapping" hooks.json table must document exactly
        // the wired hooks.json event set.
        let mut documented = doc_tokens(
            include_str!("../../../../docs/reference/agent-coverage.md"),
            "Codex mapping",
            true,
            true,
        );
        documented.sort();
        assert_eq!(
            wired, documented,
            "agent-coverage.md Codex mapping table must document exactly the wired hooks.json events"
        );

        // Failability of the docs leg: dropping the UserPromptSubmit row must be caught.
        let dropped: String = include_str!("../../../../docs/reference/agent-coverage.md")
            .lines()
            .filter(|l| !(l.trim_start().starts_with('|') && l.contains("`UserPromptSubmit`")))
            .collect::<Vec<_>>()
            .join("\n");
        let mut mutated_docs = doc_tokens(&dropped, "Codex mapping", true, true);
        mutated_docs.sort();
        assert_ne!(
            mutated_docs, documented,
            "dropping an event row from the Codex mapping table must be caught"
        );
    }

    // ---- hooks-state.toml round-trips ------------------------------------------

    // ---- OpenCode plugin adapter ------------------------------------------------

    fn opencode() -> Manifest {
        Manifest::parse(
            include_str!("../../../tma-core/manifests/opencode.toml"),
            "opencode.toml",
        )
        .unwrap()
    }

    /// The wrapper tokens the OpenCode plugin emits, from every `fire("<token>"` call (the
    /// `const fire = (…)` definition is not a call, so it is not matched).
    fn opencode_plugin_tokens(js: &str) -> Vec<String> {
        let mut out = Vec::new();
        for (i, _) in js.match_indices("fire(\"") {
            let rest = &js[i + "fire(\"".len()..];
            if let Some(end) = rest.find('"') {
                let tok = rest[..end].to_string();
                if !out.contains(&tok) {
                    out.push(tok);
                }
            }
        }
        out
    }

    fn opencode_map_events(m: &Manifest) -> Vec<String> {
        let mut v: Vec<String> = m
            .hooks
            .as_ref()
            .unwrap()
            .map
            .iter()
            .map(|h| h.event.clone())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn drift_opencode_manifest_plugin_parser_docs_consistent() {
        let oc = opencode();

        // Leg 1: the plugin emits exactly the tokens the manifest maps, plus the
        // `permission-replied` clear signal, which is deliberately NOT a state map (the intake keys
        // the `@agent_permission_request` clear on it directly), so it is excluded from the equality.
        let mut plugin = opencode_plugin_tokens(OPENCODE_PLUGIN_SRC);
        assert!(
            plugin.iter().any(|t| t == event::PERMISSION_REPLIED),
            "the plugin must emit the permission-replied clear signal"
        );
        plugin.retain(|t| t != event::PERMISSION_REPLIED);
        plugin.sort();
        let mapped = opencode_map_events(&oc);
        assert_eq!(
            plugin, mapped,
            "plugin fire() state tokens must equal the manifest map events"
        );

        // Leg 2: the generic parser resolves every wired token to something.
        let payload = r#"{"session_id":"ses_x","permission":"bash"}"#;
        for e in &mapped {
            assert_ne!(
                event::map_event(e, payload, &oc),
                event::Mapped::Unmapped,
                "parser has no arm for {e}"
            );
        }

        // Leg 3: agent-coverage.md's OpenCode mapping TABLE documents every wired token (table-rows-only
        // now: a prose mention no longer masks a dropped mapping-table row).
        let md = include_str!("../../../../docs/reference/agent-coverage.md");
        let documented = doc_tokens(md, "OpenCode mapping", true, false);
        for e in &mapped {
            assert!(
                documented.contains(e),
                "agent-coverage.md OpenCode mapping omits `{e}`"
            );
        }

        // Failability (docs leg): dropping the permission row must be caught.
        let dropped: String = md
            .lines()
            .filter(|l| !l.contains("`permission-required`"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !doc_tokens(&dropped, "OpenCode mapping", true, false)
                .contains(&"permission-required".to_string()),
            "dropping the permission-required row from the docs must change the extracted set"
        );

        // Failability (manifest↔plugin leg): a manifest event with no plugin token breaks it.
        let mutated_src = include_str!("../../../tma-core/manifests/opencode.toml").to_string()
            + "\n[[hooks.map]]\nevent = \"compact\"\nclaim = { state = \"working\" }\n";
        let mutated = Manifest::parse(&mutated_src, "mutated.toml").unwrap();
        assert_ne!(
            opencode_map_events(&mutated),
            plugin,
            "adding a manifest event without a plugin token must be caught"
        );
    }

    // ---- Codex config.toml adapter ---------------------------------------------

    fn codex() -> Manifest {
        Manifest::parse(
            include_str!("../../../tma-core/manifests/codex.toml"),
            "codex.toml",
        )
        .unwrap()
    }

    /// The `[[hooks.map]]` event names the codex manifest declares.
    fn codex_map_events(m: &Manifest) -> Vec<String> {
        let mut v: Vec<String> = m
            .hooks
            .as_ref()
            .unwrap()
            .map
            .iter()
            .map(|h| h.event.clone())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn drift_codex_manifest_notify_config_parser_docs_consistent() {
        let cx = codex();

        // Leg 1: the manifest's map events are exactly the notify token (config.toml) plus the
        // seven live-verified hooks.json events (see the other codex drift test for that side).
        assert_eq!(
            codex_map_events(&cx),
            vec![
                "PermissionRequest".to_string(),
                "PostToolUse".to_string(),
                "PreToolUse".to_string(),
                "SessionEnd".to_string(),
                "SessionStart".to_string(),
                "Stop".to_string(),
                "UserPromptSubmit".to_string(),
                CODEX_NOTIFY_EVENT.to_string(),
            ],
            "manifest must map exactly the events the installer wires on its two channels"
        );

        // Leg 2: the generic parser resolves that token (with an agent-turn-complete payload).
        let payload = r#"{"type":"agent-turn-complete","turn-id":"u"}"#;
        assert_ne!(
            event::map_event(CODEX_NOTIFY_EVENT, payload, &cx),
            event::Mapped::Unmapped,
            "parser has no arm for the notify event"
        );

        // Leg 3: agent-coverage.md's Codex mapping TABLE documents the wired token (the notify token is
        // lowercase, so this leg does NOT event-shape — it scans all table tokens).
        let md = include_str!("../../../../docs/reference/agent-coverage.md");
        assert!(
            doc_tokens(md, "Codex mapping", true, false).contains(&CODEX_NOTIFY_EVENT.to_string()),
            "agent-coverage.md Codex mapping must document `{CODEX_NOTIFY_EVENT}`"
        );

        // Failability (docs leg): dropping the notify row must change the extracted set.
        let dropped: String = md
            .lines()
            .filter(|l| !(l.contains("agent-turn-complete") && l.contains("`notify`")))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !doc_tokens(&dropped, "Codex mapping", true, false)
                .contains(&CODEX_NOTIFY_EVENT.to_string()),
            "dropping the notify row from the docs must change the extracted set"
        );

        // Failability (manifest↔config leg): a manifest event with no config token breaks it.
        let mutated_src = include_str!("../../../tma-core/manifests/codex.toml").to_string()
            + "\n[[hooks.map]]\nevent = \"other\"\nclaim = { state = \"working\" }\n";
        let mutated = Manifest::parse(&mutated_src, "mutated.toml").unwrap();
        assert_ne!(
            codex_map_events(&mutated),
            codex_map_events(&cx),
            "adding a manifest event the installer does not wire must be caught"
        );
    }

    // ---- Gemini settings.json adapter -------------------------------------------

    fn gemini() -> Manifest {
        Manifest::parse(
            include_str!("../../../tma-core/manifests/gemini.toml"),
            "gemini.toml",
        )
        .unwrap()
    }

    #[test]
    fn drift_gemini_manifest_installer_parser_docs_consistent() {
        let gemini = gemini();

        // The installer wires exactly the manifest's declared events + normative subagents (the
        // gemini adapter reuses `hook_events`, same as the Claude adapter).
        let mut wired = manifests::hook_events(&gemini);
        wired.sort();

        // Every wired event has a parser arm. The payload carries a session_id and a
        // `notification_type` so registration/tool and matched-Notification events all resolve.
        for e in &wired {
            let payload = r#"{"session_id":"s","hook_event_name":"x","prompt":"p","notification_type":"ToolPermission"}"#;
            assert_ne!(
                event::map_event(e, payload, &gemini),
                event::Mapped::Unmapped,
                "parser has no arm for {e}"
            );
        }

        // Docs leg: the agent-coverage.md "Gemini mapping" table must document exactly the wired set.
        let mut documented = doc_tokens(
            include_str!("../../../../docs/reference/agent-coverage.md"),
            "Gemini mapping",
            true,
            true,
        );
        documented.sort();
        assert_eq!(
            wired, documented,
            "agent-coverage.md Gemini mapping table must document exactly the wired hook events"
        );

        // Failability (docs leg): dropping the AfterAgent row must be caught.
        let dropped: String = include_str!("../../../../docs/reference/agent-coverage.md")
            .lines()
            .filter(|l| !(l.trim_start().starts_with('|') && l.contains("`AfterAgent`")))
            .collect::<Vec<_>>()
            .join("\n");
        let mut mutated_docs = doc_tokens(&dropped, "Gemini mapping", true, true);
        mutated_docs.sort();
        assert_ne!(
            mutated_docs, documented,
            "dropping an event row from the Gemini mapping table must be caught"
        );

        // Failability (manifest↔parser leg): BeforeModel is unmapped by intent, so mapping it
        // would surface as an Unmapped the loop above forbids.
        let mutated_src = include_str!("../../../tma-core/manifests/gemini.toml").to_string();
        let mutated = Manifest::parse(&mutated_src, "mutated.toml").unwrap();
        assert_eq!(
            event::map_event("BeforeModel", "{}", &mutated),
            event::Mapped::Unmapped,
            "BeforeModel stays unmapped (multi-fire, races the final idle)"
        );
    }

    // ---- pi extension adapter ---------------------------------------------------

    fn pi() -> Manifest {
        Manifest::parse(
            include_str!("../../../tma-core/manifests/pi.toml"),
            "pi.toml",
        )
        .unwrap()
    }

    /// The wrapper tokens the pi extension emits, from every `fire("<token>"` call (the
    /// `function fire(event, ctx)` definition is `fire(event`, not matched; mirrors OpenCode).
    fn pi_extension_tokens(js: &str) -> Vec<String> {
        let mut out = Vec::new();
        for (i, _) in js.match_indices("fire(\"") {
            let rest = &js[i + "fire(\"".len()..];
            if let Some(end) = rest.find('"') {
                let tok = rest[..end].to_string();
                if !out.contains(&tok) {
                    out.push(tok);
                }
            }
        }
        out
    }

    fn pi_map_events(m: &Manifest) -> Vec<String> {
        let mut v: Vec<String> = m
            .hooks
            .as_ref()
            .unwrap()
            .map
            .iter()
            .map(|h| h.event.clone())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn drift_pi_manifest_extension_parser_docs_consistent() {
        let p = pi();

        // Leg 1: the extension fires exactly the tokens the manifest maps.
        let mut ext = pi_extension_tokens(PI_EXTENSION_SRC);
        ext.sort();
        let mapped = pi_map_events(&p);
        assert_eq!(
            ext, mapped,
            "extension fire() tokens must equal the manifest map events"
        );

        // Leg 2: the generic parser resolves every wired token (the extension forwards
        // `{session_id}`, so registration/state events resolve).
        let payload = r#"{"session_id":"019f-x"}"#;
        for e in &mapped {
            assert_ne!(
                event::map_event(e, payload, &p),
                event::Mapped::Unmapped,
                "parser has no arm for {e}"
            );
        }

        // Leg 3: the pi mapping TABLE documents every wired token (table-rows-only, so the
        // "Deliberately unmapped" prose below can't mask a dropped row).
        let md = include_str!("../../../../docs/reference/agent-coverage.md");
        let documented = doc_tokens(md, "pi mapping", true, false);
        for e in &mapped {
            assert!(
                documented.contains(e),
                "agent-coverage.md pi mapping omits `{e}`"
            );
        }

        // Failability (docs leg): dropping the agent_settled TABLE row must be caught.
        let dropped: String = md
            .lines()
            .filter(|l| !(l.trim_start().starts_with('|') && l.contains("`agent_settled`")))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !doc_tokens(&dropped, "pi mapping", true, false).contains(&"agent_settled".to_string()),
            "dropping the agent_settled row from the docs must change the extracted set"
        );

        // Failability (manifest↔extension leg): a manifest event with no fire token breaks it.
        let mutated_src = include_str!("../../../tma-core/manifests/pi.toml").to_string()
            + "\n[[hooks.map]]\nevent = \"turn_start\"\nclaim = { state = \"working\" }\n";
        let mutated = Manifest::parse(&mutated_src, "mutated.toml").unwrap();
        assert_ne!(
            pi_map_events(&mutated),
            ext,
            "adding a manifest event without a fire token must be caught"
        );
    }
}
