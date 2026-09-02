use tma_core::render;
use tma_core::stamp::opt;
use tma_core::{AgentState, ApiReply, FoldConfig, ReadResult, StampedState};

use tma_tmux::lock::{self, Acquire, LockError};
use tma_tmux::stamp;
use tma_tmux::tmux::{PaneRecord, Tmux, TmuxError};

use crate::config::ApiSection;
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

    fn api_reply(
        &self,
        endpoint: &str,
        request_id: &str,
        reply: ApiReply,
        timeout_ms: u64,
    ) -> HttpOutcome {
        let path = format!("/permission/{request_id}/reply");
        let body = format!("{{\"reply\":\"{}\"}}", reply.token());
        http::post_json(
            endpoint,
            &path,
            &body,
            std::time::Duration::from_millis(timeout_ms),
        )
    }

    fn set_act_repeat(&self, pane_id: &str, value: &str) {
        // Discarded like the permission-request clear below: the counter is a signal, and a pane
        // that went away between the effect and this write has nothing left to count.
        let _ = self
            .tmux
            .apply(&[render::set_pane_option(pane_id, opt::ACT_REPEAT, value)]);
    }

    fn clear_permission_request(&self, pane_id: &str) {
        // Discarded like the event path's own clear (`event::permission`): a pane that went away
        // between the reply and this write has nothing left to unset.
        let _ = self
            .tmux
            .apply(&[render::unset_pane_option(pane_id, opt::PERMISSION_REQUEST)]);
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
    PaneFacts {
        agent: opt(opt::NAME),
        state: opt(opt::STATE)
            .and_then(|v| v.parse().ok())
            .unwrap_or(AgentState::Unknown),
        detail: opt(opt::DETAIL),
        session: opt(opt::SESSION).filter(|s| valid_session(s)),
        cwd,
        // The pid-less sentinel `0` is not a process-group leader, so it does not satisfy the `pid`
        // requirement and yields an empty `TMA_PID`.
        pid: opt(opt::PID).filter(|v| v != "0"),
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
    let prev = StampedState::from_options(&rec.options)
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
        .and_then(|v| v.parse().ok());
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
