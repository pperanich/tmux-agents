//! `tma debug capture` and `tma debug explain`: the manifest-authoring tools, and the
//! evidence-collection path. Both run the same read + detect pipeline; capture prints the
//! fixture form, explain prints the fold's reasoning.

use tma_core::engine::region_label;
use tma_core::evidence::{Claim, Evidence, Source, StateClaim};
use tma_core::snapshot::{PaneSnapshot, ProcInfo};
use tma_core::stamp::{opt, StampedState};
use tma_core::{
    AgentState, Evaluation, FoldConfig, GrammarError, ReadResult, SnapshotFacts, Verdict,
};

use tma_tmux::tmux::{PaneRecord, Tmux, TmuxError};

use crate::identity::{self, Identified, OutOfScope, PaneIdentity, Registration};
use crate::json::{JsonWriter, JSON_SCHEMA};
use crate::manifests::LoadedManifest;

/// Everything one observation of a pane produced: the raw read, the built snapshot, the
/// identity result, and (when the pane is an agent) the detection evidence and verdict.
pub struct Observation<'a> {
    pub record: PaneRecord,
    pub snapshot: PaneSnapshot,
    pub identified: Option<Identified<'a>>,
    /// `Some(..)` when the pane is out of scope: a remote shell, or a nested multiplexer client.
    pub out_of_scope: Option<OutOfScope>,
    /// The user set `@agent_ignore` on this pane, so nothing here is an agent whatever the walk
    /// found. Reported rather than silently folded into "no manifest matched", which is what a
    /// reader would otherwise conclude.
    pub ignored: bool,
    pub prev: Option<StampedState>,
    /// Why the stored stamp did not decode, when it did not. `prev` is `None` either way — every
    /// read path treats a corrupt option as no stamp — so without this `explain` would report
    /// "(none stamped)" for a pane whose options are plainly set.
    pub stamp_error: Option<GrammarError>,
    pub evidence: Vec<Evidence>,
    pub evaluation: Option<Evaluation>,
    pub facts: SnapshotFacts,
    pub verdict: Option<Verdict>,
    pub now: u64,
}

impl Observation<'_> {
    /// Agent name from the matched manifest, or `unknown`.
    pub fn agent(&self) -> &str {
        self.identified
            .map(|i| i.manifest.name.as_str())
            .unwrap_or("unknown")
    }

    /// Detected state token for the fixture header (the fold's verdict, or `unknown`).
    pub fn state(&self) -> AgentState {
        self.verdict
            .as_ref()
            .map(|v| v.state)
            .unwrap_or(AgentState::Unknown)
    }
}

/// Errors from the debug pipeline.
#[derive(Debug, thiserror::Error)]
pub enum DebugError {
    #[error(transparent)]
    Tmux(#[from] TmuxError),
    #[error("no pane {0:?} on this tmux server")]
    NoSuchPane(String),
}

/// Read + detect one pane. Injects `now` from the system clock here in the binary — the
/// core stays clock-free.
pub fn observe<'a>(
    tmux: &Tmux,
    pane_id: &str,
    manifests: &'a [LoadedManifest],
    cfg: &FoldConfig,
) -> Result<Observation<'a>, DebugError> {
    let record = tmux
        .list_panes()?
        .into_iter()
        .find(|r| r.pane_id == pane_id)
        .ok_or_else(|| DebugError::NoSuchPane(pane_id.to_string()))?;

    let tail_text = tmux.capture_pane(pane_id)?;
    let procs = tma_tmux::tmux::ps_all()?;
    let now = crate::now_ms();

    // A malformed stamp is treated as no prior, as everywhere else, but `explain` keeps the error:
    // it is the surface whose job is saying why a pane reads the way it does.
    let (prev, stamp_error) = match StampedState::from_options(&record.options) {
        Ok(read) => (read.map(ReadResult::into_inner), None),
        Err(err) => (None, Some(err)),
    };

    // Registered half: reconstruct the hook-registered claim from the stored `@agent_session` +
    // `@agent_name`, as the poll cycle does, so `explain` agrees with the live pipeline on a
    // registered agent whose process the ps walk momentarily cannot see.
    let registration = match (
        prev.as_ref().and_then(|p| p.session.as_deref()),
        record.options.get(opt::NAME),
    ) {
        (Some(session), Some(name)) => Some(Registration {
            agent_name: name.clone(),
            session: Some(session.to_string()),
        }),
        _ => None,
    };
    let pane_identity = identity::identify(
        record.pane_pid,
        &record.current_command,
        &record.title,
        &procs,
        manifests,
        record
            .options
            .get(opt::TITLE_MATCH_PID)
            .and_then(|v| v.parse().ok()),
        registration.as_ref(),
    );
    let out_of_scope = pane_identity.out_of_scope();
    // An ignored pane is not an agent anywhere else, so `explain` must not be the one surface that
    // still folds a verdict for it.
    let ignored = identity::is_ignored(&record.options);
    let identified = match pane_identity {
        PaneIdentity::Agent(id) if !ignored => Some(id),
        _ => None,
    };
    let pid_tree: Vec<ProcInfo> = identity::subtree(record.pane_pid, &procs)
        .into_iter()
        .cloned()
        .collect();

    let snapshot = PaneSnapshot {
        pane_id: record.pane_id.clone(),
        pid_tree,
        title: record.title.clone(),
        tail_hash: fnv1a64(tail_text.as_bytes()),
        tail_text,
        alternate_on: record.alternate_on,
        scroll_position: record.scroll_position,
        // Clamp `Region::Visible` rules to the visible screen (0 ⇒ None = whole tail).
        visible_height: (record.pane_height != 0).then_some(record.pane_height),
        captured_at: now,
    };

    let (evaluation, evidence, facts, verdict) = match &identified {
        Some(id) => {
            let evaluation = id.manifest.engine.evaluate(&snapshot);
            let mut evidence = evaluation.evidence.clone();
            // Activity-delta evidence: a changed viewport hash vs the stamped baseline is
            // working evidence. Only meaningful when a prior hash exists.
            if let Some(prev) = &prev {
                if prev.hash.is_some_and(|h| h != snapshot.tail_hash) {
                    evidence.push(Evidence {
                        source: Source::ActivityDelta,
                        claim: Claim::State(StateClaim {
                            state: AgentState::Working,
                            detail: None,
                        }),
                        at: snapshot.captured_at,
                        meta: "viewport hash changed since last stamp".to_string(),
                    });
                }
            }
            let facts = SnapshotFacts {
                pid: id.agent_pid,
                foreground_is_agent: id.foreground_is_agent,
                scrolled: snapshot.scrolled(),
                history_view: evaluation.history_view,
            };
            let verdict = tma_core::verdict(
                prev.clone(),
                &facts,
                &evidence,
                &id.manifest.manifest,
                cfg,
                now,
            );
            (Some(evaluation), evidence, facts, Some(verdict))
        }
        None => {
            let facts = SnapshotFacts {
                pid: 0,
                foreground_is_agent: false,
                scrolled: snapshot.scrolled(),
                history_view: false,
            };
            (None, Vec::new(), facts, None)
        }
    };

    Ok(Observation {
        record,
        snapshot,
        identified,
        out_of_scope,
        ignored,
        prev,
        stamp_error,
        evidence,
        evaluation,
        facts,
        verdict,
        now,
    })
}

/// `tma debug capture`: emit the observation in fixture format so it can be redacted and
/// committed directly (reuses the core `Fixture::to_text` shape).
pub fn render_capture(obs: &Observation) -> String {
    // Build the fixture text by hand to avoid gating the whole bin on the `fixtures`
    // feature; the shape is byte-identical to `Fixture::to_text`.
    format!(
        "# agent: {}\n# state: {}\n# title: {}\n# command: {}\n# pid: {}\n# captured_at: {}\n---\n{}",
        obs.agent(),
        obs.state(),
        obs.record.title,
        obs.record.current_command,
        obs.record.pane_pid,
        obs.now,
        obs.snapshot.tail_text,
    )
}

/// `tma debug explain` (text form): evidence, per-rule outcomes, verdict + winning evidence.
pub fn render_explain_text(obs: &Observation) -> String {
    let mut out = String::new();
    let p = |out: &mut String, s: String| {
        out.push_str(&s);
        out.push('\n');
    };

    p(
        &mut out,
        format!(
            "pane      {}  ({})",
            obs.record.pane_id,
            obs.record.locator()
        ),
    );
    p(
        &mut out,
        format!("command   {}", obs.record.current_command),
    );
    p(&mut out, format!("title     {}", obs.record.title));
    p(
        &mut out,
        format!(
            "flags     alternate_on={} scrolled={} history_view={} window_activity={}",
            obs.record.alternate_on,
            obs.facts.scrolled,
            obs.facts.history_view,
            obs.record.window_activity
        ),
    );

    // Before identity, because it is a fact about the raw read: a stamp that does not decode reads
    // as no stamp everywhere in the pipeline, which is why `prior` below says "(none stamped)" for
    // a pane whose `@agent_*` options are plainly set.
    if let Some(err) = &obs.stamp_error {
        p(
            &mut out,
            format!("stamp     unreadable: {err} — this pane reads as never-stamped"),
        );
    }

    match &obs.identified {
        None => {
            let why = match (obs.ignored, obs.out_of_scope) {
                (true, _) => "@agent_ignore is set on this pane".to_string(),
                (false, Some(scope)) => scope.hint(),
                (false, None) => "no manifest process_names matched".to_string(),
            };
            p(&mut out, format!("agent     (none — {why})"));
            p(
                &mut out,
                format!(
                    "process   {} procs in pane tree",
                    obs.snapshot.pid_tree.len()
                ),
            );
            return out;
        }
        Some(id) => {
            // A registration that outranked a carve-out is named here: the pane IS an agent pane,
            // and the foreground says the agent is on the far side of a boundary tma cannot read.
            let how = match id.behind {
                Some(scope) => format!("{} behind {}", identity_source(id.source), scope.label()),
                None => identity_source(id.source).to_string(),
            };
            p(
                &mut out,
                format!(
                    "agent     {} (pid {}, foreground_is_agent={}, {how})",
                    id.manifest.name, id.agent_pid, id.foreground_is_agent,
                ),
            );
            if let Some(scope) = id.behind {
                p(
                    &mut out,
                    format!(
                        "boundary  {} — the cycle holds this pane's stamps and captures nothing; \
                         hook events are its only evidence",
                        scope.label()
                    ),
                );
            }
        }
    }

    if let Some(prev) = &obs.prev {
        p(
            &mut out,
            format!(
                "prior     {} / {} src={} evidence_at={} since={}",
                prev.state,
                prev.detail.as_ref().map(|d| d.as_str()).unwrap_or("-"),
                prev.source,
                prev.evidence_at,
                prev.since,
            ),
        );
    } else {
        p(&mut out, "prior     (none stamped)".to_string());
    }

    p(&mut out, String::new());
    p(&mut out, "evidence:".to_string());
    if obs.evidence.is_empty() {
        p(&mut out, "  (none)".to_string());
    }
    for e in &obs.evidence {
        p(
            &mut out,
            format!(
                "  {:<9} {:<8} at={} {}",
                source_token(e.source),
                claim_token(&e.claim),
                e.at,
                e.meta
            ),
        );
    }

    if let Some(eval) = &obs.evaluation {
        p(&mut out, String::new());
        p(&mut out, "rules:".to_string());
        if eval.reports.is_empty() {
            p(&mut out, "  (manifest has no screen rules)".to_string());
        }
        for r in &eval.reports {
            p(
                &mut out,
                format!(
                    "  [{}] #{} {:<8} pri={:<4} {:<14} {}{}",
                    if r.matched { "match" } else { "  -  " },
                    r.index,
                    r.state,
                    r.priority,
                    region_label(r.region),
                    r.detail
                        .as_ref()
                        .map(|d| format!("detail={} ", d.as_str()))
                        .unwrap_or_default(),
                    if r.skip_state_update {
                        "skip_state_update"
                    } else {
                        ""
                    },
                ),
            );
        }
    }

    if let Some(v) = &obs.verdict {
        p(&mut out, String::new());
        p(
            &mut out,
            format!(
                "verdict   {} / {}  [{}{}]",
                v.state,
                v.detail.as_ref().map(|d| d.as_str()).unwrap_or("-"),
                write_action(v),
                if v.writes.set_attention {
                    " +attention"
                } else {
                    ""
                },
            ),
        );
        p(
            &mut out,
            format!(
                "winner    src={} at={} — {}",
                v.winning_evidence.source, v.winning_evidence.at, v.winning_evidence.label
            ),
        );
    }
    out
}

/// `tma debug explain --json`: additive-only JSON with `"schema": 1`, hand-serialized to keep the
/// bin's deps at `regex` only. Absent optional fields are explicit `null`, never dropped.
pub fn render_explain_json(obs: &Observation) -> String {
    let mut j = JsonWriter::new();
    j.begin_object();
    j.number("schema", JSON_SCHEMA);
    j.string("pane", &obs.record.pane_id);
    j.string("locator", &obs.record.locator());
    j.string("command", &obs.record.current_command);
    j.string("title", &obs.record.title);
    j.string("agent", obs.agent());
    match obs.identified {
        Some(id) => j.string("identity_source", identity_source(id.source)),
        None => j.null("identity_source"),
    }
    // `out_of_scope` stays the matched command (unchanged shape); the category rides alongside it.
    match obs.out_of_scope {
        Some(scope) => j.string("out_of_scope", scope.command()),
        None => j.null("out_of_scope"),
    }
    match obs.out_of_scope {
        Some(scope) => j.string("out_of_scope_kind", scope.token()),
        None => j.null("out_of_scope_kind"),
    }
    // The carve-out a live registration outranks: the pane is in scope (so `out_of_scope` is null)
    // while its agent runs behind a boundary no capture crosses.
    let behind = obs.identified.and_then(|id| id.behind);
    match behind {
        Some(scope) => j.string("registered_behind", scope.command()),
        None => j.null("registered_behind"),
    }
    match behind {
        Some(scope) => j.string("registered_behind_kind", scope.token()),
        None => j.null("registered_behind_kind"),
    }
    // Additive (schema stays 1): the user's `@agent_ignore` opt-out, which is why an otherwise
    // recognizable pane reports no agent and no verdict.
    j.bool("ignored", obs.ignored);
    j.bool("foreground_is_agent", obs.facts.foreground_is_agent);
    j.bool("scrolled", obs.facts.scrolled);
    j.bool("history_view", obs.facts.history_view);

    j.key("evidence");
    j.begin_array();
    for e in &obs.evidence {
        j.begin_object();
        j.string("source", source_token(e.source));
        j.string("claim", &claim_token(&e.claim));
        j.number("at", e.at as i64);
        j.string("meta", &e.meta);
        j.end_object();
    }
    j.end_array();

    j.key("rules");
    j.begin_array();
    if let Some(eval) = &obs.evaluation {
        for r in &eval.reports {
            j.begin_object();
            j.number("index", r.index as i64);
            j.bool("matched", r.matched);
            j.string("state", r.state.token());
            match &r.detail {
                Some(d) => j.string("detail", d.as_str()),
                None => j.null("detail"),
            }
            j.number("priority", r.priority);
            j.string("region", &region_label(r.region));
            j.bool("skip_state_update", r.skip_state_update);
            j.end_object();
        }
    }
    j.end_array();

    j.key("verdict");
    match &obs.verdict {
        Some(v) => {
            j.begin_object();
            j.string("state", v.state.token());
            match &v.detail {
                Some(d) => j.string("detail", d.as_str()),
                None => j.null("detail"),
            }
            j.string("action", write_action(v));
            j.bool("may_override", v.writes.may_override);
            j.bool("set_attention", v.writes.set_attention);
            j.bool("episode_reset", v.writes.episode_reset);
            j.key("winning_evidence");
            j.begin_object();
            j.string("source", &v.winning_evidence.source.to_string());
            j.number("at", v.winning_evidence.at as i64);
            j.string("label", &v.winning_evidence.label);
            j.end_object();
            j.end_object();
        }
        None => j.raw_null(),
    }

    j.end_object();
    j.finish()
}

fn write_action(v: &Verdict) -> &'static str {
    match v.writes.action {
        tma_core::WriteAction::Publish => "publish",
        tma_core::WriteAction::Hold => "hold",
    }
}

fn identity_source(s: identity::IdentitySource) -> &'static str {
    match s {
        identity::IdentitySource::Observed => "observed",
        identity::IdentitySource::Registered => "registered",
    }
}

fn source_token(s: Source) -> &'static str {
    match s {
        Source::HookEvent => "hook",
        Source::ScreenRule => "screen",
        Source::Title => "title",
        Source::ActivityDelta => "activity",
        Source::ProcessFact => "process",
    }
}

fn claim_token(c: &Claim) -> String {
    match c {
        Claim::State(sc) => match &sc.detail {
            Some(d) => format!("{}/{}", sc.state, d.as_str()),
            None => sc.state.to_string(),
        },
        Claim::Lifecycle { lifecycle } => format!("lifecycle:{lifecycle:?}"),
    }
}

/// FNV-1a 64-bit content hash for the viewport tail. Stable across processes (unlike
/// `DefaultHasher`'s randomized seed), as the cross-producer `@agent_hash` comparison requires.
pub(crate) fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use tma_core::engine::RuleReport;
    use tma_core::manifest::Region;
    use tma_core::verdict::{WinningEvidence, WritePlan};
    use tma_core::{Detail, Provenance, WriteAction};

    #[test]
    fn fnv1a_is_stable_and_sensitive() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_ne!(fnv1a64(b"abc"), fnv1a64(b"abd"));
        assert_eq!(fnv1a64(b"claude"), fnv1a64(b"claude"));
    }

    /// A fully-populated observation (agent-less, but with one evidence record, one rule report, and
    /// a verdict) so every conditional key in the emitter is exercised.
    fn full_observation() -> Observation<'static> {
        let record = PaneRecord {
            pane_id: "%1".to_string(),
            pane_pid: 4242,
            session: "s".to_string(),
            window_index: 1,
            pane_index: 0,
            current_command: "claude".to_string(),
            window_activity: 0,
            alternate_on: false,
            scroll_position: None,
            pane_height: 40,
            cwd: None,
            options: std::collections::HashMap::new(),
            window_summary: None,
            session_summary: None,
            title: "a task".to_string(),
        };
        let snapshot = PaneSnapshot {
            pane_id: "%1".to_string(),
            pid_tree: Vec::new(),
            title: "a task".to_string(),
            tail_text: "READY".to_string(),
            tail_hash: 0,
            alternate_on: false,
            scroll_position: None,
            visible_height: Some(40),
            captured_at: 1_700_000_000_000,
        };
        Observation {
            record,
            snapshot,
            identified: None,
            out_of_scope: None,
            ignored: false,
            prev: None,
            stamp_error: None,
            evidence: vec![Evidence {
                source: Source::ScreenRule,
                claim: Claim::State(StateClaim {
                    state: AgentState::Blocked,
                    detail: Some(Detail::new(Detail::PERMISSION)),
                }),
                at: 1_700_000_000_000,
                meta: "rule#0".to_string(),
            }],
            evaluation: Some(Evaluation {
                evidence: Vec::new(),
                reports: vec![RuleReport {
                    index: 0,
                    state: AgentState::Blocked,
                    detail: Some(Detail::new(Detail::PERMISSION)),
                    priority: 100,
                    region: Region::TailLines(5),
                    skip_state_update: false,
                    matched: true,
                }],
                history_view: false,
            }),
            facts: SnapshotFacts {
                pid: 4242,
                foreground_is_agent: true,
                scrolled: false,
                history_view: false,
            },
            verdict: Some(Verdict {
                state: AgentState::Blocked,
                detail: Some(Detail::new(Detail::PERMISSION)),
                winning_evidence: WinningEvidence {
                    source: Provenance::Capture,
                    at: 1_700_000_000_000,
                    label: "rule#0".to_string(),
                },
                writes: WritePlan {
                    action: WriteAction::Publish,
                    may_override: false,
                    set_attention: false,
                    episode_reset: false,
                },
            }),
            now: 1_700_000_000_000,
        }
    }

    /// The complete `explain --json` key inventory (additive-only): a dropped, renamed, or new key
    /// fails here. Every optional field is explicit `null`, so the key set is stable across values.
    #[test]
    fn explain_json_pins_full_key_set() {
        let json = render_explain_json(&full_observation());
        assert_eq!(
            json_keys(&json),
            [
                "action",
                "agent",
                "at",
                "claim",
                "command",
                "detail",
                "episode_reset",
                "evidence",
                "foreground_is_agent",
                "history_view",
                "identity_source",
                "ignored",
                "index",
                "label",
                "locator",
                "matched",
                "may_override",
                "meta",
                "out_of_scope",
                "out_of_scope_kind",
                "pane",
                "priority",
                "region",
                "registered_behind",
                "registered_behind_kind",
                "rules",
                "schema",
                "scrolled",
                "set_attention",
                "skip_state_update",
                "source",
                "state",
                "title",
                "verdict",
                "winning_evidence",
            ]
        );
    }

    /// An absent optional field (here: an agent-less observation with no verdict/rule detail)
    /// still emits the key as `null`, not dropped — the null-vs-absent unification.
    #[test]
    fn explain_json_emits_null_for_absent_rule_detail() {
        let mut obs = full_observation();
        if let Some(eval) = &mut obs.evaluation {
            eval.reports[0].detail = None;
        }
        let json = render_explain_json(&obs);
        assert!(
            json.contains("\"detail\":null"),
            "absent rule detail is explicit null, not dropped:\n{json}"
        );
    }

    /// A stamp that does not decode reads as no stamp everywhere, so `explain` — the surface whose
    /// job is saying why a pane reads the way it does — names the option and the value. Printed
    /// before the identity branch, so an unidentified pane gets it too.
    #[test]
    fn explain_names_an_unreadable_stamp() {
        let mut obs = full_observation();
        obs.stamp_error = Some(GrammarError::UnknownState("spinning".to_string()));
        let text = render_explain_text(&obs);
        assert!(
            text.contains("@agent_state") && text.contains("spinning"),
            "the option and the value that broke it are named:\n{text}"
        );
        assert!(
            text.contains("never-stamped"),
            "and what it costs the pane:\n{text}"
        );

        // A pane whose options decode says nothing about stamps.
        obs.stamp_error = None;
        assert!(!render_explain_text(&obs).contains("stamp     "));
    }

    /// The sorted, de-duplicated object keys of a JSON document (a quoted string whose next non-space
    /// char is `:`). Byte-scanned, since the structural `"`, `\`, `:` are ASCII.
    fn json_keys(json: &str) -> Vec<String> {
        let b = json.as_bytes();
        let mut out = std::collections::BTreeSet::new();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'"' {
                let start = i + 1;
                let mut j = start;
                while j < b.len() {
                    match b[j] {
                        b'\\' => j += 2,
                        b'"' => break,
                        _ => j += 1,
                    }
                }
                let mut k = j + 1;
                while k < b.len() && b[k].is_ascii_whitespace() {
                    k += 1;
                }
                if k < b.len() && b[k] == b':' {
                    out.insert(json[start..j].to_string());
                }
                i = j + 1;
            } else {
                i += 1;
            }
        }
        out.into_iter().collect()
    }
}
