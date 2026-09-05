//! State-derived tmux window names (`[daemon] window_names`), the daemon's I/O half.
//!
//! Off unless the sub-table is named. When it is, each pass reads every pane once, rolls each
//! window up to its highest-attention agent pane, renders that pane through the configured template
//! and renames the window only when the result differs from what tma last wrote. That last clause is
//! the whole cost story: a window whose rollup has not moved is one read and zero writes, so the
//! feature adds no redraws to a steady fleet.
//!
//! Restoring is the same pass seen from the other side. The first rename of a window saves the name
//! it had and its window-scope `automatic-rename`; a window that has a saved name and no agent pane
//! left gets both put back, which covers an agent exiting, its pane closing, and (through
//! [`WindowNames::restore_all`]) the daemon shutting down.

use std::collections::HashMap;

use tma_core::{sort_rank, AgentState};
use tma_runtime::config::WindowNamesSection;
use tma_runtime::repo;
use tma_runtime::window_name::{decide, NameFields, NameTemplate, WindowNameAction};
use tma_tmux::tmux::{Tmux, TmuxError, WindowPaneRow};
use tma_tmux::window_name::{rename_commands, restore_commands, SavedOriginal};

/// The daemon's window-name state: the configured template, or `None` while the feature is off.
/// No memory beyond that: every input the decision needs is stored on the window itself, so a
/// daemon restart picks up exactly where the last one left off.
pub(crate) struct WindowNames {
    template: Option<NameTemplate>,
    /// Renames written over this daemon's life (status file; tests and operators).
    renames: u64,
    restores: u64,
}

impl WindowNames {
    pub(crate) fn new(section: Option<&WindowNamesSection>) -> WindowNames {
        WindowNames {
            template: section.map(|s| s.format.clone()),
            renames: 0,
            restores: 0,
        }
    }

    /// Swap the template on a SIGHUP reload. Turning the feature OFF restores every window tma
    /// owns first: leaving them frozen on a stale state would be worse than never having renamed.
    pub(crate) fn reconfigure(&mut self, tmux: &Tmux, section: Option<&WindowNamesSection>) {
        if self.template.is_some() && section.is_none() {
            let _ = self.restore_all(tmux);
        }
        self.template = section.map(|s| s.format.clone());
    }

    pub(crate) fn enabled(&self) -> bool {
        self.template.is_some()
    }

    pub(crate) fn status_lines(&self) -> String {
        if !self.enabled() {
            return String::new();
        }
        format!(
            "window_renames={}\nwindow_restores={}\n",
            self.renames, self.restores
        )
    }

    /// One pass: rename every window whose rendered name has moved, restore every window whose last
    /// agent pane is gone. A per-window write failure is swallowed (a window can close between the
    /// read and its own write); only a gone server propagates.
    pub(crate) fn reconcile(&mut self, tmux: &Tmux) -> Result<(), TmuxError> {
        let Some(template) = self.template.clone() else {
            return Ok(());
        };
        let rows = tmux.list_window_pane_rows()?;
        for window in group_windows(&rows) {
            let desired = window.winner().map(|pane| render_name(&template, pane));
            let action = decide(
                desired.as_deref(),
                window.name,
                window.name_orig.is_some(),
                window.name_last,
            );
            match self.apply(tmux, &window, action) {
                Ok(()) => {}
                Err(TmuxError::ServerGone) => return Err(TmuxError::ServerGone),
                Err(_) => {} // one window lost mid-pass: the next pass sees the truth
            }
        }
        Ok(())
    }

    /// Put every window tma renamed back, whatever its agents are doing. The daemon's shutdown
    /// path, and the reload path that turns the feature off.
    pub(crate) fn restore_all(&mut self, tmux: &Tmux) -> Result<(), TmuxError> {
        let rows = tmux.list_window_pane_rows()?;
        for window in group_windows(&rows) {
            if window.name_orig.is_none() {
                continue;
            }
            match self.apply(tmux, &window, WindowNameAction::Restore) {
                Ok(()) => {}
                Err(TmuxError::ServerGone) => return Err(TmuxError::ServerGone),
                Err(_) => {}
            }
        }
        Ok(())
    }

    /// Execute one window's decision as a single chained tmux invocation.
    fn apply(
        &mut self,
        tmux: &Tmux,
        window: &Window<'_>,
        action: WindowNameAction,
    ) -> Result<(), TmuxError> {
        match action {
            WindowNameAction::Leave => Ok(()),
            WindowNameAction::Rename {
                name,
                save_original,
            } => {
                let saved = if save_original {
                    // Read the window-scope value BEFORE renaming: `rename-window` sets it to off.
                    Some(SavedOriginal {
                        name: window.name.to_string(),
                        automatic_rename: tmux.window_automatic_rename(window.id)?,
                    })
                } else {
                    None
                };
                tmux.apply(&rename_commands(window.id, &name, saved))?;
                self.renames += 1;
                Ok(())
            }
            WindowNameAction::Restore => {
                let Some(original) = window.name_orig else {
                    return Ok(());
                };
                tmux.apply(&restore_commands(
                    window.id,
                    original,
                    window.autorename_orig,
                ))?;
                self.restores += 1;
                Ok(())
            }
        }
    }
}

/// One window's panes plus its stored bookkeeping, borrowed from the pass's single read.
struct Window<'a> {
    id: &'a str,
    name: &'a str,
    name_orig: Option<&'a str>,
    autorename_orig: Option<&'a str>,
    name_last: Option<&'a str>,
    agents: Vec<AgentPane<'a>>,
}

/// One agent pane's contribution to its window's name.
struct AgentPane<'a> {
    state: AgentState,
    agent: &'a str,
    detail: &'a str,
    cwd: &'a str,
}

impl<'a> Window<'a> {
    /// The pane the name is rendered from: the window's highest-attention state (blocked, then
    /// working, then idle, then unknown), ties going to the first pane tmux listed. Same ordering
    /// the `@agent_summary` rollup prints in, so the name and the rollup never disagree.
    fn winner(&self) -> Option<&AgentPane<'a>> {
        self.agents.iter().min_by_key(|p| sort_rank(p.state))
    }
}

/// Group the read's rows into windows, in tmux's own order, keeping only rows that parse. A window
/// with no agent pane is still yielded: that is what a restore is decided from.
fn group_windows(rows: &[WindowPaneRow]) -> Vec<Window<'_>> {
    let mut index: HashMap<&str, usize> = HashMap::new();
    let mut windows: Vec<Window<'_>> = Vec::new();
    for row in rows {
        let at = match index.get(row.window_id.as_str()) {
            Some(at) => *at,
            None => {
                windows.push(Window {
                    id: &row.window_id,
                    name: &row.window_name,
                    name_orig: row.name_orig.as_deref(),
                    autorename_orig: row.autorename_orig.as_deref(),
                    name_last: row.name_last.as_deref(),
                    agents: Vec::new(),
                });
                index.insert(&row.window_id, windows.len() - 1);
                windows.len() - 1
            }
        };
        let Some(state) = row.state.as_deref().and_then(|s| s.parse().ok()) else {
            continue; // not an agent pane
        };
        windows[at].agents.push(AgentPane {
            state,
            agent: row.agent.as_deref().unwrap_or_default(),
            detail: row.detail.as_deref().unwrap_or_default(),
            cwd: row.cwd.as_deref().unwrap_or_default(),
        });
    }
    windows
}

/// Render one window's name from its winning pane. The repo/branch labels come from the same
/// memoized resolver the surfaces use, and only when the template actually names one of them.
fn render_name(template: &NameTemplate, pane: &AgentPane<'_>) -> String {
    let info = template
        .needs_repo()
        .then(|| repo::resolve(pane.cwd))
        .flatten();
    template.render(&NameFields {
        agent: pane.agent,
        state: pane.state.token(),
        detail: pane.detail,
        repo: info.as_ref().map(|i| i.repo_name.as_str()).unwrap_or(""),
        branch: info.as_ref().map(|i| i.branch.as_str()).unwrap_or(""),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(window: &str, name: &str, state: Option<&str>) -> WindowPaneRow {
        WindowPaneRow {
            window_id: window.to_string(),
            window_name: name.to_string(),
            name_orig: None,
            autorename_orig: None,
            name_last: None,
            agent: state.map(|_| "claude".to_string()),
            state: state.map(String::from),
            detail: None,
            cwd: None,
        }
    }

    #[test]
    fn windows_group_in_tmux_order_and_keep_agentless_ones() {
        let rows = vec![
            row("@0", "shell", None),
            row("@1", "work", Some("idle")),
            row("@1", "work", Some("blocked")),
        ];
        let windows = group_windows(&rows);
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].id, "@0");
        assert!(windows[0].winner().is_none());
        assert_eq!(windows[1].agents.len(), 2);
    }

    #[test]
    fn the_winner_is_the_highest_attention_pane() {
        let rows = vec![
            row("@1", "work", Some("idle")),
            row("@1", "work", Some("working")),
            row("@1", "work", Some("blocked")),
        ];
        let windows = group_windows(&rows);
        assert_eq!(windows[0].winner().unwrap().state, AgentState::Blocked);

        let rows = vec![
            row("@1", "work", Some("idle")),
            row("@1", "work", Some("working")),
        ];
        let windows = group_windows(&rows);
        assert_eq!(windows[0].winner().unwrap().state, AgentState::Working);

        let rows = vec![
            row("@1", "work", Some("unknown")),
            row("@1", "work", Some("idle")),
        ];
        let windows = group_windows(&rows);
        assert_eq!(windows[0].winner().unwrap().state, AgentState::Idle);
    }

    #[test]
    fn an_unparsable_state_is_not_an_agent_pane() {
        let rows = vec![row("@1", "work", Some("wat"))];
        assert!(group_windows(&rows)[0].winner().is_none());
    }
}
