//! The tmux attention-clear server hooks: the command an install writes, the per-server install
//! record, and the daemon's startup re-arm.
//!
//! Here rather than in the binary because two callers have to write the SAME string: `tma
//! install-hooks`, and the daemon, which re-arms a recorded hook the server no longer has. tmux
//! hooks are runtime server state, so a `kill-server` or a reboot wipes the array while the record
//! still says they are installed; a second rendering of the command would read as drift the moment
//! the two spellings diverged.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tma_tmux::tmux::{DepartureKind, Tmux};

/// The environment variable carrying WHICH focus hook fired, so `clear-attention` can also clear the
/// pane the user just left (seen-on-leave). An environment variable, deliberately, not an argv flag:
/// the command below is late-bound, so a hook string written by a new install routinely invokes an
/// older binary. An unknown flag would make clap error on every single pane switch; an unknown
/// environment variable is ignored in silence, which is the only acceptable failure mode for a hook
/// that fires on every navigation. Read in the bin's `dispatch::run_clear_attention`.
pub const HOOK_KIND_ENV: &str = "TMA_HOOK_KIND";

/// The tma binary path referenced by the tmux hook command (`$TMA_BIN`, else the running exe).
/// Tests point `$TMA_BIN` at the built binary.
pub fn tma_bin() -> PathBuf {
    if let Some(p) = std::env::var_os("TMA_BIN").filter(|v| !v.is_empty()) {
        return PathBuf::from(p);
    }
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("tma"))
}

/// The tmux-hook command clearing attention on focus change. `#{pane_id}` (never `$TMUX_PANE`, and
/// never `#{hook_pane}` - see below) binds the pane at hook time. The binary is LATE-BOUND like the
/// `tma-hook` wrapper: the install-time absolute path when it is still executable, else plain `tma`
/// off `$PATH`, so a rebuilt, moved, or re-installed binary keeps the hook working instead of
/// leaving a dead command behind. The middle-tier nudge lives inside the same `clear-attention`
/// subcommand. `hook` is the tmux hook the command is being written for; it selects the
/// seen-on-leave posture (see [`HOOK_KIND_ENV`]).
pub fn clear_attention_command(bin: &Path, hook: &str) -> String {
    // `#{pane_id}`, NOT `#{hook_pane}`. `hook_pane` is populated only on the notify_pane-style hooks
    // (`pane-focus-in` and friends); on `after-select-pane` and on `session-window-changed`
    // it expands EMPTY, which `clear-attention` treats as a no-op, so the always-on pair cleared
    // nothing at all, for anyone, and the flag only ever came off via the picker, jump, or the
    // opt-in focus hook. Verified on tmux 3.6a, attached and detached, key-driven and out-of-band:
    // `after-select-pane` gives `hook_pane=[]` / `pane_id=[%0]`, `pane-focus-in` gives both. The man
    // page hedges it under FORMATS ("ID of pane where hook was run, if any"). `#{pane_id}` resolves
    // in all three hooks, so one shape serves them all; it stays quoted so an empty expansion still
    // passes an argument rather than shifting the argv.
    //
    // Single quotes only: the whole string is a tmux double-quoted argument, where tmux expands
    // `#{...}` (wanted) and `$name` (not wanted), so the shell side stays `$`-free. The PATH
    // fallback swallows its own failure: with no `tma` anywhere, sh exits 127 and tmux would flash
    // "returned 127" on every pane switch, so that branch stays silent like the tma-hook wrapper.
    // The `-x` branch swallows its own failure for the same reason, and for a second one: a hook
    // string written by a NEW install can still invoke an OLD binary (that is what late binding
    // buys), and a mismatch must not turn into a message on every pane switch.
    let kind = departure_kind_env(hook);
    format!(
        "run-shell \"if [ -x '{0}' ]; then {1}'{0}' clear-attention '#{{pane_id}}' 2>/dev/null \
         || true; else {1}tma clear-attention '#{{pane_id}}' 2>/dev/null || true; fi\"",
        bin.display(),
        kind
    )
}

/// The `VAR=value ` shell prefix for a hook, empty for a hook that carries no departure. Kept as a
/// prefix rather than an exported variable so it scopes to the one command.
fn departure_kind_env(hook: &str) -> String {
    match DepartureKind::from_hook_name(hook) {
        Some(_) => format!("{HOOK_KIND_ENV}={hook} "),
        None => String::new(),
    }
}

/// Whether an installed hook command is what install would write now. Compared modulo whitespace and
/// quoting: tmux re-serializes the stored command when printing it back, so quote style is its
/// choice, while a changed binary path or command shape (the drift this detects) survives
/// normalization.
pub fn hook_command_current(installed: &str, expected: &str) -> bool {
    fn normalize(s: &str) -> String {
        s.chars()
            .filter(|c| !c.is_whitespace() && !matches!(c, '"' | '\'' | '\\'))
            .collect()
    }
    normalize(installed) == normalize(expected)
}

/// A hook command is ours iff it invokes `clear-attention`: the OWNERSHIP test (what uninstall may
/// remove), deliberately path-blind. Whether an owned entry is still CURRENT is
/// [`hook_command_current`]'s job; the two questions have different answers after a binary moves.
pub fn is_ours(command: &str) -> bool {
    command.contains("clear-attention")
}

/// Recorded tmux-hook install metadata, persisted to `hooks-state-<key>.toml`.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct HooksState {
    #[serde(default)]
    pub tmux_hooks: Vec<TmuxHookRecord>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TmuxHookRecord {
    pub hook: String,
    pub index: usize,
}

/// The legacy, pre-per-server-keying state filename. Kept as the migration source (and the
/// server-gone fallback) so a single-server install written before keying is still honored.
pub const LEGACY_HOOKS_STATE: &str = "hooks-state.toml";

/// The per-server hooks-state path `hooks-state-<key>.toml`, keyed by a hash of `#{socket_path}`
/// ([`crate::ipc::socket_key`]) since tmux `set-hook -g` indexes are per-server. Falls back to the
/// legacy unkeyed name when the server is unreachable.
pub fn hooks_state_path(config_dir: &Path, tmux: &Tmux) -> PathBuf {
    match crate::ipc::resolve_socket_path(tmux) {
        Some(socket_path) => config_dir.join(format!(
            "hooks-state-{}.toml",
            crate::ipc::socket_key(&socket_path)
        )),
        None => config_dir.join(LEGACY_HOOKS_STATE),
    }
}

/// Read the target server's tmux-hook metadata, `None` when absent/unparseable. Primary: the keyed
/// file. Migration: when absent, fall back to the legacy unkeyed `hooks-state.toml` (this server's in
/// the common single-server setup). The returned flag is `true` on the legacy source, so uninstall
/// removes it only when consumed; [`is_ours`] content matching bounds the damage if it was another
/// server's record.
pub fn read_hooks_state(config_dir: &Path, tmux: &Tmux) -> Option<(HooksState, bool)> {
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

pub fn write_hooks_state(config_dir: &Path, tmux: &Tmux, state: &HooksState) -> io::Result<()> {
    std::fs::create_dir_all(config_dir)?;
    let toml = toml::to_string(state)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    let header = "# tma tmux-hook install record, keyed per server (tmux -g hook indexes\n\
                  # are per-server). Install metadata, exempt from the no-files rule. Do not\n\
                  # hand-edit; `tma install-hooks --uninstall` clears it.\n";
    std::fs::write(
        hooks_state_path(config_dir, tmux),
        format!("{header}{toml}"),
    )
}

/// Re-install the recorded attention-clear hooks this server no longer carries, returning the hook
/// names actually re-armed (empty when there was nothing to do). The daemon runs this at startup.
///
/// tmux hooks live in the server, not on disk, so a `kill-server` or a reboot drops every one of
/// them while the install record still says they are installed: before this, the pane you selected
/// stopped clearing its attention flag until someone re-ran `tma install-hooks`. Guarded three ways:
/// no record means never installed here (nothing is set), a hook that still carries an entry of ours
/// is left exactly as it is (a DRIFTED entry is install's repair, not the daemon's), and the record
/// is rewritten only when tmux assigned an index other than the recorded one.
pub fn rearm_installed_hooks(tmux: &Tmux, config_dir: &Path) -> Vec<String> {
    let Some((state, _)) = read_hooks_state(config_dir, tmux) else {
        return Vec::new();
    };
    let bin = tma_bin();
    let mut rearmed = Vec::new();
    let mut records: Vec<TmuxHookRecord> = Vec::new();
    let mut moved = false;
    for record in &state.tmux_hooks {
        if records.iter().any(|r| r.hook == record.hook) {
            continue; // a duplicated record must not append a second entry
        }
        let index = match tmux.show_global_hook(&record.hook) {
            Ok(entries) if entries.iter().any(|(_, c)| is_ours(c)) => ours_index(&entries),
            Ok(_) => rearm_one(tmux, &record.hook, &bin).inspect(|_| {
                rearmed.push(record.hook.clone());
            }),
            // An unreadable server is nothing to repair, and the record stays as it was.
            Err(_) => None,
        };
        let index = index.unwrap_or(record.index);
        moved |= index != record.index;
        records.push(TmuxHookRecord {
            hook: record.hook.clone(),
            index,
        });
    }
    if !rearmed.is_empty() && moved {
        // Keep the record true: `--check` and `doctor` look our entry up at its recorded index.
        let _ = write_hooks_state(
            config_dir,
            tmux,
            &HooksState {
                tmux_hooks: records,
            },
        );
    }
    rearmed
}

/// Append our clear-attention command to one hook array and return the index tmux assigned it,
/// `None` when either tmux call failed.
fn rearm_one(tmux: &Tmux, hook: &str, bin: &Path) -> Option<usize> {
    tmux.append_global_hook(hook, &clear_attention_command(bin, hook))
        .ok()?;
    ours_index(&tmux.show_global_hook(hook).ok()?)
}

/// The index of our entry in one hook array, `None` when it holds none.
fn ours_index(entries: &[(usize, String)]) -> Option<usize> {
    entries.iter().find(|(_, c)| is_ours(c)).map(|(i, _)| *i)
}
