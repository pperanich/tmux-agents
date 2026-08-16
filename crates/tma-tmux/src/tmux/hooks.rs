//! Global hook array management: append/show/remove for the attention auto-clear install path.

use super::{Tmux, TmuxError};

impl Tmux {
    /// Append a global hook via unindexed `set-hook -ga`: tmux assigns the next free index (an
    /// explicit index would silently overwrite its occupant). The attention auto-clear install path.
    pub fn append_global_hook(&self, hook: &str, command: &str) -> Result<(), TmuxError> {
        self.run(&["set-hook", "-ga", hook, command]).map(|_| ())
    }

    /// Read a global hook's array as `(index, command)` pairs (`show-hooks -g <hook>`), empty when
    /// unset. Records assigned indexes and lets `--check` detect a config-reload wipe.
    pub fn show_global_hook(&self, hook: &str) -> Result<Vec<(usize, String)>, TmuxError> {
        let out = self.run(&["show-hooks", "-g", hook])?;
        let mut entries = Vec::new();
        for line in out.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            // `after-select-pane[0] run-shell "..."` or (rare) `after-select-pane cmd`.
            let (head, rest) = match line.split_once(' ') {
                Some((h, r)) => (h, r),
                None => continue, // a bare hook name with no command: not one of ours
            };
            let index = head
                .split_once('[')
                .and_then(|(_, i)| i.strip_suffix(']'))
                .and_then(|i| i.parse::<usize>().ok())
                .unwrap_or(0);
            entries.push((index, rest.to_string()));
        }
        Ok(entries)
    }

    /// Replace one indexed entry in a global hook array (`set-hook -g <hook>[index]`). The drift
    /// rewrite path: repointing in place keeps the index the install record already holds.
    pub fn set_global_hook_index(
        &self,
        hook: &str,
        index: usize,
        command: &str,
    ) -> Result<(), TmuxError> {
        let target = format!("{hook}[{index}]");
        self.run(&["set-hook", "-g", &target, command]).map(|_| ())
    }

    /// Remove one indexed entry from a global hook array (`set-hook -gu <hook>[index]`).
    /// tmux leaves the other indexes in place (verified: removing `[0]` keeps `[1]` at 1).
    pub fn remove_global_hook_index(&self, hook: &str, index: usize) -> Result<(), TmuxError> {
        let target = format!("{hook}[{index}]");
        self.run(&["set-hook", "-gu", &target]).map(|_| ())
    }
}
