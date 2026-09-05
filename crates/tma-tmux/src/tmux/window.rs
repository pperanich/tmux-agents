//! The window-scoped read behind `[daemon] window_names`: one `list-panes -a` that carries each
//! pane's window identity, the window's current name, tma's rename bookkeeping, and the agent
//! fields a name is rendered from. Kept out of [`super::read`]'s `PaneRecord` format on purpose:
//! the feature is opt-in, and the poll cycle must not pay for a read it never uses.

use super::{Tmux, TmuxError, SEP};
use tma_core::stamp::opt;

/// One `list-panes -a` row, reduced to what the window-name pass reads. Window fields repeat across
/// the panes of a window; the caller groups on [`WindowPaneRow::window_id`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowPaneRow {
    /// `#{window_id}` (`@N`), stable across renames and the `-t` target for every write.
    pub window_id: String,
    pub window_name: String,
    /// `@tma_window_name_orig`: the pre-tma name, present exactly while tma owns this window's name.
    pub name_orig: Option<String>,
    /// `@tma_window_autorename_orig`: the saved window-scope `automatic-rename`, or the
    /// [`tma_core::stamp::AUTORENAME_UNSET`] sentinel when it was inherited.
    pub autorename_orig: Option<String>,
    /// `@tma_window_name_last`: the last name tma wrote, the hand-rename detector.
    pub name_last: Option<String>,
    /// `@agent_name`, `None` on a pane with no agent stamp.
    pub agent: Option<String>,
    /// `@agent_state` as its raw token; the caller parses it.
    pub state: Option<String>,
    pub detail: Option<String>,
    /// `#{pane_current_path}`, the repo/branch resolver's input.
    pub cwd: Option<String>,
}

/// The fields, in order. `window_name` and `cwd` are free-form, so they sit behind the option
/// fields that a stray separator could otherwise be confused with.
fn window_rows_format() -> String {
    [
        "#{window_id}",
        &format!("#{{{}}}", opt::WINDOW_NAME_ORIG),
        &format!("#{{{}}}", opt::WINDOW_AUTORENAME_ORIG),
        &format!("#{{{}}}", opt::WINDOW_NAME_LAST),
        &format!("#{{{}}}", opt::NAME),
        &format!("#{{{}}}", opt::STATE),
        &format!("#{{{}}}", opt::DETAIL),
        "#{pane_current_path}",
        "#{window_name}",
    ]
    .join(&SEP.to_string())
}

/// An empty tmux format field means "option unset" everywhere in the stamp grammar; keep that.
fn present(field: &str) -> Option<String> {
    (!field.is_empty()).then(|| field.to_string())
}

impl Tmux {
    /// One `list-panes -a` read for the window-name pass. Runs only while `[daemon] window_names`
    /// is configured.
    pub fn list_window_pane_rows(&self) -> Result<Vec<WindowPaneRow>, TmuxError> {
        let out = self.run(&["list-panes", "-a", "-F", &window_rows_format()])?;
        let mut rows = Vec::new();
        for line in out.lines() {
            if line.is_empty() {
                continue;
            }
            let mut f = line.split(SEP);
            let (
                Some(window_id),
                Some(name_orig),
                Some(autorename_orig),
                Some(name_last),
                Some(agent),
                Some(state),
                Some(detail),
                Some(cwd),
                Some(window_name),
            ) = (
                f.next(),
                f.next(),
                f.next(),
                f.next(),
                f.next(),
                f.next(),
                f.next(),
                f.next(),
                f.next(),
            )
            else {
                // A truncated row names no window, so it cannot be acted on; skip it rather than
                // failing a pass that would otherwise rename every other window correctly.
                continue;
            };
            rows.push(WindowPaneRow {
                window_id: window_id.to_string(),
                window_name: window_name.to_string(),
                name_orig: present(name_orig),
                autorename_orig: present(autorename_orig),
                name_last: present(name_last),
                agent: present(agent),
                state: present(state),
                detail: present(detail),
                cwd: present(cwd),
            });
        }
        Ok(rows)
    }

    /// The `automatic-rename` value set at WINDOW scope (`show-options -wqv`), `None` when the
    /// window inherits it. Read once per window, right before tma's first rename turns it off, so
    /// the restore can put back an inherited option by unsetting rather than pinning a value.
    pub fn window_automatic_rename(&self, window_id: &str) -> Result<Option<String>, TmuxError> {
        let out = self.run(&["show-options", "-wqv", "-t", window_id, "automatic-rename"])?;
        let trimmed = out.trim();
        Ok((!trimmed.is_empty()).then(|| trimmed.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_format_has_one_field_per_parsed_column() {
        let format = window_rows_format();
        assert_eq!(format.split(SEP).count(), 9);
        assert!(format.starts_with("#{window_id}"));
        assert!(format.ends_with("#{window_name}"));
    }

    #[test]
    fn an_empty_field_reads_as_an_unset_option() {
        assert_eq!(present(""), None);
        assert_eq!(present("claude"), Some("claude".to_string()));
    }
}
