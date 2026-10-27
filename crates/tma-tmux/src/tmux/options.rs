//! Pane- and server-scoped user option get/set/unset, plus the guarded `apply` write path that
//! chains [`StampCommand`]s into one server-side invocation.

use tma_core::StampCommand;

use super::{Tmux, TmuxError};

impl Tmux {
    /// Keep common writes on argv. Complete ancestry can exceed tmux's roughly 16 KiB command
    /// message, so stream a bounded, fully quoted script for larger persistence operations.
    /// source-file waits for EOF before parsing; the inner conditional group makes the decision
    /// only after that wait, and still contains the same explicitly targeted literal mutations.
    pub(crate) fn apply_persistence(
        &self,
        pane: &str,
        commands: &[StampCommand],
    ) -> Result<(), TmuxError> {
        let bytes: usize = commands
            .iter()
            .flat_map(|command| &command.argv)
            .map(|arg| arg.len() + 1)
            .sum();
        if bytes <= 12 * 1024 {
            self.apply(commands)
        } else {
            self.apply_persistence_script(pane, commands)
        }
    }

    pub(crate) fn apply_persistence_script(
        &self,
        pane: &str,
        commands: &[StampCommand],
    ) -> Result<(), TmuxError> {
        let mut script = String::new();
        for command in commands {
            if !script.is_empty() {
                script.push_str(" ; ");
            }
            for (index, arg) in command.argv.iter().enumerate() {
                if index != 0 {
                    script.push(' ');
                }
                let quoted_len = arg
                    .len()
                    .saturating_add(
                        arg.bytes()
                            .filter(|byte| *byte == b'\'')
                            .count()
                            .saturating_mul(3),
                    )
                    .saturating_add(
                        arg.bytes()
                            .filter(|byte| *byte == b'\n')
                            .count()
                            .saturating_mul(5),
                    )
                    .saturating_add(2);
                // Include the terminal newline and reject before allocating the quoted operand.
                if script.len().saturating_add(quoted_len).saturating_add(1) > 256 * 1024 {
                    return Err(TmuxError::Parse {
                        cmd: "source-file persistence".into(),
                        reason: "generated script exceeds 256 KiB".into(),
                    });
                }
                script.push_str(&tma_core::render::quote_tmux_argument(arg));
            }
        }
        script.push('\n');
        self.run_with_input_timeout(
            &["source-file", "-t", pane, "-"],
            Some(script.into_bytes()),
            super::TMUX_TIMEOUT,
        )
        .map(|_| ())
    }

    /// Apply [`StampCommand`]s as ONE `;`-chained `tmux` invocation: the tuple commits sequentially,
    /// so a reader that observes the last write (`@agent_stamped_at`) has observed the whole chain.
    pub fn apply(&self, commands: &[StampCommand]) -> Result<(), TmuxError> {
        if commands.is_empty() {
            return Ok(());
        }
        // Flatten into one argv with literal `;` separators. Spawned without a shell, so `;` is a
        // plain argument tmux reads as its command separator, never a shell metacharacter.
        let mut args: Vec<String> = Vec::new();
        for (i, cmd) in commands.iter().enumerate() {
            if i > 0 {
                args.push(";".to_string());
            }
            args.extend(cmd.argv.iter().cloned());
        }
        let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
        self.run(&borrowed).map(|_| ())
    }

    /// Unset a pane user option (`set-option -pu`). Used to clear `@agent_attention` on a picker
    /// jump. A pane-affecting *option* write, not input injection.
    pub fn unset_pane_option(&self, pane_id: &str, key: &str) -> Result<(), TmuxError> {
        self.run(&["set-option", "-pu", "-t", pane_id, key])
            .map(|_| ())
    }

    /// Clear every option tma writes from every pane on the server, returning the panes swept. The
    /// uninstall sweep: nothing refreshes a stamp once the wiring is gone, so a `#{@agent_state}`
    /// left in a user's border format would read one frozen state forever. One chained invocation
    /// per pane keeps the argv bounded on a server with many panes.
    pub fn clear_all_pane_stamps(&self) -> Result<usize, TmuxError> {
        let out = self.run(&["list-panes", "-a", "-F", "#{pane_id}"])?;
        let panes: Vec<String> = out
            .lines()
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect();
        for pane in &panes {
            self.apply(&tma_core::render::render_purge(pane))?;
        }
        Ok(panes.len())
    }

    /// Set a pane user option (`set-option -p`): `tma watch` advertises its pid in `@tma_watch_pid`
    /// on its own pane for the SIGUSR1 nudge. A pane-affecting option write, not input injection.
    pub fn set_pane_option(&self, pane_id: &str, key: &str, value: &str) -> Result<(), TmuxError> {
        self.run(&["set-option", "-p", "-t", pane_id, key, value])
            .map(|_| ())
    }

    /// Read one pane user option (`show-options -pqv`), `None` when unset. The event path reads back
    /// `@agent_notified_at` after its guarded write, so a daemonless fire never fires on a losing event.
    pub fn get_pane_option(&self, pane_id: &str, key: &str) -> Result<Option<String>, TmuxError> {
        let out = self.run(&["show-options", "-pqv", "-t", pane_id, key])?;
        let trimmed = out.trim();
        Ok((!trimmed.is_empty()).then(|| trimmed.to_string()))
    }

    /// Read a server-scoped user option (`show-options -sqv`), `None` when unset. Used for
    /// the `@tma_last_poll` stampede-guard hint.
    pub fn get_server_option(&self, key: &str) -> Result<Option<String>, TmuxError> {
        let out = self.run(&["show-options", "-sqv", key])?;
        let trimmed = out.trim();
        Ok((!trimmed.is_empty()).then(|| trimmed.to_string()))
    }

    /// Read a global session option (`show-options -gqv`), `None` when unset. `tma doctor` reads
    /// `status` this way: with it off, neither the `#()` ambient driver nor `display-message` runs.
    pub fn get_global_option(&self, key: &str) -> Result<Option<String>, TmuxError> {
        let out = self.run(&["show-options", "-gqv", key])?;
        let trimmed = out.trim();
        Ok((!trimmed.is_empty()).then(|| trimmed.to_string()))
    }

    /// Set a server-scoped user option (`set-option -s`).
    pub fn set_server_option(&self, key: &str, value: &str) -> Result<(), TmuxError> {
        self.run(&["set-option", "-s", key, value]).map(|_| ())
    }
}
