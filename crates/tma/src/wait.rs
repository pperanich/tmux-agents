//! `tma wait`: block a script on an agent pane reaching an `--until` state; exit codes are the
//! contract (documented in docs/reference/cli.md). Tier 2, cycle-authoritative: a thin driver over
//! [`cycle::run_cycle`] with config+manifest hot-reload; a daemon push (via [`WaitSubscription`])
//! is only a wake hint, so the exit always comes from a cycle, and no daemon degrades silently to
//! the 1 s poll loop. Level-triggered, never edge-triggered: `wait` returns the first cycle that
//! OBSERVES the target (entry cycle included), never from a raw stamp. `--agent` pins to the first
//! pane it observes, then behaves as `--pane` on it, so a later same-named pane never flips a
//! running wait to the ambiguity error (raised only if the FIRST observation matches >1 pane).
//!
//! The fleet targets share that pin-or-not distinction. `--all` is a barrier over a KNOWN fleet: it
//! pins its membership at the first observation, so a pane launched mid-wait never extends the
//! barrier and a member that dies ends it. `--count <n>` is a quorum over whoever shows up, so it
//! re-evaluates membership every cycle and ignores departures. `--since` narrows every target to a
//! state that BEGAN after a timestamp, which is how a supervisor loop avoids re-satisfying on the
//! same episode it just acted on.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use tma_core::{AgentRow, Selector, StateToken};
use tma_runtime::ipc::{WaitSubscription, WaitWake};

use crate::cli_support;
use crate::config::{self, Config};
use crate::cycle;
use crate::tmux::{self, Tmux, TmuxError};

/// The no-daemon poll cadence: after the entry cycle, one guarded hot-reload cycle per second.
const POLL_TICK: Duration = Duration::from_secs(1);

/// The fallback re-cycle cadence in daemon-push mode, deliberately longer than [`POLL_TICK`]: a
/// belt for an edge the daemon did not push, and long enough that the push-latency test's
/// sub-second return provably came from a push, not this tick.
const PUSH_FALLBACK: Duration = Duration::from_secs(5);

/// The parsed `--until` set: a non-empty, de-duplicated list of target states. A row satisfies the
/// wait when it matches ANY member (comma-separated union). The token vocabulary and the `done`
/// semantics are [`tma_core::StateToken`]'s, shared with the `--state` selector.
#[derive(Clone, Debug)]
pub(crate) struct UntilSet(Vec<StateToken>);

impl UntilSet {
    fn matches(&self, row: &AgentRow) -> bool {
        self.0.iter().any(|w| w.matches(row))
    }

    /// A human description for the timeout/vanish stderr lines (`blocked, done`).
    fn describe(&self) -> String {
        self.0
            .iter()
            .map(|w| w.token())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// What a row must satisfy: an `--until` state, and with `--since` a transition strictly newer than
/// that epoch-ms timestamp. The `--since` half is the level-trigger escape hatch — a supervisor loop
/// (`wait --until blocked`, act, loop) would otherwise re-satisfy on the very episode it just acted
/// on, because the state it waited for is still the current one.
///
/// The comparison is [`AgentRow::episode_at`], not `since` alone: a second completion on a pane
/// that never left `idle` moves only `@agent_turn_at`, and a `since`-only floor would hide it from
/// the very loop `--since` exists to serve.
struct Goal {
    until: UntilSet,
    since: Option<u64>,
}

impl Goal {
    fn matches(&self, row: &AgentRow) -> bool {
        self.until.matches(row) && self.since.is_none_or(|floor| row.episode_at() > floor)
    }

    fn describe(&self) -> String {
        match self.since {
            Some(floor) => format!("{} since {floor}", self.until.describe()),
            None => self.until.describe(),
        }
    }
}

/// clap value parser for `--until`. A bad token is a clap error (exit 2) naming the valid set.
/// At least one token required.
pub(crate) fn parse_until(s: &str) -> Result<UntilSet, String> {
    let out = cli_support::parse_states(s, "--until")?;
    if out.is_empty() {
        return Err(format!(
            "--until needs at least one state ({})",
            StateToken::VOCABULARY
        ));
    }
    Ok(UntilSet(out))
}

/// What the waiter is watching: `--pane`/`--any`/`--all`/`--count` (clap keeps them mutually
/// exclusive), or the selector's `--agent` when none of those is given. The selector flags narrow
/// the candidate rows for every target but `--pane`, which names a pane id outright, so a selector
/// alongside it is a usage error.
enum Target {
    /// A specific pane id. Its disappearance is exit 3.
    Pane(String),
    /// An agent by name, within the selector's scope. Pins to the first pane observed (>1 at that
    /// first observation is an error); once pinned it behaves as [`Target::Pane`], vanish = exit 3.
    Agent { name: String },
    /// Any agent pane in the selector's scope. The first in surface-sort order wins.
    Any,
    /// A barrier over every in-scope pane: success only when ALL of them satisfy the goal.
    /// Membership pins at the first observation (see [`Latches::members`]).
    All,
    /// A quorum: success when at least `n` in-scope panes satisfy the goal. Membership is
    /// re-evaluated every cycle, so panes may come and go under it.
    Count(u32),
}

/// The outcome of evaluating one cycle's rows against the target.
enum Step<'a> {
    /// The target is satisfied — print the rows, exit 0. One row for the single-pane targets, the
    /// whole satisfied set for `--all`/`--count`.
    Matched(Vec<&'a AgentRow>),
    /// A watched pane (a `--pane`, a pinned `--agent`, or an `--all` member) was observed and then
    /// disappeared — exit 3.
    Vanished(String),
    /// A watched pane is still alive but the agent row it carried is gone: the agent process died
    /// under the wait — exit 4, rather than blocking to a `--timeout` that says nothing.
    Departed(String),
    /// `--agent` matched more than one pane at the FIRST observation — exit 1, naming the
    /// candidates (there is no deterministic pane to pin).
    Ambiguous(Vec<String>),
    /// `--all` observed no in-scope pane at its first cycle: a barrier over an empty fleet would be
    /// vacuous success, so it is a usage error (exit 2) instead.
    Empty,
    /// No decision this cycle; keep waiting.
    Waiting,
}

/// Everything `tma wait` needs, assembled by the bin's dispatch from the CLI args + loaded config.
pub(crate) struct WaitOpts {
    pub target_pane: Option<String>,
    pub target_any: bool,
    /// The selector flags. Its `agent` field doubles as the `--agent` target name (the flag that
    /// names one agent is the same flag that scopes to it).
    pub selector: Selector,
    pub target_all: bool,
    pub target_count: Option<u32>,
    pub until: UntilSet,
    /// `--since`: only a state that began strictly after this epoch-ms timestamp satisfies.
    pub since: Option<u64>,
    pub timeout: Option<u64>,
    pub json: bool,
    pub server: tmux::Server,
    pub manifest_dir: Option<PathBuf>,
    pub config: Config,
    /// The `--config` path (env/defaults resolve inside the loader), for the tick reload.
    pub config_path: Option<PathBuf>,
}

/// One watched pane's cross-cycle latches.
#[derive(Default)]
struct PaneLatch {
    /// Whether the pane was ever seen alive, so a later disappearance is a true vanish (exit 3)
    /// rather than a not-yet-launched pane (which keeps waiting).
    observed_pane: bool,
    /// Whether the pane was ever seen carrying an agent row. A live pane that then loses its row has
    /// lost its agent process (a crash), which is a distinct end (exit 4) from a pane that never had
    /// one and may still launch.
    observed_agent: bool,
    /// One-shot stderr hint when the pane is alive-but-not-an-agent (a blocks-forever typo guard).
    warned_nonagent: bool,
}

/// The waiter's cross-cycle latches, threaded through every evaluation.
#[derive(Default)]
struct Latches {
    /// The single watched pane's latches (`--pane`, or a pinned `--agent`).
    pane: PaneLatch,
    /// The pane an `--agent` target locked onto at its first unambiguous observation.
    pinned: Option<String>,
    /// The `--all` membership, pinned at the first observation: a barrier is over the fleet that
    /// existed when it started, so a pane launched mid-wait never extends it.
    members: Option<Vec<String>>,
}

/// Evaluate one concrete pane id against a cycle's rows (shared by `--pane`, a pinned `--agent`, and
/// each `--all` member).
fn eval_pane<'a>(
    pane_id: &str,
    rows: &'a [AgentRow],
    goal: &Goal,
    latch: &mut PaneLatch,
    tmux: &Tmux,
) -> Step<'a> {
    if let Some(row) = rows.iter().find(|r| r.pane_id == pane_id) {
        latch.observed_pane = true;
        latch.observed_agent = true;
        return if goal.matches(row) {
            Step::Matched(vec![row])
        } else {
            Step::Waiting
        };
    }
    // No agent row this cycle: a live pane that never carried one keeps waiting (still launching),
    // one that carried an agent lost it (a crash), and a once-seen pane now gone is a vanish. A
    // list-panes error defers to the next `run_cycle` (server-gone → exit 1).
    match tmux.list_panes() {
        Ok(panes) => {
            if panes.iter().any(|p| p.pane_id == pane_id) {
                latch.observed_pane = true;
                if latch.observed_agent {
                    return Step::Departed(pane_id.to_string());
                }
                // Alive but never an agent: emit the one-time typo hint.
                if !latch.warned_nonagent {
                    eprintln!("tma: pane {pane_id} is not currently an agent; waiting");
                    latch.warned_nonagent = true;
                }
                Step::Waiting
            } else if latch.observed_pane {
                Step::Vanished(pane_id.to_string())
            } else {
                Step::Waiting
            }
        }
        Err(_) => Step::Waiting,
    }
}

/// The `--all` barrier: every pinned member must satisfy the goal in the SAME cycle. A member that
/// dies ends the wait (exit 3) even while others are still working, so a barrier never silently
/// shrinks to the survivors.
fn eval_all<'a>(
    rows: &'a [AgentRow],
    selector: &Selector,
    goal: &Goal,
    latches: &mut Latches,
    tmux: &Tmux,
) -> Step<'a> {
    let members = match &latches.members {
        Some(members) => members.clone(),
        None => {
            let members: Vec<String> = rows
                .iter()
                .filter(|r| selector.matches(r))
                .map(|r| r.pane_id.clone())
                .collect();
            if members.is_empty() {
                return Step::Empty;
            }
            latches.members = Some(members.clone());
            members
        }
    };

    let mut matched = Vec::with_capacity(members.len());
    let mut waiting = false;
    for id in &members {
        // Membership was pinned FROM agent rows, so the member was observed alive carrying a row;
        // the hint latch is pre-set because a barrier member is never a typo.
        let mut latch = PaneLatch {
            observed_pane: true,
            observed_agent: true,
            warned_nonagent: true,
        };
        match eval_pane(id, rows, goal, &mut latch, tmux) {
            Step::Matched(rows) => matched.extend(rows),
            Step::Waiting => waiting = true,
            // A vanish (or any other terminal verdict) ends the barrier at once, ahead of the
            // members still merely waiting.
            terminal => return terminal,
        }
    }
    if waiting {
        Step::Waiting
    } else {
        Step::Matched(matched)
    }
}

/// Evaluate one cycle's rows against the target. `selector` scopes the candidate rows; `latches`
/// carries the pin, the membership, and the one-shot hints.
fn evaluate<'a>(
    target: &Target,
    rows: &'a [AgentRow],
    selector: &Selector,
    goal: &Goal,
    latches: &mut Latches,
    tmux: &Tmux,
) -> Step<'a> {
    match target {
        Target::Pane(pane_id) => eval_pane(pane_id, rows, goal, &mut latches.pane, tmux),
        Target::Agent { name: _ } => {
            // Pin-to-first-observed: once locked in, `--agent` behaves exactly as `--pane` on that
            // id. The selector gated only the first observation, not the pinned pane.
            if let Some(id) = latches.pinned.clone() {
                return eval_pane(&id, rows, goal, &mut latches.pane, tmux);
            }
            let candidates: Vec<&AgentRow> = rows.iter().filter(|r| selector.matches(r)).collect();
            match candidates.as_slice() {
                // Not launched yet (or not yet an agent): keep waiting, nothing to pin.
                [] => Step::Waiting,
                // First observation is unambiguous: pin to this pane and evaluate it as `--pane`
                // this very cycle (so an already-in-state pane still returns on the entry tick).
                [only] => {
                    latches.pinned = Some(only.pane_id.clone());
                    eval_pane(&only.pane_id, rows, goal, &mut latches.pane, tmux)
                }
                // Ambiguity is an error ONLY at the first observation (>1 pane simultaneously):
                // there is no deterministic pane to pin. Do not pin; exit 1 naming the candidates.
                many => Step::Ambiguous(many.iter().map(|r| r.pane_id.clone()).collect()),
            }
        }
        Target::Any => rows
            .iter()
            .find(|r| selector.matches(r) && goal.matches(r))
            .map(|row| Step::Matched(vec![row]))
            .unwrap_or(Step::Waiting),
        Target::All => eval_all(rows, selector, goal, latches, tmux),
        // A quorum re-reads the scope every cycle (no pin): it waits for `n` matches among whoever
        // is present, so a pane appearing mid-wait counts and one leaving is not an error.
        Target::Count(n) => {
            let hits: Vec<&AgentRow> = rows
                .iter()
                .filter(|r| selector.matches(r) && goal.matches(r))
                .collect();
            if hits.len() >= *n as usize {
                Step::Matched(hits)
            } else {
                Step::Waiting
            }
        }
    }
}

/// Block until the target is satisfied, times out, or fails; the loop lives in the bin per the
/// ls/status precedent (nothing here widens `tma-runtime`'s API).
pub(crate) fn run(opts: WaitOpts) -> ExitCode {
    let WaitOpts {
        target_pane,
        target_any,
        selector,
        target_all,
        target_count,
        until,
        since,
        timeout,
        json,
        server,
        manifest_dir,
        mut config,
        config_path,
    } = opts;

    // clap keeps pane/any/all/count mutually exclusive; the selector's `--agent` is the target only
    // when none of them is given. A pane id is already unique, so scoping flags alongside `--pane`
    // are a usage error rather than a silently-ignored narrowing.
    let target = if let Some(pane) = target_pane {
        if !selector.is_empty() {
            eprintln!(
                "tma: --pane names one pane; drop the selector flags \
                 (--session/--repo/--branch/--state) or target with --agent/--any/--all/--count"
            );
            return ExitCode::from(2);
        }
        Target::Pane(pane)
    } else if target_all {
        Target::All
    } else if let Some(n) = target_count {
        Target::Count(n)
    } else if target_any {
        Target::Any
    } else if let Some(name) = selector.agent.clone() {
        Target::Agent { name }
    } else {
        eprintln!(
            "tma: name a target: --pane <ID>, --agent <NAME>, --any, --all, or --count <N> (exit 2)"
        );
        return ExitCode::from(2);
    };
    let goal = Goal { until, since };
    // The fleet targets emit a set, so their `--json` is a document of rows, not one row object.
    let fleet = matches!(target, Target::All | Target::Count(_));

    let mut manifests =
        match cli_support::load_manifests_or_exit(manifest_dir.as_deref(), &config.agent_overrides)
        {
            Ok(m) => m,
            Err(code) => return code,
        };
    let tmux = tmux::Tmux::connect(&server);
    // The matched rows' provenance keys, resolved once before the loop rather than at the emit that
    // may never happen — one tmux call against a wait that can run for hours.
    let origin = tma_runtime::origin::Origin::resolve(&tmux);

    let start = Instant::now();
    let deadline = timeout.map(|secs| start + Duration::from_secs(secs));
    let mut latches = Latches::default();
    // One-shot latch for the transient-timeout note, so a wedged server logs once, not every tick.
    let mut warned_timeout = false;
    // The last reported hot-reload failure, so a config left malformed says so once per breakage.
    let mut reload_error: Option<String> = None;

    // Try to ride the daemon's edge pushes. `None` (no daemon/server, a pre-push daemon, or any
    // I/O error) leaves `wait` on the poll loop, a silent degrade; the entry cycle runs regardless.
    let mut subscription = WaitSubscription::try_subscribe(&tmux);

    loop {
        // Hot-reload all-or-nothing (keep the last good pair on a mid-save error), then one cycle.
        if let Some(msg) = config::reload_notice(
            config::reload_pair(
                &mut config,
                &mut manifests,
                config_path.as_deref(),
                manifest_dir.as_deref(),
            ),
            &mut reload_error,
        ) {
            eprintln!("{msg}");
        }
        let report = match cycle::run_cycle(&tmux, &manifests, &config.fold_config()) {
            Ok(mut r) => {
                // A repo/branch selector needs the labels the cycle deliberately leaves unresolved.
                // Only then: the resolver memoizes, but an unfiltered wait must stay spawn-free.
                if selector.needs_repo() {
                    tma_runtime::repo::annotate_rows(&mut r.rows);
                }
                Some(r)
            }
            Err(TmuxError::ServerGone) => return cli_support::no_server(),
            // A one-shot timeout is a transient server blip (a 3s socket stall), not a fatal error:
            // ride it to the next tick exactly as the daemon and `eval_pane` swallow the same stall.
            Err(TmuxError::Timeout { .. }) => {
                if !warned_timeout {
                    eprintln!("tma: tmux slow to respond; still waiting");
                    warned_timeout = true;
                }
                None
            }
            Err(err) => {
                eprintln!("tma: {err}");
                return ExitCode::FAILURE;
            }
        };

        // Skip evaluation on a transient timeout; the deadline check and wait below still run.
        if let Some(report) = report {
            match evaluate(&target, &report.rows, &selector, &goal, &mut latches, &tmux) {
                Step::Matched(matched) => {
                    // Resolve the matched rows' repo/branch/worktree only here, at emit — never in
                    // the poll loop, where a git spawn per tick per row would defeat the bounded memo.
                    let mut rows: Vec<AgentRow> = matched.into_iter().cloned().collect();
                    tma_runtime::repo::annotate_rows(&mut rows);
                    match (json, fleet) {
                        (true, true) => {
                            println!(
                                "{}",
                                tma_ui::surfaces::render_wait_json_rows(&rows, &origin)
                            )
                        }
                        (true, false) => {
                            println!("{}", tma_ui::surfaces::render_wait_json(&rows[0], &origin))
                        }
                        (false, _) => {
                            for row in &rows {
                                print!("{}", tma_ui::surfaces::render_ls_row(row));
                            }
                        }
                    }
                    return ExitCode::SUCCESS;
                }
                Step::Vanished(pane_id) => {
                    eprintln!(
                        "tma: the waited-on pane {pane_id} vanished before reaching {} (exit 3)",
                        goal.describe()
                    );
                    return ExitCode::from(3);
                }
                Step::Departed(pane_id) => {
                    // The pane outlived its agent: say so instead of blocking to a timeout that
                    // cannot distinguish a crashed agent from a slow one.
                    eprintln!(
                        "tma: the agent on pane {pane_id} exited before reaching {} (exit 4)",
                        goal.describe()
                    );
                    return ExitCode::from(4);
                }
                Step::Empty => {
                    // Only reachable for `--all`: an empty fleet would make the barrier vacuous.
                    eprintln!(
                        "tma: --all matched no agent panes in scope; nothing to wait for (exit 2)"
                    );
                    return ExitCode::from(2);
                }
                Step::Ambiguous(ids) => {
                    let name = match &target {
                        Target::Agent { name } => name.as_str(),
                        _ => "",
                    };
                    eprintln!(
                        "tma: --agent {name:?} matches {} panes ({}); target one with --pane",
                        ids.len(),
                        ids.join(", ")
                    );
                    return ExitCode::FAILURE;
                }
                Step::Waiting => {}
            }
        }

        // Timeout is checked AFTER the cycle, so the entry cycle always runs (an already-in-state
        // pane returns on tick 0 even with `--timeout 0`). Absent `--timeout` waits forever.
        if let Some(d) = deadline {
            if Instant::now() >= d {
                eprintln!(
                    "tma: timed out after {}s waiting for {} (exit 124)",
                    timeout.unwrap_or_default(),
                    goal.describe()
                );
                return ExitCode::from(124);
            }
        }

        // Wait for the next wake, capped at the remaining time so the timeout is honored within a
        // wake in both modes. A zero cap means the deadline is here: loop back for the check above.
        let base = if subscription.is_some() {
            PUSH_FALLBACK
        } else {
            POLL_TICK
        };
        let cap = match deadline {
            Some(d) => base.min(d.saturating_duration_since(Instant::now())),
            None => base,
        };
        if cap.is_zero() {
            continue;
        }
        match subscription.as_mut() {
            Some(sub) => match sub.wait_edge(cap) {
                // A push arrived or the fallback cap elapsed: re-cycle (the cycle decides). `wait` is
                // level-triggered, so the two wake kinds are equivalent here.
                WaitWake::Pushed | WaitWake::Elapsed => {}
                // Daemon died/restarted mid-wait: drop the subscription and degrade to the poll loop.
                WaitWake::Closed => subscription = None,
            },
            None => std::thread::sleep(cap),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tma_core::AgentState;

    fn row(pane: &str, agent: &str, state: AgentState, attention: bool, session: &str) -> AgentRow {
        AgentRow {
            pane_id: pane.to_string(),
            agent: agent.to_string(),
            state,
            detail: None,
            since: 0,
            turn_at: 0,
            session: session.to_string(),
            window_index: 0,
            pane_index: 0,
            title: String::new(),
            attention,
            agent_session: None,
            context_pct: None,
            context_at: None,
            tokens: None,
            muted: false,
            model: None,
            cwd: None,
            repo: None,
        }
    }

    #[test]
    fn parse_until_accepts_closed_tokens_and_done() {
        let set = parse_until("blocked,done").unwrap();
        assert!(set.matches(&row("%1", "c", AgentState::Blocked, false, "s")));
        // done requires idle + attention, not a bare idle.
        assert!(!set.matches(&row("%1", "c", AgentState::Idle, false, "s")));
        assert!(set.matches(&row("%1", "c", AgentState::Idle, true, "s")));
    }

    #[test]
    fn parse_until_dedups_and_trims() {
        let set = parse_until(" idle , idle , working ").unwrap();
        assert_eq!(set.0.len(), 2, "duplicates collapse, whitespace ignored");
    }

    #[test]
    fn parse_until_rejects_bad_token_naming_the_set() {
        let err = parse_until("blocked,running").unwrap_err();
        assert!(err.contains("running"));
        assert!(err.contains("done"), "the error names the valid set");
    }

    #[test]
    fn parse_until_rejects_empty() {
        assert!(parse_until("").is_err());
        assert!(parse_until(" , ").is_err());
    }

    /// `--until idle` matches a "done" pane too (done is a presentation surface over the idle
    /// token); `--until done` does not match a plain idle pane.
    #[test]
    fn idle_token_is_broader_than_done() {
        let idle = parse_until("idle").unwrap();
        let done = parse_until("done").unwrap();
        let done_pane = row("%1", "c", AgentState::Idle, true, "s");
        let idle_pane = row("%2", "c", AgentState::Idle, false, "s");
        assert!(idle.matches(&done_pane) && idle.matches(&idle_pane));
        assert!(done.matches(&done_pane) && !done.matches(&idle_pane));
    }

    /// A socket name no server listens on: the `--any`/`--count` paths never reach tmux (only the
    /// pane path's vanish check does), so `evaluate` runs offline.
    fn offline_tmux() -> Tmux {
        Tmux::new(Some("tma_wait_unit_no_such_server".to_string()))
    }

    /// The plain goal for `spec`, with no `--since` floor.
    fn goal(spec: &str) -> Goal {
        Goal {
            until: parse_until(spec).unwrap(),
            since: None,
        }
    }

    /// The pane ids of a matched step, for the fleet assertions.
    fn matched_ids(step: Step<'_>) -> Vec<String> {
        match step {
            Step::Matched(rows) => rows.iter().map(|r| r.pane_id.clone()).collect(),
            _ => panic!("expected a match"),
        }
    }

    /// `--any` picks the first row in surface-sort order that is BOTH in the selector's scope and in
    /// an `--until` state: an out-of-scope pane in that state keeps the wait blocked.
    #[test]
    fn any_target_honors_the_selector_scope() {
        let rows = vec![
            row("%1", "claude", AgentState::Idle, false, "other"),
            row("%2", "claude", AgentState::Idle, false, "work"),
        ];
        let goal = goal("idle");
        let mut selector = Selector {
            session: Some("work".to_string()),
            ..Default::default()
        };
        let mut latches = Latches::default();
        let tmux = offline_tmux();
        assert_eq!(
            matched_ids(evaluate(
                &Target::Any,
                &rows,
                &selector,
                &goal,
                &mut latches,
                &tmux
            )),
            ["%2"],
            "the out-of-scope idle pane is skipped"
        );

        // Nothing in scope reaches the state: keep waiting rather than matching the other session.
        selector.session = Some("nowhere".to_string());
        assert!(matches!(
            evaluate(&Target::Any, &rows, &selector, &goal, &mut latches, &tmux),
            Step::Waiting
        ));
    }

    /// The selector scopes which panes `--agent` may pin to: two same-named panes are ambiguous only
    /// among those in scope, so adding `--session` disambiguates instead of erroring.
    #[test]
    fn agent_target_pins_within_the_selector_scope() {
        let rows = vec![
            row("%1", "claude", AgentState::Working, false, "other"),
            row("%2", "claude", AgentState::Working, false, "work"),
        ];
        let goal = goal("idle");
        let mut selector = Selector {
            agent: Some("claude".to_string()),
            ..Default::default()
        };
        let target = Target::Agent {
            name: "claude".to_string(),
        };
        let mut latches = Latches::default();
        let tmux = offline_tmux();
        let ambiguous = evaluate(&target, &rows, &selector, &goal, &mut latches, &tmux);
        assert!(
            matches!(ambiguous, Step::Ambiguous(ref ids) if ids.len() == 2),
            "both panes match the bare --agent"
        );
        assert!(
            latches.pinned.is_none(),
            "an ambiguous first observation pins nothing"
        );

        selector.session = Some("work".to_string());
        let scoped = evaluate(&target, &rows, &selector, &goal, &mut latches, &tmux);
        assert!(
            matches!(scoped, Step::Waiting),
            "working is not the target state"
        );
        assert_eq!(
            latches.pinned.as_deref(),
            Some("%2"),
            "the scoped observation is unambiguous and pins"
        );
    }

    // ---- --since ---------------------------------------------------------------------------------

    /// `--since` narrows an `--until` match to a state that BEGAN after the floor: the same episode
    /// that satisfied a previous wait no longer satisfies this one, and its successor does.
    #[test]
    fn since_requires_a_transition_strictly_newer_than_the_floor() {
        let mut r = row("%1", "claude", AgentState::Blocked, false, "work");
        r.since = 500;
        let goal = Goal {
            until: parse_until("blocked").unwrap(),
            since: Some(500),
        };
        assert!(
            !goal.matches(&r),
            "the floor is exclusive: since_ms > floor"
        );
        r.since = 501;
        assert!(goal.matches(&r), "a newer episode satisfies");
        // The state token still has to match: a fresh transition into the wrong state does not.
        r.state = AgentState::Idle;
        assert!(!goal.matches(&r));
    }

    /// A supervisor loop's second lap: `wait --until done`, act, clear the marker, loop with
    /// `--since <the completion just handled>`. The pane never leaves `idle` between the two
    /// completions, so `@agent_since` is stuck at the first one and the floor would hide the
    /// second forever. `@agent_turn_at` carries it. Compare `row.since` alone and this fails.
    #[test]
    fn since_sees_a_second_completion_on_a_pane_that_never_left_idle() {
        let mut r = row("%1", "claude", AgentState::Idle, true, "work");
        r.since = 500; // the idle run began here, and write-once pins it
        r.turn_at = 500; // …as did the first completion
        let goal = Goal {
            until: parse_until("done").unwrap(),
            since: Some(500),
        };
        assert!(
            !goal.matches(&r),
            "the handled completion does not re-satisfy"
        );
        r.turn_at = 900; // a second turn ended; `since` did not move and cannot
        assert!(goal.matches(&r), "the next completion satisfies");
    }

    /// The `--since` floor rides the stderr description, so a timeout line says which episode window
    /// the wait was actually asking about.
    #[test]
    fn since_appears_in_the_goal_description() {
        let floored = Goal {
            until: parse_until("idle,blocked").unwrap(),
            since: Some(1785114068740),
        };
        assert_eq!(floored.describe(), "idle, blocked since 1785114068740");
        assert_eq!(goal("idle").describe(), "idle");
    }

    // ---- agent departure -------------------------------------------------------------------------

    /// The departure latch is what separates "not an agent yet" (keep waiting) from "the agent
    /// died" (exit 4): only a pane already observed carrying a row can depart. The live-pane branch
    /// itself needs a server, so this pins the latch transition the branch reads.
    #[test]
    fn observing_a_row_arms_the_departure_latch() {
        let rows = vec![row("%1", "claude", AgentState::Working, false, "work")];
        let mut latch = PaneLatch::default();
        let tmux = offline_tmux();
        assert!(matches!(
            eval_pane("%1", &rows, &goal("idle"), &mut latch, &tmux),
            Step::Waiting
        ));
        assert!(
            latch.observed_agent && latch.observed_pane,
            "observing the row arms both latches"
        );

        // A pane that has never carried a row leaves the latch disarmed, so its absence keeps the
        // wait blocked rather than reporting a death that never happened.
        let mut fresh = PaneLatch::default();
        assert!(matches!(
            eval_pane("%9", &rows, &goal("idle"), &mut fresh, &tmux),
            Step::Waiting
        ));
        assert!(!fresh.observed_agent && !fresh.observed_pane);
    }

    // ---- --all / --count -------------------------------------------------------------------------

    /// `--all` is a barrier: it holds while ANY member is outside the target states, and returns
    /// every member's row once they all land.
    #[test]
    fn all_holds_until_every_member_lands() {
        let mut rows = vec![
            row("%1", "claude", AgentState::Idle, false, "work"),
            row("%2", "claude", AgentState::Working, false, "work"),
        ];
        let goal = goal("idle");
        let selector = Selector::default();
        let mut latches = Latches::default();
        let tmux = offline_tmux();
        assert!(matches!(
            evaluate(&Target::All, &rows, &selector, &goal, &mut latches, &tmux),
            Step::Waiting
        ));
        rows[1].state = AgentState::Idle;
        assert_eq!(
            matched_ids(evaluate(
                &Target::All,
                &rows,
                &selector,
                &goal,
                &mut latches,
                &tmux
            )),
            ["%1", "%2"],
            "the barrier returns the whole satisfied fleet"
        );
    }

    /// `--all` pins its membership at the first observation: a pane that appears mid-wait is not a
    /// new member, so it cannot hold the barrier open (the fleet is the one the wait started over).
    #[test]
    fn all_pins_membership_at_the_first_observation() {
        let first = vec![row("%1", "claude", AgentState::Working, false, "work")];
        let goal = goal("idle");
        let selector = Selector::default();
        let mut latches = Latches::default();
        let tmux = offline_tmux();
        assert!(matches!(
            evaluate(&Target::All, &first, &selector, &goal, &mut latches, &tmux),
            Step::Waiting
        ));
        assert_eq!(
            latches.members.as_deref(),
            Some(["%1".to_string()].as_ref())
        );

        let later = vec![
            row("%1", "claude", AgentState::Idle, false, "work"),
            row("%2", "claude", AgentState::Working, false, "work"),
        ];
        assert_eq!(
            matched_ids(evaluate(
                &Target::All,
                &later,
                &selector,
                &goal,
                &mut latches,
                &tmux
            )),
            ["%1"],
            "the later pane never joined the barrier"
        );
    }

    /// An `--all` whose scope is empty at the first observation is a usage error, not a vacuous
    /// success: there is nothing to wait for.
    #[test]
    fn all_over_an_empty_scope_is_a_usage_error() {
        let rows = vec![row("%1", "claude", AgentState::Idle, false, "other")];
        let selector = Selector {
            session: Some("work".to_string()),
            ..Default::default()
        };
        let mut latches = Latches::default();
        assert!(matches!(
            evaluate(
                &Target::All,
                &rows,
                &selector,
                &goal("idle"),
                &mut latches,
                &offline_tmux()
            ),
            Step::Empty
        ));
        assert!(latches.members.is_none(), "an empty scope pins nothing");
    }

    /// `--count` is a quorum over whoever is present: it returns as soon as N in-scope panes are in
    /// a target state, and (unlike `--all`) re-reads the scope every cycle, so a pane appearing
    /// mid-wait counts toward it.
    #[test]
    fn count_returns_once_the_quorum_is_present() {
        let mut rows = vec![
            row("%1", "claude", AgentState::Idle, false, "work"),
            row("%2", "claude", AgentState::Working, false, "work"),
        ];
        let goal = goal("idle");
        let selector = Selector::default();
        let mut latches = Latches::default();
        let tmux = offline_tmux();
        assert_eq!(
            matched_ids(evaluate(
                &Target::Count(1),
                &rows,
                &selector,
                &goal,
                &mut latches,
                &tmux
            )),
            ["%1"]
        );
        assert!(
            matches!(
                evaluate(
                    &Target::Count(2),
                    &rows,
                    &selector,
                    &goal,
                    &mut latches,
                    &tmux
                ),
                Step::Waiting
            ),
            "one idle pane is not a quorum of two"
        );
        // A pane that appears mid-wait counts: membership is re-read, never pinned.
        rows.push(row("%3", "claude", AgentState::Idle, false, "work"));
        assert_eq!(
            matched_ids(evaluate(
                &Target::Count(2),
                &rows,
                &selector,
                &goal,
                &mut latches,
                &tmux
            )),
            ["%1", "%3"]
        );
        assert!(
            latches.members.is_none(),
            "a quorum pins no membership at all"
        );
    }

    /// The selector scopes both fleet targets: out-of-scope panes are neither barrier members nor
    /// quorum votes.
    #[test]
    fn fleet_targets_honor_the_selector_scope() {
        let rows = vec![
            row("%1", "claude", AgentState::Idle, false, "other"),
            row("%2", "claude", AgentState::Idle, false, "work"),
        ];
        let goal = goal("idle");
        let selector = Selector {
            session: Some("work".to_string()),
            ..Default::default()
        };
        let mut latches = Latches::default();
        let tmux = offline_tmux();
        assert_eq!(
            matched_ids(evaluate(
                &Target::All,
                &rows,
                &selector,
                &goal,
                &mut latches,
                &tmux
            )),
            ["%2"]
        );
        let mut fresh = Latches::default();
        assert!(
            matches!(
                evaluate(
                    &Target::Count(2),
                    &rows,
                    &selector,
                    &goal,
                    &mut fresh,
                    &tmux
                ),
                Step::Waiting
            ),
            "only one pane is in scope, so a quorum of two never forms"
        );
    }
}
