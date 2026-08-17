//! The clap derive tree: every subcommand, its args, and the shared selector. Parsing lives here so
//! `main` stays the dispatch table; the parse tests that pin flag placement and exclusivity ride
//! with the types they describe.

use std::path::PathBuf;

use clap::Parser;

use crate::{cli_support, wait};

/// tmux-agents — agent state monitor for tmux.
#[derive(Parser)]
#[command(name = "tma", version, about)]
pub(crate) struct Cli {
    /// Load manifests only from this directory (test isolation). Global: accepted before or
    /// after any subcommand, read from one canonical field so targeting can never diverge.
    #[arg(long = "manifest-dir", global = true, value_name = "DIR")]
    pub(crate) manifest_dir: Option<PathBuf>,
    /// Target a specific tmux server socket by name (`tmux -L <name>`); test isolation. Global:
    /// accepted before or after any subcommand so `tma --socket-name X ls` and
    /// `tma ls --socket-name X` hit the same server (the isolation the whole tool depends on).
    #[arg(long = "socket-name", global = true, value_name = "NAME")]
    pub(crate) socket_name: Option<String>,
    /// Target a tmux server by socket PATH (`tmux -S <path>`), the form tmate and a hand-placed
    /// socket need; env `TMA_SOCKET_PATH` when neither socket flag is given. Global, like
    /// `--socket-name`, and mutually exclusive with it (naming a server two ways is a usage error).
    #[arg(
        long = "socket-path",
        global = true,
        value_name = "PATH",
        conflicts_with = "socket_name"
    )]
    pub(crate) socket_path: Option<PathBuf>,
    /// Print cycle timing and producer/consumer/capture counts to stderr. Global; only the
    /// poll surfaces (ls/status/jump) act on it.
    #[arg(long = "debug-timing", global = true)]
    pub(crate) debug_timing: bool,
    /// Load config from this path instead of `~/.config/tma/config.toml` (test isolation; env
    /// `TMA_CONFIG`). Global: read from one canonical field regardless of position, mirroring
    /// `--socket-name`/`--manifest-dir`. An absent file is the zero-config floor (all defaults).
    #[arg(long = "config", global = true, value_name = "PATH")]
    pub(crate) config: Option<PathBuf>,
    /// The invoking tmux client for the picker/jump/watch Enter-jump. The `run-shell` jump
    /// bindings pass `--client "#{client_name}"` so the correct client is switched; absent, empty,
    /// or a still-unexpanded format (a binding context that does not expand, such as
    /// `display-popup`) falls back to targetless best-effort. Global (like
    /// `--socket-name`/`--config`): read from one
    /// canonical field regardless of position, so `tma --client X jump` and `tma jump --client X`
    /// are identical.
    #[arg(long, short = 'c', global = true, value_name = "NAME")]
    pub(crate) client: Option<String>,
    #[command(subcommand)]
    pub(crate) command: Option<Command>,
}

#[allow(clippy::large_enum_variant)]
#[derive(clap::Subcommand)]
pub(crate) enum Command {
    /// Print version and build information.
    Version,
    /// List agent panes (one tab-separated line each; `--json` for the versioned schema).
    Ls(LsArgs),
    /// Print the status-line one-liner: state counts with glyphs and `#[fg=]` styling.
    Status(StatusArgs),
    /// Jump focus to an agent pane across sessions
    /// (`--attention` / `--blocked` / `--next` / `--back` / `--home`).
    Jump(JumpArgs),
    /// Block until the target reaches one of `--until`'s states, then print the matched row(s). The
    /// scripting primitive: one pane, or a fleet (`--all` / `--count`). Exit 0 = observed,
    /// 124 = timeout, 3 = a watched pane vanished, 4 = its agent died.
    Wait(WaitArgs),
    /// Fire a guarded action into an agent pane (`tma act <name>`, `--all` for every pane in
    /// scope), or enumerate/menu the fireable ones (`--list` / `--menu`). Exit 0 acted, 4 gate
    /// refused, 5 pane locked, 3 target gone, 124 exec timeout, 2 usage, 1 runtime failure.
    Act(ActArgs),
    /// Suppress notifications for the matched panes (`--for <DURATION>`, or indefinitely), or lift
    /// it with `--clear`. Detection, stamping, and the counts are untouched; the deadline lives in
    /// `@agent_mute_until`, so a mute survives a tma or daemon restart.
    Mute(MuteArgs),
    /// Stream the read path: one complete `ls --json` document per line, riding the daemon's edge
    /// pushes when present and degrading to an `--interval` poll otherwise (contract-identical). A
    /// plugin spawns this instead of a polling timer; it exits on a signal or when its stdout closes.
    Subscribe(SubscribeArgs),
    /// Persistent live dashboard for a normal pane, window, or terminal of its own:
    /// `new-window "tma watch"`. Enter jumps (stays open); `q`/Esc quits.
    Watch(WatchArgs),
    /// INTERNAL, UNSTABLE: bridge one agent hook event to a stamp. Agent configs reference the
    /// `tma-hook` wrapper, never this subcommand directly (see DAEMON.md).
    #[command(hide = true)]
    Event(EventCmdArgs),
    /// Run the event-hub daemon in the foreground; `--ensure` spawns it if absent then exits
    /// (idempotent launcher, the tier-3 driver / DAEMON.md). Strictly additive: never required.
    Daemon(DaemonCmdArgs),
    /// Signal the running daemon to hot-reload its config + manifests (SIGHUP). A no-op message
    /// if none is running for this server; one-shots and the picker reload on their own.
    Reload,
    /// First-run setup: detect the agents you have installed and wire their hooks, install the
    /// keybindings, print the `status-right` line tma never writes for you, then run `tma doctor`.
    Init(InitArgs),
    /// Install/uninstall/verify the agent + tmux hook wiring.
    InstallHooks(InstallHooksArgs),
    /// Install/uninstall/verify tma's tmux keybindings (managed file + one `source-file` line).
    InstallKeys(InstallKeysArgs),
    /// Diagnose each agent pane's effective tier (3/2/1) and why: hooks wired, daemon alive,
    /// last evidence source + age, and the ambient-driver check. Read-only; `--json` for the schema.
    Doctor(DoctorArgs),
    /// INTERNAL: clear a pane's `@agent_attention` flag; the tmux focus hooks call this with
    /// `#{hook_pane}`. Also invoked by the picker's Enter-jump.
    #[command(hide = true)]
    ClearAttention(ClearAttentionArgs),
    /// INTERNAL: the detached-action supervisor. Spawned by the `tma act` broker's detach path
    /// to hold the single-flight lock for the child's lifetime, kill it at the deadline, then clear the
    /// lock and fire the completion notification. Never user-invoked.
    #[command(hide = true)]
    Supervise(SuperviseArgs),
    /// Manifest-authoring and inspection tools.
    Debug(DebugArgs),
}

/// Args for `tma init`. The target server and the manifest dir come from the globals; the two
/// flags here are the ones the steps it chains need (`install-hooks`/`install-keys` `--config-dir`,
/// `install-keys` `--conf`), spelled the same way they are there.
#[derive(clap::Args)]
pub(crate) struct InitArgs {
    /// Apply every step without the interactive diff confirmations (scripts, tests).
    #[arg(long)]
    pub(crate) yes: bool,
    /// Also start the event-hub daemon for this server right now (what `tma daemon --ensure`
    /// does). The launcher for future servers is written either way; `--no-daemon` skips both.
    #[arg(long)]
    pub(crate) daemon: bool,
    /// Wire no daemon at all: omit the server-start launcher `install-keys` writes by default,
    /// and start none for this server.
    #[arg(long = "no-daemon", conflicts_with = "daemon")]
    pub(crate) no_daemon: bool,
    /// Override the tma config dir holding the managed `tmux.conf` and the per-server
    /// `hooks-state-<server>.toml` (env `TMA_CONFIG_DIR`). Defaults to `~/.config/tma`.
    #[arg(long = "config-dir", value_name = "DIR")]
    pub(crate) config_dir: Option<PathBuf>,
    /// The tmux config to mark with the keybindings `source-file` line, and the file the
    /// status-line instructions name. Same default as `install-keys --conf`.
    #[arg(long, value_name = "PATH")]
    pub(crate) conf: Option<PathBuf>,
}

/// Args for `tma install-hooks`.
#[derive(clap::Args)]
pub(crate) struct InstallHooksArgs {
    /// The agent whose config to wire (e.g. `claude`). Optional only with `--check`.
    pub(crate) agent: Option<String>,
    /// Remove tma's hook wiring (symmetric to install).
    #[arg(long)]
    pub(crate) uninstall: bool,
    /// Verify hook wiring and report drift. Bare (`--check`) inspects every known agent; with an
    /// agent named (`install-hooks <agent> --check`) the drift report and exit code scope to that
    /// agent. The shared wrapper + tmux server hooks are always checked.
    #[arg(long)]
    pub(crate) check: bool,
    /// Also wire the statusline context shim, which composes tma's context intake into the agent's
    /// own `statusLine` command (Claude, Cursor) to read the context-window gauge. Opt-in, because
    /// it edits a command you own. With `--check`, require it.
    #[arg(long)]
    pub(crate) statusline: bool,
    /// Remove the statusline context shim, restoring the command it wrapped. With `--check`,
    /// require its absence. Given neither flag, an installed shim is left alone but reported.
    #[arg(long = "no-statusline", conflicts_with = "statusline")]
    pub(crate) no_statusline: bool,
    /// Apply without the interactive diff confirmation (scripts, tests).
    #[arg(long)]
    pub(crate) yes: bool,
    /// Override the agent settings path (test isolation; env `TMA_CLAUDE_SETTINGS`).
    #[arg(long, value_name = "PATH")]
    pub(crate) settings: Option<PathBuf>,
    /// Override Gemini's `settings.json` path (test isolation; env `TMA_GEMINI_SETTINGS`).
    /// Defaults to `~/.gemini/settings.json`.
    #[arg(long = "gemini-settings", value_name = "PATH")]
    pub(crate) gemini_settings: Option<PathBuf>,
    /// Override the tma config dir holding the per-server `hooks-state-<server>.toml` (env
    /// `TMA_CONFIG_DIR`).
    #[arg(long = "config-dir", value_name = "DIR")]
    pub(crate) config_dir: Option<PathBuf>,
    /// Override where the `tma-hook` wrapper is written (env `TMA_WRAPPER_PATH`).
    #[arg(long = "wrapper-path", value_name = "PATH")]
    pub(crate) wrapper_path: Option<PathBuf>,
    /// Override where the OpenCode plugin is written (test isolation; env `TMA_OPENCODE_PLUGIN`).
    #[arg(long = "opencode-plugin", value_name = "PATH")]
    pub(crate) opencode_plugin: Option<PathBuf>,
    /// Override Codex's `config.toml` path (test isolation; env `TMA_CODEX_CONFIG`). Defaults to
    /// `$CODEX_HOME/config.toml`, else `~/.codex/config.toml`.
    #[arg(long = "codex-config", value_name = "PATH")]
    pub(crate) codex_config: Option<PathBuf>,
    /// Override Codex's `hooks.json` path (test isolation; env `TMA_CODEX_HOOKS`). Defaults to
    /// `$CODEX_HOME/hooks.json`, else `~/.codex/hooks.json`.
    #[arg(long = "codex-hooks", value_name = "PATH")]
    pub(crate) codex_hooks: Option<PathBuf>,
    /// Override Cursor's `hooks.json` path (test isolation; env `TMA_CURSOR_HOOKS`). Defaults to
    /// `~/.cursor/hooks.json`.
    #[arg(long = "cursor-hooks", value_name = "PATH")]
    pub(crate) cursor_hooks: Option<PathBuf>,
    /// Override Cursor's `cli-config.json` path holding the statusLine context shim (test isolation;
    /// env `TMA_CURSOR_CLI_CONFIG`). Defaults to `~/.cursor/cli-config.json`.
    #[arg(long = "cursor-cli-config", value_name = "PATH")]
    pub(crate) cursor_cli_config: Option<PathBuf>,
    /// Override pi's extension file path (test isolation; env `TMA_PI_EXTENSION`). Defaults to
    /// `$PI_CODING_AGENT_DIR/extensions/tma.js`, else `~/.pi/agent/extensions/tma.js`.
    #[arg(long = "pi-extension", value_name = "PATH")]
    pub(crate) pi_extension: Option<PathBuf>,
}

/// Args for `tma install-keys`. Mirrors the `install-hooks` flag naming (`--uninstall`/`--check`/
/// `--yes`); `--conf` overrides which tmux config gets the `source-file` line.
#[derive(clap::Args)]
pub(crate) struct InstallKeysArgs {
    /// Remove tma's keybindings (the managed file and the marked `source-file` line).
    #[arg(long)]
    pub(crate) uninstall: bool,
    /// Verify the keybindings are installed and current; report drift.
    #[arg(long)]
    pub(crate) check: bool,
    /// Also bind the clickable status-line segments (needs `set -g mouse on`, which tma never sets).
    /// With `--check`, require them.
    #[arg(long)]
    pub(crate) mouse: bool,
    /// Omit the `run-shell` line that starts the event-hub daemon for every tmux server that
    /// sources the file. Written by default; with `--check`, stop requiring it.
    #[arg(long = "no-daemon")]
    pub(crate) no_daemon: bool,
    /// Apply without the interactive diff confirmation (scripts, tests).
    #[arg(long)]
    pub(crate) yes: bool,
    /// The tmux config to mark with the `source-file` line. Defaults to the first tmux config
    /// that exists: `~/.tmux.conf`, `$XDG_CONFIG_HOME/tmux/tmux.conf`, `~/.config/tmux/tmux.conf`.
    #[arg(long, value_name = "PATH")]
    pub(crate) conf: Option<PathBuf>,
    /// Override the tma config dir holding the managed `tmux.conf` (test isolation; env
    /// `TMA_CONFIG_DIR`). Defaults to `~/.config/tma`.
    #[arg(long = "config-dir", value_name = "DIR")]
    pub(crate) config_dir: Option<PathBuf>,
}

/// Args for `tma daemon [--ensure]`. The target server comes from the global
/// `--socket-name`; `--manifest-dir` is forwarded to a spawned daemon for test isolation.
#[derive(clap::Args)]
pub(crate) struct DaemonCmdArgs {
    /// Spawn a detached daemon if none is running for this server, then exit 0 (idempotent).
    #[arg(long)]
    pub(crate) ensure: bool,
    /// INTERNAL/TEST: write the control-pool introspection status (membership, `-F` probe
    /// verdict, sweep interval, edge + recovery counts) to this file. No effect on behavior.
    #[arg(long, hide = true, value_name = "PATH")]
    pub(crate) status_file: Option<std::path::PathBuf>,
    /// INTERNAL/TEST: force the `-F` behavior probe into the deliberately-useless cross-session
    /// configuration so the faster-sweep degrade path is exercisable.
    #[arg(long, hide = true)]
    pub(crate) probe_cross_session: bool,
    /// INTERNAL/TEST: override the reconciliation-sweep cadence (milliseconds) so the sweep
    /// acceptance runs deterministically fast. No effect on the on-demand capture path.
    #[arg(long, hide = true, value_name = "MS")]
    pub(crate) sweep_ms: Option<u64>,
    /// INTERNAL: intermediate detach stage (launcher-set). Re-execs the daemon detached, then exits.
    #[arg(long = "detach-stage2", hide = true)]
    pub(crate) detach_stage2: bool,
    /// INTERNAL: detached daemon (intermediate-set). Triggers the startup `setsid`.
    #[arg(long = "detach-session", hide = true)]
    pub(crate) detach_session: bool,
}

/// Args for the internal `tma clear-attention <pane>` command.
#[derive(clap::Args)]
pub(crate) struct ClearAttentionArgs {
    /// The pane whose attention flag to clear (the tmux hook passes `#{hook_pane}`).
    pub(crate) pane: String,
}

/// Args for the internal `tma supervise` command. All fields are set by the broker's detach
/// spawn; the `TMA_*` context env is inherited so the supervised command sees it.
#[derive(clap::Args)]
pub(crate) struct SuperviseArgs {
    /// The target agent pane.
    #[arg(long)]
    pub(crate) pane: String,
    /// The held lock's nonce (the supervisor rewrites the value with its own pid, keeping this).
    #[arg(long)]
    pub(crate) nonce: String,
    /// The held lock's absolute expiry in epoch ms (preserved across the pid rewrite).
    #[arg(long = "expiry-ms")]
    pub(crate) expiry_ms: u64,
    /// The action name (the lock's name field and the completion payload's `action`).
    #[arg(long)]
    pub(crate) name: String,
    /// The resolved agent (the completion payload's `agent`).
    #[arg(long)]
    pub(crate) agent: String,
    /// The `sh -c` command string to run detached.
    #[arg(long)]
    pub(crate) command: String,
    /// The wall-clock deadline in ms; the process group is killed at it.
    #[arg(long = "detach-timeout-ms")]
    pub(crate) detach_timeout_ms: u64,
    /// The completion notify command (forwarded config `notify.command`); `TMA_NOTIFY_CMD` overrides.
    #[arg(long = "notify-command")]
    pub(crate) notify_command: Option<String>,
}

/// Args for the internal `tma event` bridge (built by the `tma-hook` wrapper).
#[derive(clap::Args)]
pub(crate) struct EventCmdArgs {
    /// Agent name (must match a bundled/user manifest to be mapped).
    #[arg(long)]
    pub(crate) agent: String,
    /// Hook event name (e.g. `Notification`, `SessionStart`), or `context` for the telemetry intake.
    #[arg(long)]
    pub(crate) kind: String,
    /// Target pane id (the statusline shim passes `$TMUX_PANE`). Falls back to the `$TMUX_PANE`
    /// env. Only consulted by the `context` intake.
    #[arg(long)]
    pub(crate) pane: Option<String>,
    /// Payload source: `-` for stdin, a path, or omitted for none.
    #[arg(long)]
    pub(crate) payload: Option<String>,
}

/// The shared selector: one partition vocabulary across every read surface (`ls`, `status`, `jump`,
/// `wait`, `subscribe`, `watch`). Flattened into each surface's args and converted once by
/// [`SelectorArgs::selector`], so a flag means the same thing everywhere.
///
/// Filtering is display-only and happens strictly AFTER the cycle: a filtered invocation still
/// stamps every pane an unfiltered one would, so a filtered ambient driver keeps hidden panes fresh.
#[derive(clap::Args, Clone)]
pub(crate) struct SelectorArgs {
    /// Only agents in this tmux session (exact name).
    #[arg(long, value_name = "NAME")]
    pub(crate) session: Option<String>,
    /// Only agents whose pane resolves to this git repo (the label surfaces render, so linked
    /// worktrees match their origin's name). A pane in no repo never matches.
    #[arg(long, value_name = "NAME")]
    pub(crate) repo: Option<String>,
    /// Only agents on this branch (the literal `HEAD` when detached).
    #[arg(long, value_name = "NAME")]
    pub(crate) branch: Option<String>,
    /// Only agents with this manifest name (e.g. `claude`).
    #[arg(long, value_name = "NAME")]
    pub(crate) agent: Option<String>,
    /// Only agents in one of these states, comma-separated: `idle`, `working`, `blocked`,
    /// `unknown`, and `done` (idle + attention, the finished-and-unreviewed surface).
    #[arg(long, value_name = "STATES", value_parser = parse_state_filter)]
    pub(crate) state: Option<StateSet>,
}

/// The parsed `--state` value. A newtype rather than a bare `Vec`, because clap's derive reads a
/// `Vec` field as one value per occurrence while this parser consumes the whole comma-separated
/// list in one go (the shape `--until` already uses).
#[derive(Clone, Debug)]
pub(crate) struct StateSet(Vec<tma_core::StateToken>);

impl SelectorArgs {
    pub(crate) fn selector(&self) -> tma_core::Selector {
        tma_core::Selector {
            session: self.session.clone(),
            repo: self.repo.clone(),
            branch: self.branch.clone(),
            agent: self.agent.clone(),
            state: self.state.clone().map(|s| s.0).unwrap_or_default(),
        }
    }
}

/// clap value parser for `--state`: the `--until` grammar (comma-separated, `done` included). An
/// all-empty value is a usage error rather than a silently ignored flag.
fn parse_state_filter(s: &str) -> Result<StateSet, String> {
    let states = cli_support::parse_states(s, "--state")?;
    if states.is_empty() {
        return Err(format!(
            "--state needs at least one state ({})",
            tma_core::StateToken::VOCABULARY
        ));
    }
    Ok(StateSet(states))
}

#[derive(clap::Args)]
pub(crate) struct JumpArgs {
    /// Jump to the next agent that wants you: blocked first (longest-blocked first), then
    /// finished-unreviewed, advancing from the current pane and wrapping.
    #[arg(long, group = "target")]
    pub(crate) attention: bool,
    /// Jump to the longest-blocked agent.
    #[arg(long, group = "target")]
    pub(crate) blocked: bool,
    /// Jump to the next agent after the current pane (session → window → pane order).
    #[arg(long, group = "target")]
    pub(crate) next: bool,
    /// Return one step along the trail (the previous jump's origin).
    #[arg(long, group = "target")]
    pub(crate) back: bool,
    /// Return to the oldest recorded origin (the bottom of the trail) and clear the trail.
    #[arg(long, group = "target")]
    pub(crate) home: bool,
    /// Jump to this pane id (what the `--menu` entries fire); clears its attention flag.
    #[arg(long, group = "target", value_name = "ID")]
    pub(crate) pane: Option<String>,
    /// Render a tmux `display-menu` of every agent, each entry jumping to that pane.
    #[arg(long, group = "target")]
    pub(crate) menu: bool,
    /// Scope the candidate agents (ignored by `--back`/`--home`, which replay the trail, and by
    /// `--pane`, which names its target).
    #[command(flatten)]
    pub(crate) selector: SelectorArgs,
}

/// Args for `tma wait`. `--pane`/`--any`/`--all`/`--count` are mutually exclusive targets; with none
/// of them the selector's `--agent` is the target. `--pane` rejects the selector flags. The global
/// flags drive the tier-2 poll cycle.
#[derive(clap::Args)]
#[command(
    group = clap::ArgGroup::new("wait_target").args(["pane", "any", "all", "count"]),
    long_about = "Block until the target reaches one of --until's states, then print the matched \
row(s) and exit. A tier-2 poll loop (immediate first cycle, then ~1 s ticks with config + manifest \
hot-reload); level-triggered, so an already-in-state target returns immediately.\n\n\
--agent pins to the first pane it observes: after that it behaves as --pane on that id (a vanish is \
exit 3), and ambiguity is an error ONLY when the first observation already matches >1 pane, so a \
same-named pane appearing mid-wait never flips a running wait to an error. --any never pins and \
keeps waiting on a vanish.\n\n\
--all is a barrier over the fleet in scope: it pins its membership at the first observation (a pane \
launched mid-wait never joins), needs every member in a target state at once, and ends at exit 3 if \
a member's pane dies. --count <N> is a quorum instead: it re-reads the scope every cycle and \
returns once N panes are in a target state, ignoring departures.\n\n\
--since <EPOCH_MS> requires the satisfying state to have BEGUN after that timestamp, which is how a \
supervisor loop avoids re-satisfying on the episode it just acted on.\n\n\
--pane on a pane that exists but is not (yet) an agent blocks forever by design (the agent may \
launch later); a one-time stderr hint flags a likely typo without breaking scripts. Once a watched \
pane HAS carried an agent, losing that row while the pane lives is the agent dying: exit 4, naming \
the pane, rather than a timeout that says nothing.\n\n\
Exit codes:\n  \
0    a target state was observed (the row(s) on stdout; --json for a schema-1 object/document)\n  \
124  timed out (--timeout elapsed); nothing on stdout\n  \
3    a watched pane vanished while waiting (a --pane, a pinned --agent, or an --all member)\n  \
4    the agent died while its pane lived on (same targets as 3)\n  \
2    usage error (bad --until token, an invalid target combination, or --all with an empty scope)\n  \
1    a generic runtime failure (ambiguous --agent at first observation, or no tmux server)"
)]
pub(crate) struct WaitArgs {
    /// Wait on this specific tmux pane id (e.g. `%5`). Its disappearance while waiting is exit 3.
    #[arg(long, value_name = "ID", conflicts_with = "agent")]
    pub(crate) pane: Option<String>,
    /// Wait on any agent pane in scope; the first to reach a target state (in surface-sort order) wins.
    #[arg(long)]
    pub(crate) any: bool,
    /// Barrier: wait until EVERY agent pane in scope is in a target state. Membership pins at the
    /// first observation; an empty scope is a usage error (exit 2) and a member's death is exit 3.
    #[arg(long)]
    pub(crate) all: bool,
    /// Quorum: wait until at least N agent panes in scope are in a target state. Membership is
    /// re-read every cycle, so panes may appear or leave under it.
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
    pub(crate) count: Option<u32>,
    /// The scope for `--agent`/`--any`, evaluated at the first observation. Its `--agent` is also
    /// the by-name target: `wait` pins to the first pane observed, then behaves as `--pane` on it
    /// (a vanish is exit 3); matching more than one in-scope pane at that FIRST observation is an
    /// error suggesting `--pane`, never a silent first-match.
    #[command(flatten)]
    pub(crate) selector: SelectorArgs,
    /// The state(s) to wait for, comma-separated: `idle`, `working`, `blocked`, `unknown`, and
    /// `done` (idle + attention, the finished-and-unreviewed surface). At least one required;
    /// `wait` returns as soon as a cycle observes the target in ANY of them.
    #[arg(long, value_name = "STATES", value_parser = wait::parse_until)]
    pub(crate) until: wait::UntilSet,
    /// Only a state that BEGAN after this epoch-ms timestamp satisfies (the row's `since_ms` must
    /// be strictly greater). The escape hatch from level-triggering for supervisor loops.
    #[arg(long, value_name = "EPOCH_MS")]
    pub(crate) since: Option<u64>,
    /// Give up after this many seconds and exit 124 (the `timeout(1)` convention). Absent waits
    /// forever; compose with `timeout(1)` for an external belt.
    #[arg(long, value_name = "SECS")]
    pub(crate) timeout: Option<u64>,
    /// Emit the matched row as one schema-1 JSON object (same keys as an `ls --json` row) instead
    /// of the tab-separated line; `--all`/`--count` emit the schema-1 `agents` document instead.
    #[arg(long)]
    pub(crate) json: bool,
}

/// Args for `tma act`. One verb, three modes: fire `<name>`, `--list`, or `--menu`. Target
/// resolution mirrors `wait` (`--pane`, a unique selector match, or the current pane), with `--all`
/// fanning out over every match; the mode flags are mutually exclusive with the fire flags.
#[derive(clap::Args)]
pub(crate) struct ActArgs {
    /// The action to fire (omit with `--list` / `--menu`).
    pub(crate) name: Option<String>,
    /// Target this pane id (e.g. `%5`); defaults to the current pane inside tmux.
    #[arg(long, value_name = "ID", conflicts_with_all = ["agent", "all"])]
    pub(crate) pane: Option<String>,
    /// Scope the target. `--agent <NAME>` alone must resolve to exactly one pane (ambiguous is an
    /// error, like `wait --agent`); with `--all` the whole selection is the target set.
    #[command(flatten)]
    pub(crate) selector: SelectorArgs,
    /// Fire on EVERY pane the selector matches, one after another (`--yes` covers the batch).
    #[arg(long, conflicts_with_all = ["list", "menu"])]
    pub(crate) all: bool,
    /// Print the resolved targets and each one's gate verdict; execute nothing.
    #[arg(long, conflicts_with_all = ["list", "menu"])]
    pub(crate) dry_run: bool,
    /// Pass a value to an `exec` action's command, as environment only (`TMA_ARG`, plus
    /// `TMA_ARG_1..N` and `TMA_ARG_COUNT` when repeated); never interpolated into the command
    /// string. A `keys` action takes none (its sequence is manifest-static), so `--arg` alongside
    /// one is a usage error.
    #[arg(long = "arg", value_name = "VALUE", conflicts_with_all = ["list", "menu"])]
    pub(crate) args: Vec<String>,
    /// Skip the `when` gate only (never `requires`, never the lock).
    #[arg(long, conflicts_with_all = ["list", "menu"])]
    pub(crate) force: bool,
    /// Satisfy a `confirm` action non-interactively.
    #[arg(long, conflicts_with_all = ["list", "menu"])]
    pub(crate) yes: bool,
    /// Emit schema-1 JSON: the fire result object, or the `--list` document.
    #[arg(long, conflicts_with = "menu")]
    pub(crate) json: bool,
    /// Enumerate actions; with `--pane`, include each one's fireability verdict.
    #[arg(long, conflicts_with_all = ["menu", "name", "agent"])]
    pub(crate) list: bool,
    /// Render a tmux `display-menu` of the currently-fireable actions.
    #[arg(long, conflicts_with_all = ["list", "name", "agent"])]
    pub(crate) menu: bool,
}

/// Args for `tma watch`. The invoking tmux client comes from the global `--client`
/// flag (bindings pass `--client "#{client_name}"`); absent falls back to targetless best-effort.
#[derive(clap::Args)]
pub(crate) struct WatchArgs {
    /// Open directly in the full-width status table (preview hidden) when the pane is wide enough;
    /// `p` toggles back to the preview at runtime. A narrow pane still falls back to the list.
    #[arg(long)]
    pub(crate) table: bool,
    /// Show only the agents in scope. The watcher's own poll cycle still refreshes every pane.
    #[command(flatten)]
    pub(crate) selector: SelectorArgs,
}

/// Args for `tma mute`. Target resolution mirrors `act`: `--pane` names one, the selector mutes
/// every pane it matches, and with neither the current pane is the target.
#[derive(clap::Args)]
pub(crate) struct MuteArgs {
    /// Mute this pane id (e.g. `%5`); defaults to the current pane inside tmux.
    #[arg(long, value_name = "ID")]
    pub(crate) pane: Option<String>,
    /// Mute every pane in scope. Unlike `act`, no `--all` is needed: a mute is per-pane and
    /// idempotent, so fanning out costs nothing to undo.
    #[command(flatten)]
    pub(crate) selector: SelectorArgs,
    /// How long to stay muted: a number with an optional unit (`45s`, `30m`, `2h`, `1d`; a bare
    /// number is seconds). Without it the mute holds until `--clear`.
    #[arg(long = "for", value_name = "DURATION", value_parser = crate::mute::parse_duration)]
    pub(crate) for_ms: Option<u64>,
    /// Lift the mute on the matched panes (unsets `@agent_mute_until`).
    #[arg(long)]
    pub(crate) clear: bool,
}

#[derive(clap::Args)]
pub(crate) struct LsArgs {
    /// Emit JSON (`"schema": 1`) instead of tab-separated lines.
    #[arg(long)]
    pub(crate) json: bool,
    /// List only this pane id (e.g. `%5`), the single-row form. Prints nothing (exit 0) when the
    /// pane carries no agent.
    #[arg(long, value_name = "ID")]
    pub(crate) pane: Option<String>,
    #[command(flatten)]
    pub(crate) selector: SelectorArgs,
}

/// Args for `tma subscribe`: a long-running stream of `ls --json` documents, one per line.
#[derive(clap::Args)]
pub(crate) struct SubscribeArgs {
    /// Emit one `ls --json` (`"schema": 1`) document per line. Required (the only emission today).
    #[arg(long)]
    pub(crate) json: bool,
    /// Poll cadence in seconds when no daemon is present, and the degrade cadence when one dies
    /// (default 1). Push mode delivers on the daemon's edge, so this only bounds the daemonless path.
    #[arg(long, value_name = "SECS", default_value_t = 1)]
    pub(crate) interval: u64,
    /// Skip a poll-mode emission that would repeat the last document. Push mode already emits only
    /// on an edge, so the flag is a silent no-op there (and under `--events`).
    #[arg(long = "changes-only")]
    pub(crate) changes_only: bool,
    /// Emit one schema-1 edge record per state transition instead of snapshots. The first cycle
    /// establishes the baseline and emits nothing.
    #[arg(long)]
    pub(crate) events: bool,
    /// Emit only the agents in scope. Each document is the same schema-1 shape with a narrower
    /// `agents` array; the emission cadence is unchanged.
    #[command(flatten)]
    pub(crate) selector: SelectorArgs,
}

/// Args for `tma status`: the counts, optionally scoped and in one of four forms. A per-session line
/// is `#(tma status --session #{session_name})`.
#[derive(clap::Args)]
pub(crate) struct StatusArgs {
    /// Output form. `tmux` (default) is the status-line one-liner; `plain` drops the color codes for
    /// an external bar; `json` and `prom` are the machine forms.
    #[arg(long, value_enum, default_value_t = StatusFormat::Tmux)]
    pub(crate) format: StatusFormat,
    #[command(flatten)]
    pub(crate) selector: SelectorArgs,
}

/// `tma status --format`: one set of counts, four renderings. Every variant runs the same cycle over
/// the same selected rows, so which one you poll never changes what gets stamped.
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum StatusFormat {
    /// The tmux status-line one-liner: glyphs with `#[fg=]` styling (the default).
    Tmux,
    /// The same glyphs and counts with no color codes (starship, sketchybar, waybar).
    Plain,
    /// A schema-1 `{"schema":1,"counts":{…}}` document.
    Json,
    /// Prometheus text exposition, for a node_exporter textfile collector.
    Prom,
}

#[derive(clap::Args)]
pub(crate) struct DoctorArgs {
    /// Emit JSON (`"schema": 1`) instead of the human-readable report.
    #[arg(long)]
    pub(crate) json: bool,
    /// Exit 1 when the report carries a warning or a pane is below the tier its manifest
    /// supports, for CI gating. Without it doctor always exits 0 unless the server is unreachable.
    #[arg(long)]
    pub(crate) exit_code: bool,
}

#[derive(clap::Args)]
pub(crate) struct DebugArgs {
    #[command(subcommand)]
    pub(crate) command: DebugCommand,
}

/// Shared read-path options for the tmux-touching debug commands. The socket and manifest-dir
/// come from the global `Cli` flags (`--socket-name` / `--manifest-dir`).
#[derive(clap::Args)]
pub(crate) struct ReadOpts {
    /// tmux pane id (e.g. `%13`).
    pub(crate) pane: String,
}

// The internal `Stamp` harness carries many optional knobs, making its variant large; this
// is a CLI arg enum parsed once, never a hot path, so boxing the args is not worth it.
#[allow(clippy::large_enum_variant)]
#[derive(clap::Subcommand)]
pub(crate) enum DebugCommand {
    /// Redact a capture (paths, emails, and `--pattern` regexes) to stdout, preserving
    /// layout width, so it can be committed as a fixture.
    Redact {
        /// Capture file to redact.
        file: PathBuf,
        /// Extra regex to redact; repeatable.
        #[arg(long = "pattern", value_name = "REGEX")]
        pattern: Vec<String>,
    },
    /// Print exactly what the detector saw for a pane, in fixture format.
    Capture {
        #[command(flatten)]
        read: ReadOpts,
    },
    /// Run identity + rule engine + fold for a pane; print evidence, matched/failed rules,
    /// and the verdict. `--json` emits the versioned schema.
    Explain {
        #[command(flatten)]
        read: ReadOpts,
        /// Emit JSON (`"schema": 1`) instead of the human-readable form.
        #[arg(long)]
        json: bool,
    },
    /// Print the running daemon's recent state transitions (its in-memory ring). `--json` emits
    /// the versioned schema.
    Transitions {
        /// Emit JSON (`"schema": 1`) instead of the human-readable form.
        #[arg(long)]
        json: bool,
    },
    /// Fire the notify command a trigger resolves to against a representative payload, printing
    /// the command, the payload, and how it exited. Unlike a real fire this waits and shows stderr.
    NotifyTest {
        /// blocked | done | context_high.
        #[arg(long, default_value = "blocked")]
        trigger: String,
    },
    /// INTERNAL, UNSTABLE: apply a guarded stamp to a pane (the write adapter), for testing
    /// the server-side write guards directly. Not a public interface.
    Stamp(StampArgs),
}

/// Args for the internal `debug stamp` write-adapter harness.
#[derive(clap::Args)]
pub(crate) struct StampArgs {
    /// tmux pane id (e.g. `%13`).
    pub(crate) pane: String,
    /// publish | hold | remove.
    #[arg(long, default_value = "publish")]
    pub(crate) mode: String,
    #[arg(long)]
    pub(crate) state: Option<String>,
    #[arg(long)]
    pub(crate) detail: Option<String>,
    #[arg(long)]
    pub(crate) source: Option<String>,
    #[arg(long = "evidence-at")]
    pub(crate) evidence_at: Option<u64>,
    #[arg(long)]
    pub(crate) since: Option<u64>,
    #[arg(long = "stamped-at")]
    pub(crate) stamped_at: Option<u64>,
    #[arg(long)]
    pub(crate) hash: Option<u64>,
    #[arg(long)]
    pub(crate) pid: Option<u32>,
    #[arg(long)]
    pub(crate) name: Option<String>,
    #[arg(long)]
    pub(crate) attention: bool,
    #[arg(long = "episode-reset")]
    pub(crate) episode_reset: bool,
    /// unconditional | protect-hook | `carveout:<epoch>` | `refresh:<state>`.
    #[arg(long, default_value = "unconditional")]
    pub(crate) guard: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The global `--client` reads one canonical field before or after the subcommand: `tma --client
    /// X jump` and `tma jump --client X` must parse identically (a per-subcommand copy used to shadow
    /// it and mis-target the Enter-jump's `switch-client`).
    #[test]
    fn client_is_global_for_jump() {
        let pre = Cli::parse_from(["tma", "--client", "c0", "jump", "--blocked"]);
        let post = Cli::parse_from(["tma", "jump", "--blocked", "--client", "c0"]);
        assert_eq!(pre.client.as_deref(), Some("c0"));
        assert_eq!(pre.client, post.client);
        assert!(matches!(pre.command, Some(Command::Jump(_))));
        assert!(matches!(post.command, Some(Command::Jump(_))));
    }

    /// `tma watch` reads the client only from the global flag now (its local copy is gone).
    #[test]
    fn client_is_global_for_watch() {
        let pre = Cli::parse_from(["tma", "--client", "c1", "watch"]);
        let post = Cli::parse_from(["tma", "watch", "--client", "c1"]);
        assert_eq!(pre.client.as_deref(), Some("c1"));
        assert_eq!(pre.client, post.client);
        assert!(matches!(pre.command, Some(Command::Watch(_))));
    }

    /// The picker (no subcommand) reads the same global client field.
    #[test]
    fn client_is_global_for_picker() {
        let cli = Cli::parse_from(["tma", "--client", "c2"]);
        assert_eq!(cli.client.as_deref(), Some("c2"));
        assert!(cli.command.is_none());
    }

    /// The new jump directions parse into the same `target` group as the existing ones.
    #[test]
    fn jump_attention_and_home_parse() {
        for flag in ["--attention", "--blocked", "--next", "--back", "--home"] {
            let cli = Cli::parse_from(["tma", "jump", flag]);
            assert!(
                matches!(cli.command, Some(Command::Jump(_))),
                "`jump {flag}` parses"
            );
        }
        let attn = Cli::parse_from(["tma", "jump", "--attention"]);
        match attn.command {
            Some(Command::Jump(a)) => assert!(a.attention && !a.blocked && !a.home),
            _ => panic!("expected jump"),
        }
        let home = Cli::parse_from(["tma", "jump", "--home"]);
        match home.command {
            Some(Command::Jump(a)) => assert!(a.home && !a.back && !a.attention),
            _ => panic!("expected jump"),
        }
    }

    /// The one selector vocabulary is accepted by every read surface, and converts to the same core
    /// predicate wherever it is flattened.
    #[test]
    fn selector_flags_parse_on_every_read_surface() {
        let flags = [
            "--session",
            "s",
            "--repo",
            "r",
            "--branch",
            "b",
            "--agent",
            "a",
        ];
        for verb in ["ls", "status", "jump", "watch", "subscribe"] {
            let mut argv = vec!["tma", verb];
            argv.extend(flags);
            if verb == "subscribe" {
                argv.push("--json");
            }
            let cli = Cli::parse_from(&argv);
            let sel = match cli.command {
                Some(Command::Ls(a)) => a.selector.selector(),
                Some(Command::Status(a)) => a.selector.selector(),
                Some(Command::Jump(a)) => a.selector.selector(),
                Some(Command::Watch(a)) => a.selector.selector(),
                Some(Command::Subscribe(a)) => a.selector.selector(),
                _ => panic!("{verb} did not parse into its own subcommand"),
            };
            assert_eq!(sel.session.as_deref(), Some("s"), "{verb} --session");
            assert_eq!(sel.repo.as_deref(), Some("r"), "{verb} --repo");
            assert_eq!(sel.branch.as_deref(), Some("b"), "{verb} --branch");
            assert_eq!(sel.agent.as_deref(), Some("a"), "{verb} --agent");
            assert!(sel.needs_repo(), "{verb} selector reads the repo label");
        }
    }

    /// `--state` takes the `--until` comma-separated grammar, `done` included, and rejects an
    /// unknown or empty list at parse time (exit 2).
    #[test]
    fn state_filter_parses_the_until_grammar() {
        let cli = Cli::parse_from(["tma", "ls", "--state", "blocked,done"]);
        let sel = match cli.command {
            Some(Command::Ls(a)) => a.selector.selector(),
            _ => panic!("expected ls"),
        };
        assert_eq!(
            sel.state,
            vec![
                tma_core::StateToken::Closed(tma_core::AgentState::Blocked),
                tma_core::StateToken::Done
            ]
        );
        assert!(Cli::try_parse_from(["tma", "ls", "--state", "running"]).is_err());
        assert!(Cli::try_parse_from(["tma", "ls", "--state", ""]).is_err());
    }

    /// `wait` reads its by-name target from the same `--agent` the selector uses, so the flag has
    /// one meaning; `--pane` still excludes it.
    #[test]
    fn wait_agent_target_comes_from_the_selector() {
        let cli = Cli::parse_from(["tma", "wait", "--agent", "claude", "--until", "idle"]);
        match cli.command {
            Some(Command::Wait(a)) => {
                assert_eq!(a.selector.selector().agent.as_deref(), Some("claude"));
                assert!(a.pane.is_none() && !a.any);
            }
            _ => panic!("expected wait"),
        }
        assert!(
            Cli::try_parse_from(["tma", "wait", "--pane", "%1", "--agent", "c", "--until", "idle"])
                .is_err(),
            "the target group stays mutually exclusive"
        );
    }

    /// The fleet targets are mutually exclusive with the single-pane ones, ride alongside the
    /// selector (`--all --agent claude` is "every claude pane"), and `--count` refuses a vacuous 0.
    #[test]
    fn wait_fleet_targets_parse_and_stay_exclusive() {
        let cli = Cli::parse_from([
            "tma", "wait", "--all", "--agent", "claude", "--until", "idle", "--since", "1700",
        ]);
        match cli.command {
            Some(Command::Wait(a)) => {
                assert!(a.all && a.count.is_none());
                assert_eq!(a.selector.selector().agent.as_deref(), Some("claude"));
                assert_eq!(a.since, Some(1700));
            }
            _ => panic!("expected wait"),
        }
        for pair in [
            ["--all", "--any"],
            ["--all", "--count=2"],
            ["--any", "--count=2"],
        ] {
            assert!(
                Cli::try_parse_from(["tma", "wait", pair[0], pair[1], "--until", "idle"]).is_err(),
                "`wait {} {}` must conflict",
                pair[0],
                pair[1]
            );
        }
        assert!(
            Cli::try_parse_from(["tma", "wait", "--count", "0", "--until", "idle"]).is_err(),
            "a quorum of zero would be vacuous success"
        );
    }

    /// `act` reads its target from the shared selector, takes repeated `--arg` values, and keeps
    /// `--pane` exclusive with both `--agent` and the fan-out.
    #[test]
    fn act_takes_a_selector_all_and_repeated_args() {
        let cli = Cli::parse_from([
            "tma",
            "act",
            "queue",
            "--all",
            "--session",
            "work",
            "--arg",
            "one",
            "--arg",
            "two",
        ]);
        match cli.command {
            Some(Command::Act(a)) => {
                assert_eq!(a.name.as_deref(), Some("queue"));
                assert!(a.all);
                assert_eq!(a.selector.selector().session.as_deref(), Some("work"));
                assert_eq!(a.args, ["one", "two"]);
            }
            _ => panic!("expected act"),
        }
        assert!(
            Cli::try_parse_from(["tma", "act", "approve", "--pane", "%1", "--agent", "claude"])
                .is_err(),
            "a pane id and a by-name target conflict"
        );
        assert!(
            Cli::try_parse_from(["tma", "act", "approve", "--pane", "%1", "--all"]).is_err(),
            "a pane id and the fan-out conflict"
        );
    }

    /// `status --format` defaults to the tmux one-liner (so an existing `#(tma status)` is
    /// untouched), accepts the four documented forms, and rejects anything else at parse time.
    #[test]
    fn status_format_defaults_to_tmux_and_takes_the_four_forms() {
        let default = Cli::parse_from(["tma", "status"]);
        match default.command {
            Some(Command::Status(a)) => assert!(a.format == StatusFormat::Tmux),
            _ => panic!("expected status"),
        }
        for (flag, want) in [
            ("tmux", StatusFormat::Tmux),
            ("plain", StatusFormat::Plain),
            ("json", StatusFormat::Json),
            ("prom", StatusFormat::Prom),
        ] {
            let cli = Cli::parse_from(["tma", "status", "--format", flag]);
            match cli.command {
                Some(Command::Status(a)) => assert!(a.format == want, "--format {flag}"),
                _ => panic!("expected status"),
            }
        }
        assert!(Cli::try_parse_from(["tma", "status", "--format", "yaml"]).is_err());
    }

    /// `mute` takes the shared selector, parses `--for` into milliseconds at parse time (so a bad
    /// duration is exit 2 before any pane is touched), and reads a bare invocation as "no window".
    #[test]
    fn mute_parses_its_target_and_duration() {
        match Cli::parse_from(["tma", "mute", "--session", "work", "--for", "30m"]).command {
            Some(Command::Mute(a)) => {
                assert_eq!(a.selector.selector().session.as_deref(), Some("work"));
                assert_eq!(a.for_ms, Some(1_800_000));
                assert!(!a.clear && a.pane.is_none());
            }
            _ => panic!("expected mute"),
        }
        match Cli::parse_from(["tma", "mute", "--clear", "--pane", "%5"]).command {
            Some(Command::Mute(a)) => {
                assert!(a.clear && a.for_ms.is_none());
                assert_eq!(a.pane.as_deref(), Some("%5"));
            }
            _ => panic!("expected mute"),
        }
        // A bare `tma mute` is the current pane, muted until cleared.
        match Cli::parse_from(["tma", "mute"]).command {
            Some(Command::Mute(a)) => assert!(a.for_ms.is_none() && !a.clear && a.pane.is_none()),
            _ => panic!("expected mute"),
        }
        assert!(Cli::try_parse_from(["tma", "mute", "--for", "soon"]).is_err());
        assert!(Cli::try_parse_from(["tma", "mute", "--for", "0"]).is_err());
    }

    /// `ls --pane` is the single-row form and rides alongside the selector.
    #[test]
    fn ls_takes_a_pane_and_a_selector() {
        let cli = Cli::parse_from(["tma", "ls", "--json", "--pane", "%5", "--state", "idle"]);
        match cli.command {
            Some(Command::Ls(a)) => {
                assert!(a.json);
                assert_eq!(a.pane.as_deref(), Some("%5"));
                assert_eq!(a.selector.selector().state.len(), 1);
            }
            _ => panic!("expected ls"),
        }
    }

    /// The `target` group is mutually exclusive: any two direction flags conflict, exactly like the
    /// pre-existing `--blocked`/`--next`/`--back` exclusion.
    #[test]
    fn jump_directions_are_mutually_exclusive() {
        for pair in [
            ["--attention", "--blocked"],
            ["--attention", "--home"],
            ["--home", "--back"],
            ["--blocked", "--next"],
        ] {
            assert!(
                Cli::try_parse_from(["tma", "jump", pair[0], pair[1]]).is_err(),
                "`jump {} {}` must conflict",
                pair[0],
                pair[1]
            );
        }
        // `--menu` and `--pane` join the same group: they are targets, not modifiers of one.
        assert!(Cli::try_parse_from(["tma", "jump", "--menu", "--blocked"]).is_err());
        assert!(Cli::try_parse_from(["tma", "jump", "--pane", "%1", "--menu"]).is_err());
    }

    /// `jump --pane` names a target pane (the id the menu entries fire) and `--menu` is its own mode.
    #[test]
    fn jump_pane_and_menu_parse() {
        match Cli::parse_from(["tma", "jump", "--pane", "%5"]).command {
            Some(Command::Jump(a)) => {
                assert_eq!(a.pane.as_deref(), Some("%5"));
                assert!(!a.menu && !a.blocked);
            }
            _ => panic!("expected jump"),
        }
        match Cli::parse_from(["tma", "jump", "--menu"]).command {
            Some(Command::Jump(a)) => assert!(a.menu && a.pane.is_none()),
            _ => panic!("expected jump"),
        }
    }
}
