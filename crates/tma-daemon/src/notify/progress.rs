//! The `notify.osc_progress` edge detector: which windows just gained their first `working` pane,
//! and which just lost their last one.
//!
//! Pure, so the edge rule is testable without a tmux server. The lane is deliberately edge-driven
//! rather than level-driven: OSC 9;4 sets a state the emulator then keeps, so re-sending it every
//! poll cycle would be pure noise on the wire and one more thing to blame for a redraw.

use std::collections::BTreeSet;

use tma_core::stamp::opt;
use tma_core::AgentState;
use tma_tmux::tmux::{PaneRecord, Progress};

/// A window, keyed the way `list-panes` reports one. `#{window_id}` would be tighter, but the
/// notify pass reads [`PaneRecord`]s, which carry the session name and window index.
pub(crate) type WindowKey = (String, u32);

/// One pane reduced to what this lane reads.
pub(crate) struct ProgressPane {
    key: WindowKey,
    pane_id: String,
    working: bool,
}

/// Reduce the pass's `list-panes` read to the lane's input, in tmux's own order.
pub(crate) fn progress_panes(panes: &[PaneRecord]) -> Vec<ProgressPane> {
    panes
        .iter()
        .map(|p| ProgressPane {
            key: (p.session.clone(), p.window_index),
            pane_id: p.pane_id.clone(),
            working: p.options.get(opt::STATE).and_then(|v| v.parse().ok())
                == Some(AgentState::Working),
        })
        .collect()
}

/// The edges between `previous`'s working windows and this pass's, plus the new set to remember.
///
/// A window that gained work is written through its first working pane; one that lost its last is
/// written through its first remaining pane, whichever that is. A window that has vanished entirely
/// leaves the set with no edge, since there is no tty left to clear anything on.
pub(crate) fn progress_edges(
    previous: &BTreeSet<WindowKey>,
    panes: &[ProgressPane],
) -> (BTreeSet<WindowKey>, Vec<(String, Progress)>) {
    let working: BTreeSet<WindowKey> = panes
        .iter()
        .filter(|p| p.working)
        .map(|p| p.key.clone())
        .collect();

    let mut edges = Vec::new();
    for key in working.difference(previous) {
        if let Some(pane) = panes.iter().find(|p| p.working && &p.key == key) {
            edges.push((pane.pane_id.clone(), Progress::Indeterminate));
        }
    }
    for key in previous.difference(&working) {
        if let Some(pane) = panes.iter().find(|p| &p.key == key) {
            edges.push((pane.pane_id.clone(), Progress::Clear));
        }
    }
    (working, edges)
}

/// The clears that take every remembered window's indicator down: the shutdown path, and the
/// reload that turns `osc_progress` off. Leaving a progress bar lit on a tab nobody is working in
/// would outlive the daemon that set it.
pub(crate) fn clear_edges(
    previous: &BTreeSet<WindowKey>,
    panes: &[ProgressPane],
) -> Vec<(String, Progress)> {
    previous
        .iter()
        .filter_map(|key| panes.iter().find(|p| &p.key == key))
        .map(|p| (p.pane_id.clone(), Progress::Clear))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(session: &str, window: u32, id: &str, working: bool) -> ProgressPane {
        ProgressPane {
            key: (session.to_string(), window),
            pane_id: id.to_string(),
            working,
        }
    }

    fn keys(of: &[(&str, u32)]) -> BTreeSet<WindowKey> {
        of.iter().map(|(s, w)| (s.to_string(), *w)).collect()
    }

    #[test]
    fn the_first_working_pane_raises_the_indicator() {
        let panes = [pane("s", 0, "%1", true), pane("s", 0, "%2", false)];
        let (working, edges) = progress_edges(&BTreeSet::new(), &panes);
        assert_eq!(working, keys(&[("s", 0)]));
        assert_eq!(edges, vec![("%1".to_string(), Progress::Indeterminate)]);
    }

    #[test]
    fn a_second_working_pane_emits_nothing() {
        let panes = [pane("s", 0, "%1", true), pane("s", 0, "%2", true)];
        let (working, edges) = progress_edges(&keys(&[("s", 0)]), &panes);
        assert_eq!(working, keys(&[("s", 0)]));
        assert!(edges.is_empty(), "the state is already set: {edges:?}");
    }

    #[test]
    fn the_last_working_pane_leaving_clears_it() {
        let panes = [pane("s", 0, "%1", false), pane("s", 0, "%2", false)];
        let (working, edges) = progress_edges(&keys(&[("s", 0)]), &panes);
        assert!(working.is_empty());
        assert_eq!(edges, vec![("%1".to_string(), Progress::Clear)]);
    }

    #[test]
    fn a_window_that_vanished_leaves_no_edge() {
        let (working, edges) = progress_edges(&keys(&[("s", 0)]), &[]);
        assert!(working.is_empty());
        assert!(edges.is_empty(), "no tty is left to write to: {edges:?}");
    }

    #[test]
    fn each_window_carries_its_own_state() {
        let panes = [pane("s", 0, "%1", true), pane("s", 1, "%9", false)];
        let (working, edges) = progress_edges(&keys(&[("s", 1)]), &panes);
        assert_eq!(working, keys(&[("s", 0)]));
        assert_eq!(
            edges,
            vec![
                ("%1".to_string(), Progress::Indeterminate),
                ("%9".to_string(), Progress::Clear),
            ]
        );
    }

    #[test]
    fn the_shutdown_clear_covers_every_remembered_window() {
        let panes = [pane("s", 0, "%1", true), pane("s", 1, "%9", true)];
        assert_eq!(
            clear_edges(&keys(&[("s", 0), ("s", 1)]), &panes),
            vec![
                ("%1".to_string(), Progress::Clear),
                ("%9".to_string(), Progress::Clear),
            ]
        );
    }
}
