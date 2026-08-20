//! Per-pane observation input to the detection core.

/// One process in a pane's tree, from a `ps -eo pid,ppid,pgid,tpgid,comm` parse. The identity
/// engine walks these to resolve pane ownership; the core stores them as opaque facts, never
/// spawning `ps`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcInfo {
    pub pid: u32,
    pub ppid: u32,
    pub pgid: u32,
    /// Foreground process group of this process's controlling terminal, or `None` when it has no
    /// tty (`ps` reports `0` on BSD, `-1` on Linux). Read on the PANE ROOT, this is the kernel's
    /// own answer to "which process group owns the screen right now" — the name-free foreground
    /// test. See `identity::foreground_owns_tty`.
    pub tpgid: Option<u32>,
    pub comm: String,
}

/// Everything the detector observed about one pane in a single cycle. All timestamps are injected
/// epoch milliseconds — the core reads no clock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaneSnapshot {
    /// Stable tmux pane id (e.g. `%13`) — identity is keyed on this, never on
    /// window/pane indexes.
    pub pane_id: String,
    /// Process tree rooted at the pane, for identity resolution.
    pub pid_tree: Vec<ProcInfo>,
    /// `#{pane_title}` — carries agent OSC titles.
    pub title: String,
    /// Live-viewport tail from `capture-pane -p -e -S -N`. The match surface.
    pub tail_text: String,
    /// Hash of `tail_text`, paired with the stamped `@agent_hash` to decide whether a cycle can
    /// reuse the stored stamp. Scheduling only — it makes no state claim. Injected so the core
    /// does not choose a hash algorithm.
    pub tail_hash: u64,
    /// `#{alternate_on}` — alt-screen agents report 1.
    pub alternate_on: bool,
    /// `#{scroll_position}`: `None` outside copy-mode, `Some(n)` in copy-mode with the viewport
    /// `n` lines above the live screen. See [`PaneSnapshot::scrolled`].
    pub scroll_position: Option<u32>,
    /// `#{pane_height}` — visible-screen row count. `capture-pane -S -50` may reach 50 lines into
    /// scrollback, so [`Region::Visible`](crate::manifest::Region) clamps evaluation to the last
    /// `visible_height` lines. `None` means "unknown, do not clamp" (the poll path always supplies it).
    pub visible_height: Option<u32>,
    /// When this snapshot was captured (epoch milliseconds, injected).
    pub captured_at: u64,
}

impl PaneSnapshot {
    /// Is the viewport something other than the live screen (the fold's freeze fact)? Entering
    /// copy-mode at the bottom reports offset 0, which is still the live screen, so only a
    /// positive offset freezes; treating bare copy-mode as frozen suspends detection invisibly.
    pub fn scrolled(&self) -> bool {
        matches!(self.scroll_position, Some(n) if n > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(scroll_position: Option<u32>) -> PaneSnapshot {
        PaneSnapshot {
            pane_id: "%1".to_string(),
            pid_tree: Vec::new(),
            title: String::new(),
            tail_text: String::new(),
            tail_hash: 0,
            alternate_on: false,
            scroll_position,
            visible_height: None,
            captured_at: 0,
        }
    }

    #[test]
    fn copy_mode_at_offset_zero_is_not_scrolled() {
        // tmux sets `#{scroll_position}` to 0 the instant a pane enters copy-mode, even at the
        // bottom: the screen under it is still live, so detection must keep reading it.
        assert!(!snapshot(Some(0)).scrolled());
        assert!(!snapshot(None).scrolled());
        assert!(snapshot(Some(1)).scrolled());
        assert!(snapshot(Some(500)).scrolled());
    }
}
