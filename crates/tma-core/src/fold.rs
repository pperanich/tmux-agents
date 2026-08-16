//! The verdict fold: a pure function folding the prior stamp plus this cycle's evidence into a
//! [`Verdict`] and [`WritePlan`]. No clock, no I/O — `now` and every timestamp are injected epoch
//! **milliseconds** (`u64`); [`FoldConfig`] windows stay in seconds and convert via `*_ms` helpers.
//! Process/screen facts that don't fit an [`Evidence`] thread in as [`SnapshotFacts`].
//!
//! Precedence: (1) a fresh hook event this cycle bypasses all; (2) foreground cap — non-agent
//! foreground ⇒ `unknown`, unless a live hook claim covers a pane whose agent process is still
//! alive; (3) freeze (scrolled / history view) holds without touching the screen; (4) the
//! screen/activity/title order against the persisted claim.

use crate::evidence::{Claim, Evidence, Provenance, Source};
use crate::manifest::Manifest;
use crate::stamp::StampedState;
use crate::state::{AgentState, Detail};
use crate::verdict::{Verdict, WinningEvidence, WriteAction, WritePlan};

/// Fold tuning, injected by the bin (tma-core reads no config). The `Default` values are the
/// canonical zero-config floor; the bin's `[fold]` section defaults each field back to these.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FoldConfig {
    /// Asymmetric dwell before publishing working→idle.
    pub dwell_secs: u64,
    /// How long a working/idle hook claim survives without corroboration before screen
    /// evidence may expire it (coverage-aware decay).
    pub hook_decay_secs: u64,
    /// The same window for a `blocked` hook claim. Much longer than [`FoldConfig::hook_decay_secs`]
    /// because a permission prompt legitimately sits silent for minutes.
    pub blocked_decay_secs: u64,
    /// Per-pane stamp freshness window. Consumed by the poll driver; carried here so all tuning
    /// lives in one struct.
    pub freshness_secs: u64,
}

impl Default for FoldConfig {
    fn default() -> Self {
        FoldConfig {
            dwell_secs: 3,           // config: [fold] dwell_secs
            hook_decay_secs: 60,     // config: [fold] hook_decay_secs
            blocked_decay_secs: 300, // config: [fold] blocked_decay_secs
            freshness_secs: 3,       // config: [fold] freshness_secs
        }
    }
}

impl FoldConfig {
    /// Dwell window in **milliseconds** (the injected-timestamp unit). Config stays in
    /// seconds; this is the sole conversion point for the dwell comparison.
    pub fn dwell_ms(&self) -> u64 {
        self.dwell_secs.saturating_mul(1000)
    }
    /// Hook-decay window in milliseconds (see [`FoldConfig::dwell_ms`]).
    pub fn hook_decay_ms(&self) -> u64 {
        self.hook_decay_secs.saturating_mul(1000)
    }
    /// Blocked-hook-decay window in milliseconds (see [`FoldConfig::dwell_ms`]).
    pub fn blocked_decay_ms(&self) -> u64 {
        self.blocked_decay_secs.saturating_mul(1000)
    }
    /// The decay window that applies to a stored hook claim of `state`. Public because the poll
    /// driver needs the same boundary to decide whether a hook claim still owes a decay check.
    pub fn decay_ms_for(&self, state: AgentState) -> u64 {
        match state {
            AgentState::Blocked => self.blocked_decay_ms(),
            _ => self.hook_decay_ms(),
        }
    }
    /// Per-pane stamp freshness window in milliseconds. Consumed by the poll driver;
    /// carried here so all tuning conversions live in one place.
    pub fn freshness_ms(&self) -> u64 {
        self.freshness_secs.saturating_mul(1000)
    }
}

/// Process/screen facts that are not evidence claims but gate the fold (the cap, freeze,
/// and episode-boundary rules).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotFacts {
    /// The identified agent's pid in this pane's process tree, 0 when no agent process is walkable.
    /// Compared with the stamped pid for the episode boundary, and read as the agent's liveness by
    /// the foreground cap.
    pub pid: u32,
    /// Whether the pane's foreground process is the identified agent. When false, screen
    /// evidence is capped at `unknown`.
    pub foreground_is_agent: bool,
    /// The viewport is not the live screen ([`PaneSnapshot::scrolled`](crate::snapshot::PaneSnapshot::scrolled)).
    pub scrolled: bool,
    /// A matched screen rule carried `skip_state_update` — a history view.
    pub history_view: bool,
}

/// Fold the prior stamp and this cycle's evidence into a verdict (see module docs).
pub fn verdict(
    prev: Option<StampedState>,
    facts: &SnapshotFacts,
    evidence: &[Evidence],
    manifest: &Manifest,
    cfg: &FoldConfig,
    now: u64,
) -> Verdict {
    // Episode boundary: a pid change between cycles is same-pane agent replacement — discard the
    // prior claim and flag the reset (clears since/notified/attention). Guarded on nonzero pids
    // so an unknown pid never forces a spurious reset.
    let episode_reset = match &prev {
        Some(p) => p.pid != 0 && facts.pid != 0 && p.pid != facts.pid,
        None => false,
    };
    let prev = if episode_reset { None } else { prev };
    let prev_ref = prev.as_ref();

    // Precedence 1 — a fresh hook event this cycle wins outright. Safe to run before the
    // foreground cap only because hook evidence reaches the fold exclusively from agent-foreground
    // paths; if that changes, move the cap ahead. Filter on state claims: the newest hook may be a
    // `Lifecycle` claim (no state), and selecting it would drop an older same-cycle state hook.
    if let Some(hook) = latest(evidence, |e| {
        e.source == Source::HookEvent && matches!(e.claim, Claim::State(_))
    }) {
        if let Claim::State(sc) = &hook.claim {
            return finish_publish(
                Publish {
                    state: sc.state,
                    detail: sc.detail.clone(),
                    source: Provenance::Hook,
                    at: hook.at,
                    may_override: true,
                    label: format!("fresh hook event ({})", hook.meta),
                },
                prev_ref,
                episode_reset,
            );
        }
    }

    // Precedence 2 — foreground cap. The foreground is not the agent, so the screen belongs to
    // something else; cap at unknown. Precedes freeze so a scrolled/history non-agent pane still
    // caps rather than holding a stale claim.
    if !facts.foreground_is_agent {
        // Foreground identity gates the SCREEN, not what the agent itself reported: handing the tty
        // to $EDITOR or a pager leaves the claim standing while the agent's process is still in the
        // pane's tree. A gone pid (`facts.pid == 0`) falls through, so process evidence expires the
        // claim exactly as before; an observation-only pane has only the stale walk to go on and
        // caps honestly.
        if let Some(p) = protected_hook(prev_ref).filter(|_| facts.pid != 0) {
            return hold(
                Some(p),
                "foreground is not the agent; hook claim held (agent alive)",
                episode_reset,
                now,
            );
        }
        return finish_publish(
            Publish {
                state: AgentState::Unknown,
                detail: None,
                source: Provenance::Process,
                at: now,
                may_override: true,
                label: "foreground is not the agent".to_string(),
            },
            prev_ref,
            episode_reset,
        );
    }

    // Precedence 3 — freeze. Scrolled or history-view panes hold; we never match the screen.
    if facts.scrolled || facts.history_view {
        let why = if facts.scrolled {
            "frozen: pane scrolled"
        } else {
            "frozen: history view"
        };
        return hold(prev_ref, why, episode_reset, now);
    }

    // Precedence 4 — screen/activity/title order. Blocker chrome is a `blocked` claim from a
    // screen rule *or* the title (both fold to `capture`); accepting only `ScreenRule` here
    // silently dropped a `blocked` rule on `region = "title"`, so the three slots stay symmetric.
    let claims = ScreenClaims {
        blocker: latest(evidence, |e| {
            matches!(e.source, Source::ScreenRule | Source::Title)
                && claim_state(e) == Some(AgentState::Blocked)
        }),
        working: latest(evidence, |e| {
            e.source != Source::HookEvent && claim_state(e) == Some(AgentState::Working)
        }),
        idle: latest(evidence, |e| {
            e.source != Source::HookEvent && claim_state(e) == Some(AgentState::Idle)
        }),
    };

    // A live, non-Unknown hook claim in the store is protected: only the carve-out or
    // coverage-aware decay may flip it.
    if let Some(p) = protected_hook(prev_ref) {
        return fold_against_hook(p, &claims, manifest, cfg, now, episode_reset);
    }

    // Non-hook prior (or none): the plain ladder with hold-previous / unknown as the floor.
    if let Some(b) = claims.blocker {
        return finish_publish(
            publish_from(b, AgentState::Blocked),
            prev_ref,
            episode_reset,
        );
    }
    if let Some(w) = claims.working {
        return finish_publish(
            publish_from(w, AgentState::Working),
            prev_ref,
            episode_reset,
        );
    }
    if let Some(i) = claims.idle {
        // Asymmetric dwell applies only to working→idle.
        if prev_ref.map(|p| p.state) == Some(AgentState::Working) {
            let last_working = prev_ref.map(|p| p.evidence_at).unwrap_or(0);
            if now.saturating_sub(last_working) <= cfg.dwell_ms() {
                return hold(
                    prev_ref,
                    "dwell suppresses working→idle",
                    episode_reset,
                    now,
                );
            }
        }
        return finish_publish(publish_from(i, AgentState::Idle), prev_ref, episode_reset);
    }

    // No fresh state evidence: hold previous (stateful) or unknown (one-shot).
    hold(
        prev_ref,
        "hold previous (no fresh evidence)",
        episode_reset,
        now,
    )
}

/// The freshest non-hook state claim in each slot this cycle. `blocker` is blocked chrome from a
/// screen rule or the title; the other two are any non-hook source.
struct ScreenClaims<'a> {
    blocker: Option<&'a Evidence>,
    working: Option<&'a Evidence>,
    idle: Option<&'a Evidence>,
}

/// Fold fresh screen/activity evidence against a protected hook claim (`p.state` is
/// working, idle, or blocked).
fn fold_against_hook(
    p: &StampedState,
    claims: &ScreenClaims<'_>,
    manifest: &Manifest,
    cfg: &FoldConfig,
    now: u64,
    episode_reset: bool,
) -> Verdict {
    if p.state == AgentState::Blocked {
        // Blocker chrome under a blocked hook claim is corroboration, not a carve-out: refreshing
        // it keeps the claim's protected `hook` provenance instead of downgrading it to capture.
        if let Some(b) = claims.blocker {
            return corroborate(p, b, episode_reset);
        }
    } else if let Some(b) = claims.blocker {
        // Carve-out: visible blocker chrome overrides a working/idle hook claim iff the hook's
        // stamped evidence_at predates the capture.
        if b.at > p.evidence_at {
            return finish_publish(publish_from(b, AgentState::Blocked), Some(p), episode_reset);
        }
        return hold(
            Some(p),
            "hook claim newer than blocker chrome: carve-out suppressed (answered-prompt race)",
            episode_reset,
            now,
        );
    }

    // No blocker chrome. Consider the best contradicting/corroborating screen claim
    // (working outranks idle).
    let candidate = claims.working.or(claims.idle);
    let Some(c) = candidate else {
        return hold(
            Some(p),
            "hook held: no fresh screen evidence",
            episode_reset,
            now,
        );
    };
    let c_state = claim_state(c).expect("candidate is a state claim");

    // Corroboration: fresh evidence consistent with the hook state advances evidence_at
    // (resetting the decay clock) while keeping source = hook.
    if c_state == p.state {
        return corroborate(p, c, episode_reset);
    }

    // Contradicting screen claim: screen may expire a hook only past the decay window AND when
    // the hook's asserted state is capture-visible (`[capture].visible`). If `working` isn't
    // capture-visible, absent working chrome isn't evidence work ended — trust the hook. Mere
    // silence never decays anything: this gate is only reached with positive contrary chrome.
    let decayed = now.saturating_sub(p.evidence_at) > cfg.decay_ms_for(p.state)
        && manifest.capture.visible.contains(&p.state);
    if !decayed {
        return hold(
            Some(p),
            "hook not decayed: contradicting screen evidence suppressed",
            episode_reset,
            now,
        );
    }

    // Decayed. Screen wins, still subject to working→idle dwell.
    if p.state == AgentState::Working
        && c_state == AgentState::Idle
        && now.saturating_sub(p.evidence_at) <= cfg.dwell_ms()
    {
        return hold(Some(p), "dwell suppresses working→idle", episode_reset, now);
    }
    let mut publish = publish_from(c, c_state);
    publish.may_override = true; // decay expired the hook claim
    publish.label = format!("hook decayed; {} wins", source_name(c.source));
    finish_publish(publish, Some(p), episode_reset)
}

/// Fresh evidence consistent with a stored hook claim: advance `evidence_at` (resetting the decay
/// clock) while keeping `source = hook`, so the claim stays protected.
fn corroborate(p: &StampedState, e: &Evidence, episode_reset: bool) -> Verdict {
    Verdict {
        state: p.state,
        detail: p.detail.clone(),
        winning_evidence: WinningEvidence {
            source: Provenance::Hook,
            at: e.at,
            label: format!("hook {} corroborated by {}", p.state, source_name(e.source)),
        },
        writes: WritePlan {
            action: WriteAction::Publish,
            may_override: false,
            set_attention: false,
            episode_reset,
        },
    }
}

/// A pending publish before transition-dependent fields (attention, since) are resolved.
struct Publish {
    state: AgentState,
    detail: Option<Detail>,
    source: Provenance,
    at: u64,
    may_override: bool,
    label: String,
}

fn publish_from(e: &Evidence, state: AgentState) -> Publish {
    Publish {
        state,
        detail: claim_detail(e),
        source: e.source.provenance(),
        at: e.at,
        may_override: false,
        label: format!("{} evidence ({})", source_name(e.source), e.meta),
    }
}

/// Resolve a [`Publish`] into a [`Verdict`], computing `set_attention` from the
/// transition (blocked entry; working→idle completion — the noteworthy ones).
fn finish_publish(p: Publish, prev: Option<&StampedState>, episode_reset: bool) -> Verdict {
    let prev_state = prev.map(|s| s.state);
    let set_attention = match p.state {
        AgentState::Blocked => prev_state != Some(AgentState::Blocked),
        AgentState::Idle => prev_state == Some(AgentState::Working),
        _ => false,
    };
    Verdict {
        state: p.state,
        detail: p.detail,
        winning_evidence: WinningEvidence {
            source: p.source,
            at: p.at,
            label: p.label,
        },
        writes: WritePlan {
            action: WriteAction::Publish,
            may_override: p.may_override,
            set_attention,
            episode_reset,
        },
    }
}

/// A hold verdict: refresh freshness only. With no prior to hold, fall back to a
/// first-sight `unknown` publish (the one-shot floor).
fn hold(prev: Option<&StampedState>, label: &str, episode_reset: bool, now: u64) -> Verdict {
    match prev {
        Some(p) => Verdict {
            state: p.state,
            detail: p.detail.clone(),
            winning_evidence: WinningEvidence {
                source: p.source,
                at: p.evidence_at,
                label: label.to_string(),
            },
            writes: WritePlan {
                action: WriteAction::Hold,
                may_override: false,
                set_attention: false,
                episode_reset,
            },
        },
        None => Verdict {
            state: AgentState::Unknown,
            detail: None,
            winning_evidence: WinningEvidence {
                source: Provenance::Capture,
                at: now,
                label: format!("{label} (no prior; unknown)"),
            },
            writes: WritePlan {
                action: WriteAction::Publish,
                may_override: false,
                set_attention: false,
                episode_reset,
            },
        },
    }
}

/// The stored claim when it is a protected hook claim: hook-sourced and asserting a real state.
/// Only the blocker carve-out, coverage-aware decay, or a dead agent pid may flip one.
fn protected_hook(prev: Option<&StampedState>) -> Option<&StampedState> {
    prev.filter(|p| p.source == Provenance::Hook && p.state != AgentState::Unknown)
}

fn latest(evidence: &[Evidence], pred: impl Fn(&Evidence) -> bool) -> Option<&Evidence> {
    evidence.iter().filter(|e| pred(e)).max_by_key(|e| e.at)
}

fn claim_state(e: &Evidence) -> Option<AgentState> {
    match &e.claim {
        Claim::State(sc) => Some(sc.state),
        Claim::Lifecycle { .. } => None,
    }
}

fn claim_detail(e: &Evidence) -> Option<Detail> {
    match &e.claim {
        Claim::State(sc) => sc.detail.clone(),
        Claim::Lifecycle { .. } => None,
    }
}

fn source_name(s: Source) -> &'static str {
    match s {
        Source::HookEvent => "hook",
        Source::ScreenRule => "screen",
        Source::Title => "title",
        Source::ActivityDelta => "activity",
        Source::ProcessFact => "process",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evidence::StateClaim;

    const AGENT_PID: u32 = 100;

    fn manifest_with(visible: &str) -> Manifest {
        let src = format!(
            "min_engine_version = \"0.1\"\n[identity]\nprocess_names = [\"x\"]\n[capture]\nvisible = [{visible}]\n"
        );
        Manifest::parse(&src, "test.toml").unwrap()
    }

    /// Default manifest: all three real states are capture-visible.
    fn manifest() -> Manifest {
        manifest_with("\"working\", \"idle\", \"blocked\"")
    }

    fn facts() -> SnapshotFacts {
        SnapshotFacts {
            pid: AGENT_PID,
            foreground_is_agent: true,
            scrolled: false,
            history_view: false,
        }
    }

    fn ev(source: Source, state: AgentState, at: u64) -> Evidence {
        Evidence {
            source,
            claim: Claim::State(StateClaim {
                state,
                detail: None,
            }),
            at,
            meta: "t".to_string(),
        }
    }

    fn ev_detail(source: Source, state: AgentState, detail: &str, at: u64) -> Evidence {
        Evidence {
            source,
            claim: Claim::State(StateClaim {
                state,
                detail: Some(Detail::new(detail)),
            }),
            at,
            meta: "t".to_string(),
        }
    }

    /// A stamped prior with the transition-time fields collapsed onto `evidence_at`.
    fn stamp(state: AgentState, source: Provenance, evidence_at: u64) -> StampedState {
        StampedState {
            state,
            detail: None,
            source,
            evidence_at,
            since: evidence_at,
            stamped_at: evidence_at,
            attention: false,
            notified_at: None,
            hash: None,
            pid: AGENT_PID,
            session: None,
            subagents: vec![],
        }
    }

    fn run(
        prev: Option<StampedState>,
        f: SnapshotFacts,
        evidence: &[Evidence],
        now: u64,
    ) -> Verdict {
        verdict(prev, &f, evidence, &manifest(), &FoldConfig::default(), now)
    }

    // ---- Precedence 4: title-region blocker chrome -----------------------------

    #[test]
    fn title_region_blocked_evidence_wins_the_blocker_slot() {
        // A `blocked` rule declared on `region = "title"` reaches the fold as `Source::Title`.
        // It must fill the blocker slot the same as a `ScreenRule` (both fold to `capture`) —
        // regression guard for the silently-dropped combination.
        let v = run(
            None,
            facts(),
            &[ev(Source::Title, AgentState::Blocked, 100)],
            200,
        );
        assert_eq!(v.state, AgentState::Blocked);
        assert_eq!(v.writes.action, WriteAction::Publish);
        assert_eq!(v.winning_evidence.source, Provenance::Capture);
    }

    #[test]
    fn title_blocker_carves_out_a_stale_working_hook() {
        // Title blocker chrome overrides a working hook claim it postdates (the carve-out),
        // exactly as screen-rule blocker chrome does.
        let prev = Some(stamp(AgentState::Working, Provenance::Hook, 100));
        let v = run(
            prev,
            facts(),
            &[ev(Source::Title, AgentState::Blocked, 200)],
            300,
        );
        assert_eq!(v.state, AgentState::Blocked);
        assert!(v.writes.set_attention, "entering blocked flags attention");
    }

    #[test]
    fn manifest_blocked_title_rule_drives_a_blocked_verdict() {
        // Manifest-load level: a `blocked` rule on `region = "title"` parses, the engine emits
        // `Source::Title` evidence, and the fold resolves it to a blocked verdict end to end.
        use crate::engine::RuleEngine;
        use crate::snapshot::PaneSnapshot;

        let src = "min_engine_version = \"0.1\"\n[identity]\nprocess_names = [\"x\"]\n\
                   [capture]\nvisible = [\"working\", \"idle\", \"blocked\"]\n\
                   [[rules]]\nstate = \"blocked\"\ndetail = \"permission\"\npriority = 100\n\
                   region = \"title\"\nmatch = { contains = \"Waiting\" }\n";
        let m = Manifest::parse(src, "t.toml").unwrap();
        let eng = RuleEngine::build(&m).unwrap();
        let snap = PaneSnapshot {
            pane_id: "%1".to_string(),
            pid_tree: vec![],
            title: "Waiting for approval".to_string(),
            tail_text: String::new(),
            tail_hash: 0,
            alternate_on: true,
            scroll_position: None,
            visible_height: None,
            captured_at: 500,
        };
        let evaluation = eng.evaluate(&snap);
        assert!(
            evaluation
                .evidence
                .iter()
                .any(|e| e.source == Source::Title),
            "the blocked title rule emits Source::Title evidence"
        );
        let v = verdict(
            None,
            &facts(),
            &evaluation.evidence,
            &m,
            &FoldConfig::default(),
            600,
        );
        assert_eq!(v.state, AgentState::Blocked);
        assert_eq!(
            v.detail.as_ref().map(|d| d.as_str()),
            Some("permission"),
            "the rule's detail rides through"
        );
    }

    // ---- Precedence 1: fresh hook event -----------------------------------------

    #[test]
    fn hook_state_wins_when_a_newer_hook_is_lifecycle() {
        // Newest hook by timestamp is a lifecycle claim (no state). The fold must still take
        // the latest *state*-claim hook (working at t=10) rather than falling through to the
        // screen ladder.
        let evidence = [
            ev(Source::HookEvent, AgentState::Working, 10),
            Evidence {
                source: Source::HookEvent,
                claim: Claim::Lifecycle {
                    lifecycle: crate::evidence::Lifecycle::Start,
                },
                at: 20,
                meta: "t".to_string(),
            },
        ];
        let v = run(None, facts(), &evidence, 30);
        assert_eq!(v.state, AgentState::Working);
        assert_eq!(v.writes.action, WriteAction::Publish);
        assert_eq!(v.winning_evidence.source, Provenance::Hook);
    }

    // ---- Rule 1: foreground cap -------------------------------------------------

    #[test]
    fn foreground_not_agent_caps_unknown_over_hold() {
        // An observation-only pane (capture-sourced prior): the walk's identity is stale the moment
        // the foreground is someone else's, so the honest answer is unknown.
        let mut f = facts();
        f.foreground_is_agent = false;
        let prev = Some(stamp(AgentState::Blocked, Provenance::Capture, 50));
        let v = run(prev, f, &[], 100);
        assert_eq!(v.state, AgentState::Unknown);
        assert_eq!(v.writes.action, WriteAction::Publish);
        assert!(v.writes.may_override);
        assert_eq!(v.winning_evidence.source, Provenance::Process);
    }

    #[test]
    fn foreground_cap_beats_freeze_on_non_agent_pane() {
        let mut f = facts();
        f.foreground_is_agent = false;
        f.scrolled = true;
        let prev = Some(stamp(AgentState::Working, Provenance::Capture, 50));
        let v = run(prev, f, &[], 100);
        assert_eq!(v.state, AgentState::Unknown);
        assert!(v.writes.may_override);
    }

    // ---- Rule 1b: a live hook claim outlives a foreign foreground ----------------

    #[test]
    fn blocked_hook_survives_an_editor_foreground_while_the_agent_lives() {
        // The agent shelled out to $EDITOR mid-prompt: the foreground is vim, but the agent process
        // is still in the pane's tree, so its blocked claim stands.
        let mut f = facts();
        f.foreground_is_agent = false;
        let prev = Some(stamp(AgentState::Blocked, Provenance::Hook, 50));
        let v = run(prev, f, &[], 100);
        assert_eq!(v.state, AgentState::Blocked);
        assert_eq!(v.writes.action, WriteAction::Hold);
        assert!(!v.writes.may_override);
        assert_eq!(v.winning_evidence.source, Provenance::Hook);
    }

    #[test]
    fn a_foreign_foreground_expires_the_claim_once_the_agent_pid_is_gone() {
        // Same pane, but the agent died behind the editor: no walkable pid, so process evidence
        // expires the claim exactly as before the grace.
        let mut f = facts();
        f.foreground_is_agent = false;
        f.pid = 0;
        let prev = Some(stamp(AgentState::Blocked, Provenance::Hook, 50));
        let v = run(prev, f, &[], 100);
        assert_eq!(v.state, AgentState::Unknown);
        assert_eq!(v.writes.action, WriteAction::Publish);
        assert!(v.writes.may_override);
        assert_eq!(v.winning_evidence.source, Provenance::Process);
    }

    #[test]
    fn working_hook_survives_a_pager_foreground() {
        // A pager over the agent's own output: still mid-task, still working.
        let mut f = facts();
        f.foreground_is_agent = false;
        let prev = Some(stamp(AgentState::Working, Provenance::Hook, 50));
        let v = run(
            prev,
            f,
            &[ev(Source::ScreenRule, AgentState::Idle, 100)],
            100,
        );
        assert_eq!(v.state, AgentState::Working);
        assert_eq!(v.writes.action, WriteAction::Hold);
        assert_eq!(
            v.winning_evidence.source,
            Provenance::Hook,
            "the pager's screen is capped, so it cannot promote or demote anything"
        );
    }

    // ---- Rule 2: evidence order -------------------------------------------------

    #[test]
    fn blocker_chrome_beats_activity_working() {
        let evidence = [
            ev(Source::ActivityDelta, AgentState::Working, 10),
            ev(Source::ScreenRule, AgentState::Blocked, 10),
        ];
        let v = run(None, facts(), &evidence, 20);
        assert_eq!(v.state, AgentState::Blocked);
    }

    #[test]
    fn activity_working_beats_idle_chrome() {
        let evidence = [
            ev(Source::ScreenRule, AgentState::Idle, 10),
            ev(Source::ActivityDelta, AgentState::Working, 10),
        ];
        let v = run(None, facts(), &evidence, 20);
        assert_eq!(v.state, AgentState::Working);
    }

    #[test]
    fn idle_chrome_when_only_idle() {
        let evidence = [ev(Source::ScreenRule, AgentState::Idle, 10)];
        let v = run(None, facts(), &evidence, 20);
        assert_eq!(v.state, AgentState::Idle);
    }

    #[test]
    fn holds_previous_with_no_evidence() {
        let prev = Some(stamp(AgentState::Working, Provenance::Capture, 10));
        let v = run(prev, facts(), &[], 20);
        assert_eq!(v.state, AgentState::Working);
        assert_eq!(v.writes.action, WriteAction::Hold);
    }

    #[test]
    fn unknown_one_shot_with_no_prev_no_evidence() {
        let v = run(None, facts(), &[], 20);
        assert_eq!(v.state, AgentState::Unknown);
        assert_eq!(v.writes.action, WriteAction::Publish);
    }

    // ---- Rule 3: coverage-aware decay ------------------------------------------

    // Timestamps are epoch **milliseconds**; the 60 s decay / 3 s dwell windows are
    // expressed here at ms scale (`60_000` / `3_000`) — a unit conversion of the boundary
    // values, not a change to the boundary semantics.

    #[test]
    fn decay_expires_stale_working_hook_for_visible_idle() {
        // Working hook claim aged past 60 s, contradicting idle chrome, idle is visible.
        let prev = Some(stamp(AgentState::Working, Provenance::Hook, 0));
        let evidence = [ev(Source::ScreenRule, AgentState::Idle, 100_000)];
        let v = run(prev, facts(), &evidence, 100_000);
        assert_eq!(v.state, AgentState::Idle);
        assert_eq!(v.writes.action, WriteAction::Publish);
        assert!(v.writes.may_override);
    }

    #[test]
    fn no_decay_within_window_holds_hook() {
        let prev = Some(stamp(AgentState::Working, Provenance::Hook, 90_000));
        let evidence = [ev(Source::ScreenRule, AgentState::Idle, 100_000)];
        let v = run(prev, facts(), &evidence, 100_000);
        assert_eq!(v.state, AgentState::Working);
        assert_eq!(v.writes.action, WriteAction::Hold);
    }

    #[test]
    fn decay_boundary_at_exactly_60s_holds() {
        // Age == hook_decay window (60 s = 60_000 ms). The gate is strict
        // (`> cfg.hook_decay_ms()`), so exactly-at-boundary is NOT decayed and the hook holds.
        let prev = Some(stamp(AgentState::Working, Provenance::Hook, 40_000));
        let evidence = [ev(Source::ScreenRule, AgentState::Idle, 100_000)];
        let v = run(prev, facts(), &evidence, 100_000);
        assert_eq!(v.state, AgentState::Working);
        assert_eq!(v.writes.action, WriteAction::Hold);
    }

    #[test]
    fn decay_boundary_at_61s_expires() {
        // Age == 61 s (61_000 ms) > 60_000: decayed, so contradicting idle chrome expires it.
        let prev = Some(stamp(AgentState::Working, Provenance::Hook, 39_000));
        let evidence = [ev(Source::ScreenRule, AgentState::Idle, 100_000)];
        let v = run(prev, facts(), &evidence, 100_000);
        assert_eq!(v.state, AgentState::Idle);
        assert_eq!(v.writes.action, WriteAction::Publish);
        assert!(v.writes.may_override);
    }

    #[test]
    fn screen_never_decays_hook_for_capture_invisible_state() {
        // Working hook claim, but `working` is NOT capture-visible: screen may not expire it
        // even past the decay window (the hook's own state is unobservable on screen).
        let m = manifest_with("\"blocked\""); // only blocked is visible; working is not
        let prev = Some(stamp(AgentState::Working, Provenance::Hook, 0));
        let evidence = [ev(Source::ScreenRule, AgentState::Idle, 200)];
        let v = verdict(prev, &facts(), &evidence, &m, &FoldConfig::default(), 200);
        assert_eq!(v.state, AgentState::Working);
        assert_eq!(v.writes.action, WriteAction::Hold);
    }

    // ---- blocked-hook decay (the longer window) ---------------------------------

    // The blocked window is 300 s (300_000 ms): long enough that a permission prompt sitting
    // silent through a reconciliation sweep is never expired, bounded so a missed follow-up
    // hook cannot pin a pane blocked forever.

    #[test]
    fn stale_blocked_hook_decays_for_visible_idle_chrome() {
        // claude-style manifest (blocked is capture-visible), blocked hook claim aged past 300 s,
        // fresh idle chrome: the claim expires and idle publishes.
        let prev = Some(stamp(AgentState::Blocked, Provenance::Hook, 0));
        let evidence = [ev(Source::ScreenRule, AgentState::Idle, 400_000)];
        let v = run(prev, facts(), &evidence, 400_000);
        assert_eq!(v.state, AgentState::Idle);
        assert_eq!(v.writes.action, WriteAction::Publish);
        assert!(v.writes.may_override, "decay expired the blocked claim");
    }

    #[test]
    fn fresh_blocked_hook_outlives_the_hook_decay_window() {
        // Age 100 s: past `hook_decay_secs` (60 s) but well inside the blocked window, so the
        // claim holds. This is the answered-in-a-minute prompt that must not flip.
        let prev = Some(stamp(AgentState::Blocked, Provenance::Hook, 0));
        let evidence = [ev(Source::ScreenRule, AgentState::Idle, 100_000)];
        let v = run(prev, facts(), &evidence, 100_000);
        assert_eq!(v.state, AgentState::Blocked);
        assert_eq!(v.writes.action, WriteAction::Hold);
    }

    #[test]
    fn silence_never_decays_a_blocked_hook() {
        // No fresh screen claim at all (a permission prompt produces no output): the claim holds
        // regardless of age. Only positive contrary chrome may expire it.
        let prev = Some(stamp(AgentState::Blocked, Provenance::Hook, 0));
        let v = run(prev, facts(), &[], 10_000_000);
        assert_eq!(v.state, AgentState::Blocked);
        assert_eq!(v.writes.action, WriteAction::Hold);
        assert_eq!(v.winning_evidence.source, Provenance::Hook);
    }

    #[test]
    fn blocked_hook_never_decays_when_blocked_is_not_capture_visible() {
        // pi-style manifest: `blocked` is not screen-visible, so absent blocker chrome says
        // nothing about the prompt. The claim holds at any age.
        let m = manifest_with("\"working\"");
        let prev = Some(stamp(AgentState::Blocked, Provenance::Hook, 0));
        let evidence = [ev(Source::ScreenRule, AgentState::Idle, 10_000_000)];
        let v = verdict(
            prev,
            &facts(),
            &evidence,
            &m,
            &FoldConfig::default(),
            10_000_000,
        );
        assert_eq!(v.state, AgentState::Blocked);
        assert_eq!(v.writes.action, WriteAction::Hold);
    }

    #[test]
    fn blocker_chrome_corroborates_a_blocked_hook_and_resets_decay() {
        // Blocker chrome under a blocked hook claim refreshes evidence_at and keeps source = hook
        // (a capture-sourced republish would drop the claim's protection).
        let prev = Some(stamp(AgentState::Blocked, Provenance::Hook, 0));
        let evidence = [ev(Source::ScreenRule, AgentState::Blocked, 400_000)];
        let v = run(prev, facts(), &evidence, 400_000);
        assert_eq!(v.state, AgentState::Blocked);
        assert_eq!(v.winning_evidence.source, Provenance::Hook);
        assert_eq!(v.winning_evidence.at, 400_000);
        assert!(
            !v.writes.set_attention,
            "still the same blocked episode: no re-alert"
        );
    }

    #[test]
    fn corroborating_evidence_refreshes_hook_and_resets_decay() {
        // Working activity consistent with a working hook advances evidence_at, keeping
        // source = hook (so the decay clock restarts).
        let prev = Some(stamp(AgentState::Working, Provenance::Hook, 10));
        let evidence = [ev(Source::ActivityDelta, AgentState::Working, 95)];
        let v = run(prev, facts(), &evidence, 100);
        assert_eq!(v.state, AgentState::Working);
        assert_eq!(v.writes.action, WriteAction::Publish);
        assert_eq!(v.winning_evidence.source, Provenance::Hook);
        assert_eq!(v.winning_evidence.at, 95);
    }

    // ---- Rule 4: carve-out ------------------------------------------------------

    #[test]
    fn carveout_blocker_newer_than_hook_overrides() {
        let prev = Some(stamp(AgentState::Working, Provenance::Hook, 10));
        let evidence = [ev_detail(
            Source::ScreenRule,
            AgentState::Blocked,
            "permission",
            20,
        )];
        let v = run(prev, facts(), &evidence, 25);
        assert_eq!(v.state, AgentState::Blocked);
        assert_eq!(v.detail.as_ref().map(Detail::as_str), Some("permission"));
        assert!(v.writes.set_attention);
    }

    #[test]
    fn carveout_hook_newer_than_blocker_suppresses() {
        let prev = Some(stamp(AgentState::Working, Provenance::Hook, 20));
        let evidence = [ev(Source::ScreenRule, AgentState::Blocked, 10)];
        let v = run(prev, facts(), &evidence, 25);
        assert_eq!(v.state, AgentState::Working);
        assert_eq!(v.writes.action, WriteAction::Hold);
    }

    #[test]
    fn carveout_equal_timestamps_holds() {
        // Blocker chrome captured at exactly the hook's evidence_at: the carve-out is
        // strict (`b.at > p.evidence_at`, "iff evidence_at < capture"), so equal holds.
        let prev = Some(stamp(AgentState::Working, Provenance::Hook, 20));
        let evidence = [ev(Source::ScreenRule, AgentState::Blocked, 20)];
        let v = run(prev, facts(), &evidence, 25);
        assert_eq!(v.state, AgentState::Working);
        assert_eq!(v.writes.action, WriteAction::Hold);
    }

    // ---- Rule 5: dwell ----------------------------------------------------------

    #[test]
    fn dwell_suppresses_working_to_idle_within_window() {
        // age 2 s (2_000 ms) < dwell 3 s → held.
        let prev = Some(stamp(AgentState::Working, Provenance::Capture, 100_000));
        let evidence = [ev(Source::ScreenRule, AgentState::Idle, 102_000)];
        let v = run(prev, facts(), &evidence, 102_000);
        assert_eq!(v.state, AgentState::Working);
        assert_eq!(v.writes.action, WriteAction::Hold);
    }

    #[test]
    fn dwell_publishes_idle_after_window() {
        // age 4 s (4_000 ms) > dwell 3 s → idle publishes.
        let prev = Some(stamp(AgentState::Working, Provenance::Capture, 100_000));
        let evidence = [ev(Source::ScreenRule, AgentState::Idle, 104_000)];
        let v = run(prev, facts(), &evidence, 104_000);
        assert_eq!(v.state, AgentState::Idle);
        assert_eq!(v.writes.action, WriteAction::Publish);
        assert!(
            v.writes.set_attention,
            "working→idle completion is noteworthy"
        );
    }

    #[test]
    fn idle_to_working_is_immediate() {
        let prev = Some(stamp(AgentState::Idle, Provenance::Capture, 100));
        let evidence = [ev(Source::ActivityDelta, AgentState::Working, 101)];
        let v = run(prev, facts(), &evidence, 101);
        assert_eq!(v.state, AgentState::Working);
        assert_eq!(v.writes.action, WriteAction::Publish);
    }

    #[test]
    fn blocked_is_immediate_no_dwell() {
        let prev = Some(stamp(AgentState::Working, Provenance::Capture, 100));
        let evidence = [ev(Source::ScreenRule, AgentState::Blocked, 101)];
        let v = run(prev, facts(), &evidence, 101);
        assert_eq!(v.state, AgentState::Blocked);
        assert_eq!(v.writes.action, WriteAction::Publish);
    }

    // ---- Rule 6: freeze ---------------------------------------------------------

    #[test]
    fn scrolled_pane_freezes_state() {
        let mut f = facts();
        f.scrolled = true;
        let prev = Some(stamp(AgentState::Working, Provenance::Capture, 100));
        let evidence = [ev(Source::ScreenRule, AgentState::Idle, 200)];
        let v = run(prev, f, &evidence, 200);
        assert_eq!(v.state, AgentState::Working);
        assert_eq!(v.writes.action, WriteAction::Hold);
    }

    #[test]
    fn copy_mode_at_offset_zero_still_reads_the_screen() {
        // Facts built the way every driver builds them, from a snapshot whose pane just entered
        // copy-mode at the bottom (`#{scroll_position}` = 0). The screen under it is live, so
        // blocker chrome must still land — a hookless pane the user is reading stays detected.
        use crate::snapshot::PaneSnapshot;
        let snapshot = PaneSnapshot {
            pane_id: "%1".to_string(),
            pid_tree: Vec::new(),
            title: String::new(),
            tail_text: String::new(),
            tail_hash: 0,
            alternate_on: true,
            scroll_position: Some(0),
            visible_height: None,
            captured_at: 200,
        };
        let f = SnapshotFacts {
            scrolled: snapshot.scrolled(),
            ..facts()
        };
        let prev = Some(stamp(AgentState::Working, Provenance::Capture, 100));
        let evidence = [ev(Source::ScreenRule, AgentState::Blocked, 200)];
        let v = run(prev, f, &evidence, 200);
        assert_eq!(v.state, AgentState::Blocked);
        assert_eq!(v.writes.action, WriteAction::Publish);
    }

    #[test]
    fn history_view_freezes_state() {
        let mut f = facts();
        f.history_view = true;
        let prev = Some(stamp(AgentState::Blocked, Provenance::Capture, 100));
        let evidence = [ev(Source::ScreenRule, AgentState::Idle, 200)];
        let v = run(prev, f, &evidence, 200);
        assert_eq!(v.state, AgentState::Blocked);
        assert_eq!(v.writes.action, WriteAction::Hold);
    }

    // ---- Rule 7: episode boundary ----------------------------------------------

    #[test]
    fn pid_change_is_episode_boundary() {
        let mut prev = stamp(AgentState::Blocked, Provenance::Hook, 50);
        prev.pid = 999; // different from facts().pid
        let evidence = [ev(Source::ActivityDelta, AgentState::Working, 100)];
        let v = run(Some(prev), facts(), &evidence, 100);
        // Prior discarded: fresh working publishes, and the reset flag is set.
        assert_eq!(v.state, AgentState::Working);
        assert!(v.writes.episode_reset);
    }

    // ---- The three adversarial-review regression traces -------------------------

    /// (a) Hook-blocked must survive a stale capture-working clobber.
    #[test]
    fn trace_a_hook_blocked_survives_stale_capture_working() {
        let prev = Some(stamp(AgentState::Blocked, Provenance::Hook, 50));
        let evidence = [ev(Source::ScreenRule, AgentState::Working, 100)];
        let v = run(prev, facts(), &evidence, 120);
        assert_eq!(
            v.state,
            AgentState::Blocked,
            "blocked hook must not be clobbered"
        );
        assert_eq!(v.writes.action, WriteAction::Hold);
        assert_eq!(v.winning_evidence.source, Provenance::Hook);
    }

    /// (b) Answered-prompt race: a hook `working` at t1 beats capture-`blocked` from t0.
    #[test]
    fn trace_b_answered_prompt_hook_working_beats_stale_capture_blocked() {
        // Hook stamped working at evidence_at = 100 (t1); capture saw the prompt at
        // t0 = 50 (< t1), so the carve-out suppresses the blocked write.
        let prev = Some(stamp(AgentState::Working, Provenance::Hook, 100));
        let evidence = [ev(Source::ScreenRule, AgentState::Blocked, 50)];
        let v = run(prev, facts(), &evidence, 110);
        assert_eq!(v.state, AgentState::Working, "hook working (newer) wins");
        assert_eq!(v.writes.action, WriteAction::Hold);
    }

    /// (c) Dwell no-livelock: fresh idle chrome every cycle must still publish idle at
    /// t+dwell after the stream pauses — the frozen evidence_at is the key.
    #[test]
    fn trace_c_dwell_publishes_despite_fresh_idle_each_cycle() {
        // Working since t=100_000 ms (last working-consistent evidence). The stream pauses;
        // idle chrome is fresh every cycle. Hold does not advance evidence_at, so the
        // prior we feed each cycle keeps evidence_at = 100_000. Times are ms.
        let prev = || Some(stamp(AgentState::Working, Provenance::Capture, 100_000));

        // age 1..=3 s, all within the 3 s dwell → held.
        for now in [101_000, 102_000, 103_000] {
            let evidence = [ev(Source::ScreenRule, AgentState::Idle, now)];
            let v = run(prev(), facts(), &evidence, now);
            assert_eq!(v.state, AgentState::Working, "held at t={now}");
            assert_eq!(v.writes.action, WriteAction::Hold, "no publish at t={now}");
        }

        // t = 104_000 → age 4 s > dwell 3 s → idle finally publishes.
        let evidence = [ev(Source::ScreenRule, AgentState::Idle, 104_000)];
        let v = run(prev(), facts(), &evidence, 104_000);
        assert_eq!(v.state, AgentState::Idle);
        assert_eq!(v.writes.action, WriteAction::Publish);
        assert!(v.writes.set_attention);
    }

    // ---- property: never idle while a live blocker claim exists -----------------
    //
    // The golden ("never idle while blocked") guarantee, at the fold level: blocker chrome
    // overrides coexisting idle/working screen evidence. Scoped to the cleanly-generatable slice
    // of the contract — no hook evidence and no prior — so the blocker slot always wins outright.
    // Left out (each a deliberate, documented override, not an idle-while-blocked violation): a
    // fresh hook event (precedence 1) and the hook-carve-out both let a hook claim outrank the
    // screen, and a frozen (scrolled/history) pane holds its prior regardless of the screen.

    use proptest::prelude::*;

    fn arb_non_hook_source() -> impl Strategy<Value = Source> {
        prop_oneof![
            Just(Source::ScreenRule),
            Just(Source::Title),
            Just(Source::ActivityDelta),
            Just(Source::ProcessFact),
        ]
    }

    fn arb_state() -> impl Strategy<Value = AgentState> {
        prop_oneof![
            Just(AgentState::Blocked),
            Just(AgentState::Working),
            Just(AgentState::Idle),
            Just(AgentState::Unknown),
        ]
    }

    proptest! {
        /// With a live screen-rule blocked claim among arbitrary non-hook evidence (no prior),
        /// the fold resolves to blocked — never idle.
        #[test]
        fn blocker_chrome_never_folds_to_idle(
            noise in prop::collection::vec((arb_non_hook_source(), arb_state(), 0u64..1000), 0..8),
            blocker_at in 0u64..1000,
            now in 0u64..2000,
        ) {
            let mut evidence: Vec<Evidence> =
                noise.into_iter().map(|(s, st, at)| ev(s, st, at)).collect();
            evidence.push(ev(Source::ScreenRule, AgentState::Blocked, blocker_at));
            let v = run(None, facts(), &evidence, now);
            prop_assert_eq!(v.state, AgentState::Blocked);
        }
    }
}
