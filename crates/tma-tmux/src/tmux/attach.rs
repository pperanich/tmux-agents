//! The process-replacement handover behind `tma attach`: point a session at a pane, then hand this
//! process's terminal to a real `tmux attach-session` client.
//!
//! A jump is `switch-client`, which needs a client that is already attached; a fresh terminal (an
//! ssh session, a phone's terminal app) has none, so the only way in is to *become* the client.
//! Building that argv is building a tmux command line, so it belongs in this crate with every
//! other one. The `exec` is a safe `std` wrapper, not a raw syscall: no `unsafe`, no fork.

use std::os::unix::process::CommandExt;
use std::process::Command;

use super::{Tmux, TmuxError};

impl Tmux {
    /// The argv an attach to `session` runs: the tmux binary resolved at construction, this
    /// target's socket selector, then `attach-session -t <session>`. Public so `--print` can show
    /// what would run without running it.
    pub fn attach_argv(&self, session: &str) -> Result<Vec<String>, TmuxError> {
        let bin = self.bin.as_ref().ok_or(TmuxError::NotInstalled)?;
        let mut argv = vec![bin.to_string_lossy().into_owned()];
        argv.extend(self.socket_args.iter().cloned());
        argv.push("attach-session".to_string());
        argv.push("-t".to_string());
        argv.push(session.to_string());
        Ok(argv)
    }

    /// Replace this process with [`Tmux::attach_argv`]'s client. Returns only on failure: a
    /// successful exec never comes back, so there is no post-attach path for a caller to write.
    pub fn exec_attach(&self, session: &str) -> Result<(), TmuxError> {
        let argv = self.attach_argv(session)?;
        let source = Command::new(&argv[0]).args(&argv[1..]).exec();
        Err(TmuxError::Spawn {
            cmd: argv.join(" "),
            source,
        })
    }

    /// Point a session at `pane_target` without touching any client (`select-window` +
    /// `select-pane`): the pre-attach half of a handover, which has no client to `switch-client`.
    /// [`Tmux::focus`] is the attached counterpart, and skips the `select-window` for the same reason.
    pub fn select_pane_in_window(
        &self,
        window_target: &str,
        pane_target: &str,
    ) -> Result<(), TmuxError> {
        if !self.window_is_current(window_target) {
            self.run(&["select-window", "-t", window_target])?;
        }
        self.run(&["select-pane", "-t", pane_target]).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::super::Server;
    use super::*;
    use std::path::PathBuf;

    fn tmux_with(server: Server) -> Tmux {
        Tmux::with_bin(Some(PathBuf::from("/usr/bin/tmux")), &server)
    }

    /// The ambient server: binary, subcommand, session target, and nothing else. The `-u` every
    /// parsed one-shot carries is deliberately absent, since this client parses no output and
    /// forcing UTF-8 would override the locale of the terminal being handed over.
    #[test]
    fn attach_argv_is_the_bare_attach_on_the_ambient_server() {
        let argv = tmux_with(Server::default()).attach_argv("work").unwrap();
        assert_eq!(argv, ["/usr/bin/tmux", "attach-session", "-t", "work"]);
    }

    /// `--socket-name` and `--socket-path` both reach the argv, in tmux's own spelling, so an
    /// attach lands on the same server every other tma command targeted.
    #[test]
    fn attach_argv_carries_the_socket_selector() {
        let named = tmux_with(Server::named(Some("scratch".to_string())))
            .attach_argv("home")
            .unwrap();
        assert_eq!(
            named,
            [
                "/usr/bin/tmux",
                "-L",
                "scratch",
                "attach-session",
                "-t",
                "home"
            ]
        );

        let by_path = tmux_with(Server {
            socket_path: Some(PathBuf::from("/tmp/tmate-501/x y")),
            ..Server::default()
        })
        .attach_argv("home")
        .unwrap();
        assert_eq!(
            by_path,
            [
                "/usr/bin/tmux",
                "-S",
                "/tmp/tmate-501/x y",
                "attach-session",
                "-t",
                "home"
            ]
        );
    }

    /// The session target is passed through verbatim: a session name with a space or a `#` is a
    /// single argv element, so nothing has to be quoted for a shell that is never involved.
    #[test]
    fn attach_argv_passes_the_session_name_through_whole() {
        let argv = tmux_with(Server::default())
            .attach_argv("my project #2")
            .unwrap();
        assert_eq!(argv.last().unwrap(), "my project #2");
        assert_eq!(argv.len(), 4);
    }

    /// No tmux on PATH is the same guided error every other call returns, raised before anything
    /// tries to exec a binary that is not there.
    #[test]
    fn attach_argv_without_a_binary_is_not_installed() {
        let tmux = Tmux::with_bin(None, &Server::default());
        assert!(matches!(
            tmux.attach_argv("work"),
            Err(TmuxError::NotInstalled)
        ));
    }
}
