//! The notification *firing* primitive: the tmux `display-message` baseline plus the optional hook
//! command. Tier 2 because both notification paths call it — the daemonless `tma event` blocked
//! direct-fire ([`crate::event`], `TMA_NOTIFY_FROM_EVENT`) and the tier-3 daemon's dispatch
//! (`tma_daemon`'s `NotifyState`) — so keeping this one [`fire`] here is what stops the two paths
//! diverging. The daemon owns the *when* (per-episode dedup, cold-start, history); this module only
//! owns the *how* of one fire. tma-core stays pure: all notification I/O lives above it.

use std::process::{Child, Command, Stdio};

use tma_core::stamp::opt;
use tma_core::AgentState;

use crate::config::{NotifySinks, NotifyTrigger};
use crate::json::JsonWriter;
use tma_tmux::tmux::{PaneRecord, Tmux};

pub mod failure;
mod log;
mod test_fire;

pub use test_fire::{notify_test, NotifyTest, TestTrigger};

/// Map a noteworthy transition into `state` to the [`NotifyTrigger`] it fires (`blocked` → Blocked,
/// a working→idle completion → Done), or `None`. The one place this mapping lives, so the daemon
/// dispatch and daemonless `tma event` cannot disagree. `noteworthy` is each caller's transition
/// signal (the event path's `set_attention`, or the daemon's `state == blocked || @agent_attention`).
pub fn trigger_for(state: AgentState, noteworthy: bool) -> Option<NotifyTrigger> {
    match state {
        AgentState::Blocked if noteworthy => Some(NotifyTrigger::Blocked),
        AgentState::Idle if noteworthy => Some(NotifyTrigger::Done),
        _ => None,
    }
}

/// Whether `tma mute` is still suppressing this pane's notifications at `now`. Reads the deadline
/// off the pane read both fire paths already hold (no round-trip) and decides in
/// [`tma_core::stamp::mute_active`], so the daemon and the daemonless `tma event` share one rule.
///
/// Mute is deliberately checked AFTER the episode marker commits: a fire suppressed here is spent,
/// not queued, so an expiring mute never rings for episodes that came and went while it held.
pub fn muted(rec: &PaneRecord, now: u64) -> bool {
    tma_core::stamp::mute_active(
        rec.options
            .get(opt::MUTE_UNTIL)
            .and_then(|v| v.parse::<u64>().ok()),
        now,
    )
}

/// The structured data a notification carries (the hook payload; metadata only, never captured
/// screen content).
pub struct Notification {
    pub agent: String,
    pub pane: String,
    /// The trigger word (`blocked`/`done`). For a `done` fire this is the transition word, not the
    /// landing token (`idle`), so a hook can tell finished from blocked.
    pub state: String,
    pub detail: Option<String>,
    pub session: Option<String>,
    /// `session:window.pane` locator.
    pub locator: String,
    pub title: String,
    /// Repo name resolved from the pane's cwd, empty when it is not a checkout (or git is missing).
    pub repo: String,
    /// Branch (the literal `HEAD` when detached), empty when unresolved.
    pub branch: String,
    /// Age of the episode this fire belongs to, in ms: `now - max(@agent_since, @agent_turn_at)`.
    /// The turn instant, not the state transition — a second completion inside one idle run does
    /// not move `@agent_since`, so reading it alone would report the whole idle run's age. A hook's
    /// direct fire lands on its own transition, so it reads 0; the daemon's reads its dispatch
    /// latency.
    pub since_ms: u64,
    /// The episode this fire belongs to, as an **absolute** epoch-ms stamp: `max(@agent_since,
    /// @agent_turn_at)`, the same instant `AgentRow::episode_at()` reports and `wait --since`
    /// compares against. [`Self::since_ms`] is that instant's *age*, and an age cannot be compared
    /// for equality against a stored stamp, so a sink cannot collapse on it: two fires for one
    /// episode carry two different `since_ms` and look like two episodes. This is the field a
    /// collapse key is built from (`apns-collapse-id` and friends); `since_ms` stays for display.
    pub episode_ms: u64,
    /// The pane's stored context-utilization percent, `None` when the agent reports none.
    pub context_pct: Option<u8>,
}

/// Whether the pane title may leave the host on this fire. Carried as a named type rather than a
/// bare `bool` because it is threaded through four writers and a silent argument transposition would
/// be a privacy leak, not a test failure. Built from `[notify] include_title`, default
/// [`Redact`](TitlePolicy::Redact).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TitlePolicy {
    /// Drop the title from every carrier: the payload's `title` key, the audit line and `TMA_TITLE`.
    Redact,
    /// The user opted back in with `[notify] include_title = true`.
    Carry,
}

impl TitlePolicy {
    fn from_include(include_title: bool) -> TitlePolicy {
        if include_title {
            TitlePolicy::Carry
        } else {
            TitlePolicy::Redact
        }
    }

    fn carries(self) -> bool {
        self == TitlePolicy::Carry
    }
}

/// Build the notification both fire paths send: the ONE place payload fields are derived from a pane
/// read, so a daemonless `tma event` fire and the daemon's dispatch produce identical payloads for the
/// same transition. `agent` is the caller's authoritative name (the manifest name on the hook path,
/// the stamped `@agent_name` in the daemon); `since` is the episode start this fire belongs to.
pub fn notification_for(
    rec: &PaneRecord,
    agent: &str,
    state_word: &str,
    detail: Option<String>,
    session: Option<String>,
    since: u64,
    now: u64,
) -> Notification {
    // Resolved only here, on the rare fire path: the resolver memoizes per cwd and latches a missing
    // git, so the daemon pays one bounded `git rev-parse` per repo per TTL and a one-shot hook at most one.
    let repo = rec.cwd.as_deref().and_then(crate::repo::resolve);
    Notification {
        agent: agent.to_string(),
        pane: rec.pane_id.clone(),
        state: state_word.to_string(),
        detail,
        session,
        locator: rec.locator(),
        title: rec.title.clone(),
        repo: repo
            .as_ref()
            .map(|r| r.repo_name.clone())
            .unwrap_or_default(),
        branch: repo.map(|r| r.branch).unwrap_or_default(),
        since_ms: now.saturating_sub(since),
        // The same instant `since_ms` is the age of, kept absolute. Every caller must pass the
        // episode start the ROW reports (`episode_at()` = `max(@agent_since, @agent_turn_at)`), or
        // a sink keying on this disagrees with `tma ls --json` for the same episode.
        episode_ms: since,
        context_pct: rec
            .options
            .get(opt::CONTEXT_PCT)
            .and_then(|v| v.parse().ok()),
    }
}

/// Fire one notification: the `display-message` baseline (always), the opted-in tty sinks, then the
/// resolved hook `command` (fire-and-forget, env vars + JSON on stdin). Every sink's failure is
/// swallowed. Returns the spawned child (if any) for the caller to reap. The tty sinks ring only
/// here, and both fire paths gate on their write-before-fire marker before calling this, so they
/// inherit that dedup.
pub fn fire(
    tmux: &Tmux,
    n: &Notification,
    command: Option<&str>,
    sinks: &NotifySinks,
) -> Option<Child> {
    // Baseline `display-message`, best-effort: no attached client just means nowhere to show it, and
    // the durable record is the marker, not this line.
    let msg = if n.title.is_empty() {
        format!("tma: {} {} in {}", n.agent, n.state, n.pane)
    } else {
        format!("tma: {} {} — {}", n.agent, n.state, n.title)
    };
    let _ = tmux.message(&msg);

    // Bell companion: ring the firing pane's tty (best-effort). `display-message` above does not
    // ring, only in-pane output does, so the bell is a distinct sink.
    if sinks.bell {
        tmux.ring_bell(&n.pane);
    }

    // OSC 9 companion: the same tty, so it reaches the emulator at the far end of an ssh/mosh/tmate
    // session. The text is agent + state only — never the pane title, which the pane's own program
    // controls and could stuff with escape bytes.
    if sinks.osc {
        tmux.osc_notify(&n.pane, &format!("{} {}", n.agent, n.state));
    }

    // One decision for all three carriers below; `display-message` above already ran with the real
    // title, because it is host-local and never leaves the machine.
    let title = TitlePolicy::from_include(sinks.include_title);

    // Audit line, written before the command so a hung or missing sink cannot cost the record. Both
    // fire paths pass through here, so the log holds every fired notification either way.
    if let Some(path) = &sinks.log {
        log::append(path, &log_line(n, crate::now_ms(), title));
    }

    command.and_then(|cmd| spawn_command(cmd, n, title))
}

/// Spawn the hook command via `sh -c`, delivering the payload two ways (a hook reads whichever): as
/// `TMA_*` env vars and as a compact JSON object on stdin. The write cannot block the loop (the
/// payload is far under a pipe buffer), and stdin is dropped to signal EOF. Fire-and-forget; the
/// caller reaps the child. A command that cannot even be spawned leaves a [`failure`] marker: it
/// delivers nothing and would otherwise be silent forever.
fn spawn_command(cmd: &str, n: &Notification, title: TitlePolicy) -> Option<Child> {
    let mut command = hook_command(cmd, n, title);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            failure::record(cmd, &format!("spawn failed: {err}"), crate::now_ms());
            return None;
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        let _ = stdin.write_all(payload_json(n, title).as_bytes());
        // `stdin` drops here → EOF for the child's reader.
    }
    Some(child)
}

/// The `sh -c` invocation carrying one notification's `TMA_*` env, with stdio left to the caller (the
/// fire path discards output; `tma debug notify-test` keeps stderr). Shared so a test fire runs the
/// command in exactly the environment a real one does.
fn hook_command(cmd: &str, n: &Notification, title: TitlePolicy) -> Command {
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(cmd)
        .env("TMA_AGENT", &n.agent)
        .env("TMA_PANE", &n.pane)
        .env("TMA_STATE", &n.state)
        .env("TMA_LOCATOR", &n.locator)
        .env("TMA_SINCE_MS", n.since_ms.to_string())
        .env("TMA_EPISODE_MS", n.episode_ms.to_string());
    // `TMA_TITLE` is the channel a shell one-liner actually interpolates, so it redacts with the
    // payload, not after it. Unset (not empty) when redacted, so `${TMA_TITLE:-}` degrades cleanly.
    if title.carries() {
        command.env("TMA_TITLE", &n.title);
    } else {
        command.env_remove("TMA_TITLE");
    }
    if let Some(d) = &n.detail {
        command.env("TMA_DETAIL", d);
    }
    if let Some(s) = &n.session {
        command.env("TMA_SESSION", s);
    }
    // Unresolved repo metadata and an absent gauge are omitted rather than exported empty, so a hook
    // can test with `${TMA_REPO:-}` (the JSON keeps them present, empty/null).
    if !n.repo.is_empty() {
        command.env("TMA_REPO", &n.repo);
    }
    if !n.branch.is_empty() {
        command.env("TMA_BRANCH", &n.branch);
    }
    if let Some(pct) = n.context_pct {
        command.env("TMA_CONTEXT_PCT", pct.to_string());
    }
    command
}

/// The `schema` version of the notify hook stdin payload. Additive keys keep it; a breaking
/// rename/removal bumps it (and the pin test with it). Kept the first key so a reader sees it first.
///
/// `2`: `title` became conditional on `[notify] include_title` (default off) and stopped being sent
/// to third-party carriers, and the absolute `episode_ms` was added beside the `since_ms` age. The
/// title removal is the breaking half. The completion payload has its own
/// [`COMPLETION_PAYLOAD_SCHEMA`] and did not move.
const NOTIFY_PAYLOAD_SCHEMA: i64 = 2;

/// The JSON stdin payload (correctly escaped via the shared writer): the notification metadata.
/// The top-level key set is pinned by `payload_json_pins_the_exact_key_set` (a drift guard).
fn payload_json(n: &Notification, title: TitlePolicy) -> String {
    let mut j = JsonWriter::new();
    j.begin_object();
    j.number("schema", NOTIFY_PAYLOAD_SCHEMA);
    write_payload_fields(&mut j, n, title);
    j.end_object();
    j.finish()
}

/// One `[notify] log` line: the payload plus the `at` epoch the fire happened at. The extra key is
/// what makes the file an audit record rather than a pile of undated payloads; every other key is
/// the payload's, written by the same code, so the two cannot drift.
fn log_line(n: &Notification, at: u64, title: TitlePolicy) -> String {
    let mut j = JsonWriter::new();
    j.begin_object();
    j.number("schema", NOTIFY_PAYLOAD_SCHEMA);
    j.number("at", at as i64);
    write_payload_fields(&mut j, n, title);
    j.end_object();
    j.finish()
}

/// The payload's fields, after `schema`. Shared by the stdin payload and the log line — which is
/// why the log redacts with the payload rather than after it. The log is also the file most likely
/// to be pasted into an issue, so the standing rule is: no field enters this writer that is not safe
/// world-readable.
fn write_payload_fields(j: &mut JsonWriter, n: &Notification, title: TitlePolicy) {
    j.string("agent", &n.agent);
    j.string("pane", &n.pane);
    j.string("state", &n.state);
    match &n.detail {
        Some(d) => j.string("detail", d),
        None => j.null("detail"),
    }
    match &n.session {
        Some(s) => j.string("session", s),
        None => j.null("session"),
    }
    j.string("locator", &n.locator);
    // The pane title is the one payload field whose content the pane's own program controls, and it
    // routinely holds a branch name, a repo path or a prompt fragment. Absent unless the user opted
    // in, so a reader can tell "redacted" from "this pane has no title" (which writes `""`).
    if title.carries() {
        j.string("title", &n.title);
    }
    // Additive under the schema rule. `repo`/`branch` are empty (never absent) when the pane's cwd
    // is not a checkout, so a reader's key lookup never depends on the resolve succeeding.
    j.string("repo", &n.repo);
    j.string("branch", &n.branch);
    j.number("since_ms", n.since_ms as i64);
    j.number("episode_ms", n.episode_ms as i64);
    match n.context_pct {
        Some(pct) => j.number("context_pct", pct as i64),
        None => j.null("context_pct"),
    }
}

// ---- context_high notify -----------------------------------------------------------------------

/// The trigger word carried in a `context_high` notification's payload `state` field, so a
/// hook can tell a context-utilization alert from a `blocked`/`done` one. Pinned by ACTIONS.md.
pub const CONTEXT_HIGH_WORD: &str = "context_high";

/// What the `context_high` armed-flag decision resolves to for one observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextNotify {
    /// Armed and a real observation landed at/above the threshold: arm the marker and fire.
    Fire,
    /// Already fired and a real observation dipped below `threshold - 10`: clear the marker.
    Rearm,
    /// Hold the flag as-is (below threshold while armed, at/above while fired, a null observation, or
    /// a torn pair deferred to the next cycle).
    Idle,
}

/// The `context_high` armed-flag decision: given the stored gauge (`pct`, `at`) and the
/// `@agent_context_notified_at` marker (absent = armed, present = fired), decide for one observation.
/// Fires only on a real observation at/above `threshold` while armed; rearms only below
/// `threshold - 10` while fired; a null observation (`pct = None`) holds the flag either way, so a
/// `/compact` clear neither rings the bell nor wedges it. Honors the torn-pair rule: a `pct` present
/// with its `at` not yet written is an in-progress chain, deferred (returns [`ContextNotify::Idle`]).
pub fn decide_context_high(
    pct: Option<u8>,
    at: Option<u64>,
    marker: Option<u64>,
    threshold: u8,
) -> ContextNotify {
    // Torn pair: `@agent_context_at` is written last, so a pct with no at is a chain caught mid-flight.
    if pct.is_some() && at.is_none() {
        return ContextNotify::Idle;
    }
    let Some(pct) = pct else {
        return ContextNotify::Idle; // null observation: the flag holds until a real value decides it
    };
    match marker {
        None if pct >= threshold => ContextNotify::Fire,
        Some(_) if pct < threshold.saturating_sub(10) => ContextNotify::Rearm,
        _ => ContextNotify::Idle,
    }
}

/// Evaluate and apply the `context_high` decision for one pane, the single path both the
/// daemonless intake and the daemon reconcile call so they cannot diverge. The gauge, its armed flag,
/// and the payload identity all come off the one `rec` both callers already hold. On
/// [`ContextNotify::Fire`] it arms the marker (guarded set-from-absent + read-back) and fires one
/// notification only when it won the race; on [`ContextNotify::Rearm`] it clears the marker; otherwise
/// nothing. Best-effort: every tmux failure is swallowed. Returns the notify child to reap, or `None`.
pub fn evaluate_context_high(
    tmux: &Tmux,
    guarded: bool,
    rec: &PaneRecord,
    threshold: u8,
    command: Option<&str>,
    sinks: &NotifySinks,
    now: u64,
) -> Option<Child> {
    let num = |key: &str| rec.options.get(key).and_then(|v| v.parse::<u64>().ok());
    let pct = rec
        .options
        .get(opt::CONTEXT_PCT)
        .and_then(|v| v.parse::<u8>().ok());
    match decide_context_high(
        pct,
        num(opt::CONTEXT_AT),
        num(opt::CONTEXT_NOTIFIED_AT),
        threshold,
    ) {
        ContextNotify::Fire => {
            // Fire iff the guarded set-from-absent won: a loser reads the winner's marker and stays
            // silent, so concurrent firers resolve to one bell.
            match tma_tmux::stamp::arm_context_notify(tmux, &rec.pane_id, now, guarded) {
                // Arm first, then check the mute: a muted alert is consumed like a fired one, so
                // the gauge has to dip below the rearm band before it can ring again.
                Ok(true) if muted(rec, now) => None,
                Ok(true) => {
                    // The episode start the ROW reports: `max(@agent_since, @agent_turn_at)`, the
                    // same instant `AgentRow::episode_at()` yields and the daemon's dispatch passes.
                    // Reading `@agent_since` alone disagreed with the row on any pane with a
                    // recorded turn end, so `episode_ms` named an episode `tma ls --json` did not
                    // — and a sink keying on it could not collapse the two. Neither stamped ⇒ `now`,
                    // which is the pre-existing fallback (a zero-age episode).
                    let episode = num(opt::SINCE)
                        .into_iter()
                        .chain(num(opt::TURN_AT))
                        .max()
                        .unwrap_or(now);
                    let n = notification_for(
                        rec,
                        rec.options.get(opt::NAME).map(String::as_str).unwrap_or(""),
                        CONTEXT_HIGH_WORD,
                        None,
                        rec.options
                            .get(opt::SESSION)
                            .filter(|v| !v.is_empty())
                            .cloned(),
                        episode,
                        now,
                    );
                    fire(tmux, &n, command, sinks)
                }
                _ => None, // lost the race, or a tmux error: no fire
            }
        }
        ContextNotify::Rearm => {
            let _ = tma_tmux::stamp::rearm_context_notify(tmux, &rec.pane_id);
            None
        }
        ContextNotify::Idle => None,
    }
}

// ---- detached-action completion ----------------------------------------------------------------

/// The completion notification a detached action's supervisor fires on child exit. Its own
/// pinned contract, distinct from [`Notification`]: a completion has no `state`, and its pane may
/// already be gone (`locator` is then null). It rides the same fire *dispatch* (a capped child) but
/// carries no dedup marker — a completion is single-shot per spawn.
pub struct CompletionNotification {
    pub action: String,
    pub pane: String,
    pub agent: String,
    /// The `outcome` token the child finished with (`exited` / `timeout` / `error`).
    pub outcome: String,
    /// The child's own exit code for `exited`; `None` for a deadline kill or a spawn failure.
    pub exit_code: Option<i32>,
    /// `session:window.pane` locator, or `None` when the pane is already gone.
    pub locator: Option<String>,
    /// The supervisor's nonce-conditional lock clear failed. Surfaced only when `true` (the
    /// key is absent on success), so a consumer sees the otherwise-silent release failure; a dead
    /// pane's failing option write correlates with a null `locator`.
    pub lock_release_failed: bool,
}

/// Fire one completion notification: the best-effort `display-message` baseline, the opted-in tty
/// sinks, the audit line, then the optional hook `command` (fire-and-forget, env vars + JSON on
/// stdin). Returns the spawned child (if any) for the caller to reap — it is the reap that judges
/// the command, via [`failure::record_exit`]. Mirrors [`fire`] sink for sink, so a completion is as
/// visible as a state change.
pub fn fire_completion(
    tmux: &Tmux,
    c: &CompletionNotification,
    command: Option<&str>,
    sinks: &NotifySinks,
) -> Option<Child> {
    let _ = tmux.message(&format!("tma: {} {} in {}", c.action, c.outcome, c.pane));

    // The tty sinks reach the emulator at the far end of an ssh/mosh session, where a
    // `display-message` on a detached client would land nowhere. The text is action + outcome only,
    // both closed vocabularies, so no pane-controlled bytes ride out.
    if sinks.bell {
        tmux.ring_bell(&c.pane);
    }
    if sinks.osc {
        tmux.osc_notify(&c.pane, &format!("{} {}", c.action, c.outcome));
    }

    // Audit line before the command, so a hung or missing hook cannot cost the record.
    if let Some(path) = &sinks.log {
        log::append(path, &completion_log_line(c, crate::now_ms()));
    }

    command.and_then(|cmd| spawn_completion_command(cmd, c))
}

/// Spawn the completion hook via `sh -c`, delivering the payload as `TMA_*` env vars and as the
/// pinned JSON object on stdin (a hook reads whichever). Fire-and-forget; the caller reaps. A
/// command that cannot even be spawned leaves a [`failure`] marker, as on the state path: the
/// supervisor's stderr is `/dev/null`, so this is its only channel.
fn spawn_completion_command(cmd: &str, c: &CompletionNotification) -> Option<Child> {
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(cmd)
        .env("TMA_ACTION", &c.action)
        .env("TMA_PANE", &c.pane)
        .env("TMA_AGENT", &c.agent)
        .env("TMA_OUTCOME", &c.outcome)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(code) = c.exit_code {
        command.env("TMA_EXIT_CODE", code.to_string());
    }
    if let Some(l) = &c.locator {
        command.env("TMA_LOCATOR", l);
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            failure::record(cmd, &format!("spawn failed: {err}"), crate::now_ms());
            return None;
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        let _ = stdin.write_all(completion_payload_json(c).as_bytes());
    }
    Some(child)
}

/// The `schema` version of the completion payload; additive keys keep it `1`.
const COMPLETION_PAYLOAD_SCHEMA: i64 = 1;

/// The completion JSON stdin payload. Top-level key set pinned by
/// `completion_payload_pins_the_exact_key_set`: `schema`, `action`, `pane`, `agent`, `outcome`,
/// `exit_code` (number or null), `locator` (string or null when the pane is gone), plus the additive
/// `lock_release_failed` (`true`, present only when the supervisor's lock clear failed).
fn completion_payload_json(c: &CompletionNotification) -> String {
    let mut j = JsonWriter::new();
    j.begin_object();
    j.number("schema", COMPLETION_PAYLOAD_SCHEMA);
    write_completion_fields(&mut j, c);
    j.end_object();
    j.finish()
}

/// The completion audit line: the stdin payload with `at` inserted right after `schema`, matching
/// [`log_line`]'s shape so one reader parses both kinds of record. A completion line carries
/// `action`/`outcome` where a state line carries `state`, which is how a reader tells them apart.
fn completion_log_line(c: &CompletionNotification, at: u64) -> String {
    let mut j = JsonWriter::new();
    j.begin_object();
    j.number("schema", COMPLETION_PAYLOAD_SCHEMA);
    j.number("at", at as i64);
    write_completion_fields(&mut j, c);
    j.end_object();
    j.finish()
}

/// The completion payload's fields, after `schema`. Shared by the stdin payload and the log line.
fn write_completion_fields(j: &mut JsonWriter, c: &CompletionNotification) {
    j.string("action", &c.action);
    j.string("pane", &c.pane);
    j.string("agent", &c.agent);
    j.string("outcome", &c.outcome);
    match c.exit_code {
        Some(code) => j.number("exit_code", code as i64),
        None => j.null("exit_code"),
    }
    match &c.locator {
        Some(l) => j.string("locator", l),
        None => j.null("locator"),
    }
    // Additive under the schema-1 rule: emitted only on a failed release, absent on success, so the
    // pinned success-case key set (and its drift test) is unchanged.
    if c.lock_release_failed {
        j.bool("lock_release_failed", true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigger_for_maps_noteworthy_landings() {
        // Entering blocked → blocked; a working→idle completion (noteworthy idle) → done.
        assert_eq!(
            trigger_for(AgentState::Blocked, true),
            Some(NotifyTrigger::Blocked)
        );
        assert_eq!(
            trigger_for(AgentState::Idle, true),
            Some(NotifyTrigger::Done)
        );
        // A non-noteworthy landing (re-block, plain idle, a register) is never notifiable.
        assert_eq!(trigger_for(AgentState::Blocked, false), None);
        assert_eq!(trigger_for(AgentState::Idle, false), None);
        // Working / unknown never notify regardless.
        assert_eq!(trigger_for(AgentState::Working, true), None);
        assert_eq!(trigger_for(AgentState::Unknown, true), None);
    }

    /// A fully-populated notification (every optional present), the payload tests' base.
    fn notification(title: &str) -> Notification {
        Notification {
            agent: "claude".to_string(),
            pane: "%3".to_string(),
            state: "blocked".to_string(),
            detail: Some("permission".to_string()),
            session: Some("sess-1".to_string()),
            locator: "work:1.0".to_string(),
            title: title.to_string(),
            repo: "tmux-agents".to_string(),
            branch: "main".to_string(),
            since_ms: 40,
            episode_ms: 1_699_999_999_960,
            context_pct: Some(72),
        }
    }

    #[test]
    fn payload_json_carries_metadata_only() {
        let json = payload_json(&notification("vim \"file\""), TitlePolicy::Carry);
        assert!(json.contains(r#""agent":"claude""#));
        assert!(json.contains(r#""state":"blocked""#));
        assert!(json.contains(r#""locator":"work:1.0""#));
        // Correctly escaped (no screen content — only metadata).
        assert!(json.contains(r#""title":"vim \"file\"""#));
    }

    #[test]
    fn payload_json_nulls_absent_optionals() {
        let mut n = notification("");
        n.detail = None;
        n.session = None;
        n.context_pct = None;
        // An unresolved repo is empty rather than null: the key is always a string.
        n.repo = String::new();
        n.branch = String::new();
        let json = payload_json(&n, TitlePolicy::Redact);
        assert!(json.contains(r#""detail":null"#));
        assert!(json.contains(r#""session":null"#));
        assert!(json.contains(r#""context_pct":null"#));
        assert!(json.contains(r#""repo":"","branch":"""#));
    }

    #[test]
    fn payload_json_pins_the_exact_key_set() {
        // Drift pin: the notify hook payload is a documented contract, so adding/removing/reordering
        // a key changes this serialization and forces a deliberate `schema` bump + doc update.
        // Schema 2: no `title` by default, and the absolute `episode_ms` beside the `since_ms` age.
        assert_eq!(
            payload_json(&notification("vim"), TitlePolicy::Redact),
            r#"{"schema":2,"agent":"claude","pane":"%3","state":"blocked","detail":"permission","session":"sess-1","locator":"work:1.0","repo":"tmux-agents","branch":"main","since_ms":40,"episode_ms":1699999999960,"context_pct":72}"#
        );
        // `[notify] include_title = true` restores the key, in its historical position.
        assert_eq!(
            payload_json(&notification("vim"), TitlePolicy::Carry),
            r#"{"schema":2,"agent":"claude","pane":"%3","state":"blocked","detail":"permission","session":"sess-1","locator":"work:1.0","title":"vim","repo":"tmux-agents","branch":"main","since_ms":40,"episode_ms":1699999999960,"context_pct":72}"#
        );
    }

    /// A-512. The pane title is the one payload field the pane's own program controls, and it
    /// routinely holds a branch name, a repo path or a prompt fragment. `notify.command` pipes this
    /// payload to whatever the user configured — ntfy, Pushover, an Apple Shortcut — so before this
    /// it reached that service's operator on every fire.
    #[test]
    fn the_pane_title_never_reaches_a_carrier_by_default() {
        let n = notification("feat/ACME-1234-rotate-customer-api-keys");
        let json = payload_json(&n, TitlePolicy::Redact);
        assert!(
            !json.contains("ACME-1234"),
            "the title leaked into the payload: {json}"
        );
        assert!(
            !json.contains(r#""title""#),
            "the key itself must be absent"
        );
        // The opt-in is what restores it, and nothing else.
        assert!(payload_json(&n, TitlePolicy::Carry).contains("ACME-1234"));
        // A default-constructed config redacts: the back-compat lever is opt-IN, so an existing
        // installation stops shipping titles on upgrade rather than waiting for a checkbox.
        assert_eq!(
            TitlePolicy::from_include(crate::config::NotifySinks::default().include_title),
            TitlePolicy::Redact
        );
    }

    /// A-514. The payload's writer is also the log's writer, so the audit line redacts WITH the
    /// payload rather than after it — and the log is the file most likely to be pasted into an issue.
    /// `TMA_TITLE` is the third carrier: it is the channel a shell one-liner actually interpolates,
    /// so redacting only the JSON would leave the title in the variable the hook reads.
    #[test]
    fn the_log_line_and_tma_title_redact_with_the_payload() {
        let n = notification("feat/ACME-1234-rotate-customer-api-keys");

        let line = log_line(&n, 1_700_000_000_000, TitlePolicy::Redact);
        assert!(!line.contains("ACME-1234"), "the title leaked into the log");

        let redacted = hook_command("true", &n, TitlePolicy::Redact);
        let env: Vec<_> = redacted.get_envs().collect();
        assert!(
            !env.iter().any(|(k, v)| *k == "TMA_TITLE" && v.is_some()),
            "TMA_TITLE must be unset, not empty: {env:?}"
        );
        // Every other field still rides, so a hook keeps something to format.
        let value = |key: &str| {
            env.iter()
                .find(|(k, _)| *k == key)
                .and_then(|(_, v)| *v)
                .map(|v| v.to_string_lossy().to_string())
        };
        assert_eq!(value("TMA_AGENT").as_deref(), Some("claude"));
        assert_eq!(value("TMA_LOCATOR").as_deref(), Some("work:1.0"));

        // And the opt-in restores all three together.
        assert!(log_line(&n, 0, TitlePolicy::Carry).contains("ACME-1234"));
        let carried = hook_command("true", &n, TitlePolicy::Carry);
        assert!(carried.get_envs().any(|(k, v)| k == "TMA_TITLE"
            && v.is_some_and(|v| v.to_string_lossy().contains("ACME-1234"))));
    }

    /// A-515. `episode_ms` is an ABSOLUTE epoch-ms stamp, not an age. `since_ms` is the age and it
    /// survives; the two must not be confused, because only the absolute one can be compared for
    /// equality against a stored stamp — which is what a sink's collapse key needs.
    #[test]
    fn episode_ms_is_absolute_and_since_ms_survives_beside_it() {
        let rec = pane_record(None);
        let episode = 1_700_000_000_000;
        let now = episode + 250;
        let n = notification_for(&rec, "claude", "blocked", None, None, episode, now);

        assert_eq!(
            n.episode_ms, episode,
            "episode_ms is the stamp, not its age"
        );
        assert_eq!(n.since_ms, 250, "since_ms is still the age");
        assert_ne!(n.episode_ms, n.since_ms);

        let json = payload_json(&n, TitlePolicy::Redact);
        assert!(json.contains(r#""episode_ms":1700000000000"#), "{json}");
        assert!(json.contains(r#""since_ms":250"#), "{json}");

        // Two fires inside ONE episode: the ages differ, the episode stamp does not. That is exactly
        // why an age cannot serve as a collapse key and this field had to be added.
        let later = notification_for(&rec, "claude", "blocked", None, None, episode, now + 9_000);
        assert_ne!(later.since_ms, n.since_ms);
        assert_eq!(later.episode_ms, n.episode_ms);

        // Exported to a shell hook under its own name.
        let cmd = hook_command("true", &n, TitlePolicy::Redact);
        assert!(cmd.get_envs().any(|(k, v)| k == "TMA_EPISODE_MS"
            && v.is_some_and(|v| v.to_string_lossy() == episode.to_string())));
    }

    #[test]
    fn log_line_is_the_payload_plus_the_fire_time() {
        // The audit line must stay parseable by whatever reads the payload: same keys, same order,
        // with `at` inserted right after `schema` so a reader sees when before what.
        let n = notification("vim");
        let line = log_line(&n, 1_700_000_000_000, TitlePolicy::Redact);
        assert_eq!(
            line,
            r#"{"schema":2,"at":1700000000000,"agent":"claude","pane":"%3","state":"blocked","detail":"permission","session":"sess-1","locator":"work:1.0","repo":"tmux-agents","branch":"main","since_ms":40,"episode_ms":1699999999960,"context_pct":72}"#
        );
        // Every payload key survives verbatim in the line (the two share one writer) — including,
        // critically, the redaction: the log must not carry what the payload dropped.
        let payload = payload_json(&n, TitlePolicy::Redact);
        assert!(line.ends_with(payload.trim_start_matches(r#"{"schema":2,"#)));
    }

    /// A pane record carrying a stored gauge, for the shared builder.
    fn pane_record(cwd: Option<&str>) -> PaneRecord {
        let mut options = std::collections::HashMap::new();
        options.insert(opt::CONTEXT_PCT.to_string(), "81".to_string());
        PaneRecord {
            pane_id: "%3".to_string(),
            pane_pid: 4242,
            session: "work".to_string(),
            window_index: 1,
            pane_index: 0,
            current_command: "claude".to_string(),
            window_activity: 0,
            alternate_on: false,
            scroll_position: None,
            pane_height: 40,
            cwd: cwd.map(str::to_string),
            options,
            window_summary: None,
            session_summary: None,
            title: "a task".to_string(),
        }
    }

    #[test]
    fn notification_for_derives_the_payload_from_one_pane_read() {
        let rec = pane_record(None);
        let n = notification_for(
            &rec,
            "claude",
            "blocked",
            Some("permission".to_string()),
            Some("sess-1".to_string()),
            1_000,
            1_250,
        );
        assert_eq!(n.pane, "%3");
        assert_eq!(n.locator, "work:1.0");
        assert_eq!(n.title, "a task");
        assert_eq!(
            n.context_pct,
            Some(81),
            "the stored gauge rides the payload"
        );
        assert_eq!(n.since_ms, 250, "episode age = now - since");
        // No cwd ⇒ nothing to resolve: the repo labels are empty, never a failure.
        assert!(n.repo.is_empty() && n.branch.is_empty());
    }

    #[test]
    fn notification_for_labels_a_checkout_from_its_cwd() {
        // The pane's cwd drives repo/branch through the memoized resolver, so the payload agrees with
        // it by construction (the crate's own directory is a checkout on any dev machine / CI clone).
        let cwd = env!("CARGO_MANIFEST_DIR");
        let Some(want) = crate::repo::resolve(cwd) else {
            eprintln!("skipping: {cwd} does not resolve as a checkout");
            return;
        };
        let n = notification_for(&pane_record(Some(cwd)), "claude", "done", None, None, 0, 0);
        assert_eq!(n.repo, want.repo_name);
        assert_eq!(n.branch, want.branch);
        // A backward clock step never underflows the episode age.
        assert_eq!(n.since_ms, 0);
    }

    #[test]
    fn completion_payload_pins_the_exact_key_set() {
        // Drift pin: the completion payload is a documented contract, so adding/removing/
        // reordering a key forces a deliberate `schema` bump + doc update.
        let c = CompletionNotification {
            action: "summarize".to_string(),
            pane: "%3".to_string(),
            agent: "claude".to_string(),
            outcome: "exited".to_string(),
            exit_code: Some(0),
            locator: Some("work:1.0".to_string()),
            lock_release_failed: false,
        };
        assert_eq!(
            completion_payload_json(&c),
            r#"{"schema":1,"action":"summarize","pane":"%3","agent":"claude","outcome":"exited","exit_code":0,"locator":"work:1.0"}"#
        );
    }

    #[test]
    fn completion_payload_flags_a_failed_lock_release() {
        // The supervisor's lock clear failed: the additive `lock_release_failed:true` key appears
        // after `locator`, while a clean release omits it entirely (asserted by the pin above).
        let c = CompletionNotification {
            action: "summarize".to_string(),
            pane: "%3".to_string(),
            agent: "claude".to_string(),
            outcome: "exited".to_string(),
            exit_code: Some(0),
            locator: Some("work:1.0".to_string()),
            lock_release_failed: true,
        };
        assert_eq!(
            completion_payload_json(&c),
            r#"{"schema":1,"action":"summarize","pane":"%3","agent":"claude","outcome":"exited","exit_code":0,"locator":"work:1.0","lock_release_failed":true}"#
        );
    }

    /// The completion audit line is the payload with `at` after `schema`, exactly as the state
    /// line is, so one reader parses both records off the same file.
    #[test]
    fn completion_log_line_is_the_payload_plus_the_fire_time() {
        let c = CompletionNotification {
            action: "summarize".to_string(),
            pane: "%3".to_string(),
            agent: "claude".to_string(),
            outcome: "timeout".to_string(),
            exit_code: None,
            locator: None,
            lock_release_failed: false,
        };
        let line = completion_log_line(&c, 1_700_000_000_000);
        assert_eq!(
            line,
            r#"{"schema":1,"at":1700000000000,"action":"summarize","pane":"%3","agent":"claude","outcome":"timeout","exit_code":null,"locator":null}"#
        );
        let payload = completion_payload_json(&c);
        assert!(line.ends_with(payload.trim_start_matches(r#"{"schema":1,"#)));
    }

    /// A completion fires its sinks and its hook is judged on the reap, exactly as a state fire is.
    /// Before this, a broken completion hook left no artifact anywhere: the supervisor's stderr is
    /// `/dev/null`, nothing waited on the exit, and no sink but `display-message` ran.
    #[test]
    fn a_completion_rides_the_log_sink_and_its_hook_exit_is_recorded() {
        // The failure marker is one file per user, so a sibling test's fire would otherwise
        // overwrite or clear the record this one asserts on.
        let _marker = failure::PrivateMarker::new("completion");
        let log = std::env::temp_dir().join(format!(
            "tma_completion_log_{}_{}.jsonl",
            std::process::id(),
            crate::now_ms()
        ));
        let sinks = NotifySinks {
            log: Some(log.clone()),
            ..NotifySinks::default()
        };
        let c = CompletionNotification {
            action: "summarize".to_string(),
            pane: "%3".to_string(),
            agent: "claude".to_string(),
            outcome: "exited".to_string(),
            exit_code: Some(3),
            locator: Some("work:1.0".to_string()),
            lock_release_failed: false,
        };
        // No server behind this socket: the `display-message` baseline fails, as it does whenever
        // the completion's pane is already gone, and the other sinks must still run.
        let server = tma_tmux::tmux::Server {
            socket_path: Some(std::path::PathBuf::from(
                "/nonexistent/tma-notify-test.sock",
            )),
            ..Default::default()
        };
        let tmux = Tmux::connect(&server);

        let cmd = "exit 3";
        let mut child = fire_completion(&tmux, &c, Some(cmd), &sinks).expect("the hook spawned");
        let status = child.wait().expect("the hook was reaped");
        failure::record_exit(cmd, &status, crate::now_ms());

        let line = std::fs::read_to_string(&log).expect("the audit line was appended");
        assert!(
            line.contains(r#""action":"summarize""#) && line.contains(r#""outcome":"exited""#),
            "the completion record names the action and its outcome, got {line:?}"
        );
        let recorded = failure::last().expect("the failing hook left a marker");
        assert_eq!(recorded.command, cmd);
        assert_eq!(recorded.reason, "exited 3");

        let _ = std::fs::remove_file(&log);
    }

    // ---- context_high armed-flag decision ---------------------------------------------------

    #[test]
    fn context_high_fires_once_then_holds_until_rearm() {
        // Armed (marker absent) + at/above threshold ⇒ fire.
        assert_eq!(
            decide_context_high(Some(80), Some(100), None, 75),
            ContextNotify::Fire
        );
        // Fired (marker present) + still at/above ⇒ hold (no re-fire).
        assert_eq!(
            decide_context_high(Some(80), Some(100), Some(100), 75),
            ContextNotify::Idle
        );
        // Fired + dips into the hysteresis band (75..=65 with threshold 75) ⇒ still no rearm.
        assert_eq!(
            decide_context_high(Some(70), Some(100), Some(100), 75),
            ContextNotify::Idle
        );
        // Fired + dips below threshold - 10 ⇒ rearm.
        assert_eq!(
            decide_context_high(Some(64), Some(100), Some(100), 75),
            ContextNotify::Rearm
        );
    }

    #[test]
    fn context_high_below_threshold_while_armed_holds() {
        assert_eq!(
            decide_context_high(Some(50), Some(100), None, 75),
            ContextNotify::Idle
        );
    }

    #[test]
    fn context_high_null_observation_holds_the_flag() {
        // A null-clear (post-/compact) neither fires while armed nor rearms while fired.
        assert_eq!(
            decide_context_high(None, Some(100), None, 75),
            ContextNotify::Idle
        );
        assert_eq!(
            decide_context_high(None, Some(100), Some(100), 75),
            ContextNotify::Idle
        );
    }

    #[test]
    fn context_high_torn_pair_defers() {
        // pct written but its `at` not yet (the chain caught mid-flight): defer, never fire.
        assert_eq!(
            decide_context_high(Some(90), None, None, 75),
            ContextNotify::Idle
        );
    }

    #[test]
    fn completion_payload_nulls_a_dead_pane_and_a_killed_child() {
        // A deadline-killed child carries no exit code, and a pane that died first has no locator:
        // both render as JSON null.
        let c = CompletionNotification {
            action: "summarize".to_string(),
            pane: "%9".to_string(),
            agent: "codex".to_string(),
            outcome: "timeout".to_string(),
            exit_code: None,
            locator: None,
            lock_release_failed: false,
        };
        let json = completion_payload_json(&c);
        assert!(json.contains(r#""outcome":"timeout""#));
        assert!(json.contains(r#""exit_code":null"#));
        assert!(json.contains(r#""locator":null"#));
        // A clean release never emits the additive key.
        assert!(!json.contains("lock_release_failed"));
    }
}
