//! The action broker (synchronous half): the guarded path from "a surface invoked
//! `tma act <name> --pane %N`" to keystrokes on the pane or a spawned process. The sequence is
//! pinned:
//!
//! 0. **payload**: a `text` action's caller-supplied string satisfies the host-side rules
//!    ([`tma_core::ActionManifest::check_text`]), checked before any tmux command runs at all;
//! 1. **identity**: the pane exists and `@agent_name` matches an agent the action applies to;
//! 2. **gate**: the stamped state satisfies `when`, and for a keystroke-sending kind
//!    ([`ActionKind::sends_keystrokes`]) an on-demand single-pane re-verify runs first when the
//!    stamp is older than [`FRESHNESS_MS`] (a stale paint must never fire a blind keystroke);
//! 3. **lock**: acquire the single-flight `@agent_action` lock ([`tma_tmux::lock`]) with an
//!    absolute expiry of `timeout_ms + `[`SLACK_MS`]` and the broker's pid;
//! 4. **act**: re-assert the gate once under the held lock, then deliver the keys or the literal
//!    text (through the `tma-tmux` `send_keys` / `send_text` choke point) or spawn the exec command.
//!    The caller's binder ([`FireArgs::expect_episode_ms`] / [`FireArgs::expect_permission_request`])
//!    is checked against that same under-lock read, between the gate and the effect;
//! 5. **release**: clear the lock nonce-conditionally on *every* synchronous exit path.
//!
//! `--force` skips the `when` gate only, never `requires` and never the lock. A `detach = true`
//! action does not run synchronously: after the same identity/gate/lock sequence the broker
//! hands the lock to a detached supervisor (the same binary, an internal mode; [`supervise`]) and
//! returns `spawned` at once. The supervisor holds the lock for the child's lifetime, kills the
//! process group at `detach_timeout_ms`, then clears the lock and fires the completion notification.
//!
//! The broker takes no daemon: it is tier 2 by construction. All tmux effects go through a
//! [`BrokerIo`] seam so the refusal matrix, the re-verify branch, and the detach handoff are
//! unit-testable with a mock, and the process-spawn half ([`run_exec`]) is a free function testable
//! without tmux.

use tma_core::{
    ActionKind, ActionManifest, AgentState, ApiOp, ApiReply, ContextKeys, FoldConfig, GateInput,
    GateOutcome, HookVerdict, RefusalReason, Requirement, TextRefusal,
};

use tma_tmux::lock::{Acquire, LockError, LockValue};
use tma_tmux::tmux::{Tmux, TmuxError};

use crate::config::ApiSection;
use crate::hook_lane::VerdictWrite;
use crate::http::HttpOutcome;
use crate::manifests::LoadedManifest;

pub mod audit;
mod exec;
mod surface;
mod tmux_io;

use audit::{ActObserved, AuditCtx};
use exec::{assemble_env, run_exec, ExecOutcome};
pub use exec::{supervise, SuperviseParams};
pub use surface::{dry_run, list_fireability, ContextValue, DryGate, DryRun, Effect, ListVerdict};
pub use tmux_io::TmuxBroker;

/// Keys-action freshness bound: a `@agent_stamped_at` older than this forces one
/// on-demand detection cycle before gating, so a stale surface paint cannot fire a blind keystroke.
/// The status-line cadence (~1 s) plus slack.
pub const FRESHNESS_MS: u64 = 3_000;

/// Lock-expiry slack added to `timeout_ms`: the absolute expiry is the invocation's
/// deadline plus this, so the nonce-conditional release normally lands well before expiry and a
/// SIGKILLed broker's lock is still reclaimable at a bounded time.
pub const SLACK_MS: u64 = 5_000;

/// The result of a synchronous `tma act`, carrying the closed `outcome` vocabulary. The exit-code and
/// reason mapping live here (the broker decides, the CLI only formats).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActResult {
    pub action: String,
    pub pane: String,
    pub outcome: Outcome,
}

/// The closed `outcome` vocabulary. `spawned` is the detached path; every other
/// value is reachable synchronously.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Keystrokes were delivered: a `keys` sequence, or a `text` action's wrapped literal string.
    Sent,
    /// An API-channel answer was delivered (2xx). Distinct from `sent` (keys) so a pinned
    /// meaning does not silently change under scripts.
    Replied,
    /// A synchronous exec child finished; carries its own exit code (passed through verbatim).
    Exited(i32),
    /// A detached supervisor was launched; the lock is now its responsibility.
    Spawned,
    /// A synchronous exec child was killed at `timeout_ms`.
    Timeout,
    /// A gate or the single-flight lock refused before any effect; carries which.
    Refused(Refusal),
    /// The act's target disappeared mid-act; carries which target ([`Gone`]).
    Vanished(Gone),
    /// A broker runtime failure (a tmux error, a spawn failure, or a not-yet-supported path).
    Error(String),
}

impl Outcome {
    /// The `outcome` token for the `--json` result. Closed vocabulary, drift-tested.
    pub const fn token(&self) -> &'static str {
        match self {
            Outcome::Sent => "sent",
            Outcome::Replied => "replied",
            Outcome::Exited(_) => "exited",
            Outcome::Spawned => "spawned",
            Outcome::Timeout => "timeout",
            Outcome::Refused(_) => "refused",
            Outcome::Vanished(_) => "vanished",
            Outcome::Error(_) => "error",
        }
    }
}

/// Which of an act's two targets went away. `vanished` is one outcome token for two very different
/// events, and only this distinguishes them: an API 404 means the permission request was spent, on
/// a pane that is still perfectly alive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gone {
    /// tmux reports the pane no longer exists.
    Pane,
    /// The API server answered `404`: the permission request was already answered or withdrawn.
    Request,
}

impl Gone {
    /// The `reason` token, in the same closed vocabulary as [`Refusal::token`].
    pub const fn token(self) -> &'static str {
        match self {
            Gone::Pane => "pane-gone",
            Gone::Request => "request-gone",
        }
    }
}

/// Why the broker refused before acting. The gate reasons plus the broker-time verdicts that are
/// not part of [`RefusalReason`]: the lock, the caller's two binder expectations, and the `text`
/// payload rules, which refuse earlier than any of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// A gate refusal: `wrong-agent` / `no-coverage` / `requires-unmet` / `gated`.
    Gate(RefusalReason),
    /// The single-flight lock is held by a live, unexpired holder.
    Locked,
    /// [`FireArgs::expect_episode_ms`] named an episode the pane is no longer in.
    EpisodeChanged,
    /// [`FireArgs::expect_permission_request`] named a request the pane no longer carries.
    RequestGone,
    /// The caller's `text` payload broke a host-side rule: `sigil` / `control-bytes` /
    /// `too-long` / `empty`.
    Payload(TextRefusal),
}

impl Refusal {
    /// The reason token, matching the fireability vocabulary. `request-gone` is deliberately the
    /// same word [`Gone::Request`] uses; the `outcome` beside it says which of the two happened.
    pub const fn token(self) -> &'static str {
        match self {
            Refusal::Gate(r) => r.token(),
            Refusal::Locked => "locked",
            Refusal::EpisodeChanged => "episode-changed",
            Refusal::RequestGone => "request-gone",
            Refusal::Payload(r) => r.token(),
        }
    }
}

impl ActResult {
    /// The process exit code. The reserved band (`3`/`4`/`5`/`124`) is pre-spawn broker
    /// verdicts; a synchronous exec child's own code passes through verbatim and may itself land in
    /// that band, which is why scripted consumers branching beyond success use the `--json` result.
    pub const fn exit_code(&self) -> i32 {
        match &self.outcome {
            Outcome::Sent | Outcome::Replied | Outcome::Spawned => 0,
            Outcome::Exited(code) => *code,
            Outcome::Timeout => 124,
            // A payload refusal exits with the gate refusals: the world (here, the string) has to
            // change before this fire can land, which is exactly what 4 means.
            Outcome::Refused(
                Refusal::Gate(_)
                | Refusal::EpisodeChanged
                | Refusal::RequestGone
                | Refusal::Payload(_),
            ) => 4,
            Outcome::Refused(Refusal::Locked) => 5,
            Outcome::Vanished(_) => 3,
            Outcome::Error(_) => 1,
        }
    }

    /// How alarming this outcome is, for aggregating a fan-out (`tma act --all` reports the exit
    /// code of its worst result). Ordered least to most: acted; a refusal that would pass on a
    /// retry (`locked`); a refusal that needs a different world (the gate reasons); the target
    /// going away; a child that ran out of time; a child that failed; a broker failure.
    pub const fn severity(&self) -> u8 {
        match &self.outcome {
            Outcome::Sent | Outcome::Replied | Outcome::Spawned | Outcome::Exited(0) => 0,
            Outcome::Refused(Refusal::Locked) => 1,
            Outcome::Refused(
                Refusal::Gate(_)
                | Refusal::EpisodeChanged
                | Refusal::RequestGone
                | Refusal::Payload(_),
            ) => 2,
            Outcome::Vanished(_) => 3,
            Outcome::Timeout => 4,
            Outcome::Exited(_) => 5,
            Outcome::Error(_) => 6,
        }
    }

    /// The `reason` token: which gate refused, or which target vanished. `None` for every other
    /// outcome, whose `outcome` token already says everything there is to say.
    pub const fn reason(&self) -> Option<&'static str> {
        match &self.outcome {
            Outcome::Refused(r) => Some(r.token()),
            Outcome::Vanished(g) => Some(g.token()),
            _ => None,
        }
    }
}

/// One pane's facts, assembled once from the stamped options plus the live `#{pane_current_path}`.
/// The pure input to the gate and to context env assembly; the broker re-reads it after a
/// re-verify and again under the held lock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaneFacts {
    /// `@agent_name`, `None` when the pane carries no agent stamp (identity mismatch).
    pub agent: Option<String>,
    pub state: AgentState,
    /// `@agent_detail`, `None` when absent/empty.
    pub detail: Option<String>,
    /// `@agent_session`, `None` when absent or failing the read-time charset validation.
    pub session: Option<String>,
    /// `#{pane_current_path}` (TMA_CWD); empty only for a pane with no cwd.
    pub cwd: String,
    /// `@agent_pid` (process-group leader); `None` when absent or the pid-less sentinel `0`.
    pub pid: Option<String>,
    /// `#{pane_title}` (TMA_TITLE); untrusted text, kept inert by env transport.
    pub title: String,
    /// `session:window.pane` locator (TMA_LOCATOR).
    pub locator: String,
    /// `@agent_stamped_at`, `0` when never stamped: the freshness basis.
    pub stamped_at: u64,
    /// `@agent_context_pct`: the stamped context gauge, `None` when absent.
    pub context_pct: Option<u8>,
    /// Whether the agent's manifest declares a context telemetry channel. Distinguishes a
    /// `no-coverage` refusal (false, permanent) from a `gated` one (true, metric merely absent).
    pub context_covered: bool,
    /// `@agent_permission_request`: the pending OpenCode permission id, `None`/empty when no
    /// prompt is open. An `api` `permission-reply` op refuses `requires-unmet` when it is empty.
    pub permission_request: Option<String>,
    /// The resolved API endpoint: the pane-stamped `@agent_api_endpoint`, else the config
    /// `[api.<name>] api_base` fallback. `None` when neither is set (the op refuses `requires-unmet`).
    pub api_endpoint: Option<String>,
    /// `max(@agent_since, @agent_turn_at)`: the episode this fire lands in, the same instant
    /// `AgentRow::episode_at` reports. The audit line's `episode_ms` and the repeat counter's key.
    pub episode_ms: u64,
    /// `@agent_pending_tool` / `@agent_pending_call`: the pending call's tool name and id. Audit
    /// material only; the sibling `@agent_pending_summary` is agent-supplied prose and is never read
    /// here, so it cannot reach the log.
    pub pending_tool: Option<String>,
    pub pending_call: Option<String>,
    /// `@agent_act_repeat`, the stored `<episode_ms>:<action>:<count>` run. Read with the rest of
    /// the options so the counter costs one write rather than a read and a write.
    pub act_repeat: Option<String>,
}

impl PaneFacts {
    /// Which `requires` context keys are currently non-empty.
    fn context_keys(&self) -> ContextKeys {
        ContextKeys {
            session: self.session.is_some(),
            cwd: !self.cwd.is_empty(),
            pid: self.pid.is_some(),
            title: !self.title.is_empty(),
        }
    }

    /// Whether every `requires` token of `action` is satisfied. The `--force` path checks this
    /// directly (it skips the full gate but never `requires`).
    fn requires_met(&self, action: &ActionManifest) -> bool {
        let keys = self.context_keys();
        action.requires.iter().all(|&r| match r {
            Requirement::Session => keys.session,
            Requirement::Cwd => keys.cwd,
            Requirement::Pid => keys.pid,
            Requirement::Title => keys.title,
        })
    }

    fn gate_input<'a>(&'a self, agent: &'a str) -> GateInput<'a> {
        GateInput {
            agent,
            state: self.state,
            detail: self.detail.as_deref(),
            context_pct: self.context_pct,
            context_covered: self.context_covered,
            context_keys: self.context_keys(),
        }
    }
}

/// Everything the detached supervisor needs to hold the lock and run one child. Assembled by
/// the broker under the held lock and handed to [`BrokerIo::spawn_supervisor`]; the real spawn re-execs
/// the tma binary in its internal [`supervise`] mode with these as args + `TMA_*` env.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SupervisorSpec {
    pub pane_id: String,
    /// The held lock's nonce; the supervisor rewrites the value with its own pid, keeping this.
    pub nonce: String,
    /// The held lock's absolute expiry (deadline + slack); preserved across the pid rewrite.
    pub expiry_ms: u64,
    pub action: String,
    pub agent: String,
    /// The `sh -c` command string, passed verbatim.
    pub command: String,
    pub detach_timeout_ms: u64,
    /// The assembled `TMA_*` context env the child inherits.
    pub env: Vec<(String, String)>,
}

/// The tmux effects the broker needs, behind a seam so the refusal matrix, the re-verify branch, and
/// the detach handoff are unit-testable with a mock. The real implementation is [`TmuxBroker`].
pub trait BrokerIo {
    fn now_ms(&self) -> u64;
    /// Read the pane's facts; `Ok(None)` when the pane is gone (vanished).
    fn read_pane(&self, pane_id: &str) -> Result<Option<PaneFacts>, TmuxError>;
    /// Run one on-demand detection cycle on the pane (capture → fold → guarded stamp), so the next
    /// [`BrokerIo::read_pane`] gates on a fresh state.
    fn reverify(&self, pane_id: &str) -> Result<(), TmuxError>;
    /// Deliver a key sequence through the `tma-tmux` `send_keys` choke point.
    fn send_keys(&self, pane_id: &str, keys: &[String]) -> Result<(), TmuxError>;
    /// Deliver a `text` action: the prefix keys, the caller's string literally, then the suffix
    /// keys, through the `tma-tmux` `send_text` choke point and inside one lock hold.
    fn send_text(
        &self,
        pane_id: &str,
        prefix: &[String],
        text: &str,
        suffix: &[String],
    ) -> Result<(), TmuxError>;
    /// Answer an OpenCode permission over HTTP: one POST to `{endpoint}/permission/{request}/
    /// reply` with the reply verdict, bounded by `timeout_ms` (connect + total), no retry. The real
    /// impl calls [`crate::http`]; the mock returns canned outcomes.
    fn api_reply(
        &self,
        endpoint: &str,
        request_id: &str,
        reply: ApiReply,
        timeout_ms: u64,
    ) -> HttpOutcome;
    /// Whether a hook-lane request record is parked for `request_id`. The test for "a hook is
    /// holding on this prompt": false means no lane to answer over, and the caller uses keystrokes.
    fn hook_request_pending(&self, request_id: &str) -> bool;
    /// Create the hook-lane verdict for `request_id`, exclusively. The real impl calls
    /// [`crate::hook_lane::write_verdict`]; the mock records the call and returns a canned outcome.
    fn write_hook_verdict(&self, request_id: &str, verdict: HookVerdict) -> VerdictWrite;
    /// Write the pane's `@agent_act_repeat` run under the held lock. Best-effort and infallible by
    /// construction, like [`BrokerIo::clear_permission_request`]: the counter is a mis-tap signal,
    /// and a failed option write must never turn a delivered action into a reported failure.
    fn set_act_repeat(&self, pane_id: &str, value: &str);
    /// Unset `@agent_permission_request` after a request has been spent. Best-effort and
    /// infallible by construction: it runs after the answer is already delivered, so a failed
    /// option write must not turn a successful reply into a reported failure.
    fn clear_permission_request(&self, pane_id: &str);
    /// Acquire the single-flight lock; the implementation supplies the broker pid and nonce.
    fn acquire(
        &self,
        pane_id: &str,
        now_ms: u64,
        expiry_ms: u64,
        name: &str,
    ) -> Result<Acquire, LockError>;
    /// Clear the lock nonce-conditionally (release).
    fn clear(&self, pane_id: &str, nonce: &str) -> Result<(), LockError>;
    /// Launch the detached supervisor for `spec`. `Ok(())` means the supervisor process is on
    /// its way (the lock is now its responsibility); `Err(msg)` means the spawn failed and the caller
    /// clears the lock synchronously.
    fn spawn_supervisor(&self, spec: &SupervisorSpec) -> Result<(), String>;
    /// Whether a live (unexpired) single-flight lock is currently held on the pane, for the
    /// `--list` `locked` verdict. A read-only peek: unlike [`BrokerIo::acquire`] it never
    /// takes the lock. `now_ms` is the clock the expiry is compared against.
    fn lock_held(&self, pane_id: &str, now_ms: u64) -> Result<bool, TmuxError>;
}

/// The forwarding context the detach path hands to the spawned supervisor: the target server
/// and the completion notify command. Unused by the synchronous path, so callers that never fire a
/// detached action pass [`DetachCtx::default`] — whose `None` server means the ambient one, which is
/// the only sensible reading for a caller that spawns nothing.
#[derive(Clone, Copy, Default)]
pub struct DetachCtx<'a> {
    pub server: Option<&'a tma_tmux::tmux::Server>,
    pub notify_command: Option<&'a str>,
}

/// The per-invocation inputs beyond the action itself. [`FireArgs::default`] is the plain fire.
#[derive(Clone, Copy, Default)]
pub struct FireArgs<'a> {
    /// `--force`: skip the `when` gate only, never `requires` and never the lock.
    pub force: bool,
    /// The caller's `--arg` values. They reach an `exec` action's child as environment
    /// (`TMA_ARG`/`TMA_ARG_<n>`) and nothing else: the command string is never rewritten, so a
    /// value stays data the shell cannot re-parse, exactly as the pane title does. A `keys` action
    /// takes no values — its sequence is manifest-static — and the CLI rejects them before here.
    pub args: &'a [String],
    /// The caller's `--text` string, for a `text` action and nothing else. It is checked against
    /// the host-side payload rules before any tmux command runs, then delivered literally.
    pub text: Option<&'a str>,
    /// Where the `[act] log` line goes and which surface asked for the fire. The default writes
    /// nothing, so a caller that is not the `tma act` CLI stays silent.
    pub audit: AuditCtx<'a>,
    /// Refuse `episode-changed` unless the pane is still in this episode: the `episode_ms` of the
    /// row the caller acted on. `None` checks nothing, which is every pre-binder caller.
    pub expect_episode_ms: Option<u64>,
    /// Refuse `request-gone` unless the pane still carries this `@agent_permission_request`. A
    /// necessary condition, not proof of liveness: a matching id can still name a spent request.
    pub expect_permission_request: Option<&'a str>,
}

/// The caller's binder against one pane read: `None` when every expectation it supplied still
/// holds. Both callers pass the facts read under the held lock, which is the whole point of it.
fn binder_refusal(facts: &PaneFacts, fire_args: FireArgs) -> Option<Refusal> {
    // Any difference counts, a backward clock step included: the caller named one instant, and a
    // pane that is not at it is not the pane the caller saw. `episode_ms` is the `ls --json` value.
    if fire_args
        .expect_episode_ms
        .is_some_and(|want| want != facts.episode_ms)
    {
        return Some(Refusal::EpisodeChanged);
    }
    if fire_args
        .expect_permission_request
        .is_some_and(|want| facts.permission_request.as_deref() != Some(want))
    {
        return Some(Refusal::RequestGone);
    }
    None
}

/// Fire an action against `pane_id`. `fire_args` carries `--force` (the `when` gate only), the
/// `--arg` values, the audit context, and the caller's binder expectations. A synchronous action
/// runs and releases the lock here; a `detach = true` action hands the lock to a spawned supervisor
/// and returns `spawned`. `detach` carries the server + notify command forwarded to that
/// supervisor. The ergonomic entry the CLI calls; builds the real [`TmuxBroker`] and writes the
/// `[act] log` line for whatever came back.
#[allow(clippy::too_many_arguments)]
pub fn fire(
    tmux: &Tmux,
    manifests: &[LoadedManifest],
    cfg: &FoldConfig,
    api_bases: &ApiSection,
    detach: DetachCtx,
    action: &ActionManifest,
    pane_id: &str,
    fire_args: FireArgs,
) -> ActResult {
    let io = TmuxBroker {
        tmux,
        manifests,
        cfg,
        api_bases,
        server: detach.server.cloned().unwrap_or_default(),
        notify_command: detach.notify_command.map(str::to_string),
    };
    let (result, observed) = act_observed(&io, action, pane_id, fire_args);
    write_audit_line(&io, action, pane_id, &result, &observed, &fire_args.audit);
    result
}

/// Append the fire's `[act] log` line, or do nothing when `[act] log` is unconfigured. Every
/// outcome is recorded, refusals included: "it refused" is exactly the fact a later reader needs,
/// and a log that only holds successes cannot answer why nothing happened.
fn write_audit_line<T: BrokerIo>(
    io: &T,
    action: &ActionManifest,
    pane_id: &str,
    result: &ActResult,
    observed: &ActObserved,
    ctx: &AuditCtx,
) {
    let Some(path) = ctx.log else { return };
    let api = observed
        .agent
        .as_deref()
        .is_some_and(|a| action.api_for(a).is_some());
    crate::audit::append(
        path,
        &audit::act_log_line(
            io.now_ms(),
            pane_id,
            &action.name,
            audit::kind_token(action.kind, api, observed.took_hook_lane),
            result.outcome.token(),
            result.reason(),
            observed,
            ctx,
        ),
    );
}

/// The broker sequence over any [`BrokerIo`]. Release is guaranteed on every exit path after
/// acquire because [`act_under_lock`] returns the outcome and the caller always clears.
pub fn act<T: BrokerIo>(
    io: &T,
    action: &ActionManifest,
    pane_id: &str,
    fire_args: FireArgs,
) -> ActResult {
    act_observed(io, action, pane_id, fire_args).0
}

/// [`act`] plus what it saw on the way: the pane facts the audit line records, filled in as far as
/// the sequence got. Split out rather than folded into [`ActResult`] because those facts are audit
/// material and no surface renders them.
pub fn act_observed<T: BrokerIo>(
    io: &T,
    action: &ActionManifest,
    pane_id: &str,
    fire_args: FireArgs,
) -> (ActResult, ActObserved) {
    let mut observed = ActObserved::default();
    let outcome = act_sequence(io, action, pane_id, fire_args, &mut observed);
    (
        ActResult {
            action: action.name.clone(),
            pane: pane_id.to_string(),
            outcome,
        },
        observed,
    )
}

/// The pinned identity → gate → lock → act → release sequence. Every early return is an outcome, and
/// `observed` accumulates whatever the pane had told us by then.
fn act_sequence<T: BrokerIo>(
    io: &T,
    action: &ActionManifest,
    pane_id: &str,
    fire_args: FireArgs,
    observed: &mut ActObserved,
) -> Outcome {
    // 0. payload rules, host-side and before the first tmux command. A refused steer never reaches
    // the pane, the re-verify, or the lock, so a client that can reach the broker at all still
    // cannot reach the agent's own command plane through a message.
    if action.kind == ActionKind::Text {
        if let Err(r) = action.check_text(fire_args.text.unwrap_or_default()) {
            return Outcome::Refused(Refusal::Payload(r));
        }
    }

    // The gate half is read here; the values ride on to whichever under-lock arm assembles the env.
    let force = fire_args.force;
    let now = io.now_ms();

    // 1. identity + initial facts.
    let facts = match io.read_pane(pane_id) {
        Ok(Some(f)) => f,
        Ok(None) => return Outcome::Vanished(Gone::Pane),
        Err(e) => return io_error(e),
    };
    observe(observed, &facts);
    let Some(agent) = facts.agent.clone().filter(|a| action.applies_to(a)) else {
        return Outcome::Refused(Refusal::Gate(RefusalReason::WrongAgent));
    };

    // 2. gate. A stale keystroke-sending action re-verifies on-demand first (skipped under
    // `--force`, which does not gate on state at all). The predicate is the kind's own answer, so a
    // fourth kind cannot inherit "no freshness check" by omission.
    let facts = if action.kind.sends_keystrokes() && !force && is_stale(now, facts.stamped_at) {
        match io.reverify(pane_id) {
            Ok(()) => {}
            Err(e) => return io_error(e),
        }
        match io.read_pane(pane_id) {
            Ok(Some(f)) => {
                observe(observed, &f);
                f
            }
            Ok(None) => return Outcome::Vanished(Gone::Pane),
            Err(e) => return io_error(e),
        }
    } else {
        facts
    };
    if let Some(refusal) = gate_refusal(action, &facts, &agent, force) {
        return Outcome::Refused(refusal);
    }

    // 3. acquire the single-flight lock (expiry = deadline + slack, broker pid). A detached action's
    // deadline is `detach_timeout_ms`, not `timeout_ms`.
    let bound = if action.detach {
        action.detach_timeout_ms
    } else {
        action.timeout_ms
    };
    let expiry = now.saturating_add(bound).saturating_add(SLACK_MS);
    let lock = match io.acquire(pane_id, now, expiry, &action.name) {
        Ok(Acquire::Acquired(v)) => v,
        Ok(Acquire::Contended) => return Outcome::Refused(Refusal::Locked),
        Err(LockError::Tmux(e)) => return io_error(e),
        Err(e) => return Outcome::Error(e.to_string()),
    };

    // 4. act under the held lock. Detached: hand the lock to the supervisor and DO NOT clear it on the
    // spawn path (the supervisor owns it now); every refusal / vanish / spawn failure still clears.
    // Synchronous: 5. release on every exit path.
    if action.detach {
        let outcome =
            spawn_detached_under_lock(io, action, pane_id, fire_args, &agent, &lock, observed);
        if !matches!(outcome, Outcome::Spawned) {
            report_release_failure(pane_id, io.clear(pane_id, &lock.nonce));
        }
        return outcome;
    }
    let outcome = act_under_lock(io, action, pane_id, fire_args, &agent, facts, observed);
    report_release_failure(pane_id, io.clear(pane_id, &lock.nonce));
    outcome
}

/// Copy the audit-visible half of a pane read into the accumulator. The later read wins: the line
/// should describe the world the effect landed in, not the one the gate first saw.
fn observe(observed: &mut ActObserved, facts: &PaneFacts) {
    observed.agent = facts.agent.clone();
    observed.episode_ms = Some(facts.episode_ms);
    observed.pending_tool = facts.pending_tool.clone();
    observed.pending_call = facts.pending_call.clone();
}

/// Advance the pane's consecutive-fire run for this action and warn once it reaches
/// [`audit::REPEAT_WARN`]. Called under the held lock, only on the path that is about to have an
/// effect: a refusal changed nothing and must not extend a run. This never refuses. The agent that
/// keeps re-asking, and the finger that keeps answering, are both things a person should be told
/// about rather than things the broker should decide for them.
fn bump_repeat<T: BrokerIo>(io: &T, action: &str, pane_id: &str, facts: &PaneFacts) -> u32 {
    let (value, count) = audit::next_repeat(facts.act_repeat.as_deref(), facts.episode_ms, action);
    io.set_act_repeat(pane_id, &value);
    if count >= audit::REPEAT_WARN {
        eprintln!(
            "tma: {count} consecutive {action} on {pane_id} in this episode; \
             the agent may be re-asking"
        );
    }
    count
}

/// The detached handoff: re-assert identity + gate under the held lock (the same residual-race
/// shrink the synchronous path applies), assemble the context env, then spawn the supervisor. Returns
/// `Spawned` when the lock has been handed off, or a refusal/error the caller clears the lock for. A
/// `detach` action is always `exec` (a `keys` action cannot set `detach`, enforced at parse), so there
/// is no keys arm here.
#[allow(clippy::too_many_arguments)]
fn spawn_detached_under_lock<T: BrokerIo>(
    io: &T,
    action: &ActionManifest,
    pane_id: &str,
    fire_args: FireArgs,
    agent: &str,
    lock: &LockValue,
    observed: &mut ActObserved,
) -> Outcome {
    let FireArgs { force, args, .. } = fire_args;
    let facts = match io.read_pane(pane_id) {
        Ok(Some(f)) => f,
        Ok(None) => return Outcome::Vanished(Gone::Pane),
        Err(e) => return io_error(e),
    };
    observe(observed, &facts);
    if facts.agent.as_deref() != Some(agent) || !action.applies_to(agent) {
        return Outcome::Refused(Refusal::Gate(RefusalReason::WrongAgent));
    }
    if let Some(refusal) = gate_refusal(action, &facts, agent, force) {
        return Outcome::Refused(refusal);
    }
    if let Some(refusal) = binder_refusal(&facts, fire_args) {
        return Outcome::Refused(refusal);
    }
    observed.repeat = bump_repeat(io, &action.name, pane_id, &facts);
    let spec = SupervisorSpec {
        pane_id: pane_id.to_string(),
        nonce: lock.nonce.clone(),
        expiry_ms: lock.expiry_ms,
        action: action.name.clone(),
        agent: agent.to_string(),
        command: action.command.clone().unwrap_or_default(),
        detach_timeout_ms: action.detach_timeout_ms,
        env: assemble_env(action, &facts, pane_id, agent, args),
    };
    match io.spawn_supervisor(&spec) {
        Ok(()) => Outcome::Spawned,
        Err(msg) => Outcome::Error(msg),
    }
}

/// Re-assert the gate under the held lock (one option read, shrinks the residual
/// window), then deliver keys or spawn the command. The lock is released by the caller regardless of
/// which arm this returns.
#[allow(clippy::too_many_arguments)]
fn act_under_lock<T: BrokerIo>(
    io: &T,
    action: &ActionManifest,
    pane_id: &str,
    fire_args: FireArgs,
    agent: &str,
    prior: PaneFacts,
    observed: &mut ActObserved,
) -> Outcome {
    let FireArgs { force, args, .. } = fire_args;
    // Re-read once under the lock and re-assert (a state flip between step 2 and now refuses here,
    // bounding the residual race to milliseconds). `--force` did not gate on state, so it re-asserts
    // only identity + requires.
    let facts = match io.read_pane(pane_id) {
        Ok(Some(f)) => f,
        Ok(None) => return Outcome::Vanished(Gone::Pane),
        Err(e) => return io_error(e),
    };
    observe(observed, &facts);
    if facts.agent.as_deref() != Some(agent) || !action.applies_to(agent) {
        return Outcome::Refused(Refusal::Gate(RefusalReason::WrongAgent));
    }
    if let Some(refusal) = gate_refusal(action, &facts, agent, force) {
        return Outcome::Refused(refusal);
    }
    // The caller's binder, checked here and nowhere earlier: from the under-lock read, so a prompt
    // that turned over between the caller's observation and now cannot be answered by mistake.
    if let Some(refusal) = binder_refusal(&facts, fire_args) {
        return Outcome::Refused(refusal);
    }
    observed.repeat = bump_repeat(io, &action.name, pane_id, &facts);
    let _ = prior; // the pre-lock facts are superseded by the re-read; kept for a clear signature.

    match action.kind {
        ActionKind::Keys => {
            // Three arms, tried in order. `api` and `hook` are structured and mutually exclusive
            // per agent; `keys` is the floor every fire lands on when neither one applies.
            if let Some(transport) = action.api_for(agent) {
                // The endpoint + request id are guaranteed present (the API requires re-asserted
                // under the lock). Empty defaults here would only surface as the server's own 404.
                let endpoint = facts.api_endpoint.as_deref().unwrap_or_default();
                let request = facts.permission_request.as_deref().unwrap_or_default();
                return match io.api_reply(endpoint, request, transport.reply, action.timeout_ms) {
                    HttpOutcome::Ok => {
                        // The id is spent: drop it here rather than waiting for the plugin's
                        // `permission.replied` event, which runs on its own schedule. Not a
                        // compare-and-swap — tmux has no conditional option write and the plugin
                        // does not take this lock — so a request stamped in the gap is erased. That
                        // fails safe: a missing stamp refuses the next dispatch `requires-unmet`
                        // instead of firing at a stale id.
                        io.clear_permission_request(pane_id);
                        Outcome::Replied
                    }
                    // The prompt was answered/withdrawn between gate and act: the act's
                    // target disappeared, so `vanished`, exit 3.
                    HttpOutcome::NotFound => Outcome::Vanished(Gone::Request),
                    HttpOutcome::Error(msg) => Outcome::Error(msg),
                };
            }
            // The hook reply lane, taken only when a hook is actually parked on this request:
            // with no record on disk this falls through to the keys arm below, which is the whole
            // degradation guarantee. `binder_refusal` has already re-asserted the id under the lock.
            if let Some(transport) = action.hook_for(agent) {
                let request = facts.permission_request.as_deref().unwrap_or_default();
                if io.hook_request_pending(request) {
                    return match io.write_hook_verdict(request, transport.verdict) {
                        VerdictWrite::Written => {
                            observed.took_hook_lane = true;
                            // Spend the id here, under the same lock, for the same reason the API
                            // arm does: a second dispatch then refuses `request-gone` at the binder
                            // rather than reaching a verdict file that would refuse it anyway.
                            io.clear_permission_request(pane_id);
                            Outcome::Replied
                        }
                        // A verdict already existed: the request was answered between the binder's
                        // read and this write, so its target is gone.
                        VerdictWrite::Exists => Outcome::Vanished(Gone::Request),
                        VerdictWrite::Error(msg) => Outcome::Error(msg),
                    };
                }
            }
            // Applicability guaranteed the sequence exists; an empty one is a no-op send.
            let seq = action.keys_for(agent).unwrap_or(&[]);
            match io.send_keys(pane_id, seq) {
                Ok(()) => Outcome::Sent,
                Err(e) => io_error(e),
            }
        }
        ActionKind::Text => {
            // The payload cleared the host-side rules before the lock; what is left is the
            // manifest's own wrapping, delivered prefix → text → suffix inside this one hold.
            let Some(transport) = action.text_for(agent) else {
                return Outcome::Refused(Refusal::Gate(RefusalReason::WrongAgent));
            };
            let text = fire_args.text.unwrap_or_default();
            match io.send_text(pane_id, &transport.prefix, text, &transport.suffix) {
                Ok(()) => Outcome::Sent,
                Err(e) => io_error(e),
            }
        }
        ActionKind::Exec => {
            let env = assemble_env(action, &facts, pane_id, agent, args);
            let command = action.command.as_deref().unwrap_or("");
            match run_exec(command, &env, action.timeout_ms) {
                ExecOutcome::Exited(code) => Outcome::Exited(code),
                ExecOutcome::Timeout => Outcome::Timeout,
                ExecOutcome::SpawnError(msg) => Outcome::Error(msg),
            }
        }
    }
}

/// The gate verdict for the act path: `None` fireable, `Some(refusal)` otherwise. Under `--force`
/// the `when` gate is skipped but `requires` (and identity, checked by the caller) still apply.
fn gate_refusal(
    action: &ActionManifest,
    facts: &PaneFacts,
    agent: &str,
    force: bool,
) -> Option<Refusal> {
    let base = if force {
        (!facts.requires_met(action)).then_some(Refusal::Gate(RefusalReason::RequiresUnmet))
    } else {
        match action.evaluate_gate(&facts.gate_input(agent)) {
            GateOutcome::Fireable => None,
            GateOutcome::Refused(r) => Some(Refusal::Gate(r)),
        }
    };
    // The API-lane preconditions are requires-like: never skipped by `--force`, and only surfaced
    // when the pure gate is otherwise satisfied (so a state/coverage reason still wins).
    base.or_else(|| api_requires_refusal(action, facts, agent))
}

/// The API-lane `requires` check: for an agent whose transport is `api`, an empty
/// `@agent_permission_request` or an unresolvable endpoint refuses `requires-unmet` (no new
/// user-facing `requires` token; a broker-level precondition). `None` for the keys/exec path.
fn api_requires_refusal(
    action: &ActionManifest,
    facts: &PaneFacts,
    agent: &str,
) -> Option<Refusal> {
    let transport = action.api_for(agent)?;
    let unmet = match transport.op {
        ApiOp::PermissionReply => {
            let has_request = facts
                .permission_request
                .as_deref()
                .is_some_and(|s| !s.is_empty());
            let has_endpoint = facts.api_endpoint.as_deref().is_some_and(|s| !s.is_empty());
            !(has_request && has_endpoint)
        }
    };
    unmet.then_some(Refusal::Gate(RefusalReason::RequiresUnmet))
}

/// A stamp is stale when never written, stamped in the future (a backward wall-clock step), or older
/// than [`FRESHNESS_MS`].
fn is_stale(now: u64, stamped_at: u64) -> bool {
    stamped_at == 0 || now < stamped_at || now - stamped_at >= FRESHNESS_MS
}

/// Map a tmux read/write error to an outcome: only tmux saying the pane is gone is `vanished`,
/// anything else is a broker `error` carrying tmux's own stderr, which is the whole diagnostic.
/// (`ServerGone` reaches here as an error, which is correct — there is nothing to act on.) Treating
/// every failed command as `vanished` reported "pane vanished" for a key spelling tmux rejected and
/// threw the reason away with it.
fn io_error(e: TmuxError) -> Outcome {
    match &e {
        TmuxError::Failed { stderr, .. } if pane_gone(stderr) => Outcome::Vanished(Gone::Pane),
        _ => Outcome::Error(e.to_string()),
    }
}

/// Does tmux's stderr say the target pane no longer exists? tmux 3.6a writes `can't find pane: %5`
/// for a target lookup and `no such pane: %5` for a pane-scoped option; the third spelling covers
/// older versions and the tmate fork.
fn pane_gone(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    ["can't find pane", "no such pane", "pane not found"]
        .iter()
        .any(|needle| s.contains(needle))
}

/// Log a failed action-lock release. The release stays best-effort (the caller proceeds either way),
/// but unlike the benign stamp discards a silently lingering lock is worth telemetry: the lock is
/// expiry-bounded and reclaimed on a dead pid, so this surfaces the transient without gating anything.
/// `tma act` runs in the foreground, so this reaches the invoking terminal.
fn report_release_failure(pane_id: &str, result: Result<(), LockError>) {
    if let Err(e) = result {
        eprintln!("tma: action-lock release failed for pane {pane_id}: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use tma_tmux::lock;

    fn keys_action(when: &str, keys: &str) -> ActionManifest {
        let src = format!(
            "min_engine_version = \"0.1\"\nname = \"approve\"\nlabel = \"Approve\"\nkind = \"keys\"\n{when}\n[keys]\n{keys}\n"
        );
        ActionManifest::parse(&src, "approve", "approve.toml").unwrap()
    }

    fn exec_action(extra: &str) -> ActionManifest {
        let src = format!(
            "min_engine_version = \"0.1\"\nname = \"run\"\nlabel = \"Run\"\nkind = \"exec\"\ncommand = \"true\"\n{extra}\n"
        );
        ActionManifest::parse(&src, "run", "run.toml").unwrap()
    }

    fn blocked_claude(stamped_at: u64) -> PaneFacts {
        PaneFacts {
            agent: Some("claude".to_string()),
            state: AgentState::Blocked,
            detail: Some("permission".to_string()),
            session: None,
            cwd: "/repo".to_string(),
            pid: Some("4242".to_string()),
            title: "claude".to_string(),
            locator: "s:0.0".to_string(),
            stamped_at,
            context_pct: None,
            context_covered: false,
            permission_request: None,
            api_endpoint: None,
            episode_ms: 900_000,
            pending_tool: None,
            pending_call: None,
            act_repeat: None,
        }
    }

    /// A blocked/permission OpenCode pane with a pending request id and a resolved endpoint — the
    /// fireable state for an `api` `permission-reply` action.
    fn blocked_opencode(stamped_at: u64) -> PaneFacts {
        PaneFacts {
            agent: Some("opencode".to_string()),
            permission_request: Some("per_abc123".to_string()),
            api_endpoint: Some("http://127.0.0.1:4096".to_string()),
            ..blocked_claude(stamped_at)
        }
    }

    /// An `approve`-shaped action whose OpenCode agent uses the API transport.
    fn api_action() -> ActionManifest {
        let src = "min_engine_version = \"0.1\"\nname = \"approve\"\nlabel = \"Approve\"\nkind = \"keys\"\nwhen = { state = [\"blocked\"], detail = [\"permission\"] }\n[api]\nopencode = { op = \"permission-reply\", reply = \"once\" }\n";
        ActionManifest::parse(src, "approve", "approve.toml").unwrap()
    }

    /// A scripted [`BrokerIo`]: a queue of `read_pane` results (each call pops the next, or reuses
    /// the last), a fixed clock, a canned acquire result, and recorders for the effects.
    struct MockIo {
        now: u64,
        reads: RefCell<Vec<Option<PaneFacts>>>,
        /// How many times the broker read the pane: the payload refusal must leave it at zero.
        read_calls: RefCell<u32>,
        reverify_state: RefCell<Option<PaneFacts>>,
        reverify_called: RefCell<bool>,
        acquire: Acquire,
        sent: RefCell<Option<Vec<String>>>,
        cleared: RefCell<bool>,
        lock_held: bool,
        spawn_result: Result<(), String>,
        spawned: RefCell<Option<SupervisorSpec>>,
        /// Canned HTTP outcome for the API lane, and a record of the `(endpoint, request, reply)` call.
        api_result: HttpOutcome,
        api_call: RefCell<Option<(String, String, ApiReply)>>,
        /// The panes whose `@agent_permission_request` the broker asked to unset.
        request_cleared: RefCell<Vec<String>>,
        /// Every `@agent_act_repeat` value the broker wrote, in order.
        repeat_writes: RefCell<Vec<String>>,
        /// Whether a hook-lane request record is "parked" (default false: no lane, keys arm).
        hook_pending: bool,
        /// Canned verdict-write outcome, and every `(request_id, verdict)` the broker wrote.
        hook_result: VerdictWrite,
        hook_writes: RefCell<Vec<(String, HookVerdict)>>,
    }

    impl MockIo {
        fn new(reads: Vec<Option<PaneFacts>>, acquire: Acquire) -> MockIo {
            MockIo {
                now: 1_000_000,
                reads: RefCell::new(reads),
                read_calls: RefCell::new(0),
                reverify_state: RefCell::new(None),
                reverify_called: RefCell::new(false),
                acquire,
                sent: RefCell::new(None),
                cleared: RefCell::new(false),
                lock_held: false,
                spawn_result: Ok(()),
                spawned: RefCell::new(None),
                api_result: HttpOutcome::Ok,
                api_call: RefCell::new(None),
                request_cleared: RefCell::new(Vec::new()),
                repeat_writes: RefCell::new(Vec::new()),
                hook_pending: false,
                hook_result: VerdictWrite::Written,
                hook_writes: RefCell::new(Vec::new()),
            }
        }
        /// A hook is parked on the request, so the hook arm is live.
        fn with_hook_pending(mut self) -> MockIo {
            self.hook_pending = true;
            self
        }
        /// The verdict write returns `outcome` instead of creating the file.
        fn with_hook_result(mut self, outcome: VerdictWrite) -> MockIo {
            self.hook_pending = true;
            self.hook_result = outcome;
            self
        }
        /// The API lane returns `outcome` instead of the default 2xx.
        fn with_api_result(mut self, outcome: HttpOutcome) -> MockIo {
            self.api_result = outcome;
            self
        }
        /// After a re-verify, the next read yields `facts` (the fresh stamp).
        fn with_reverify(mut self, facts: PaneFacts) -> MockIo {
            self.reverify_state = RefCell::new(Some(facts));
            self
        }
        /// The supervisor spawn fails with `msg` (the fork-failure cleanup path).
        fn with_spawn_err(mut self, msg: &str) -> MockIo {
            self.spawn_result = Err(msg.to_string());
            self
        }
    }

    /// Marks the literal element inside [`MockIo`]'s flattened `send_text` record, so the ordering
    /// assertion can tell the caller's string from a manifest key.
    const LITERAL: &str = "literal:";

    fn acquired() -> Acquire {
        Acquire::Acquired(lock::LockValue {
            expiry_ms: 2_000_000,
            nonce: "deadbeef".to_string(),
            pid: 1,
            name: "approve".to_string(),
        })
    }

    impl BrokerIo for MockIo {
        fn now_ms(&self) -> u64 {
            self.now
        }
        fn read_pane(&self, _pane: &str) -> Result<Option<PaneFacts>, TmuxError> {
            *self.read_calls.borrow_mut() += 1;
            let mut reads = self.reads.borrow_mut();
            if reads.len() > 1 {
                Ok(reads.remove(0))
            } else {
                Ok(reads.first().cloned().flatten())
            }
        }
        fn reverify(&self, _pane: &str) -> Result<(), TmuxError> {
            *self.reverify_called.borrow_mut() = true;
            if let Some(f) = self.reverify_state.borrow().clone() {
                // The fresh stamp the next read observes.
                *self.reads.borrow_mut() = vec![Some(f)];
            }
            Ok(())
        }
        fn send_keys(&self, _pane: &str, keys: &[String]) -> Result<(), TmuxError> {
            *self.sent.borrow_mut() = Some(keys.to_vec());
            Ok(())
        }
        fn send_text(
            &self,
            _pane: &str,
            prefix: &[String],
            text: &str,
            suffix: &[String],
        ) -> Result<(), TmuxError> {
            // Recorded as one flat sequence with the literal marked, so a test asserts the
            // prefix → text → suffix ordering the pane actually saw.
            let mut seq = prefix.to_vec();
            seq.push(format!("{LITERAL}{text}"));
            seq.extend(suffix.iter().cloned());
            *self.sent.borrow_mut() = Some(seq);
            Ok(())
        }
        fn api_reply(
            &self,
            endpoint: &str,
            request_id: &str,
            reply: ApiReply,
            _timeout_ms: u64,
        ) -> HttpOutcome {
            *self.api_call.borrow_mut() =
                Some((endpoint.to_string(), request_id.to_string(), reply));
            self.api_result.clone()
        }
        fn hook_request_pending(&self, request_id: &str) -> bool {
            self.hook_pending && !request_id.is_empty()
        }
        fn write_hook_verdict(&self, request_id: &str, verdict: HookVerdict) -> VerdictWrite {
            self.hook_writes
                .borrow_mut()
                .push((request_id.to_string(), verdict));
            self.hook_result.clone()
        }
        fn set_act_repeat(&self, _pane: &str, value: &str) {
            self.repeat_writes.borrow_mut().push(value.to_string());
        }
        fn clear_permission_request(&self, pane_id: &str) {
            self.request_cleared.borrow_mut().push(pane_id.to_string());
        }
        fn acquire(
            &self,
            _pane: &str,
            _now: u64,
            _expiry: u64,
            _name: &str,
        ) -> Result<Acquire, LockError> {
            Ok(self.acquire.clone())
        }
        fn clear(&self, _pane: &str, _nonce: &str) -> Result<(), LockError> {
            *self.cleared.borrow_mut() = true;
            Ok(())
        }
        fn spawn_supervisor(&self, spec: &SupervisorSpec) -> Result<(), String> {
            *self.spawned.borrow_mut() = Some(spec.clone());
            self.spawn_result.clone()
        }
        fn lock_held(&self, _pane: &str, _now: u64) -> Result<bool, TmuxError> {
            Ok(self.lock_held)
        }
    }

    /// The counter tracks *deliveries*, so a refusal must leave the run where it was. A counter
    /// that ticked on refusals would reach the warning threshold on a pane nothing was ever sent
    /// to, which is the opposite of the signal it exists to give.
    #[test]
    fn a_refusal_writes_no_repeat_and_records_no_run() {
        let mut facts = blocked_claude(1_000_000);
        facts.state = AgentState::Idle; // `approve`'s `when` no longer holds
        let io = MockIo::new(vec![Some(facts)], acquired());
        let action = keys_action("when = { state = [\"blocked\"] }", "claude = [\"1\"]");
        let (r, obs) = act_observed(&io, &action, "%9", FireArgs::default());
        assert!(matches!(r.outcome, Outcome::Refused(_)));
        assert_eq!(obs.repeat, 0, "a refusal is not part of a run");
        assert!(io.repeat_writes.borrow().is_empty(), "and writes nothing");
    }

    /// A delivered fire records the pane facts the audit line needs, read under the lock, and
    /// advances the run. The pending summary is deliberately not among them: it is never read.
    #[test]
    fn a_delivered_fire_records_the_run_and_the_pending_call() {
        let mut facts = blocked_claude(1_000_000);
        facts.pending_tool = Some("Bash".to_string());
        facts.pending_call = Some("toolu_01".to_string());
        facts.act_repeat = Some("900000:approve:2".to_string());
        let io = MockIo::new(vec![Some(facts)], acquired());
        let action = keys_action("when = { state = [\"blocked\"] }", "claude = [\"1\"]");
        let (r, obs) = act_observed(&io, &action, "%9", FireArgs::default());
        assert_eq!(r.outcome, Outcome::Sent);
        assert_eq!(obs.agent.as_deref(), Some("claude"));
        assert_eq!(obs.episode_ms, Some(900_000));
        assert_eq!(obs.pending_tool.as_deref(), Some("Bash"));
        assert_eq!(
            obs.repeat, 3,
            "the third consecutive approve in this episode"
        );
        assert_eq!(
            *io.repeat_writes.borrow(),
            vec!["900000:approve:3".to_string()]
        );
    }

    #[test]
    fn vanished_pane_exits_three() {
        let io = MockIo::new(vec![None], acquired());
        let action = keys_action("", "claude = [\"1\"]");
        let r = act(&io, &action, "%9", FireArgs::default());
        assert_eq!(r.outcome, Outcome::Vanished(Gone::Pane));
        assert_eq!(r.exit_code(), 3);
        assert!(!*io.cleared.borrow(), "no lock taken, nothing to clear");
    }

    #[test]
    fn wrong_agent_refuses_four() {
        let mut facts = blocked_claude(1_000_000); // fresh
        facts.agent = Some("codex".to_string());
        let io = MockIo::new(vec![Some(facts)], acquired());
        let action = keys_action("", "claude = [\"1\"]"); // covers claude only
        let r = act(&io, &action, "%1", FireArgs::default());
        assert_eq!(r.reason(), Some("wrong-agent"));
        assert_eq!(r.exit_code(), 4);
    }

    #[test]
    fn gated_state_refuses_four_without_a_lock() {
        let mut facts = blocked_claude(1_000_000);
        facts.state = AgentState::Idle; // not blocked
        let io = MockIo::new(vec![Some(facts)], acquired());
        let action = keys_action(
            "when = { state = [\"blocked\"], detail = [\"permission\"] }",
            "claude = [\"1\"]",
        );
        let r = act(&io, &action, "%1", FireArgs::default());
        assert_eq!(r.reason(), Some("gated"));
        assert_eq!(r.exit_code(), 4);
        assert!(!*io.cleared.borrow(), "gate refused before the lock");
    }

    #[test]
    fn requires_unmet_refuses_four() {
        let facts = blocked_claude(1_000_000); // session is None
        let io = MockIo::new(vec![Some(facts)], acquired());
        let action = exec_action("requires = [\"session\"]");
        let r = act(&io, &action, "%1", FireArgs::default());
        assert_eq!(r.reason(), Some("requires-unmet"));
        assert_eq!(r.exit_code(), 4);
    }

    #[test]
    fn context_bound_without_coverage_refuses_no_coverage() {
        let facts = blocked_claude(1_000_000); // context_covered = false
        let io = MockIo::new(vec![Some(facts)], acquired());
        let action = keys_action("when = { context_pct_min = 75 }", "claude = [\"/compact\"]");
        let r = act(&io, &action, "%1", FireArgs::default());
        assert_eq!(r.reason(), Some("no-coverage"));
        assert_eq!(r.exit_code(), 4);
    }

    #[test]
    fn held_lock_refuses_five() {
        let facts = blocked_claude(1_000_000);
        let io = MockIo::new(vec![Some(facts)], Acquire::Contended);
        let action = keys_action(
            "when = { state = [\"blocked\"], detail = [\"permission\"] }",
            "claude = [\"1\"]",
        );
        let r = act(&io, &action, "%1", FireArgs::default());
        assert_eq!(r.outcome, Outcome::Refused(Refusal::Locked));
        assert_eq!(r.exit_code(), 5);
        assert!(
            !*io.cleared.borrow(),
            "a contended acquire owns no nonce to clear"
        );
    }

    #[test]
    fn fresh_blocked_keys_action_sends_and_releases() {
        let facts = blocked_claude(1_000_000); // fresh: no re-verify
        let io = MockIo::new(vec![Some(facts)], acquired());
        let action = keys_action(
            "when = { state = [\"blocked\"], detail = [\"permission\"] }",
            "claude = [\"1\"]",
        );
        let r = act(&io, &action, "%1", FireArgs::default());
        assert_eq!(r.outcome, Outcome::Sent);
        assert_eq!(r.exit_code(), 0);
        assert_eq!(io.sent.borrow().clone(), Some(vec!["1".to_string()]));
        assert!(
            *io.cleared.borrow(),
            "the lock is released on the send path"
        );
        assert!(
            !*io.reverify_called.borrow(),
            "a fresh stamp skips re-verify"
        );
    }

    #[test]
    fn stale_keys_action_reverifies_then_gates_on_the_fresh_stamp() {
        // Stale stamp (age >> FRESHNESS_MS) that currently reads idle: the pre-verify state would be
        // gated, but the on-demand re-verify lands blocked and the action fires.
        let mut stale = blocked_claude(1); // stamped_at ~ epoch start ⇒ stale
        stale.state = AgentState::Idle;
        let io =
            MockIo::new(vec![Some(stale)], acquired()).with_reverify(blocked_claude(1_000_000));
        let action = keys_action(
            "when = { state = [\"blocked\"], detail = [\"permission\"] }",
            "claude = [\"1\"]",
        );
        let r = act(&io, &action, "%1", FireArgs::default());
        assert!(
            *io.reverify_called.borrow(),
            "a stale keys stamp re-verifies"
        );
        assert_eq!(r.outcome, Outcome::Sent);
        assert_eq!(io.sent.borrow().clone(), Some(vec!["1".to_string()]));
    }

    #[test]
    fn force_skips_when_but_not_requires() {
        // Idle claude with a blocked-only gate: --force fires anyway (skips `when`).
        let mut idle = blocked_claude(1_000_000);
        idle.state = AgentState::Idle;
        let io = MockIo::new(vec![Some(idle)], acquired());
        let action = keys_action("when = { state = [\"blocked\"] }", "claude = [\"1\"]");
        let r = act(
            &io,
            &action,
            "%1",
            FireArgs {
                force: true,
                ..Default::default()
            },
        );
        assert_eq!(r.outcome, Outcome::Sent, "force bypasses the state gate");

        // But --force never skips requires: an exec action needing a session still refuses.
        let facts = blocked_claude(1_000_000); // session None
        let io = MockIo::new(vec![Some(facts)], acquired());
        let action = exec_action("requires = [\"session\"]");
        let r = act(
            &io,
            &action,
            "%1",
            FireArgs {
                force: true,
                ..Default::default()
            },
        );
        assert_eq!(r.reason(), Some("requires-unmet"));
        assert_eq!(r.exit_code(), 4);
    }

    #[test]
    fn state_flip_after_acquire_refuses_under_lock_and_still_releases() {
        // First read (pre-lock) is blocked ⇒ gate passes, lock taken. The under-lock re-read flips to
        // idle ⇒ the re-assert refuses, and the lock is released regardless.
        let io = MockIo::new(
            vec![
                Some(blocked_claude(1_000_000)),
                Some({
                    let mut f = blocked_claude(1_000_000);
                    f.state = AgentState::Idle;
                    f
                }),
            ],
            acquired(),
        );
        let action = keys_action(
            "when = { state = [\"blocked\"], detail = [\"permission\"] }",
            "claude = [\"1\"]",
        );
        let r = act(&io, &action, "%1", FireArgs::default());
        assert_eq!(r.reason(), Some("gated"), "the flip refuses under the lock");
        assert!(
            *io.cleared.borrow(),
            "the lock is released on the refusal path"
        );
        assert!(
            io.sent.borrow().is_none(),
            "no keys delivered after the flip"
        );
    }

    #[test]
    fn detach_action_acquires_spawns_and_keeps_the_lock() {
        // A detached exec action fires the supervisor and returns `spawned` (exit 0), handing the lock
        // off — it is NOT cleared here (the supervisor owns it now).
        let facts = {
            let mut f = blocked_claude(1_000_000);
            f.session = Some("abc123".to_string());
            f
        };
        let io = MockIo::new(vec![Some(facts)], acquired());
        let action = exec_action("detach = true\nrequires = [\"session\"]");
        let r = act(&io, &action, "%1", FireArgs::default());
        assert_eq!(r.outcome, Outcome::Spawned);
        assert_eq!(r.exit_code(), 0);
        assert!(
            !*io.cleared.borrow(),
            "a spawned detach hands the lock to the supervisor; the broker must not clear it"
        );
        let spec = io.spawned.borrow().clone().expect("supervisor was spawned");
        assert_eq!(
            spec.nonce, "deadbeef",
            "the held lock's nonce is handed off"
        );
        assert_eq!(
            spec.expiry_ms, 2_000_000,
            "the held lock's expiry is preserved"
        );
        assert_eq!(spec.command, "true");
        assert_eq!(spec.agent, "claude");
        assert_eq!(spec.detach_timeout_ms, 900_000, "the default detach bound");
        // The assembled context env crosses to the supervisor.
        assert!(spec
            .env
            .iter()
            .any(|(k, v)| k == "TMA_SESSION_ID" && v == "abc123"));
    }

    #[test]
    fn detach_spawn_failure_clears_the_lock() {
        // The fork-failure cleanup path: a spawn failure is a broker `error` and the lock is
        // released synchronously rather than left to expiry.
        let facts = {
            let mut f = blocked_claude(1_000_000);
            f.session = Some("abc123".to_string());
            f
        };
        let io = MockIo::new(vec![Some(facts)], acquired()).with_spawn_err("no exe");
        let action = exec_action("detach = true\nrequires = [\"session\"]");
        let r = act(&io, &action, "%1", FireArgs::default());
        assert!(matches!(r.outcome, Outcome::Error(_)));
        assert_eq!(r.exit_code(), 1);
        assert!(
            *io.cleared.borrow(),
            "a failed supervisor spawn clears the lock synchronously"
        );
        assert!(io.spawned.borrow().is_some(), "the spawn was attempted");
    }

    #[test]
    fn detach_gate_flip_under_lock_clears_without_spawning() {
        // A state flip between the pre-lock gate and the under-lock re-assert refuses (like the sync
        // path); the lock is released and no supervisor is spawned.
        let io = MockIo::new(
            vec![
                Some(blocked_claude(1_000_000)),
                Some({
                    let mut f = blocked_claude(1_000_000);
                    f.state = AgentState::Idle;
                    f
                }),
            ],
            acquired(),
        );
        let action = exec_action(
            "detach = true\nwhen = { state = [\"blocked\"], detail = [\"permission\"] }",
        );
        let r = act(&io, &action, "%1", FireArgs::default());
        assert_eq!(r.reason(), Some("gated"), "the flip refuses under the lock");
        assert!(*io.cleared.borrow(), "the lock is released on refusal");
        assert!(
            io.spawned.borrow().is_none(),
            "a refused detach never spawns a supervisor"
        );
    }

    // ---- hook reply lane ------------------------------------------------------------------------

    /// The shipped `approve`: a `[keys]` arm for claude AND a `[hook]` arm for the same agent. The
    /// overlap is the degradation path, so both tests below run against this one manifest.
    fn hook_action() -> ActionManifest {
        let src = "min_engine_version = \"0.1\"\nname = \"approve\"\nlabel = \"Approve\"\nkind = \"keys\"\nwhen = { state = [\"blocked\"], detail = [\"permission\"] }\n[keys]\nclaude = [\"1\"]\n[hook]\nclaude = { verdict = \"allow\" }\n";
        ActionManifest::parse(src, "approve", "approve.toml").unwrap()
    }

    /// A blocked/permission claude pane carrying the minted request id the lane stamps.
    fn blocked_claude_with_request(stamped_at: u64) -> PaneFacts {
        PaneFacts {
            permission_request: Some("d41d8cd98f00b204".to_string()),
            ..blocked_claude(stamped_at)
        }
    }

    #[test]
    fn hook_verdict_replies_and_spends_the_request() {
        let io = MockIo::new(
            vec![Some(blocked_claude_with_request(1_000_000))],
            acquired(),
        )
        .with_hook_pending();
        let r = act(&io, &hook_action(), "%1", FireArgs::default());
        assert_eq!(r.outcome, Outcome::Replied);
        assert_eq!(r.exit_code(), 0);
        assert!(
            io.sent.borrow().is_none(),
            "the hook lane sends no keystrokes"
        );
        assert_eq!(
            *io.hook_writes.borrow(),
            vec![("d41d8cd98f00b204".to_string(), HookVerdict::Allow)]
        );
        assert_eq!(
            *io.request_cleared.borrow(),
            vec!["%1".to_string()],
            "the id is spent under the same held lock the verdict was written under"
        );
        assert!(*io.cleared.borrow(), "the lock is released after the reply");
    }

    /// The degradation guarantee at the broker: no record on disk, so the same action delivers the
    /// keystroke it always did.
    #[test]
    fn no_request_record_falls_through_to_the_keys_arm() {
        let io = MockIo::new(
            vec![Some(blocked_claude_with_request(1_000_000))],
            acquired(),
        );
        let r = act(&io, &hook_action(), "%1", FireArgs::default());
        assert_eq!(r.outcome, Outcome::Sent);
        assert_eq!(*io.sent.borrow(), Some(vec!["1".to_string()]));
        assert!(io.hook_writes.borrow().is_empty(), "nothing was written");
        assert!(
            io.request_cleared.borrow().is_empty(),
            "a keys fire does not spend the id"
        );
    }

    /// A pane with no stamped request cannot be on the lane at all, record or no record.
    #[test]
    fn an_unstamped_pane_takes_the_keys_arm() {
        let io = MockIo::new(vec![Some(blocked_claude(1_000_000))], acquired()).with_hook_pending();
        let r = act(&io, &hook_action(), "%1", FireArgs::default());
        assert_eq!(r.outcome, Outcome::Sent);
        assert!(io.hook_writes.borrow().is_empty());
    }

    /// A verdict that already exists is the request having been answered in the gap: `vanished`
    /// with the same `request-gone` token the API lane's 404 reports, and nothing is clobbered.
    #[test]
    fn an_existing_verdict_maps_to_vanished() {
        let io = MockIo::new(
            vec![Some(blocked_claude_with_request(1_000_000))],
            acquired(),
        )
        .with_hook_result(VerdictWrite::Exists);
        let r = act(&io, &hook_action(), "%1", FireArgs::default());
        assert_eq!(r.outcome.token(), "vanished");
        assert_eq!(r.exit_code(), 3);
        assert_eq!(r.reason(), Some("request-gone"));
        assert!(
            io.request_cleared.borrow().is_empty(),
            "a request that was already answered is not ours to unstamp"
        );
    }

    /// The binder is checked before the arm is chosen, so a dispatch quoting a request the pane no
    /// longer carries never reaches the verdict file.
    #[test]
    fn a_stale_expectation_refuses_before_the_verdict_is_written() {
        let io = MockIo::new(
            vec![Some(blocked_claude_with_request(1_000_000))],
            acquired(),
        )
        .with_hook_pending();
        let r = act(
            &io,
            &hook_action(),
            "%1",
            FireArgs {
                expect_permission_request: Some("some-other-id"),
                ..FireArgs::default()
            },
        );
        assert_eq!(r.reason(), Some("request-gone"));
        assert_eq!(r.exit_code(), 4);
        assert!(io.hook_writes.borrow().is_empty(), "nothing was written");
        assert!(io.sent.borrow().is_none(), "and nothing was typed either");
    }

    // ---- API-channel lane -----------------------------------------------------------------------

    #[test]
    fn api_reply_ok_replies_and_releases() {
        let io = MockIo::new(vec![Some(blocked_opencode(1_000_000))], acquired());
        let r = act(&io, &api_action(), "%1", FireArgs::default());
        assert_eq!(r.outcome, Outcome::Replied);
        assert_eq!(r.exit_code(), 0);
        assert!(
            io.sent.borrow().is_none(),
            "the API lane sends no keystrokes"
        );
        let call = io.api_call.borrow().clone().expect("api_reply was called");
        assert_eq!(call.0, "http://127.0.0.1:4096");
        assert_eq!(call.1, "per_abc123");
        assert_eq!(call.2, ApiReply::Once);
        assert!(*io.cleared.borrow(), "the lock is released after the reply");
        assert_eq!(
            *io.request_cleared.borrow(),
            vec!["%1".to_string()],
            "a spent request id is unstamped under the same held lock"
        );
    }

    #[test]
    fn api_reply_404_maps_to_vanished() {
        let io = MockIo::new(vec![Some(blocked_opencode(1_000_000))], acquired())
            .with_api_result(HttpOutcome::NotFound);
        let r = act(&io, &api_action(), "%1", FireArgs::default());
        assert_eq!(r.outcome.token(), "vanished");
        assert_eq!(r.exit_code(), 3, "a 404 is the target vanished (exit 3)");
        assert_eq!(
            r.reason(),
            Some("request-gone"),
            "the request went away, not the pane"
        );
        assert!(*io.cleared.borrow(), "the lock is released on 404");
        assert!(
            io.request_cleared.borrow().is_empty(),
            "a 404 leaves the stamp alone: it may already name a newer request"
        );
    }

    #[test]
    fn api_reply_server_error_maps_to_error() {
        let io = MockIo::new(vec![Some(blocked_opencode(1_000_000))], acquired())
            .with_api_result(HttpOutcome::Error("connect failed".to_string()));
        let r = act(&io, &api_action(), "%1", FireArgs::default());
        assert!(matches!(r.outcome, Outcome::Error(_)));
        assert_eq!(
            r.exit_code(),
            1,
            "unreachable/other status is a broker error"
        );
        assert!(*io.cleared.borrow());
    }

    #[test]
    fn api_missing_request_id_refuses_requires_unmet_before_the_lock() {
        let mut facts = blocked_opencode(1_000_000);
        facts.permission_request = None;
        let io = MockIo::new(vec![Some(facts)], acquired());
        let r = act(&io, &api_action(), "%1", FireArgs::default());
        assert_eq!(r.reason(), Some("requires-unmet"));
        assert_eq!(r.exit_code(), 4);
        assert!(
            !*io.cleared.borrow(),
            "refused before the lock, nothing to clear"
        );
        assert!(io.api_call.borrow().is_none(), "no HTTP call on a refusal");
    }

    #[test]
    fn api_missing_endpoint_refuses_requires_unmet() {
        let mut facts = blocked_opencode(1_000_000);
        facts.api_endpoint = None;
        let io = MockIo::new(vec![Some(facts)], acquired());
        let r = act(&io, &api_action(), "%1", FireArgs::default());
        assert_eq!(r.reason(), Some("requires-unmet"));
        assert_eq!(r.exit_code(), 4);
    }

    #[test]
    fn api_requires_not_skipped_by_force() {
        // --force skips `when`, never the API-lane requires (an empty request id is a correctness
        // precondition, like `requires`).
        let mut facts = blocked_opencode(1_000_000);
        facts.permission_request = None;
        let io = MockIo::new(vec![Some(facts)], acquired());
        let r = act(
            &io,
            &api_action(),
            "%1",
            FireArgs {
                force: true,
                ..Default::default()
            },
        );
        assert_eq!(r.reason(), Some("requires-unmet"));
    }

    #[test]
    fn api_action_dry_run_shows_endpoint_op_and_reply() {
        let io = MockIo::new(vec![Some(blocked_opencode(1_000_000))], acquired());
        let d = dry_run(&io, &api_action(), "%1");
        assert_eq!(d.gate, DryGate::Fireable);
        assert_eq!(
            d.effect,
            Effect::Api {
                endpoint: "http://127.0.0.1:4096".to_string(),
                op: "permission-reply",
                reply: "once",
            }
        );
        assert!(
            io.api_call.borrow().is_none(),
            "dry-run performs no HTTP call"
        );
    }

    #[test]
    fn exec_action_runs_and_reports_exit_code() {
        let facts = blocked_claude(1_000_000);
        let io = MockIo::new(vec![Some(facts)], acquired());
        // An exec action with no `when`: fireable, spawns `sh -c "exit 3"`, code passes through.
        let src =
            "min_engine_version = \"0.1\"\nname = \"run\"\nlabel = \"Run\"\nkind = \"exec\"\ncommand = \"exit 3\"\n";
        let action = ActionManifest::parse(src, "run", "run.toml").unwrap();
        let r = act(&io, &action, "%1", FireArgs::default());
        assert_eq!(r.outcome, Outcome::Exited(3));
        assert_eq!(r.exit_code(), 3, "child code passes through verbatim");
        assert!(
            *io.cleared.borrow(),
            "the lock is released after the child exits"
        );
    }

    // ---- the caller's binder --------------------------------------------------------------------

    /// The action every binder test fires. `blocked_claude` satisfies its `when`, so the ordinary
    /// gate passes and only the binder can decide the outcome.
    fn approve_when_blocked() -> ActionManifest {
        keys_action(
            "when = { state = [\"blocked\"], detail = [\"permission\"] }",
            "claude = [\"1\"]",
        )
    }

    /// A fresh blocked/permission claude pane in episode `ms`.
    fn at_episode(ms: u64) -> PaneFacts {
        PaneFacts {
            episode_ms: ms,
            ..blocked_claude(1_000_000)
        }
    }

    fn expecting_episode(ms: u64) -> FireArgs<'static> {
        FireArgs {
            expect_episode_ms: Some(ms),
            ..Default::default()
        }
    }

    /// A-212. The pane is still blocked on a permission prompt, so the ordinary gate passes: only
    /// the binder can tell it is a DIFFERENT prompt. Delete the check and this fires, which is the
    /// stale approve the binder exists to stop.
    #[test]
    fn an_advanced_episode_refuses_episode_changed_and_sends_nothing() {
        let io = MockIo::new(vec![Some(at_episode(1_700_000_005_000))], acquired());
        let r = act(
            &io,
            &approve_when_blocked(),
            "%5",
            expecting_episode(1_700_000_000_000),
        );
        assert_eq!(r.outcome, Outcome::Refused(Refusal::EpisodeChanged));
        assert_eq!(r.reason(), Some("episode-changed"));
        assert_eq!(r.exit_code(), 4);
        assert!(io.sent.borrow().is_none(), "a stale approve sends no keys");
        assert!(
            io.repeat_writes.borrow().is_empty(),
            "and does not extend the run"
        );
        assert!(*io.cleared.borrow(), "the lock it took is released");
    }

    /// The same fire naming the episode the pane is actually in goes through: the refusal above is
    /// the binder deciding, not the gate or the mock.
    #[test]
    fn the_expected_episode_fires() {
        let io = MockIo::new(vec![Some(at_episode(1_700_000_000_000))], acquired());
        let r = act(
            &io,
            &approve_when_blocked(),
            "%5",
            expecting_episode(1_700_000_000_000),
        );
        assert_eq!(r.outcome, Outcome::Sent);
        assert_eq!(io.sent.borrow().clone(), Some(vec!["1".to_string()]));
    }

    /// A-213. The check reads the facts from UNDER the lock, not the pre-lock read: the mock's two
    /// reads disagree, and both directions follow the second one. Move the check to the pre-lock
    /// read and each half fails with the other's verdict.
    #[test]
    fn the_binder_reads_the_under_lock_facts_not_the_pre_lock_ones() {
        // Pre-lock the episode still matches; it advances before the lock is held.
        let io = MockIo::new(
            vec![
                Some(at_episode(1_700_000_000_000)),
                Some(at_episode(1_700_000_005_000)),
            ],
            acquired(),
        );
        let r = act(
            &io,
            &approve_when_blocked(),
            "%5",
            expecting_episode(1_700_000_000_000),
        );
        assert_eq!(
            r.reason(),
            Some("episode-changed"),
            "the pre-lock read matched, so a pre-lock check would have fired"
        );
        assert!(io.sent.borrow().is_none());

        // And the reverse: pre-lock it differs, under the lock it is the expected one.
        let io = MockIo::new(
            vec![
                Some(at_episode(1_700_000_005_000)),
                Some(at_episode(1_700_000_000_000)),
            ],
            acquired(),
        );
        let r = act(
            &io,
            &approve_when_blocked(),
            "%5",
            expecting_episode(1_700_000_000_000),
        );
        assert_eq!(
            r.outcome,
            Outcome::Sent,
            "the pre-lock read differed, so a pre-lock check would have refused"
        );
    }

    /// A-215. The documented residual, pinned rather than fixed: under a backward wall-clock step
    /// the pane's episode can be EARLIER than the one the caller saw, and that still counts as
    /// changed. The binder compares for equality, never for order, so the refusal is
    /// `episode-changed` either way and field instrumentation can classify it from the receipt.
    #[test]
    fn an_episode_that_moved_backwards_still_counts_as_changed() {
        let io = MockIo::new(vec![Some(at_episode(1_699_999_995_000))], acquired());
        let r = act(
            &io,
            &approve_when_blocked(),
            "%5",
            expecting_episode(1_700_000_000_000),
        );
        assert_eq!(r.reason(), Some("episode-changed"));
        assert_eq!(r.exit_code(), 4);
        assert!(io.sent.borrow().is_none());
    }

    /// A-214, the refusing half: the opencode pane's pending id has been replaced by the one for a
    /// newer prompt. The API `requires` is satisfied (an id IS stamped), so the binder is the only
    /// thing that can refuse, and no HTTP call leaves the process.
    #[test]
    fn a_replaced_permission_request_refuses_request_gone() {
        let io = MockIo::new(vec![Some(blocked_opencode(1_000_000))], acquired());
        let r = act(
            &io,
            &api_action(),
            "%1",
            FireArgs {
                expect_permission_request: Some("per_older"),
                ..Default::default()
            },
        );
        assert_eq!(r.outcome, Outcome::Refused(Refusal::RequestGone));
        assert_eq!(r.reason(), Some("request-gone"));
        assert_eq!(
            r.exit_code(),
            4,
            "a refused dispatch, not the 404 that exits 3"
        );
        assert!(io.api_call.borrow().is_none(), "nothing is answered");
    }

    /// A-214, the passing half: the id the caller quoted is the one the pane still carries, so the
    /// ordinary gate and the API lane run exactly as they would without the binder.
    #[test]
    fn a_matching_permission_request_proceeds() {
        let io = MockIo::new(vec![Some(blocked_opencode(1_000_000))], acquired());
        let r = act(
            &io,
            &api_action(),
            "%1",
            FireArgs {
                expect_permission_request: Some("per_abc123"),
                ..Default::default()
            },
        );
        assert_eq!(r.outcome, Outcome::Replied);
        let call = io.api_call.borrow().clone().expect("api_reply was called");
        assert_eq!(call.1, "per_abc123");
    }

    /// A pane carrying no request id at all refuses the same way a mismatched one does: absent and
    /// different are one case to a caller that named an id. (A keys agent, so the API-lane
    /// `requires` check is not what refuses.)
    #[test]
    fn an_absent_permission_request_refuses_request_gone() {
        let io = MockIo::new(vec![Some(blocked_claude(1_000_000))], acquired());
        let r = act(
            &io,
            &approve_when_blocked(),
            "%5",
            FireArgs {
                expect_permission_request: Some("per_abc123"),
                ..Default::default()
            },
        );
        assert_eq!(r.reason(), Some("request-gone"));
        assert!(io.sent.borrow().is_none());
    }

    /// The detach handoff re-asserts the gate under the same held lock, so the binder is checked
    /// there too: a stale expectation hands nothing to a supervisor.
    #[test]
    fn a_stale_binder_refuses_the_detach_handoff() {
        let io = MockIo::new(vec![Some(at_episode(1_700_000_005_000))], acquired());
        let action = exec_action("detach = true");
        let r = act(&io, &action, "%1", expecting_episode(1_700_000_000_000));
        assert_eq!(r.reason(), Some("episode-changed"));
        assert!(io.spawned.borrow().is_none(), "no supervisor is launched");
        assert!(*io.cleared.borrow(), "the lock is released");
    }

    // ---- outcome / exit-code vocabulary (drift) --------------------------------------------------

    #[test]
    fn outcome_tokens_match_the_pinned_vocabulary() {
        assert_eq!(Outcome::Sent.token(), "sent");
        assert_eq!(Outcome::Replied.token(), "replied");
        assert_eq!(Outcome::Exited(0).token(), "exited");
        assert_eq!(Outcome::Spawned.token(), "spawned");
        assert_eq!(Outcome::Timeout.token(), "timeout");
        assert_eq!(Outcome::Refused(Refusal::Locked).token(), "refused");
        assert_eq!(Outcome::Vanished(Gone::Pane).token(), "vanished");
        assert_eq!(Outcome::Error(String::new()).token(), "error");
    }

    #[test]
    fn refusal_tokens_match_the_reason_vocabulary() {
        assert_eq!(
            Refusal::Gate(RefusalReason::WrongAgent).token(),
            "wrong-agent"
        );
        assert_eq!(
            Refusal::Gate(RefusalReason::NoCoverage).token(),
            "no-coverage"
        );
        assert_eq!(
            Refusal::Gate(RefusalReason::RequiresUnmet).token(),
            "requires-unmet"
        );
        assert_eq!(Refusal::Gate(RefusalReason::Gated).token(), "gated");
        assert_eq!(Refusal::Locked.token(), "locked");
        assert_eq!(Refusal::EpisodeChanged.token(), "episode-changed");
        assert_eq!(Refusal::RequestGone.token(), "request-gone");
        // The same word as the `vanished` reason, on purpose: one names a request the pane stopped
        // carrying (exit 4), the other one the server answered 404 for (exit 3).
        assert_eq!(Gone::Request.token(), Refusal::RequestGone.token());
        assert_eq!(Refusal::Payload(TextRefusal::Sigil).token(), "sigil");
        assert_eq!(
            Refusal::Payload(TextRefusal::ControlBytes).token(),
            "control-bytes"
        );
        assert_eq!(Refusal::Payload(TextRefusal::TooLong).token(), "too-long");
        assert_eq!(Refusal::Payload(TextRefusal::Empty).token(), "empty");
    }

    // ---- text actions ---------------------------------------------------------------------------

    fn text_action(when: &str, table: &str) -> ActionManifest {
        let src = format!(
            "min_engine_version = \"0.1\"\nname = \"steer\"\nlabel = \"Steer\"\nkind = \"text\"\n{when}\n[text]\n{table}\n"
        );
        ActionManifest::parse(&src, "steer", "steer.toml").unwrap()
    }

    fn idle_claude(stamped_at: u64) -> PaneFacts {
        PaneFacts {
            state: AgentState::Idle,
            detail: None,
            ..blocked_claude(stamped_at)
        }
    }

    /// A-251: the manifest's keys wrap the caller's string, in that order, inside one lock hold.
    /// The caller supplies the middle element and nothing else: the wrapping is the manifest's.
    #[test]
    fn prefix_and_suffix_wrap_the_literal_in_one_lock_hold() {
        let io = MockIo::new(vec![Some(idle_claude(1_000_000))], acquired());
        let action = text_action(
            "when = { state = [\"idle\"] }",
            "claude = { prefix = [\"i\"], suffix = [\"Enter\"] }",
        );
        let r = act(
            &io,
            &action,
            "%1",
            FireArgs {
                text: Some("ship it"),
                ..FireArgs::default()
            },
        );
        assert_eq!(r.outcome, Outcome::Sent);
        assert_eq!(
            io.sent.borrow().clone(),
            Some(vec![
                "i".to_string(),
                format!("{LITERAL}ship it"),
                "Enter".to_string(),
            ])
        );
        assert!(*io.cleared.borrow(), "the one hold is released after it");
    }

    /// A-281: the sigil refusal is host-side, and it lands before the broker has asked tmux
    /// anything at all: no pane read, no re-verify, no lock, and so nothing to release.
    #[test]
    fn a_sigil_payload_refuses_before_any_tmux_call() {
        let action = text_action("", "claude = { suffix = [\"Enter\"] }");
        for payload in ["/clear", "  /compact", "!ls"] {
            let io = MockIo::new(vec![Some(idle_claude(1_000_000))], acquired());
            let r = act(
                &io,
                &action,
                "%1",
                FireArgs {
                    text: Some(payload),
                    ..FireArgs::default()
                },
            );
            assert_eq!(
                r.outcome,
                Outcome::Refused(Refusal::Payload(TextRefusal::Sigil)),
                "{payload:?}"
            );
            assert_eq!(r.exit_code(), 4);
            assert_eq!(*io.read_calls.borrow(), 0, "the pane was never read");
            assert!(io.sent.borrow().is_none(), "and nothing was delivered");
            assert!(!*io.cleared.borrow(), "no lock was taken");
        }
    }

    /// A-282(a): a control byte anywhere in the payload refuses, the newline included. A steer is
    /// one line, and a `\r` in the middle of one would submit it early.
    #[test]
    fn control_bytes_and_an_empty_payload_refuse_before_any_tmux_call() {
        let action = text_action("", "claude = { suffix = [\"Enter\"] }");
        let cases = [
            ("first\rsecond", TextRefusal::ControlBytes),
            ("first\nsecond", TextRefusal::ControlBytes),
            ("\x1b[31m", TextRefusal::ControlBytes),
            ("", TextRefusal::Empty),
            ("   ", TextRefusal::Empty),
        ];
        for (payload, want) in cases {
            let io = MockIo::new(vec![Some(idle_claude(1_000_000))], acquired());
            let r = act(
                &io,
                &action,
                "%1",
                FireArgs {
                    text: Some(payload),
                    ..FireArgs::default()
                },
            );
            assert_eq!(
                r.outcome,
                Outcome::Refused(Refusal::Payload(want)),
                "{payload:?}"
            );
            assert_eq!(*io.read_calls.borrow(), 0, "{payload:?} read the pane");
        }
    }

    /// A-252: a text action at a blocked pane is refused by the ordinary gate, `awaiting-text`
    /// included, and nothing reaches the pane.
    #[test]
    fn text_at_a_blocked_pane_is_refused_including_awaiting_text() {
        let action = text_action("when = { state = [\"idle\"] }", "claude = {}");
        for detail in ["permission", "awaiting-text"] {
            let mut facts = idle_claude(1_000_000);
            facts.state = AgentState::Blocked;
            facts.detail = Some(detail.to_string());
            let io = MockIo::new(vec![Some(facts)], acquired());
            let r = act(
                &io,
                &action,
                "%1",
                FireArgs {
                    text: Some("try the other branch"),
                    ..FireArgs::default()
                },
            );
            assert_eq!(
                r.outcome,
                Outcome::Refused(Refusal::Gate(RefusalReason::Gated)),
                "blocked/{detail}"
            );
            assert!(io.sent.borrow().is_none(), "blocked/{detail} delivered");
        }
    }

    /// A stale stamp re-verifies for a text action exactly as it does for keys: the predicate is
    /// the kind's own `sends_keystrokes`, so steering cannot fire off an arbitrarily old paint.
    #[test]
    fn a_stale_text_action_reverifies_before_it_gates() {
        let mut stale = idle_claude(1); // stamped at ~epoch start: stale
        stale.state = AgentState::Working;
        let io = MockIo::new(vec![Some(stale)], acquired()).with_reverify(idle_claude(1_000_000));
        let action = text_action("when = { state = [\"idle\"] }", "claude = {}");
        let r = act(
            &io,
            &action,
            "%1",
            FireArgs {
                text: Some("rebase onto main"),
                ..FireArgs::default()
            },
        );
        assert!(
            *io.reverify_called.borrow(),
            "a stale text stamp re-verifies"
        );
        assert_eq!(r.outcome, Outcome::Sent);
    }

    // ---- dry-run --------------------------------------------------------------------------------

    #[test]
    fn dry_run_resolves_context_gate_and_effect_without_side_effects() {
        let facts = blocked_claude(1_000_000 - 500); // 500 ms old
        let io = MockIo::new(vec![Some(facts)], acquired());
        let action = keys_action(
            "when = { state = [\"blocked\"], detail = [\"permission\"] }",
            "claude = [\"1\"]",
        );
        let d = dry_run(&io, &action, "%1");
        assert_eq!(d.gate, DryGate::Fireable);
        assert_eq!(d.effect, Effect::Keys(vec!["1".to_string()]));
        assert_eq!(d.agent.as_deref(), Some("claude"));
        // The stamp-derived TMA_STATE carries an age; the live TMA_CWD does not.
        let state = d.context.iter().find(|c| c.name == "TMA_STATE").unwrap();
        assert_eq!(state.age_ms, Some(500));
        let cwd = d.context.iter().find(|c| c.name == "TMA_CWD").unwrap();
        assert_eq!(cwd.age_ms, None);
        // No effects were performed.
        assert!(io.sent.borrow().is_none());
        assert!(!*io.cleared.borrow());
        assert!(!*io.reverify_called.borrow());
    }

    /// A held single-flight lock shows up in the dry-run verdict (the fan-out's "locked" line), and
    /// the peek stays read-only: no acquire, no release.
    #[test]
    fn dry_run_reports_a_held_lock_but_the_gate_reason_wins() {
        let mut io = MockIo::new(vec![Some(blocked_claude(1_000_000))], acquired());
        io.lock_held = true;
        let fireable = keys_action(
            "when = { state = [\"blocked\"], detail = [\"permission\"] }",
            "claude = [\"1\"]",
        );
        assert_eq!(
            dry_run(&io, &fireable, "%1").gate,
            DryGate::Refused(Refusal::Locked)
        );
        assert!(!*io.cleared.borrow(), "the lock peek takes nothing");

        // An already-gated action keeps the gate reason: it is the more useful verdict, exactly as
        // `list_fireability` orders the two.
        let gated = keys_action("when = { state = [\"idle\"] }", "claude = [\"1\"]");
        assert_eq!(
            dry_run(&io, &gated, "%1").gate,
            DryGate::Refused(Refusal::Gate(RefusalReason::Gated))
        );
    }

    /// Only tmux saying the pane is gone reads as `vanished`; every other failed command is an
    /// `error` that keeps tmux's stderr. Before this, a key spelling tmux rejected reported
    /// "pane vanished (exit 3)" with the reason discarded.
    #[test]
    fn io_error_separates_a_gone_pane_from_a_rejected_command() {
        // `cmd` carries the whole joined argv, as `describe_argv` builds it at the tmux edge.
        let failed = |stderr: &str| TmuxError::Failed {
            cmd: "send-keys -t %999 1".to_string(),
            code: 1,
            stderr: stderr.to_string(),
        };
        // tmux 3.6a's two pane-lookup wordings, verified against a live server, plus the third
        // spelling `pane_gone` covers. All three are the PANE, which the reason token has to say:
        // the API 404 arm produces the same `vanished` outcome for a pane that is still there.
        for spelling in [
            "can't find pane: %999",
            "no such pane: %999",
            "pane not found: %999",
        ] {
            let r = ActResult {
                action: "approve".to_string(),
                pane: "%999".to_string(),
                outcome: io_error(failed(spelling)),
            };
            assert_eq!(r.outcome.token(), "vanished", "{spelling}");
            assert_eq!(r.reason(), Some("pane-gone"), "{spelling}");
            assert_eq!(r.exit_code(), 3, "{spelling}");
        }

        let rejected = io_error(failed("command send-keys: unknown flag -Z"));
        match rejected {
            Outcome::Error(msg) => assert!(
                msg.contains("unknown flag -Z"),
                "the tmux stderr is the diagnostic and must survive, got {msg:?}"
            ),
            other => panic!("a rejected command is a broker error, got {other:?}"),
        }

        // A gone server and a wedged one stay errors: there is nothing to act on either way.
        assert!(matches!(io_error(TmuxError::ServerGone), Outcome::Error(_)));
        assert!(matches!(
            io_error(TmuxError::Timeout {
                cmd: "send-keys -t %1 1".to_string(),
                secs: 3
            }),
            Outcome::Error(_)
        ));
    }

    /// The severity ladder a fan-out aggregates over: acted < locked < gated < vanished < timeout <
    /// a failed child < a broker error. A new outcome that is not ranked here fails to compile.
    #[test]
    fn severity_orders_the_outcomes_for_a_fan_out() {
        let rank = |outcome| {
            ActResult {
                action: "approve".to_string(),
                pane: "%1".to_string(),
                outcome,
            }
            .severity()
        };
        let ladder = [
            rank(Outcome::Sent),
            rank(Outcome::Refused(Refusal::Locked)),
            rank(Outcome::Refused(Refusal::Gate(RefusalReason::Gated))),
            rank(Outcome::Vanished(Gone::Pane)),
            rank(Outcome::Timeout),
            rank(Outcome::Exited(2)),
            rank(Outcome::Error(String::new())),
        ];
        assert!(
            ladder.windows(2).all(|w| w[0] < w[1]),
            "strictly increasing: {ladder:?}"
        );
        // Every success outcome ties at the floor, so a mixed batch of them still exits 0.
        for ok in [Outcome::Replied, Outcome::Spawned, Outcome::Exited(0)] {
            assert_eq!(rank(ok), ladder[0]);
        }
    }

    #[test]
    fn dry_run_on_vanished_pane_reports_vanished() {
        let io = MockIo::new(vec![None], acquired());
        let action = keys_action("", "claude = [\"1\"]");
        let d = dry_run(&io, &action, "%9");
        assert_eq!(d.gate, DryGate::Vanished);
        assert_eq!(d.effect, Effect::None);
    }

    // ---- list fireability -----------------------------------------------------------------------

    #[test]
    fn list_fireability_reports_per_action_reasons() {
        let facts = blocked_claude(1_000_000); // blocked/permission claude, session None
        let io = MockIo::new(vec![Some(facts)], acquired());
        let fireable = keys_action(
            "when = { state = [\"blocked\"], detail = [\"permission\"] }",
            "claude = [\"1\"]",
        );
        let gated = keys_action("when = { state = [\"idle\"] }", "claude = [\"1\"]");
        let wrong_agent = keys_action("", "codex = [\"y\"]");
        let needs_session = exec_action("requires = [\"session\"]");
        let actions = [fireable, gated, wrong_agent, needs_session];
        let verdicts = list_fireability(&io, &actions, "%1").unwrap().unwrap();
        assert_eq!(verdicts[0], None, "the blocked gate is satisfied");
        assert_eq!(verdicts[1].map(|r| r.token()), Some("gated"));
        assert_eq!(verdicts[2].map(|r| r.token()), Some("wrong-agent"));
        assert_eq!(verdicts[3].map(|r| r.token()), Some("requires-unmet"));
    }

    #[test]
    fn list_fireability_marks_a_held_lock_locked_but_gate_wins() {
        let mut io = MockIo::new(vec![Some(blocked_claude(1_000_000))], acquired());
        io.lock_held = true;
        let fireable = keys_action(
            "when = { state = [\"blocked\"], detail = [\"permission\"] }",
            "claude = [\"1\"]",
        );
        let gated = keys_action("when = { state = [\"idle\"] }", "claude = [\"1\"]");
        let actions = [fireable, gated];
        let verdicts = list_fireability(&io, &actions, "%1").unwrap().unwrap();
        // Otherwise-fireable → locked; already-gated stays gated (the gate reason is more useful).
        assert_eq!(verdicts[0].map(|r| r.token()), Some("locked"));
        assert_eq!(verdicts[1].map(|r| r.token()), Some("gated"));
    }

    #[test]
    fn list_fireability_on_vanished_pane_is_none() {
        let io = MockIo::new(vec![None], acquired());
        let action = keys_action("", "claude = [\"1\"]");
        assert!(list_fireability(&io, &[action], "%9").unwrap().is_none());
    }
}
