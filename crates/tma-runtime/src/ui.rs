//! The display layer's complete tmux surface: every tmux read and write the `tma-ui` crate performs
//! (the picker's preview, jump's focus/origin/trail, the `watch` sidebar's pid advertisement, and the
//! `display-menu` action) is routed through these wrappers, so `tma-ui` depends on runtime and never
//! on [`tma_tmux`]. Beyond these touchpoints the display layer only reads [`crate::cycle::CycleReport`]
//! + config.

use tma_core::stamp::opt;
use tma_tmux::tmux::{MenuItem, Tmux, TmuxError};

/// Capture a pane's visible tail (last `lines`) *with* SGR escapes (`capture-pane -e`) for the
/// picker's live preview. Honors the picker's refresh contract as implemented: a capture
/// failure collapses to an empty string (a preview is best-effort and must never abort the
/// refresh loop). The caller decodes the returned ANSI into styled spans.
pub fn capture_preview(tmux: &Tmux, pane_id: &str) -> String {
    tmux.capture_ansi(pane_id).unwrap_or_default()
}

/// Focus a pane across sessions (`switch-client` + `select-window` + `select-pane`) — the one
/// pane-affecting action `tma`'s UI performs. `client` is the invoking client, so the
/// `switch-client` moves that exact client when `Some`; the window/pane targets are absolute and
/// unaffected by the client.
pub fn focus_pane(
    tmux: &Tmux,
    client: Option<&str>,
    session: &str,
    window_target: &str,
    pane_target: &str,
) -> Result<(), TmuxError> {
    tmux.focus(client, session, window_target, pane_target)
}

/// Render a tmux `display-menu` of `items` on the client viewing `target_pane`. The one
/// menu touchpoint `tma-ui` performs, routed through runtime so the UI crate keeps no `tma-tmux`
/// edge. The caller filters to fireable actions first (tmux rejects an empty menu).
pub fn display_menu(
    tmux: &Tmux,
    target_pane: &str,
    title: &str,
    items: &[MenuItem],
) -> Result<(), TmuxError> {
    tmux.display_menu(target_pane, title, items)
}

/// Hand `command` to the server's `run-shell -b` (the display layer's one background-command edge).
/// The dashboards use it to open the action menu on the selected pane: the menu must outlive the
/// surface that asked for it, and a tmux-owned child does while a child of the surface would not.
pub fn run_shell_background(tmux: &Tmux, command: &str) -> Result<(), TmuxError> {
    tmux.run_shell_background(command)
}

/// Read the acting client's current pane as a `session:window.pane` locator: jump's origin and the
/// value pushed onto the return trail. A read failure collapses to an empty string, which the callers
/// treat as "no origin"; `client` `Some` reads that exact client, `None` the most-recently-active.
pub fn active_locator(tmux: &Tmux, client: Option<&str>) -> String {
    tmux.display_active_client(client, "#{session_name}:#{window_index}.#{pane_index}")
        .unwrap_or_default()
}

/// Read the acting client's session name (the picker's session scope). A read failure collapses to
/// an empty string (unscoped); `client` `Some` reads that exact client, `None` the most-recently-active.
pub fn active_session(tmux: &Tmux, client: Option<&str>) -> String {
    tmux.display_active_client(client, "#{session_name}")
        .unwrap_or_default()
}

/// Read the acting client's active pane id (the picker's self-exclusion). `None` when the read fails
/// or comes back empty (no client, outside tmux), which the caller treats as "exclude nothing".
/// `$TMUX_PANE` cannot serve here: inside a `display-popup` it is unset, so this goes through the
/// client the same way [`active_locator`] does.
pub fn active_pane_id(tmux: &Tmux, client: Option<&str>) -> Option<String> {
    tmux.display_active_client(client, "#{pane_id}")
        .ok()
        .filter(|pane| !pane.is_empty())
}

/// Read the current client's name, targetless (jump's `resolve_client` fallback when the keybinding
/// passed none). A read failure collapses to an empty string.
pub fn active_client_name(tmux: &Tmux) -> String {
    tmux.display_active("#{client_name}").unwrap_or_default()
}

/// Clear a pane's attention flag (`@agent_attention`): the picker's Enter path and the "go deal with
/// this" jumps mark a focused waiter reviewed. Best-effort at the call sites (they discard the result).
pub fn clear_attention(tmux: &Tmux, pane_id: &str) -> Result<(), TmuxError> {
    tmux.unset_pane_option(pane_id, opt::ATTENTION)
}

/// Read a client's return-trail server option, `None` when unset. The jump return-trail is the only
/// display-layer server-option use; tmux has no client-scoped options, so jump stores the trail
/// server-scoped under a per-client key, keying/parsing staying in `jump`.
pub fn trail_read(tmux: &Tmux, key: &str) -> Result<Option<String>, TmuxError> {
    tmux.get_server_option(key)
}

/// Write a client's return-trail server option (the only display-layer server-option use). The
/// trail's keying and encoding stay in `jump`; this is only the tmux touch.
pub fn trail_write(tmux: &Tmux, key: &str, value: &str) -> Result<(), TmuxError> {
    tmux.set_server_option(key, value)
}

/// Advertise `tma watch`'s pid in `@tma_watch_pid` on its own pane, so a `clear-attention` from
/// another pane can SIGUSR1-nudge the sidebar. Best-effort at the call site (advertised post-setup).
pub fn advertise_watch_pid(tmux: &Tmux, pane_id: &str, pid: u32) -> Result<(), TmuxError> {
    tmux.set_pane_option(pane_id, opt::WATCH_PID, &pid.to_string())
}

/// Clear the advertised `@tma_watch_pid` on the sidebar's pane (the guard's `Drop` path). Best-effort:
/// a tmux-killed pane already destroyed the pane-scoped option with the pane.
pub fn unadvertise_watch_pid(tmux: &Tmux, pane_id: &str) -> Result<(), TmuxError> {
    tmux.unset_pane_option(pane_id, opt::WATCH_PID)
}
