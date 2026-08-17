//! The interactive/effect surface: single-value `display-message` reads, `focus`
//! (switch-client/select-window/select-pane), key delivery, the `display-menu`, the
//! status-line message (fanned out to every attached client), and the two pane-tty notification
//! sinks (the terminal bell and the OSC 9 desktop notification).

use super::{Tmux, TmuxError};

/// One entry in a tmux `display-menu`: the shown `label`, an optional mnemonic `key` (empty
/// for none), and the tmux `command` run when it is selected. Built by the action surfaces and
/// handed to [`Tmux::display_menu`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuItem {
    pub label: String,
    pub key: String,
    pub command: String,
}

/// Escape a string for use as a [`MenuItem::label`]. A menu label is a tmux format string, so a `#`
/// in a session name (or a configured glyph) would be read as the start of `#{...}`/`#[...]`. `##`
/// is tmux's literal `#`. Lives beside `MenuItem` so every menu builder escapes the same way.
pub fn escape_menu_label(label: &str) -> String {
    label.replace('#', "##")
}

impl Tmux {
    /// The target server's `#{socket_path}`: the per-server identity the daemon keys its socket/lock
    /// on. `tma event` and `tma daemon --ensure` both resolve it here, so they can never mis-target.
    pub fn socket_path(&self) -> Result<String, TmuxError> {
        self.display_active("#{socket_path}")
    }

    /// The running server's `#{version}` (`3.6a`, `next-3.7`). Read from the SERVER, not from
    /// `tmux -V`: the client on `$PATH` can be a different build than the server tma is talking to.
    pub fn server_version(&self) -> Result<String, TmuxError> {
        self.display_active("#{version}")
    }

    /// Read one format string against a pane (`display-message -p`). Used by the probe and by
    /// consumers needing a single value; server-gone degrades cleanly.
    pub(crate) fn display(&self, pane_id: &str, format: &str) -> Result<String, TmuxError> {
        self.run(&["display-message", "-p", "-t", pane_id, format])
            .map(|s| s.trim_end_matches('\n').to_string())
    }

    /// Read a single `-F` format against a specific pane (`display-message -p -t`). Read-only; the
    /// action broker resolves `#{pane_current_path}` (TMA_CWD) here for context env assembly.
    pub fn pane_format(&self, pane_id: &str, format: &str) -> Result<String, TmuxError> {
        self.display(pane_id, format)
    }

    /// Read a format against the *current* client / active pane (no `-t`). How jump resolves its
    /// origin: via client queries, never `$TMUX_PANE` (a hidden internal pane under `display-popup`).
    pub fn display_active(&self, format: &str) -> Result<String, TmuxError> {
        self.display_active_client(None, format)
    }

    /// Read a format against a *specific* client (`-c <client>`), or the targetless `display_active`
    /// when `None`, so the invoking client resolves the origin, not the most-recently-active one.
    pub fn display_active_client(
        &self,
        client: Option<&str>,
        format: &str,
    ) -> Result<String, TmuxError> {
        let argv = display_message_argv(client, format);
        let borrowed: Vec<&str> = argv.iter().map(String::as_str).collect();
        self.run(&borrowed)
            .map(|s| s.trim_end_matches('\n').to_string())
    }

    /// Focus a pane across sessions (`switch-client` + `select-window` + `select-pane`): the only
    /// pane-affecting action `tma` performs. `Some(client)` moves that exact client; `None` is targetless.
    pub fn focus(
        &self,
        client: Option<&str>,
        session: &str,
        window_target: &str,
        pane_target: &str,
    ) -> Result<(), TmuxError> {
        let switch = switch_client_argv(client, session);
        let borrowed: Vec<&str> = switch.iter().map(String::as_str).collect();
        self.run(&borrowed)?;
        self.run(&["select-window", "-t", window_target])?;
        self.run(&["select-pane", "-t", pane_target])?;
        Ok(())
    }

    /// Deliver a key sequence into a pane as ONE `send-keys` invocation with named-key
    /// interpretation on (`Enter`, `C-c`, `/compact` mean what tmux says), no inter-key delay.
    /// This crate is the sole constructor of `send-keys`: the action broker's guarded keys
    /// path routes here, never a raw shell-out. An empty sequence is a no-op.
    pub fn send_keys(&self, pane_id: &str, keys: &[String]) -> Result<(), TmuxError> {
        if keys.is_empty() {
            return Ok(());
        }
        let mut argv: Vec<&str> = vec!["send-keys", "-t", pane_id];
        argv.extend(keys.iter().map(String::as_str));
        self.run(&argv).map(|_| ())
    }

    /// Run `command` through the server's own `run-shell -b`, returning as soon as tmux has taken
    /// custody. The child is the tmux server's, not the caller's: it survives the caller exiting and
    /// is reaped by tmux, which is what a surface needs when it hands a menu off and keeps drawing
    /// (a `display-menu` opened by a caller's own child would die with a popup that the menu closes).
    /// stdout, if any, lands in the target pane's copy mode — the tmux behavior, not ours.
    pub fn run_shell_background(&self, command: &str) -> Result<(), TmuxError> {
        self.run(&["run-shell", "-b", command]).map(|_| ())
    }

    /// Render a tmux `display-menu` of `items` on the client viewing `target_pane` (the
    /// keyboard-only parity surface). Each item is a `(label, key, command)` triple; `key` is a
    /// mnemonic shortcut (empty for none) and `command` a tmux command run on selection (the action
    /// surfaces pass `run-shell 'tma act <name> --pane <id>'`). An empty `items` is a caller error
    /// (tmux rejects a menu with no entries), so the caller filters to fireable actions first.
    pub fn display_menu(
        &self,
        target_pane: &str,
        title: &str,
        items: &[MenuItem],
    ) -> Result<(), TmuxError> {
        let mut argv: Vec<&str> = vec!["display-menu", "-t", target_pane, "-T", title];
        for it in items {
            argv.push(&it.label);
            argv.push(&it.key);
            argv.push(&it.command);
        }
        self.run(&argv).map(|_| ())
    }

    /// Fire the baseline notification (`display-message -c <client> <text>`) on EVERY attached
    /// client's status line, so a paired terminal sees it too rather than only the most recently
    /// active one. Best-effort: a client that fails (detached mid-loop) does not stop the others, and
    /// no attached client just means nowhere to show it. Only the `list-clients` read can error.
    pub fn message(&self, text: &str) -> Result<(), TmuxError> {
        for client in self.list_clients()? {
            let argv = message_argv(&client, text);
            let borrowed: Vec<&str> = argv.iter().map(String::as_str).collect();
            let _ = self.run(&borrowed);
        }
        Ok(())
    }

    /// Ring a pane's terminal bell (`notify.bell`): write a BEL (0x07) to its `#{pane_tty}`. A BEL to
    /// the slave tty registers as an in-pane bell (sets `#{window_bell_flag}`, honors `monitor-bell`);
    /// `display-message` does NOT ring, so the bell writes the tty directly. Best-effort companion of
    /// an already-fired notification: every failure is swallowed.
    pub fn ring_bell(&self, pane_id: &str) {
        self.write_pane_tty(pane_id, b"\x07");
    }

    /// Post a desktop notification through the terminal itself (`notify.osc`): an OSC 9 sequence
    /// written to the pane's `#{pane_tty}`, exactly like [`Self::ring_bell`]. The emulator at the far
    /// end of an ssh/mosh/tmate connection is what renders it, so this reaches the machine you are
    /// sitting at while `notify.command` runs on the machine running tmux. Support varies by emulator
    /// (hence the opt-in); an emulator that does not understand OSC 9 ignores the sequence.
    pub fn osc_notify(&self, pane_id: &str, text: &str) {
        self.write_pane_tty(pane_id, &osc9(text));
    }

    /// Write bytes to a pane's tty, best-effort: every failure is swallowed, and the tty is opened
    /// non-blocking so a pathological unread pty can never wedge the notify path.
    fn write_pane_tty(&self, pane_id: &str, bytes: &[u8]) {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;

        let Ok(tty) = self.display(pane_id, "#{pane_tty}") else {
            return;
        };
        if tty.is_empty() {
            return;
        }
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as i32)
            .open(&tty)
        {
            let _ = f.write_all(bytes);
        }
    }
}

/// Build the `display-message` argv showing `text` on one client. Split out so the fan-out's
/// per-client targeting is unit-testable without a live server: a targetless `display-message` shows
/// on the most recently active client only, which is exactly what a pairing setup must not do.
fn message_argv(client: &str, text: &str) -> Vec<String> {
    vec![
        "display-message".to_string(),
        "-c".to_string(),
        client.to_string(),
        text.to_string(),
    ]
}

/// Build the OSC 9 notification sequence `ESC ] 9 ; <text> BEL`. Control bytes in `text` are dropped:
/// the terminator is itself a control byte, so anything that could close the sequence early (or open
/// a new one) must not survive into it. Split out so the byte layout is unit-testable.
fn osc9(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() + 5);
    out.extend_from_slice(b"\x1b]9;");
    out.extend(
        text.bytes()
            .filter(|b| *b >= 0x20 && *b != 0x7f)
            .take(OSC_TEXT_MAX),
    );
    out.push(0x07);
    out
}

/// Cap on the OSC 9 body. The text tma sends is a short `<agent> <state>`; the cap is a backstop so
/// no caller can push an unbounded escape sequence at the emulator.
const OSC_TEXT_MAX: usize = 200;

/// Build the `display-message` argv for reading one format, optionally against a specific client
/// (`-c <client>`). Split out so client targeting is unit-testable without a live server.
fn display_message_argv(client: Option<&str>, format: &str) -> Vec<String> {
    let mut argv = vec!["display-message".to_string(), "-p".to_string()];
    if let Some(c) = client {
        argv.push("-c".to_string());
        argv.push(c.to_string());
    }
    argv.push(format.to_string());
    argv
}

/// Build the `switch-client` argv, optionally targeting a specific client (`-c <client>`).
/// Split out so the client targeting is unit-testable without a live server.
fn switch_client_argv(client: Option<&str>, session: &str) -> Vec<String> {
    let mut argv = vec!["switch-client".to_string()];
    if let Some(c) = client {
        argv.push("-c".to_string());
        argv.push(c.to_string());
    }
    argv.push("-t".to_string());
    argv.push(session.to_string());
    argv
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switch_client_argv_targets_client_when_given() {
        assert_eq!(
            switch_client_argv(Some("/dev/ttys003"), "work"),
            vec!["switch-client", "-c", "/dev/ttys003", "-t", "work"]
        );
        assert_eq!(
            switch_client_argv(None, "work"),
            vec!["switch-client", "-t", "work"]
        );
    }

    #[test]
    fn message_argv_targets_one_client() {
        assert_eq!(
            message_argv("/dev/ttys003", "tma: claude blocked"),
            vec![
                "display-message",
                "-c",
                "/dev/ttys003",
                "tma: claude blocked"
            ]
        );
    }

    #[test]
    fn osc9_wraps_the_text_and_drops_control_bytes() {
        assert_eq!(
            osc9("claude blocked"),
            b"\x1b]9;claude blocked\x07".to_vec()
        );
        // A control byte in the text could terminate the sequence early or open another one.
        assert_eq!(osc9("a\x07b\x1b]0;x\x07"), b"\x1b]9;ab]0;x\x07".to_vec());
        // Bounded regardless of input length.
        let long = osc9(&"x".repeat(OSC_TEXT_MAX * 2));
        assert_eq!(long.len(), OSC_TEXT_MAX + 5);
        assert_eq!(*long.last().unwrap(), 0x07);
    }

    #[test]
    fn display_message_argv_targets_client_when_given() {
        assert_eq!(
            display_message_argv(Some("/dev/ttys003"), "#{session_name}"),
            vec![
                "display-message",
                "-p",
                "-c",
                "/dev/ttys003",
                "#{session_name}"
            ]
        );
        assert_eq!(
            display_message_argv(None, "#{session_name}"),
            vec!["display-message", "-p", "#{session_name}"]
        );
    }
}
