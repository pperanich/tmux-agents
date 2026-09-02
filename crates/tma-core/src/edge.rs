//! State-transition edges between two observations of the agent rows: the diff behind
//! `tma subscribe --events`.
//!
//! Pure, like the rest of this crate — the caller supplies two already-filtered row sets and stamps
//! the observation time itself. Rows are matched by pane id, and a row's *class* is the disjoint
//! [`StateToken`] one ([`StateToken::of`]): a finished-but-unreviewed pane is `done`, not `idle`, so
//! the edge vocabulary is the one `--state`, `--until`, and `tma status` already speak.

use std::collections::HashMap;

use crate::row::{AgentRow, RepoLabel, StateToken};

/// One observed transition. `from`/`to` are `None` at the ends of a pane's life: a pane appearing in
/// the second observation has no prior class, and one that vanished has no current class. That is
/// deliberately not `unknown`, which is a real observed state ("the pane is there, its agent's state
/// is unreadable") a consumer must be able to tell from "the pane was not there".
#[derive(Clone, Debug)]
pub struct Edge {
    pub pane_id: String,
    pub agent: String,
    /// The class the pane left; `None` when it just appeared.
    pub from: Option<StateToken>,
    /// The class it entered; `None` when it vanished.
    pub to: Option<StateToken>,
    /// The current row's detail (the vanished row's last detail for a departure).
    pub detail: Option<String>,
    pub locator: String,
    /// The resolved repo annotation, carried whole so an edge document can grow a key without a
    /// second nullable scalar (the [`AgentRow`] accretion rule).
    pub repo: Option<RepoLabel>,
}

/// The transitions between `prev` and `next`, matched by pane id.
///
/// - A pane in both whose class changed emits one edge (`from` → `to`), built from the `next` row.
/// - A pane only in `next` emits an appearance (`from: None`).
/// - A pane only in `prev` emits a departure (`to: None`), built from the last row seen.
/// - A pane whose class is unchanged emits nothing, even if its detail or title changed: this is a
///   state-transition stream, not a change feed.
///
/// Ordering is deterministic and independent of hashing: the `next` rows in their own order
/// (appearances and changes), then the `prev`-only rows in theirs (departures).
pub fn diff_rows(prev: &[AgentRow], next: &[AgentRow]) -> Vec<Edge> {
    let by_pane: HashMap<&str, &AgentRow> = prev.iter().map(|r| (r.pane_id.as_str(), r)).collect();
    let mut edges = Vec::new();

    for row in next {
        let to = StateToken::of(row);
        match by_pane.get(row.pane_id.as_str()) {
            Some(before) => {
                let from = StateToken::of(before);
                if from != to {
                    edges.push(edge(row, Some(from), Some(to)));
                }
            }
            None => edges.push(edge(row, None, Some(to))),
        }
    }

    let present: HashMap<&str, ()> = next.iter().map(|r| (r.pane_id.as_str(), ())).collect();
    for row in prev {
        if !present.contains_key(row.pane_id.as_str()) {
            edges.push(edge(row, Some(StateToken::of(row)), None));
        }
    }
    edges
}

/// One edge carrying `row`'s identity/annotation fields.
fn edge(row: &AgentRow, from: Option<StateToken>, to: Option<StateToken>) -> Edge {
    Edge {
        pane_id: row.pane_id.clone(),
        agent: row.agent.clone(),
        from,
        to,
        detail: row.detail.clone(),
        locator: row.locator(),
        repo: row.repo.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentState;

    fn row(pane: &str, state: AgentState) -> AgentRow {
        AgentRow {
            pane_id: pane.to_string(),
            agent: "claude".to_string(),
            state,
            detail: None,
            since: 0,
            turn_at: 0,
            session: "work".to_string(),
            window_index: 1,
            pane_index: 0,
            title: String::new(),
            attention: false,
            agent_session: None,
            context_pct: None,
            context_at: None,
            tokens: None,
            muted: false,
            model: None,
            cwd: None,
            repo: None,
            pending: None,
        }
    }

    fn done(pane: &str) -> AgentRow {
        AgentRow {
            attention: true,
            ..row(pane, AgentState::Idle)
        }
    }

    fn closed(state: AgentState) -> Option<StateToken> {
        Some(StateToken::Closed(state))
    }

    #[test]
    fn identical_observations_emit_nothing() {
        let rows = vec![row("%1", AgentState::Working), done("%2")];
        assert!(diff_rows(&rows, &rows).is_empty());
        assert!(diff_rows(&[], &[]).is_empty());
    }

    #[test]
    fn a_changed_class_emits_one_edge_from_the_new_row() {
        let prev = vec![row("%1", AgentState::Working)];
        let mut next = vec![row("%1", AgentState::Blocked)];
        next[0].detail = Some("permission".to_string());

        let edges = diff_rows(&prev, &next);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].from, closed(AgentState::Working));
        assert_eq!(edges[0].to, closed(AgentState::Blocked));
        assert_eq!(edges[0].detail.as_deref(), Some("permission"));
        assert_eq!(edges[0].locator, "work:1.0");
    }

    /// An appearance has no prior class and a departure has no current one, and neither is spelled
    /// `unknown` — a consumer must be able to tell a new pane from one whose state is unreadable.
    #[test]
    fn appearance_and_departure_carry_an_open_end() {
        let prev = vec![row("%1", AgentState::Idle)];
        let next = vec![row("%2", AgentState::Working)];
        let edges = diff_rows(&prev, &next);
        assert_eq!(edges.len(), 2);

        assert_eq!(
            edges[0].pane_id, "%2",
            "appearances come first (next order)"
        );
        assert_eq!(edges[0].from, None);
        assert_eq!(edges[0].to, closed(AgentState::Working));

        assert_eq!(edges[1].pane_id, "%1");
        assert_eq!(edges[1].from, closed(AgentState::Idle));
        assert_eq!(edges[1].to, None);

        // The open end is distinct from an observed `unknown` on both sides.
        let seen = diff_rows(&[], &[row("%3", AgentState::Unknown)]);
        assert_eq!(seen[0].from, None);
        assert_eq!(seen[0].to, closed(AgentState::Unknown));
    }

    /// The done pseudo-state is its own class, so flagging (and clearing) attention on an idle pane
    /// is a visible transition — which is the edge a notifier actually wants.
    #[test]
    fn done_is_its_own_class_on_both_sides() {
        let idle = vec![row("%1", AgentState::Idle)];
        let finished = vec![done("%1")];

        let up = diff_rows(&idle, &finished);
        assert_eq!(up.len(), 1);
        assert_eq!(up[0].from, closed(AgentState::Idle));
        assert_eq!(up[0].to, Some(StateToken::Done));

        let down = diff_rows(&finished, &idle);
        assert_eq!(down[0].from, Some(StateToken::Done));
        assert_eq!(down[0].to, closed(AgentState::Idle));

        // working → done is one edge, not working → idle followed by idle → done.
        let edges = diff_rows(&[row("%1", AgentState::Working)], &finished);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].to, Some(StateToken::Done));
    }

    /// A detail or title change under an unchanged class is not a transition.
    #[test]
    fn detail_only_change_emits_nothing() {
        let mut prev = row("%1", AgentState::Blocked);
        prev.detail = Some("permission".to_string());
        let mut next = prev.clone();
        next.detail = Some("input".to_string());
        next.title = "renamed".to_string();
        assert!(diff_rows(&[prev], &[next]).is_empty());
    }

    /// The repo annotation rides along whole, so an edge consumer can route by repo/branch without
    /// a second lookup.
    #[test]
    fn the_repo_label_rides_along() {
        let mut next = row("%1", AgentState::Blocked);
        next.repo = Some(RepoLabel {
            name: "app".to_string(),
            branch: "main".to_string(),
            worktree: false,
        });
        let edges = diff_rows(&[row("%1", AgentState::Idle)], &[next]);
        let label = edges[0].repo.as_ref().expect("label carried");
        assert_eq!(
            (label.name.as_str(), label.branch.as_str()),
            ("app", "main")
        );
    }

    /// Multiple panes: order is `next` order then `prev`-only order, so a consumer's log is stable
    /// across runs regardless of how the panes hash.
    #[test]
    fn edges_come_out_in_a_deterministic_order() {
        let prev = vec![
            row("%1", AgentState::Working),
            row("%2", AgentState::Working),
            row("%3", AgentState::Idle),
        ];
        let next = vec![
            row("%2", AgentState::Blocked),
            row("%1", AgentState::Idle),
            row("%9", AgentState::Working),
        ];
        let edges = diff_rows(&prev, &next);
        let panes: Vec<&str> = edges.iter().map(|e| e.pane_id.as_str()).collect();
        assert_eq!(panes, ["%2", "%1", "%9", "%3"]);
    }
}
