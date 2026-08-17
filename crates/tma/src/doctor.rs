//! `tma doctor`: report which tier each agent pane is running at, read-only (no writes, no config
//! touched; the daemon only gets a socket liveness probe). Per pane (identified via
//! [`identity::identify`]): hooks wired ([`install::diagnose_hooks`], shared with `install-hooks
//! --check`), daemon alive ([`ipc::daemon_status`], a tier-2 probe not a tier-3 import), last
//! evidence source+age, and the effective tier with its reason ([`derive_tier`]). Plus the
//! server-wide checks: the ambient driver (`@tma_last_poll` age, is anything running `tma status`?),
//! what that driver depends on (an attached client to run the `#()` job, `status` left on), the
//! middle-tier nudge (resident `tma watch` sidebars advertising `@tma_watch_pid`), and the two
//! halves of the clickable status segments (the `--mouse` bindings against the `mouse` option). `--json` emits the
//! additive-only `"schema": 1` document, matching `tma ls --json`; `--exit-code` turns the warnings
//! into a CI verdict ([`gate`]).

use std::path::PathBuf;
use std::process::ExitCode;

use tma_core::stamp::opt;
use tma_core::{AgentState, Provenance, ReadResult, StampedState};
use tma_runtime::{actions, identity, ipc, notify};

use crate::cli_support;
use crate::config::{AgentConfig, ApiSection, WindowsSection};
use crate::install::{self, HookWiring, TmuxHookState};
use crate::manifests::{LoadedManifest, ManifestFailure};
use crate::tmux::{self, Tmux, TmuxError};

mod render;

use render::{render_json, render_text};

/// Options for `tma doctor` (from the CLI + the loaded config).
pub(crate) struct DoctorOpts {
    pub json: bool,
    /// `--exit-code`: exit non-zero when the report carries a warning or a pane sits below the tier
    /// its manifest supports. Off by default, so plain `doctor` stays a report.
    pub exit_code: bool,
    pub server: tma_tmux::tmux::Server,
    pub manifest_dir: Option<PathBuf>,
    /// `[[agent]]` config: enable/disable + custom process-name maps — same set the poll
    /// surfaces load, so doctor sees the identical agent roster.
    pub agents: Vec<AgentConfig>,
    /// `[focus] events`: whether the opt-in `pane-focus-in` clear hook is expected, so the
    /// tmux-hook check reports the right desired set.
    pub focus_events: bool,
    /// `[telemetry.windows]`: the recognized-model names, so doctor can report a stamped
    /// `@agent_model` no entry names.
    pub windows: WindowsSection,
    /// `[api.<name>]` API config, so doctor can flag an OpenCode pane with a pending permission
    /// request but no resolvable endpoint.
    pub api: ApiSection,
}

// --- tier derivation (pure; unit-tested) -----------------------------------------

/// How an agent participates in the hook tier, distilled from [`HookWiring`]. `Wired`/`Partial`
/// put it on the hook path (some events flow); the other three keep it on the polling floor.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum HookClass {
    Wired,
    Partial,
    NotInstalled,
    Hookless,
    NoAdapter,
}

impl HookClass {
    fn from_wiring(w: &HookWiring) -> HookClass {
        match w {
            HookWiring::Wired => HookClass::Wired,
            HookWiring::Incomplete(_) => HookClass::Partial,
            HookWiring::NotInstalled => HookClass::NotInstalled,
            HookWiring::Hookless => HookClass::Hookless,
            HookWiring::NoAdapter => HookClass::NoAdapter,
        }
    }
}

/// Why an agent is not running at a higher tier (the pure verdict; the human string is formatted
/// at the edge from this plus the agent name and daemon state).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TierReason {
    /// Tier 3 — the daemon tier; nothing higher.
    DaemonTier,
    /// Tier 2 — hook events flow but no daemon is running (they direct-stamp).
    NoDaemon,
    /// Tier 1 — a hook-capable agent whose hooks are not (fully) installed.
    HooksNotInstalled,
    /// Tier 1 — the manifest is hookless (screen-detection only).
    Hookless,
    /// Tier 1 — hook-capable manifest, but tma has no installer adapter for it.
    NoAdapter,
}

/// The effective tier and the reason it is not higher.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Tier {
    level: u8,
    reason: TierReason,
}

/// Pure 3/2/1 tier derivation, keyed to how this agent's state stays fresh: hook events + live
/// daemon ⇒ tier 3; hook events, no daemon ⇒ tier 2 (`tma event` direct-stamps); no hook events ⇒
/// tier 1 (the floor), the reason keyed to why. A hookless agent is tier 1 even under a live daemon
/// (it emits no hook events), though the formatted reason notes the daemon's fallback capture.
fn derive_tier(hooks: HookClass, daemon_alive: bool) -> Tier {
    let on_hook_path = matches!(hooks, HookClass::Wired | HookClass::Partial);
    if on_hook_path {
        if daemon_alive {
            Tier {
                level: 3,
                reason: TierReason::DaemonTier,
            }
        } else {
            Tier {
                level: 2,
                reason: TierReason::NoDaemon,
            }
        }
    } else {
        let reason = match hooks {
            HookClass::NotInstalled => TierReason::HooksNotInstalled,
            HookClass::Hookless => TierReason::Hookless,
            HookClass::NoAdapter => TierReason::NoAdapter,
            HookClass::Wired | HookClass::Partial => unreachable!("handled above"),
        };
        Tier { level: 1, reason }
    }
}

/// The human-readable "reason it is not higher" for a tier, `None` at tier 3. Appends a note when a
/// daemon is running but this agent is still tier 1, so the operator is not misled.
fn tier_reason_str(tier: Tier, agent: &str, daemon_alive: bool) -> Option<String> {
    let note = |s: String| -> String {
        if daemon_alive {
            format!("{s}; a daemon is running and provides fallback capture (tier 3)")
        } else {
            s
        }
    };
    match tier.reason {
        TierReason::DaemonTier => None,
        TierReason::NoDaemon => Some(
            "daemon not running (events direct-stamp; run `tma daemon --ensure` for the daemon tier)"
                .to_string(),
        ),
        TierReason::HooksNotInstalled => Some(note(format!(
            "hooks not installed for {agent} (run `tma install-hooks {agent}`)"
        ))),
        TierReason::Hookless => Some(note(format!(
            "{agent} is hookless — screen-detection only, so no hook tier"
        ))),
        TierReason::NoAdapter => Some(note(format!(
            "no install-hooks adapter for {agent}; wire it by hand"
        ))),
    }
}

/// The tier a pane can reach with no daemon running: 2 for an agent whose manifest declares hooks
/// and has an installer adapter, 1 for the rest (hookless or unwireable, so the floor is all there
/// is). `--exit-code` gates on falling below this, which keeps a missing daemon — a runtime choice,
/// not a misconfiguration — out of the verdict.
fn expected_tier(hooks: HookClass) -> u8 {
    match hooks {
        HookClass::Wired | HookClass::Partial | HookClass::NotInstalled => 2,
        HookClass::Hookless | HookClass::NoAdapter => 1,
    }
}

/// Whether a running daemon's recorded build version matches this CLI's. `None` when there is
/// nothing to compare: no daemon, or a lock file written before the version was recorded.
fn daemon_version_matches(daemon_version: Option<&str>) -> Option<bool> {
    daemon_version.map(|v| v == ipc::VERSION)
}

// --- tmux version floor (pure; unit-tested) ---------------------------------------

/// The tmux release tma is developed and tested against. Older servers load configs in a
/// different order and expand `display-popup` differently, so the keybindings and the picker can
/// misbehave in ways nothing else in the report explains.
pub(crate) const MIN_TMUX_VERSION: (u32, u32) = (3, 6);

/// [`MIN_TMUX_VERSION`] as tma spells it in the report and the docs.
pub(crate) const MIN_TMUX_VERSION_STR: &str = "3.6";

/// Parse a tmux `#{version}` into `(major, minor)`. tmux spells releases `3.6` and `3.6a` (the
/// patch letter) and pre-releases `next-3.7`; anything else (a distro string, a git build) is
/// `None`.
fn parse_tmux_version(version: &str) -> Option<(u32, u32)> {
    let trimmed = version.trim();
    let v = trimmed.strip_prefix("next-").unwrap_or(trimmed);
    let (major, rest) = v.split_once('.')?;
    let minor: String = rest.chars().take_while(char::is_ascii_digit).collect();
    Some((major.parse().ok()?, minor.parse().ok()?))
}

/// Whether the server is older than [`MIN_TMUX_VERSION`]. An absent or unparseable version is
/// never a warning: reading "old" out of a string tma does not understand would cry wolf.
fn tmux_below_min(version: Option<&str>) -> bool {
    version
        .and_then(parse_tmux_version)
        .is_some_and(|v| v < MIN_TMUX_VERSION)
}

// --- gathered report -------------------------------------------------------------

/// One agent pane's full diagnosis.
struct AgentReport {
    pane: String,
    agent: String,
    locator: String,
    /// The stamped state, `None` when the pane carries no `@agent_*` stamp yet.
    state: Option<AgentState>,
    /// Provenance of the current state (`@agent_source`), `None` when unstamped.
    source: Option<Provenance>,
    /// Age of the evidence behind the current state (`now - @agent_evidence_at`), `None` when
    /// unstamped or the evidence timestamp is absent.
    evidence_age_ms: Option<u64>,
    wiring: HookWiring,
    tier: Tier,
    /// The stamped `@agent_model`, `None` when the file-tail intake read no model. Best-effort
    /// label, never load-bearing.
    model: Option<String>,
    /// Whether `[telemetry.windows]` names `model`, `None` when there is no model to check.
    /// Bookkeeping, not a warning: no gauge reads that table (every context channel computes its
    /// percent from a window its own payload carries), so an unrecognized model costs nothing.
    window_covered: Option<bool>,
    /// The API endpoint verdict for a pane with a pending `@agent_permission_request`:
    /// `Some(true)` request + resolvable endpoint, `Some(false)` request but no endpoint (the
    /// warning), `None` when no request is pending (nothing to check).
    endpoint_ok: Option<bool>,
    /// This pane registered through a hook (`@agent_session` stamped) but its current evidence came
    /// from capture: the hooks stopped firing for it and it has fallen back to the polling floor.
    hook_demoted: bool,
}

/// One action-manifest problem `tma doctor` surfaces: a parse/stem/name/`requires` load
/// error, or a dangling agent reference (an agent named in `[keys]` or `agents` with no known
/// manifest). Later batches append telemetry-channel checks here.
struct ActionLint {
    file: String,
    problem: String,
}

/// A pane whose foreground is a nested multiplexer client (tmux/zellij/screen). Its agents belong
/// to the inner server, so tma running out here can neither see nor stamp them.
struct NestedMultiplexer {
    pane: String,
    locator: String,
    command: &'static str,
}

/// A pane whose foreground is a remote shell (ssh/mosh/docker/kubectl). The process walk stops at
/// the boundary and no capture crosses it, so an agent on the far side is visible here only through
/// hooks that reach this tmux socket.
struct RemotePane {
    pane: String,
    locator: String,
    command: &'static str,
    /// The pane still carries `@agent_*` options: a stamp from before the boundary went up, held
    /// because nothing can refresh it. Named so it is not read as live state.
    stamped: bool,
}

/// A pane the user took out of detection with `@agent_ignore`. Listed so the setting is
/// discoverable: a pane that stopped reporting because someone set the option a month ago is
/// otherwise indistinguishable from one tma never recognized.
struct IgnoredPane {
    pane: String,
    locator: String,
    /// The option's value, which users pick freely (`1`, `dev server`, a ticket id).
    value: String,
}

/// A pane whose `@agent_*` options do not decode. Every read path treats a corrupt stamp as no
/// stamp at all, so the pane looks never-stamped forever with nothing to explain why; this is the
/// one place the option and the value that broke it get named.
struct StampLint {
    pane: String,
    locator: String,
    problem: String,
}

/// One agent manifest the loader skipped. Every other surface warns about these on stderr and
/// carries on; doctor is where the file and its parse error are actually readable.
struct ManifestLint {
    file: String,
    problem: String,
}

/// The comm length both `ps` sources truncate to: macOS libproc (`MAXCOMLEN`) and the Linux kernel's
/// `comm` field both cap at 15 characters, so a longer `process_names` entry can never match.
const COMM_MAX: usize = 15;

/// A manifest `process_names` entry no pane can ever match ([`unreachable_process_names`]).
struct ProcessNameLint {
    agent: String,
    name: String,
}

/// The entries in one `process_names` list that a truncated comm can never match: longer than
/// [`COMM_MAX`] with no sibling spelling the same name cut to that width (codex is the worked
/// example — `codex-aarch64-a` sits beside `codex` precisely because of this).
fn unreachable_process_names(names: &[String]) -> Vec<&String> {
    names
        .iter()
        .filter(|name| {
            name.chars().count() > COMM_MAX
                && !names
                    .iter()
                    .any(|n| n.chars().eq(name.chars().take(COMM_MAX)))
        })
        .collect()
}

fn diagnose_process_names(manifests: &[LoadedManifest]) -> Vec<ProcessNameLint> {
    manifests
        .iter()
        .flat_map(|lm| {
            unreachable_process_names(&lm.manifest.identity.process_names)
                .into_iter()
                .map(|name| ProcessNameLint {
                    agent: lm.name.clone(),
                    name: name.clone(),
                })
        })
        .collect()
}

/// The whole `tma doctor` report for one server.
struct Report {
    /// The server's `#{version}`, `None` when the read failed. Reported so a server below
    /// [`MIN_TMUX_VERSION`] is named before its symptoms are blamed on tma.
    tmux_version: Option<String>,
    daemon_alive: bool,
    daemon_socket: PathBuf,
    /// Server exists but is not reachable for the daemon key (rare) ⇒ `None`.
    daemon_known: bool,
    /// The running daemon's build version from its lock file, `None` when nothing is running or the
    /// lock predates version recording.
    daemon_version: Option<String>,
    /// `@tma_last_poll` age in ms, `None` when nothing has polled (no ambient driver).
    ambient_poll_age_ms: Option<u64>,
    /// Attached clients. Zero means the `#()` status jobs never run, so the ambient floor is dead.
    attached_clients: usize,
    /// The global `status` option: `false` kills both the `#()` driver and `display-message`.
    status_enabled: bool,
    /// The opt-in clickable-status bindings are installed (`tma install-keys --mouse`).
    mouse_bindings: bool,
    /// The global `mouse` option, which those bindings need for a click to reach tmux at all.
    mouse_enabled: bool,
    /// Count of resident `tma watch` sidebars advertising `@tma_watch_pid` (the middle tier):
    /// the panes the focus-change hook nudges. `0` when none are running.
    watch_sidebars: usize,
    tmux_hooks: Vec<(String, TmuxHookState)>,
    wrapper_path: PathBuf,
    wrapper_present: bool,
    agents: Vec<AgentReport>,
    /// Count of action manifests that loaded cleanly (bundled + user dir).
    action_ok: usize,
    /// Action-manifest problems (load errors + dangling agent references).
    action_issues: Vec<ActionLint>,
    /// Count of agent manifests in the effective set (bundled + user overrides).
    manifest_ok: usize,
    /// Agent manifests the loader skipped, with the error that skipped them.
    manifest_issues: Vec<ManifestLint>,
    /// `process_names` entries longer than the 15-char comm truncation with no truncated sibling.
    process_name_issues: Vec<ProcessNameLint>,
    /// Panes running a nested multiplexer client, reported with the "run tma there" hint.
    nested: Vec<NestedMultiplexer>,
    /// Panes behind a remote shell, reported with what it takes for an agent there to be seen.
    remote: Vec<RemotePane>,
    /// Panes carrying `@agent_ignore`, so the opt-out is visible where its effect is.
    ignored: Vec<IgnoredPane>,
    /// Panes whose `@agent_*` options do not decode, with the option and value that broke them.
    stamp_issues: Vec<StampLint>,
    /// The `ps` walk failed (a stripped PATH, a sandbox that blocks the system `ps`), `None` when
    /// it ran. Process identity is then unavailable, so the pane rows below hold only what a hook
    /// registration names — the rest of the report is unaffected and still worth printing.
    process_walk_error: Option<String>,
    /// The last notify-command failure recorded by a fire, `None` when the marker is absent (nothing
    /// has failed, or the last fire was clean). A real fire discards its command's output, so this
    /// marker is the only place a broken sink surfaces.
    notify_failure: Option<notify::failure::NotifyFailure>,
    /// Age of that failure at gather time, so the renderer stays clock-free like every other age here.
    notify_failure_age_ms: Option<u64>,
}

/// Diagnose the action manifests: load errors from [`actions::diagnose`], then dangling
/// agent references (a `[keys]`/`agents` name with no matching agent manifest) for the ones that
/// parsed. `known_agents` is the loaded agent-manifest roster doctor already holds.
fn diagnose_actions(known_agents: &[String]) -> (usize, Vec<ActionLint>) {
    let mut ok = 0usize;
    let mut issues = Vec::new();
    for diag in actions::diagnose() {
        match diag.result {
            Err(problem) => issues.push(ActionLint {
                file: diag.file,
                problem,
            }),
            Ok(action) => {
                ok += 1;
                let refs: Vec<&str> = match action.kind {
                    tma_core::ActionKind::Keys => action.keys.keys().map(String::as_str).collect(),
                    tma_core::ActionKind::Exec => {
                        action.agents.iter().map(String::as_str).collect()
                    }
                };
                for agent in refs {
                    if !known_agents.iter().any(|k| k == agent) {
                        issues.push(ActionLint {
                            file: diag.file.clone(),
                            problem: format!(
                                "action {:?} references unknown agent {:?}",
                                action.name, agent
                            ),
                        });
                    }
                }
            }
        }
    }
    (ok, issues)
}

/// Gather the read-only diagnosis. One `list-panes`, one `ps` parse (the identity read path), a
/// daemon socket probe, and the hook-config reads — no captures, no stamps, no writes.
fn gather(
    tmux: &Tmux,
    manifests: &[LoadedManifest],
    manifest_failures: &[ManifestFailure],
    focus_events: bool,
    windows: &WindowsSection,
    api: &ApiSection,
) -> Result<Report, TmuxError> {
    let now = tma_runtime::now_ms();
    let panes = tmux.list_panes()?;

    // The server's build. A failed read degrades to `None` (no warning) rather than failing the
    // report: every other check below is still worth printing.
    let tmux_version = tmux
        .server_version()
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());

    // Daemon liveness (a connect probe, read-only) and the shared per-server socket path.
    let daemon = ipc::daemon_status(tmux);
    let daemon_alive = daemon.as_ref().is_some_and(|d| d.alive);

    // Ambient driver: `@tma_last_poll` (server-scoped, ms) — its age tells us whether anything
    // is invoking `tma status`. Absent/zero ⇒ no driver, the floor renders nothing.
    let ambient_poll_age_ms = tmux
        .get_server_option(opt::LAST_POLL)?
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&p| p != 0)
        .map(|p| now.saturating_sub(p));

    // The two server-wide conditions the ambient driver depends on: a `#()` status job runs only
    // while a client is drawing the status line, and only while `status` is on.
    let attached_clients = tmux.list_clients()?.len();
    let status_enabled = tmux
        .get_global_option("status")?
        .map(|v| v != "off")
        .unwrap_or(true);

    // The clickable status segments: the bindings live in the managed keys file, the click that
    // reaches them depends on the server's `mouse` option (off by default, and tma never sets it).
    let mouse_bindings = crate::install_keys::mouse_bindings_installed(None);
    let mouse_enabled = tmux.get_global_option("mouse")?.is_some_and(|v| v == "on");

    // Resident sidebars (the middle tier): panes advertising `@tma_watch_pid` are the SIGUSR1
    // nudge targets the focus-change hook signals. Read-only; a gone server yields none.
    let watch_sidebars = tmux
        .list_pane_option(opt::WATCH_PID)
        .map(|panes| panes.len())
        .unwrap_or(0);

    // Hook wiring diagnosis (the `install-hooks --check` machinery, read-only).
    let hook_diag = install::diagnose_hooks(manifests, tmux, focus_events);

    // Identify agent panes the same way the poll cycle's read half does — but never stamp. A `ps`
    // that cannot run costs the identification below (registered panes still resolve) and nothing
    // else, so it is reported as a warning rather than sinking the whole report.
    let (procs, process_walk_error) = match tmux::ps_all() {
        Ok(procs) => (procs, None),
        Err(err) => (Vec::new(), Some(err.to_string())),
    };
    let mut agents = Vec::new();
    let mut nested: Vec<NestedMultiplexer> = Vec::new();
    let mut remote: Vec<RemotePane> = Vec::new();
    let mut ignored: Vec<IgnoredPane> = Vec::new();
    let mut stamp_issues: Vec<StampLint> = Vec::new();
    for rec in &panes {
        // The user's opt-out short-circuits everything below, exactly as it does in the cycle: no
        // identity, no agent row, just the line that says the option is why.
        if let Some(value) = rec.options.get(opt::IGNORE).filter(|v| !v.is_empty()) {
            ignored.push(IgnoredPane {
                pane: rec.pane_id.clone(),
                locator: rec.locator(),
                value: value.clone(),
            });
            continue;
        }
        // Unlike every other reader, doctor keeps the decode error: a corrupt option is why a pane
        // that looks stamped reads as never-stamped, and nothing else says so.
        let read = match StampedState::from_options(&rec.options) {
            Ok(read) => read.map(ReadResult::into_inner),
            Err(err) => {
                stamp_issues.push(StampLint {
                    pane: rec.pane_id.clone(),
                    locator: rec.locator(),
                    problem: err.to_string(),
                });
                None
            }
        };

        // The registered half: a stored `@agent_session` + `@agent_name` lets identify honor a
        // hook-registered agent the ps-walk momentarily cannot see (matches cycle.rs).
        let registration = match (
            read.as_ref().and_then(|p| p.session.as_deref()),
            rec.options.get(opt::NAME),
        ) {
            (Some(session), Some(name)) => Some(identity::Registration {
                agent_name: name.clone(),
                session: Some(session.to_string()),
            }),
            _ => None,
        };

        let identity = identity::identify(
            rec.pane_pid,
            &rec.current_command,
            &rec.title,
            &procs,
            manifests,
            rec.options
                .get(opt::TITLE_MATCH_PID)
                .and_then(|v| v.parse().ok()),
            registration.as_ref(),
        );
        // The two out-of-scope foregrounds are reported separately: the walk can never see what
        // runs past either boundary, so without these the pane is simply absent with no explanation.
        match identity.out_of_scope() {
            Some(identity::OutOfScope::Multiplexer(cmd)) => nested.push(NestedMultiplexer {
                pane: rec.pane_id.clone(),
                locator: rec.locator(),
                command: cmd,
            }),
            Some(identity::OutOfScope::RemoteShell(cmd)) => remote.push(RemotePane {
                pane: rec.pane_id.clone(),
                locator: rec.locator(),
                command: cmd,
                // Options left from before the boundary: no cycle refreshes them any more.
                stamped: read.is_some(),
            }),
            None => {}
        }
        let identity::PaneIdentity::Agent(id) = identity else {
            continue;
        };
        let agent = id.manifest.name.clone();

        let wiring = hook_diag
            .agents
            .iter()
            .find(|a| a.agent == agent)
            .map(|a| a.wiring.clone())
            .unwrap_or(HookWiring::NotInstalled);
        let tier = derive_tier(HookClass::from_wiring(&wiring), daemon_alive);

        let (state, source, evidence_age_ms) = match &read {
            Some(s) => (
                Some(s.state),
                Some(s.source),
                (s.evidence_at != 0).then(|| now.saturating_sub(s.evidence_at)),
            ),
            None => (None, None, None),
        };

        // Best-effort model label, reported with whether `[telemetry.windows]` names it.
        let model = rec
            .options
            .get(opt::MODEL)
            .filter(|v| !v.is_empty())
            .cloned();
        let window_covered = model.as_deref().map(|m| windows.knows(m));

        // A pending permission request needs a resolvable endpoint (pane stamp or config
        // fallback) for the broker's API lane; flag one that has none.
        let endpoint_ok = rec
            .options
            .get(opt::PERMISSION_REQUEST)
            .filter(|v| !v.is_empty())
            .map(|_| {
                let stamped = rec
                    .options
                    .get(opt::API_ENDPOINT)
                    .is_some_and(|v| !v.is_empty());
                stamped || api.api_base(&agent).is_some()
            });

        // A pane that registered through a hook (`@agent_session` present) whose current evidence is
        // capture has been demoted to the floor: its hooks registered once and then stopped firing.
        let hook_demoted = read
            .as_ref()
            .is_some_and(|s| s.session.is_some() && s.source == Provenance::Capture);

        agents.push(AgentReport {
            pane: rec.pane_id.clone(),
            agent,
            locator: rec.locator(),
            state,
            source,
            evidence_age_ms,
            wiring,
            tier,
            model,
            window_covered,
            endpoint_ok,
            hook_demoted,
        });
    }
    agents.sort_by(|a, b| a.locator.cmp(&b.locator).then(a.pane.cmp(&b.pane)));

    // The notify sink's last recorded failure (a fire discards its command's output, so this marker
    // is the only trace), aged against the same `now` every other age here uses.
    let notify_failure = notify::failure::last();

    // Action manifests: load errors and dangling agent references. Cross-checked against
    // the loaded agent roster (so a disabled agent counts as unknown, which is honest — an action
    // for it cannot fire).
    let known_agents: Vec<String> = manifests.iter().map(|m| m.name.clone()).collect();
    let (action_ok, action_issues) = diagnose_actions(&known_agents);

    // Agent manifests the loader skipped. Every other surface reduces these to a one-line stderr
    // warning; here the file and its error are both reported.
    let manifest_issues: Vec<ManifestLint> = manifest_failures
        .iter()
        .map(|f| ManifestLint {
            file: f.path.display().to_string(),
            problem: f.error.to_string(),
        })
        .collect();

    let (daemon_socket, daemon_known, daemon_version) = match daemon {
        Some(d) => (d.socket, true, d.version),
        None => (PathBuf::new(), false, None),
    };
    Ok(Report {
        tmux_version,
        daemon_alive,
        daemon_socket,
        daemon_known,
        daemon_version,
        ambient_poll_age_ms,
        attached_clients,
        status_enabled,
        mouse_bindings,
        mouse_enabled,
        watch_sidebars,
        tmux_hooks: hook_diag.tmux_hooks,
        wrapper_path: hook_diag.wrapper_path,
        wrapper_present: hook_diag.wrapper_present,
        agents,
        action_ok,
        action_issues,
        manifest_ok: manifests.len(),
        manifest_issues,
        process_name_issues: diagnose_process_names(manifests),
        nested,
        remote,
        ignored,
        stamp_issues,
        process_walk_error,
        notify_failure_age_ms: notify_failure.as_ref().map(|f| now.saturating_sub(f.at)),
        notify_failure,
    })
}

/// The `--exit-code` verdict: how many warnings the report carries, and how many panes sit below the
/// tier their manifest supports. Every warning the report prints counts, so the flag and the report
/// cannot disagree; posture facts that are not misconfiguration (no daemon, nothing polling yet, no
/// sidebar, a model name `[telemetry.windows]` does not list) are not warnings and do not count.
fn gate(r: &Report) -> (usize, usize) {
    let mut warnings = usize::from(!r.wrapper_present);
    warnings += r.tmux_hooks.iter().filter(|(_, s)| !s.is_present()).count();
    warnings += r.manifest_issues.len() + r.action_issues.len() + r.process_name_issues.len();
    warnings += r.stamp_issues.len();
    // No process walk means detection itself is blind here, not just the report.
    warnings += usize::from(r.process_walk_error.is_some());
    // A detached server only matters while nothing else keeps state fresh.
    warnings += usize::from(r.attached_clients == 0 && !r.daemon_alive);
    warnings += usize::from(!r.status_enabled);
    // Installed mouse bindings that no click can ever reach.
    warnings += usize::from(r.mouse_bindings && !r.mouse_enabled);
    warnings += usize::from(r.notify_failure.is_some());
    for a in &r.agents {
        warnings += usize::from(matches!(a.wiring, HookWiring::Incomplete(_)));
        warnings += usize::from(a.hook_demoted);
        warnings += usize::from(a.endpoint_ok == Some(false));
    }
    let below = r
        .agents
        .iter()
        .filter(|a| a.tier.level < expected_tier(HookClass::from_wiring(&a.wiring)))
        .count();
    (warnings, below)
}

pub(crate) fn run(opts: DoctorOpts) -> ExitCode {
    // The set form, not `load_manifests_or_exit`: doctor reports the skipped files itself rather
    // than emitting the surfaces' one-line stderr warning.
    let set =
        match cli_support::load_manifest_set_or_exit(opts.manifest_dir.as_deref(), &opts.agents) {
            Ok(s) => s,
            Err(code) => return code,
        };
    let tmux = Tmux::connect(&opts.server);
    let report = match gather(
        &tmux,
        &set.manifests,
        &set.failures,
        opts.focus_events,
        &opts.windows,
        &opts.api,
    ) {
        Ok(r) => r,
        Err(TmuxError::ServerGone) => return cli_support::no_server(),
        Err(err) => {
            eprintln!("tma: {err}");
            return ExitCode::FAILURE;
        }
    };
    if opts.json {
        println!("{}", render_json(&report));
    } else {
        print!("{}", render_text(&report));
    }
    if opts.exit_code {
        let (warnings, below) = gate(&report);
        if warnings + below > 0 {
            // On stderr so `--json` output stays parseable when the two flags are combined.
            eprintln!(
                "tma: doctor: {warnings} warning(s), {below} pane(s) below the tier their \
                 manifest supports"
            );
            return ExitCode::FAILURE;
        }
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wired_agent_with_daemon_is_tier_3() {
        let t = derive_tier(HookClass::Wired, true);
        assert_eq!(t.level, 3);
        assert_eq!(t.reason, TierReason::DaemonTier);
        assert!(tier_reason_str(t, "claude", true).is_none());
    }

    #[test]
    fn wired_agent_without_daemon_is_tier_2() {
        let t = derive_tier(HookClass::Wired, false);
        assert_eq!(t.level, 2);
        assert_eq!(t.reason, TierReason::NoDaemon);
        assert!(tier_reason_str(t, "claude", false)
            .unwrap()
            .contains("daemon not running"));
    }

    #[test]
    fn partial_wiring_still_reaches_the_hook_tier() {
        // Some events flow, so the agent is on the hook path (tier 2/3), not the floor.
        assert_eq!(derive_tier(HookClass::Partial, false).level, 2);
        assert_eq!(derive_tier(HookClass::Partial, true).level, 3);
    }

    #[test]
    fn unwired_hook_capable_agent_is_tier_1_hooks_reason() {
        let t = derive_tier(HookClass::NotInstalled, false);
        assert_eq!(t.level, 1);
        assert_eq!(t.reason, TierReason::HooksNotInstalled);
        assert!(tier_reason_str(t, "claude", false)
            .unwrap()
            .contains("hooks not installed for claude"));
    }

    #[test]
    fn hookless_agent_is_tier_1_even_with_a_daemon() {
        // The task's mapping ties tier 3 to hooks; a hookless agent stays on the floor, but the
        // reason notes the daemon's fallback capture so the operator is not misled.
        let t = derive_tier(HookClass::Hookless, true);
        assert_eq!(t.level, 1);
        assert_eq!(t.reason, TierReason::Hookless);
        let reason = tier_reason_str(t, "gemini", true).unwrap();
        assert!(reason.contains("hookless"));
        assert!(
            reason.contains("fallback capture"),
            "daemon note present: {reason}"
        );
    }

    #[test]
    fn no_adapter_agent_is_tier_1() {
        let t = derive_tier(HookClass::NoAdapter, false);
        assert_eq!(t.level, 1);
        assert_eq!(t.reason, TierReason::NoAdapter);
    }

    #[test]
    fn hook_class_maps_every_wiring_category() {
        assert_eq!(HookClass::from_wiring(&HookWiring::Wired), HookClass::Wired);
        assert_eq!(
            HookClass::from_wiring(&HookWiring::Incomplete(vec!["x".into()])),
            HookClass::Partial
        );
        assert_eq!(
            HookClass::from_wiring(&HookWiring::NotInstalled),
            HookClass::NotInstalled
        );
        assert_eq!(
            HookClass::from_wiring(&HookWiring::Hookless),
            HookClass::Hookless
        );
        assert_eq!(
            HookClass::from_wiring(&HookWiring::NoAdapter),
            HookClass::NoAdapter
        );
    }

    #[test]
    fn a_process_name_past_the_comm_width_needs_a_truncated_sibling() {
        let names = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // The bundled codex manifest is the shape that works: the 15-char truncation is listed too.
        let codex = tma_core::Manifest::parse(
            include_str!("../../tma-core/manifests/codex.toml"),
            "codex.toml",
        )
        .unwrap();
        assert!(
            unreachable_process_names(&codex.identity.process_names).is_empty(),
            "codex lists `codex-aarch64-a` beside `codex`"
        );
        assert_eq!(
            unreachable_process_names(&names(&["claude"])),
            Vec::<&String>::new()
        );

        // 16 chars with nothing shorter beside it: `ps` reports at most 15, so it never matches.
        let long = names(&["my-long-agent-cli"]);
        assert_eq!(unreachable_process_names(&long), vec![&long[0]]);
        // Adding the truncation clears it.
        assert!(
            unreachable_process_names(&names(&["my-long-agent-c", "my-long-agent-cli"])).is_empty()
        );
    }

    /// The mouse pairing is only a warning when both halves disagree: installed bindings on a server
    /// with `mouse` off can never fire. Either half alone is a posture fact, not a misconfiguration.
    #[test]
    fn installed_mouse_bindings_without_mouse_mode_are_flagged() {
        let mut r = sample_report();
        let text = render_text(&r);
        assert!(
            text.contains("`mouse` option is off") && text.contains("set -g mouse on"),
            "the warning names the option and its fix: {text}"
        );

        r.mouse_enabled = true;
        assert!(!render_text(&r).contains("`mouse` option is off"));
        // Not installed: nothing to say, whatever the option says.
        r.mouse_bindings = false;
        r.mouse_enabled = false;
        assert!(!render_text(&r).contains("`mouse` option is off"));

        // The gate counts the pairing once, and only in that pairing.
        let mut c = clean_report();
        c.mouse_enabled = false;
        assert_eq!(gate(&c).0, 1);
        c.mouse_bindings = false;
        assert_eq!(gate(&c).0, 0);
    }

    /// A fully-populated report, so every conditional branch in both emitters is exercised.
    pub(super) fn sample_report() -> Report {
        Report {
            tmux_version: Some("3.6a".to_string()),
            daemon_alive: true,
            daemon_socket: PathBuf::from("/tmp/tma.sock"),
            daemon_known: true,
            daemon_version: Some("0.0.1".to_string()),
            ambient_poll_age_ms: Some(1200),
            attached_clients: 0,
            status_enabled: false,
            mouse_bindings: true,
            mouse_enabled: false,
            watch_sidebars: 1,
            tmux_hooks: vec![("after-select-pane".to_string(), TmuxHookState::Present)],
            wrapper_path: PathBuf::from("/usr/local/bin/tma-hook"),
            wrapper_present: true,
            agents: vec![AgentReport {
                pane: "%1".to_string(),
                agent: "claude".to_string(),
                locator: "s:1.0".to_string(),
                state: Some(AgentState::Blocked),
                source: Some(Provenance::Hook),
                evidence_age_ms: Some(50),
                wiring: HookWiring::Wired,
                tier: derive_tier(HookClass::Wired, true),
                model: Some("gpt-5-codex".to_string()),
                window_covered: Some(false),
                endpoint_ok: Some(false),
                hook_demoted: true,
            }],
            action_ok: 4,
            action_issues: vec![ActionLint {
                file: "<bundled>/approve.toml".to_string(),
                problem: "action \"approve\" references unknown agent \"codex\"".to_string(),
            }],
            manifest_ok: 6,
            manifest_issues: vec![ManifestLint {
                file: "/home/u/.config/tma/agents/mine.toml".to_string(),
                problem: "expected `=`".to_string(),
            }],
            process_name_issues: vec![ProcessNameLint {
                agent: "mine".to_string(),
                name: "my-very-long-agent".to_string(),
            }],
            nested: vec![NestedMultiplexer {
                pane: "%9".to_string(),
                locator: "s:2.0".to_string(),
                command: "tmux",
            }],
            remote: vec![RemotePane {
                pane: "%10".to_string(),
                locator: "s:2.1".to_string(),
                command: "ssh",
                stamped: true,
            }],
            ignored: vec![IgnoredPane {
                pane: "%12".to_string(),
                locator: "s:2.2".to_string(),
                value: "1".to_string(),
            }],
            stamp_issues: vec![StampLint {
                pane: "%11".to_string(),
                locator: "s:3.0".to_string(),
                problem: "unknown @agent_state token: \"spinning\"".to_string(),
            }],
            process_walk_error: Some("failed to spawn `ps`: No such file or directory".to_string()),
            notify_failure: Some(notify::failure::NotifyFailure {
                at: 1_700_000_000_000,
                reason: "exited 127".to_string(),
                command: "tma-notify".to_string(),
            }),
            notify_failure_age_ms: Some(4_000),
        }
    }

    /// A clean server, so the `--exit-code` gate has a green case: every warning cleared and the one
    /// pane at the tier its manifest supports without a daemon.
    fn clean_report() -> Report {
        let mut r = sample_report();
        r.daemon_alive = false;
        r.daemon_version = None;
        r.attached_clients = 1;
        r.status_enabled = true;
        r.mouse_enabled = true;
        r.action_issues.clear();
        r.manifest_issues.clear();
        r.process_name_issues.clear();
        r.nested.clear();
        // The remote pane deliberately stays: it is a posture fact (the agent lives elsewhere),
        // not misconfiguration, so a clean server still gates green with one.
        r.stamp_issues.clear();
        r.process_walk_error = None;
        r.notify_failure = None;
        r.notify_failure_age_ms = None;
        r.agents[0].tier = derive_tier(HookClass::Wired, false);
        r.agents[0].hook_demoted = false;
        r.agents[0].endpoint_ok = None;
        // `window_covered` deliberately stays `Some(false)`: an unrecognized model is a reported
        // fact, not misconfiguration, so a clean server still gates green with one.
        r
    }

    #[test]
    fn the_exit_code_gate_counts_every_warning_the_report_prints() {
        assert_eq!(gate(&clean_report()), (0, 0), "a clean server gates green");

        // Each warning is counted once, and none of them is a posture fact.
        let mut r = clean_report();
        r.wrapper_present = false;
        r.status_enabled = false;
        r.attached_clients = 0;
        r.agents[0].hook_demoted = true;
        assert_eq!(gate(&r).0, 4);

        // A daemon covering a detached server drops that one.
        r.daemon_alive = true;
        assert_eq!(gate(&r).0, 3);
    }

    /// A pane whose `@agent_*` options do not decode reads as never-stamped everywhere else in the
    /// codebase, silently and forever. Doctor names the pane and the value, and gates on it.
    #[test]
    fn a_corrupt_stamp_is_named_and_gates_red() {
        let mut r = clean_report();
        r.stamp_issues = vec![StampLint {
            pane: "%11".to_string(),
            locator: "work:3.0".to_string(),
            problem: "unknown @agent_state token: \"spinning\"".to_string(),
        }];
        assert_eq!(gate(&r).0, 1, "the corrupt stamp is one warning");

        let text = render_text(&r);
        assert!(
            text.contains("%11") && text.contains("work:3.0"),
            "the pane is named: {text}"
        );
        assert!(
            text.contains("@agent_state") && text.contains("spinning"),
            "the option and the value that broke it are named: {text}"
        );
        assert!(
            text.contains("never-stamped"),
            "and what it costs the pane: {text}"
        );

        // A pane whose options all decode says nothing about stamps.
        assert!(!render_text(&clean_report()).contains("stamps:"));
    }

    /// A `ps` that will not run used to sink the whole report. It costs pane identification only,
    /// so the server-side half still prints, the reason is named, and the gate counts it once.
    #[test]
    fn a_failed_process_walk_is_named_and_gates_red() {
        let mut r = clean_report();
        r.process_walk_error = Some("failed to spawn `ps`: Operation not permitted".to_string());
        assert_eq!(gate(&r).0, 1, "the failed walk is one warning");

        let text = render_text(&r);
        assert!(
            text.contains("Operation not permitted"),
            "the spawn error is quoted: {text}"
        );
        assert!(
            text.contains("hooks:") && text.contains("wrapper:"),
            "the server-side checks still print: {text}"
        );

        let json = render_json(&r);
        assert!(
            json.contains("\"process_walk\":{\"ok\":false"),
            "the walk's verdict rides the document: {json}"
        );
        // A working walk says nothing beyond the `ok` flag.
        assert!(!render_text(&clean_report()).contains("procs:"));
    }

    #[test]
    fn a_pane_below_the_tier_its_manifest_supports_gates_red() {
        // Hook-capable but unwired: tier 1 against an expected 2.
        let mut r = clean_report();
        r.agents[0].wiring = HookWiring::NotInstalled;
        r.agents[0].tier = derive_tier(HookClass::NotInstalled, false);
        assert_eq!(gate(&r).1, 1);

        // A missing daemon is a runtime choice, not a shortfall: wired at tier 2 is expected 2.
        assert_eq!(gate(&clean_report()).1, 0);
        // Hookless tops out at the floor, so tier 1 is not below anything.
        let mut hookless = clean_report();
        hookless.agents[0].wiring = HookWiring::Hookless;
        hookless.agents[0].tier = derive_tier(HookClass::Hookless, false);
        assert_eq!(gate(&hookless), (0, 0));
    }

    /// The version comparison is numeric per component and tolerant of the four spellings tmux
    /// actually prints: a patch letter (`3.6a`), a plain release, a `next-` pre-release, and the
    /// double-digit minor that a string compare would sort wrong (`3.10` is newer than `3.6`).
    #[test]
    fn tmux_versions_parse_and_compare_numerically() {
        assert_eq!(parse_tmux_version("3.6"), Some((3, 6)));
        assert_eq!(parse_tmux_version("3.6a"), Some((3, 6)));
        assert_eq!(parse_tmux_version("3.5b"), Some((3, 5)));
        assert_eq!(parse_tmux_version("next-3.7"), Some((3, 7)));
        assert_eq!(parse_tmux_version(" 3.6a\n"), Some((3, 6)));
        assert_eq!(parse_tmux_version("3.10"), Some((3, 10)));
        assert_eq!(parse_tmux_version("4.0"), Some((4, 0)));

        // Nothing tma can read a release out of.
        for junk in ["", "master", "openbsd-7.5", "3", "x.y", "next-", ".6"] {
            assert_eq!(parse_tmux_version(junk), None, "{junk:?}");
        }

        assert!(tmux_below_min(Some("3.5b")));
        assert!(tmux_below_min(Some("2.9a")));
        assert!(!tmux_below_min(Some("3.6")));
        assert!(!tmux_below_min(Some("3.6a")));
        assert!(!tmux_below_min(Some("3.10")));
        assert!(!tmux_below_min(Some("next-3.7")));
        // Unknown and unparseable both stay silent rather than guessing "old".
        assert!(!tmux_below_min(None));
        assert!(!tmux_below_min(Some("master")));
    }

    /// The floor is a warning line and a JSON field, never part of the `--exit-code` verdict: the
    /// user cannot fix their distro's tmux from a config file.
    #[test]
    fn an_old_tmux_warns_without_gating() {
        let mut r = clean_report();
        r.tmux_version = Some("3.5a".to_string());
        let text = render_text(&r);
        assert!(
            text.contains("tested on tmux 3.6+") && text.contains("3.5a"),
            "the found version and the floor are both named: {text}"
        );
        assert_eq!(gate(&r), (0, 0), "an old tmux is not a CI failure");
        assert!(render_json(&r)
            .contains(r#""tmux":{"version":"3.5a","min_version":"3.6","below_min":true}"#));

        // A current server says nothing in the text report, and still carries the JSON field.
        r.tmux_version = Some("3.6a".to_string());
        assert!(!render_text(&r).contains("tested on tmux"));
        assert!(render_json(&r).contains(r#""below_min":false"#));
        r.tmux_version = None;
        assert!(!render_text(&r).contains("tested on tmux"));
        assert!(render_json(&r).contains(r#""tmux":{"version":null"#));
    }

    /// A resident daemon older than the CLI driving it is reported, with the note that `tma reload`
    /// will not fix it (it re-reads config and manifests, not the binary). A same-version daemon and
    /// a lock file predating version recording both stay silent.
    #[test]
    fn a_daemon_version_skew_is_flagged_only_when_it_actually_differs() {
        assert_eq!(daemon_version_matches(None), None, "nothing to compare");
        assert_eq!(daemon_version_matches(Some(ipc::VERSION)), Some(true));
        assert_eq!(daemon_version_matches(Some("0.0.1")), Some(false));

        let mut report = sample_report();
        report.daemon_version = Some("0.0.1".to_string());
        let text = render_text(&report);
        assert!(
            text.contains("version 0.0.1 differs from this CLI"),
            "the skew is named: {text}"
        );
        assert!(
            text.contains("tma daemon --ensure"),
            "and says how to pick up the new build: {text}"
        );

        // Matching build, and an old lock file with no version: no warning either way.
        report.daemon_version = Some(ipc::VERSION.to_string());
        assert!(!render_text(&report).contains("differs from this CLI"));
        report.daemon_version = None;
        assert!(!render_text(&report).contains("differs from this CLI"));
    }
}
