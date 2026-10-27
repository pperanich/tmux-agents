//! The ordered-input clear: the effectful half of [`tma_core::seen`].
//!
//! The focus hooks handle every case where the user *navigates* away from or onto a done pane. This
//! is the case they cannot see: the user never navigates at all. A client is parked on the pane, the
//! agent finishes under their eyes, and the marker stands until they happen to move. Clearing it
//! needs one extra fact — when that client was last typed into — compared against the raise instant,
//! which is [`AgentRow::episode_at`] and NOT `@agent_since`. Those agreed when this layer was built
//! and no longer do: a second completion inside one unchanged idle run re-raises the marker and
//! records the instant in `@agent_turn_at`, while `@agent_since` cannot move (it is write-once per
//! state run). Comparing against `@agent_since` would measure the user's input against the START of
//! the idle run, so a keystroke that PREDATES the second completion reads as having seen it — the
//! marker is destroyed the cycle after it is raised, which inverts this layer's one invariant.
//!
//! Deliberately cheap and deliberately quiet: the `list-clients` read happens only once some pane
//! actually carries `@agent_attention`, and every tmux failure here is a silent no-op, because a
//! presentation flag that fails to clear costs the user one keystroke while an aborted cycle costs
//! them every row.

use tma_core::seen::seen_by_input;
use tma_core::stamp::opt;
use tma_core::AgentRow;
use tma_tmux::tmux::Tmux;

/// Clear only the exact observation used to decide input was later than the raised marker.
pub fn clear_seen(
    tmux: &Tmux,
    raised: &[tma_tmux::persistence::AttentionObservation],
) -> Vec<String> {
    if raised.is_empty() {
        return Vec::new();
    }
    let Ok(clients) = tmux.client_views() else {
        return Vec::new();
    };
    raised
        .iter()
        .filter(|observation| {
            seen_by_input(&clients, &observation.read.pane_id, observation.episode_at)
        })
        .filter_map(|observation| {
            acknowledge(tmux, observation)
                .ok()
                .filter(|cleared| *cleared)
                .map(|_| observation.read.pane_id.clone())
        })
        .collect()
}

pub fn acknowledge(
    tmux: &Tmux,
    observation: &tma_tmux::persistence::AttentionObservation,
) -> Result<bool, tma_tmux::tmux::TmuxError> {
    let outcome = tma_tmux::persistence::acknowledge_observation(tmux, observation)
        .map_err(tma_tmux::stamp::persistence_error)?;
    // Advisory clears are visible truth, never a transactional effect authorization.
    Ok(outcome
        .winner()
        .options
        .get(opt::ATTENTION)
        .map(String::as_str)
        != Some("1"))
}

/// Focus arrival/departure and UI jump capture their observation before deciding the clear.
pub fn acknowledge_current(tmux: &Tmux, pane: &str) -> Result<(), tma_tmux::tmux::TmuxError> {
    let read =
        tma_tmux::persistence::read_pane(tmux, pane).map_err(tma_tmux::stamp::persistence_error)?;
    if let Some(observation) =
        tma_tmux::persistence::AttentionObservation::from_read(read, crate::now_ms())
    {
        acknowledge(tmux, &observation)?;
    }
    Ok(())
}

/// A hook handler verifies its captured dispatch revision before forming an ACK observation.
pub fn acknowledge_revision(
    tmux: &Tmux,
    revision: &tma_core::persistence::AttentionRevision,
) -> Result<(), tma_tmux::tmux::TmuxError> {
    let target = format!("${}:.%{}", revision.session().get(), revision.pane().get());
    let read = tma_tmux::persistence::read_pane(tmux, &target)
        .map_err(tma_tmux::stamp::persistence_error)?;
    if revision.matches(&read.options) {
        if let Some(observation) =
            tma_tmux::persistence::AttentionObservation::from_read(read, crate::now_ms())
        {
            acknowledge(tmux, &observation)?;
        }
    }
    Ok(())
}

/// Older known hook commands have no dispatch witness. Retain their legacy compatibility,
/// but do not let a delayed command consume a newly published program result.
pub fn acknowledge_legacy_current(
    tmux: &Tmux,
    pane: &str,
) -> Result<(), tma_tmux::tmux::TmuxError> {
    let read =
        tma_tmux::persistence::read_pane(tmux, pane).map_err(tma_tmux::stamp::persistence_error)?;
    if !read.program.program_origin {
        if let Some(observation) =
            tma_tmux::persistence::AttentionObservation::from_read(read, crate::now_ms())
        {
            acknowledge(tmux, &observation)?;
        }
    }
    Ok(())
}

/// The `(pane_id, episode_at)` pairs a clear pass would consider: the rows whose flag stands and
/// whose raise instant is known. `episode_at` is the LATER of `@agent_since` and `@agent_turn_at`,
/// which is the instant the standing marker was actually raised — `@agent_since` alone is the start
/// of the idle run and is stale for any second completion within it. A row with neither reads as
/// zero, which every client's activity postdates, so it is left alone rather than cleared blind.
///
/// Also the cheap gate — an empty result means no `list-clients` round trip at all, which is the
/// steady state for a fleet with nothing waiting to be read.
pub fn raised_panes(rows: &[AgentRow]) -> Vec<(String, u64)> {
    rows.iter()
        .filter(|r| r.attention && r.episode_at() != 0)
        .map(|r| (r.pane_id.clone(), r.episode_at()))
        .collect()
}

/// [`clear_seen`] over a cycle's rows, clearing the flag on the rows it clears so the surface this
/// cycle feeds shows the result of its own clear rather than lagging a cycle behind it. `raised` is
/// the caller's own [`raised_panes`] result — the same list it gated on, never recomputed here.
pub fn clear_seen_rows(
    tmux: &Tmux,
    rows: &mut [AgentRow],
    raised: &[tma_tmux::persistence::AttentionObservation],
) {
    let cleared = clear_seen(tmux, raised);
    for row in rows.iter_mut() {
        if cleared.contains(&row.pane_id) {
            row.attention = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tma_core::AgentState;

    /// A done row: idle, marker standing, raised at `since` and, if `turn_at` is nonzero, re-raised
    /// by a later turn end that could not move `since`.
    fn done_row(since: u64, turn_at: u64) -> AgentRow {
        AgentRow {
            acknowledgement: None,
            pane_id: "%1".to_string(),
            agent: "codex".to_string(),
            state: AgentState::Idle,
            detail: None,
            since,
            turn_at,
            session: "s".to_string(),
            window_index: 0,
            pane_index: 0,
            title: String::new(),
            attention: true,
            agent_session: None,
            transcript: None,
            permission_request: None,
            stamped_at: None,
            context_pct: None,
            context_at: None,
            tokens: None,
            quota: None,
            cost_usd: None,
            muted: false,
            model: None,
            cwd: None,
            repo: None,
            pending: None,
        }
    }

    /// The ordinary case: one completion, `turn_at` never recorded, so the raise instant IS
    /// `@agent_since` and nothing about this layer moves.
    #[test]
    fn a_first_completion_reports_its_transition_instant() {
        assert_eq!(
            raised_panes(&[done_row(1_000, 0)]),
            [("%1".to_string(), 1_000)]
        );
    }

    /// The regression. A SECOND completion inside one unchanged idle run re-raises the marker and
    /// records `@agent_turn_at`; `@agent_since` cannot move, being write-once per state run.
    /// Reporting `since` hands the clear a floor from the START of the idle run, so a keystroke that
    /// PREDATES the second completion compares later than it and takes the fresh marker down — the
    /// cycle after it was raised, before any surface showed it, with the user having typed nothing
    /// since. That inverts this layer's one invariant: fail to clear, never clear falsely.
    ///
    /// This shipped in v0.4.1. The layer was built when `since` really was the raise instant; the
    /// change that broke the premise followed it to `wait --since`, the notify dedup, the marker
    /// clamp and the payload age — every consumer that READS the instant — but not to the one that
    /// DESTROYS the flag on it.
    #[test]
    fn a_second_completion_reports_its_own_turn_end_not_the_idle_runs_start() {
        assert_eq!(
            raised_panes(&[done_row(1_000, 9_000)]),
            [("%1".to_string(), 9_000)],
            "the floor must be the instant the STANDING marker was raised"
        );
    }

    /// A stale `turn_at` from a previous episode cannot pull the floor backwards: `episode_at` is a
    /// max, so the fresher transition wins.
    #[test]
    fn a_turn_end_older_than_the_transition_does_not_lower_the_floor() {
        assert_eq!(
            raised_panes(&[done_row(9_000, 1_000)]),
            [("%1".to_string(), 9_000)]
        );
    }

    /// Neither instant known: left alone rather than cleared blind, since every client's activity
    /// postdates zero.
    #[test]
    fn a_row_with_no_recorded_instant_is_never_considered() {
        assert!(raised_panes(&[done_row(0, 0)]).is_empty());
    }
}
