use tma_core::stamp::opt;
use tma_core::{AgentState, FoldConfig, HookVerdict, ReadResult};

use tma_tmux::lock::{self, Acquire, LockError};
use tma_tmux::stamp;
use tma_tmux::tmux::{PaneRecord, Tmux, TmuxError};

use crate::config::ApiSection;
use crate::hook_lane::{self, VerdictWrite};
use crate::http::{self, HttpOutcome};
use crate::identity::{self, PaneIdentity, Registration};
use crate::manifests::LoadedManifest;

use super::exec::spawn_supervisor_process;
use super::{BrokerIo, PaneFacts, SupervisorSpec};

// ---- the real BrokerIo over a live tmux server -------------------------------------------------

/// The production [`BrokerIo`]: a live tmux handle plus the loaded agent manifests and fold tuning
/// the on-demand re-verify needs. `server`/`notify_command` are only consulted by the detach path
/// (forwarded to the spawned supervisor); the read-only surfaces (dry-run, `--list`) leave them `None`.
pub struct TmuxBroker<'a> {
    pub tmux: &'a Tmux,
    pub manifests: &'a [LoadedManifest],
    pub cfg: &'a FoldConfig,
    /// Per-agent API endpoint fallbacks: `[api.<name>] api_base`, consulted when the pane
    /// carries no stamped `@agent_api_endpoint`.
    pub api_bases: &'a ApiSection,
    pub server: tma_tmux::tmux::Server,
    pub notify_command: Option<String>,
}

impl BrokerIo for TmuxBroker<'_> {
    fn now_ms(&self) -> u64 {
        crate::now_ms()
    }

    fn read_pane(&self, pane_id: &str) -> Result<Option<PaneFacts>, TmuxError> {
        let panes = self.tmux.list_panes()?;
        let Some(rec) = panes.iter().find(|r| r.pane_id == pane_id) else {
            return Ok(None);
        };
        let mut checked = rec.clone();
        if !program_action_current(rec) {
            checked.options.remove(opt::STATE);
            checked.options.remove(opt::PID);
        }
        if !crate::metadata::live_for_actions(&checked.options) {
            for key in tma_core::persistence::METADATA_KEYS {
                checked.options.remove(*key);
            }
        }
        let rec = &checked;
        // `#{pane_current_path}` is a live read; a failure degrades to empty (TMA_CWD unset), never
        // an act failure.
        let cwd = self
            .tmux
            .pane_format(pane_id, "#{pane_current_path}")
            .unwrap_or_default();
        // `context_covered` from the pane's agent manifest: a bound on an agent with no
        // `[telemetry.context]` channel refuses `no-coverage` (permanent), not `gated`.
        let covered = rec
            .options
            .get(opt::NAME)
            .map(|name| manifest_covers_context(self.manifests, name))
            .unwrap_or(false);
        // API endpoint: the pane-stamped value wins, else the per-agent config fallback.
        let api_endpoint = rec
            .options
            .get(opt::API_ENDPOINT)
            .filter(|v| !v.is_empty())
            .cloned()
            .or_else(|| {
                rec.options
                    .get(opt::NAME)
                    .and_then(|name| self.api_bases.api_base(name))
                    .map(str::to_string)
            });
        Ok(Some(build_facts(rec, cwd, covered, api_endpoint)))
    }

    fn reverify(&self, pane_id: &str) -> Result<(), TmuxError> {
        let panes = self.tmux.list_panes()?;
        let Some(rec) = panes.iter().find(|r| r.pane_id == pane_id) else {
            return Ok(()); // gone; the caller's re-read reports vanished
        };
        reverify_pane(
            self.tmux,
            self.manifests,
            self.cfg,
            &panes,
            rec,
            self.now_ms(),
        )
    }

    fn send_keys(&self, pane_id: &str, keys: &[String]) -> Result<(), TmuxError> {
        self.tmux.send_keys(pane_id, keys)
    }

    fn send_text(
        &self,
        pane_id: &str,
        prefix: &[String],
        text: &str,
        suffix: &[String],
    ) -> Result<(), TmuxError> {
        self.tmux.send_text(pane_id, prefix, text, suffix)
    }

    fn api_call(&self, endpoint: &str, path: &str, body: &str, timeout_ms: u64) -> HttpOutcome {
        http::post_json(
            endpoint,
            path,
            body,
            std::time::Duration::from_millis(timeout_ms),
        )
    }

    fn hook_request_pending(&self, request_id: &str) -> bool {
        hook_lane::request_pending(request_id)
    }

    fn write_hook_verdict(&self, request_id: &str, verdict: HookVerdict) -> VerdictWrite {
        hook_lane::write_verdict(request_id, verdict)
    }

    fn set_act_repeat(&self, pane_id: &str, facts: &PaneFacts, value: &str) {
        update_action_metadata(
            self.tmux,
            pane_id,
            facts,
            &[tma_tmux::persistence::OptionEdit::set(
                opt::ACT_REPEAT,
                value,
            )],
        );
    }

    fn clear_permission_request(&self, pane_id: &str, facts: &PaneFacts) {
        update_action_metadata(
            self.tmux,
            pane_id,
            facts,
            &[tma_tmux::persistence::OptionEdit::unset(
                opt::PERMISSION_REQUEST,
            )],
        );
    }

    fn clear_question_request(&self, pane_id: &str, facts: &PaneFacts) {
        update_action_metadata(
            self.tmux,
            pane_id,
            facts,
            &[tma_tmux::persistence::OptionEdit::unset(
                opt::QUESTION_REQUEST,
            )],
        );
    }

    fn acquire(
        &self,
        pane_id: &str,
        now_ms: u64,
        expiry_ms: u64,
        name: &str,
    ) -> Result<Acquire, LockError> {
        lock::acquire(
            self.tmux,
            pane_id,
            now_ms,
            expiry_ms,
            std::process::id(),
            name,
        )
    }

    fn clear(&self, pane_id: &str, nonce: &str) -> Result<(), LockError> {
        lock::clear(self.tmux, pane_id, nonce)
    }

    fn spawn_supervisor(&self, spec: &SupervisorSpec) -> Result<(), String> {
        spawn_supervisor_process(&self.server, self.notify_command.as_deref(), spec)
    }

    fn lock_held(&self, pane_id: &str, now_ms: u64) -> Result<bool, TmuxError> {
        // Peek the lock value: held iff it parses and either its absolute expiry is still in the
        // future OR its holder is still alive (the reclaim liveness rule), so `--list` agrees
        // with what a fire would decide (a live-but-expired holder still refuses exit 5).
        let stored = self.tmux.get_pane_option(pane_id, opt::ACTION)?;
        Ok(stored
            .as_deref()
            .and_then(lock::LockValue::parse)
            .is_some_and(|v| v.expiry_ms > now_ms || lock::pid_alive(v.pid)))
    }
}

fn program_action_current(rec: &PaneRecord) -> bool {
    let metadata = tma_core::persistence::ProgramMetadata::from_options(&rec.options);
    if !metadata.program_origin {
        return true;
    }
    let Some(owner) = metadata.owner else {
        return false;
    };
    if identity::program_out_of_scope(&rec.current_command).is_some() {
        return false;
    }
    tma_tmux::tmux::sample_process_chain(rec.pane_pid, owner.processes.agent().pid)
        .is_ok_and(|chain| chain == owner.processes.as_slice())
}

fn update_action_metadata(
    tmux: &Tmux,
    pane: &str,
    facts: &PaneFacts,
    edits: &[tma_tmux::persistence::OptionEdit],
) {
    let Some(binding) = crate::metadata::bind(tmux, pane, facts.session.as_deref(), false) else {
        return;
    };
    let options = &binding.read.options;
    let value = |key| options.get(key).map(String::as_str);
    let epoch = |key| {
        value(key)
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0)
    };
    if value(opt::PID).filter(|pid| *pid != "0") != facts.pid.as_deref()
        || epoch(opt::SINCE).max(epoch(opt::TURN_AT)) != facts.episode_ms
        || value(opt::PERMISSION_REQUEST) != facts.permission_request.as_deref()
        || value(opt::QUESTION_REQUEST) != facts.question_request.as_deref()
    {
        return;
    }
    crate::metadata::write(tmux, &binding, edits, false);
}

/// Assemble [`PaneFacts`] from a pane record and its live cwd. Session is charset-validated at
/// read: a value outside the safe env charset is treated as absent, so a corrupt/hostile stamp
/// never reaches `TMA_SESSION_ID` or satisfies `requires = ["session"]`.
fn build_facts(
    rec: &PaneRecord,
    cwd: String,
    context_covered: bool,
    api_endpoint: Option<String>,
) -> PaneFacts {
    let opt = |k: &str| rec.options.get(k).filter(|v| !v.is_empty()).cloned();
    let num = |k: &str| opt(k).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    let current = tma_core::persistence::current_stamp(&rec.options, crate::now_ms())
        .ok()
        .flatten()
        .map(ReadResult::into_inner);
    let usable = current.is_some() && !identity::is_ignored(&rec.options);
    PaneFacts {
        agent: opt(opt::NAME).filter(|_| {
            usable
                || !tma_core::persistence::ProgramMetadata::from_options(&rec.options)
                    .program_origin
        }),
        state: current
            .as_ref()
            .filter(|_| usable)
            .map_or(AgentState::Unknown, |stamp| stamp.state),
        detail: opt(opt::DETAIL),
        session: opt(opt::SESSION).filter(|s| valid_session(s)),
        cwd,
        // The pid-less sentinel `0` is not a process-group leader, so it does not satisfy the `pid`
        // requirement and yields an empty `TMA_PID`.
        pid: opt(opt::PID).filter(|v| usable && v != "0"),
        title: rec.title.clone(),
        locator: rec.locator(),
        stamped_at: opt(opt::STAMPED_AT)
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
        // A non-numeric or empty gauge decodes as absent (display-tolerant read).
        context_pct: opt(opt::CONTEXT_PCT).and_then(|v| v.parse().ok()),
        context_covered,
        // Both API-lane values are read-time validated like the session: the
        // request id lands in a raw HTTP request path and the endpoint in its start line, so a
        // hostile stamp with whitespace/control bytes must decode as absent, never as smuggled bytes.
        permission_request: opt(opt::PERMISSION_REQUEST).filter(|s| valid_session(s)),
        question_request: opt(opt::QUESTION_REQUEST).filter(|s| valid_session(s)),
        api_endpoint: api_endpoint.filter(|e| valid_endpoint(e)),
        // The same instant `AgentRow::episode_at` reports: the turn end wins inside an unchanged
        // idle run, where `@agent_since` alone would pin the episode to the first completion.
        episode_ms: num(opt::SINCE).max(num(opt::TURN_AT)),
        pending_tool: opt(opt::PENDING_TOOL),
        pending_call: opt(opt::PENDING_CALL),
        act_repeat: opt(opt::ACT_REPEAT),
    }
}

/// Whether `agent`'s loaded manifest declares a `[telemetry.context]` channel. Drives the
/// gate's `no-coverage` vs `gated` distinction for a context bound.
fn manifest_covers_context(manifests: &[LoadedManifest], agent: &str) -> bool {
    manifests
        .iter()
        .find(|m| m.name == agent)
        .is_some_and(|m| m.manifest.covers_context())
}

/// Validate `@agent_session` at read. A safe env token: non-empty, ASCII alphanumerics plus
/// `-` / `_`. Broader than the manifest's lowercase-only safe-token so real mixed-case ids (OpenCode
/// stamps `ses_…W6yCmb3x7wLH1X`) pass, but it still rejects commas, colons, `#{}`, whitespace,
/// control bytes, and non-ASCII, none of which a legitimate session id carries.
fn valid_session(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Read-time endpoint validation: a plain `http://` URL with no whitespace or control bytes,
/// so a hostile `@agent_api_endpoint` stamp cannot smuggle CRLF into the HTTP start line.
fn valid_endpoint(e: &str) -> bool {
    e.starts_with("http://") && e.bytes().all(|b| b.is_ascii_graphic())
}

/// One on-demand detection cycle on a single pane (the broker's re-verify gate): resolve identity,
/// then hand the capture-to-stamp path to [`capture::stamp_from_capture`], the shared guarded-stamp
/// primitive the daemon's on-demand capture also drives. The broker carries no demotion memory
/// (`demoted = false`); it only needs a fresh stamp to gate on, and probes `-F` support per call
/// (one invocation, no memo to amortize).
fn reverify_pane(
    tmux: &Tmux,
    manifests: &[LoadedManifest],
    cfg: &FoldConfig,
    panes: &[PaneRecord],
    rec: &PaneRecord,
    now: u64,
) -> Result<(), TmuxError> {
    let prev = tma_core::persistence::current_stamp(&rec.options, now)
        .ok()
        .flatten()
        .map(ReadResult::into_inner);
    let registration = match (
        prev.as_ref().and_then(|p| p.session.as_deref()),
        rec.options.get(opt::NAME),
    ) {
        (Some(session), Some(name)) => Some(Registration {
            agent_name: name.clone(),
            session: Some(session.to_string()),
        }),
        _ => None,
    };
    let procs = tma_tmux::tmux::ps_all()?;
    let anchor = rec
        .options
        .get(opt::TITLE_MATCH_PID)
        .and_then(|v| identity::TitleAnchor::parse(v));
    let identity = identity::identify(
        rec.pane_pid,
        &rec.current_command,
        &rec.title,
        &procs,
        manifests,
        anchor,
        registration.as_ref(),
    );
    let PaneIdentity::Agent(id) = identity else {
        return Ok(()); // no longer an agent; the caller's re-read gates on the (now nameless) pane
    };
    if id.agent_pid == 0 {
        return Ok(()); // pid-less registered agent: hold the existing stamp (poll-cycle parity)
    }

    if matches!(
        tma_core::persistence::read_projection(&rec.options, now),
        Ok(tma_core::persistence::ProjectionRead::Live { .. })
    ) {
        return Ok(());
    }
    let guarded = stamp::guarded_writes_supported(tmux, panes);
    crate::capture::stamp_from_capture(
        tmux,
        panes,
        rec,
        prev.as_ref(),
        &id,
        cfg,
        false,
        guarded,
        now,
    )
    // The follow-up-look verdict is the daemon's alone: this gate re-verifies on demand already.
    .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_charset_validation_allows_mixed_case_rejects_metacharacters() {
        assert!(valid_session("ses_0789d5f61ffeW6yCmb3x7wLH1X")); // OpenCode-shaped
        assert!(valid_session("65ced290-2a08-43de-aa80-d0b049d7ce30")); // UUID
        assert!(!valid_session("has,comma"));
        assert!(!valid_session("has:colon"));
        assert!(!valid_session("has space"));
        assert!(!valid_session(""));
    }

    #[test]
    fn api_endpoint_validation_rejects_smuggled_bytes() {
        assert!(valid_endpoint("http://127.0.0.1:4096"));
        assert!(valid_endpoint("http://localhost:8080/api"));
        assert!(!valid_endpoint("https://example.com")); // no TLS lane
        assert!(!valid_endpoint("http://127.0.0.1:1/\r\nX: y"));
        assert!(!valid_endpoint("http://host with space"));
        assert!(!valid_endpoint(""));
    }
}
