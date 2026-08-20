//! The ordered-input clear: the effectful half of [`tma_core::seen`].
//!
//! The focus hooks handle every case where the user *navigates* away from or onto a done pane. This
//! is the case they cannot see: the user never navigates at all. A client is parked on the pane, the
//! agent finishes under their eyes, and the marker stands until they happen to move. Clearing it
//! needs one extra fact — when that client was last typed into — compared against the raise instant
//! the pane already stores in `@agent_since`.
//!
//! Deliberately cheap and deliberately quiet: the `list-clients` read happens only once some pane
//! actually carries `@agent_attention`, and every tmux failure here is a silent no-op, because a
//! presentation flag that fails to clear costs the user one keystroke while an aborted cycle costs
//! them every row.

use tma_core::seen::seen_by_input;
use tma_core::stamp::opt;
use tma_core::{render, AgentRow};
use tma_tmux::tmux::Tmux;

/// Clear `@agent_attention` on every pane in `raised` that a client has been typed into since its
/// raise, returning the panes cleared. `raised` is `(pane_id, @agent_since)` for the panes whose
/// flag currently stands.
///
/// Nothing is reported cleared unless the write actually landed: on a failed batch the caller's
/// view stays as it was and the next cycle retries, so a row can never claim a clear tmux refused.
///
/// The `since` it decides on is as old as the rows: read at the top of the cycle, acted on at the
/// end of it (on the daemon path, after the whole sweep and the notification dispatch). A pane that
/// re-raised inside that window is retired on evidence about the *previous* episode, and the unset
/// is unguarded, so it lands anyway. Accepted rather than fixed: `render::unset_pane_option` has no
/// `-F` conditional form, the focus-hook clears have exactly the same shape, and the flag
/// self-corrects — the next cycle reads the new `since`, finds no input after it, and the marker
/// stands again for the cost of one cycle.
pub fn clear_seen(tmux: &Tmux, raised: &[(String, u64)]) -> Vec<String> {
    if raised.is_empty() {
        return Vec::new();
    }
    let Ok(clients) = tmux.client_views() else {
        return Vec::new();
    };
    let cleared: Vec<String> = raised
        .iter()
        .filter(|(pane, since)| seen_by_input(&clients, pane, *since))
        .map(|(pane, _)| pane.clone())
        .collect();
    let cmds: Vec<render::StampCommand> = cleared
        .iter()
        .map(|pane| render::unset_pane_option(pane, opt::ATTENTION))
        .collect();
    match tmux.apply(&cmds) {
        Ok(()) => cleared,
        Err(_) => Vec::new(),
    }
}

/// The `(pane_id, @agent_since)` pairs a clear pass would consider: the rows whose flag stands and
/// whose raise instant is known. A row with no `@agent_since` reads as zero, which every client's
/// activity postdates, so it is left alone rather than cleared blind.
///
/// Also the cheap gate — an empty result means no `list-clients` round trip at all, which is the
/// steady state for a fleet with nothing waiting to be read.
pub fn raised_panes(rows: &[AgentRow]) -> Vec<(String, u64)> {
    rows.iter()
        .filter(|r| r.attention && r.since != 0)
        .map(|r| (r.pane_id.clone(), r.since))
        .collect()
}

/// [`clear_seen`] over a cycle's rows, clearing the flag on the rows it clears so the surface this
/// cycle feeds shows the result of its own clear rather than lagging a cycle behind it. `raised` is
/// the caller's own [`raised_panes`] result — the same list it gated on, never recomputed here.
pub fn clear_seen_rows(tmux: &Tmux, rows: &mut [AgentRow], raised: &[(String, u64)]) {
    let cleared = clear_seen(tmux, raised);
    for row in rows.iter_mut() {
        if cleared.contains(&row.pane_id) {
            row.attention = false;
        }
    }
}
