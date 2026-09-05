//! The interactive/effect surface: single-value `display-message` reads, `focus`
//! (switch-client/select-window/select-pane), key delivery, the `display-menu`, the
//! status-line message (fanned out to every attached client), and the pane-tty notification sinks
//! (the terminal bell, the OSC 9 and OSC 777 desktop notifications, and the OSC 9;4 taskbar
//! progress indicator).

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

    /// Replace a client's previous temporary dashboard session, create a detached one running
    /// `shell_command`, and switch that client to it. The child command owns the lifecycle: its
    /// only pane closing destroys the session, so `remain-on-exit` and `destroy-unattached` are
    /// forced off even when the user enables them globally. `detach-on-destroy` is forced off so
    /// that destruction switches the client to another live session rather than detaching tmux.
    /// Any setup/switch failure cleans up the newly-created session.
    pub fn open_temporary_session(
        &self,
        client: &str,
        session: &str,
        shell_command: &str,
    ) -> Result<(), TmuxError> {
        // One dashboard per client. A prior one can survive only if the user manually switched away
        // rather than jumping/quitting; replacing it here prevents stale temporary sessions piling up.
        let _ = self.run(&["kill-session", "-t", session]);
        // Create + local override in ONE command-client queue. If the global option is on, a detached
        // session is reaped when the command client that created it exits; the chained local write
        // must therefore land before that client disconnects.
        if let Err(err) = self.run(&[
            "new-session",
            "-d",
            "-s",
            session,
            shell_command,
            ";",
            "set-option",
            "-t",
            session,
            "destroy-unattached",
            "off",
        ]) {
            let _ = self.run(&["kill-session", "-t", session]);
            return Err(err);
        }

        // Destroying the active temporary session should reveal another live session, not detach
        // the tmux client (the global default is `detach-on-destroy on`).
        if let Err(err) = self.run(&["set-option", "-t", session, "detach-on-destroy", "off"]) {
            let _ = self.run(&["kill-session", "-t", session]);
            return Err(err);
        }
        let window_target = format!("{session}:");
        if let Err(err) = self.run(&[
            "set-window-option",
            "-t",
            &window_target,
            "remain-on-exit",
            "off",
        ]) {
            let _ = self.run(&["kill-session", "-t", session]);
            return Err(err);
        }
        let switch = switch_client_argv(Some(client), session);
        let borrowed: Vec<&str> = switch.iter().map(String::as_str).collect();
        if let Err(err) = self.run(&borrowed) {
            let _ = self.run(&["kill-session", "-t", session]);
            return Err(err);
        }
        Ok(())
    }

    /// Focus a pane across sessions (`switch-client` + `select-window` + `select-pane`): the only
    /// pane-affecting action a jump performs. `Some(client)` moves that exact client; `None` is targetless.
    ///
    /// `select-window` is skipped when the destination window is already its session's current one.
    /// That call would be a no-op for the display, but it still runs tmux's `after-select-window`
    /// hook — whose `window_last_flag` at that moment names whatever window was left however long
    /// ago, so anyone's hook reading it acts on a window the user never left. Half of all jumps land
    /// in the window you are already in (`--attention` to the near one of two, `--back`, the picker
    /// with one window in play), so this is the common case, not the exotic one.
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
        if !self.window_is_current(window_target) {
            self.run(&["select-window", "-t", window_target])?;
        }
        self.run(&["select-pane", "-t", pane_target])?;
        Ok(())
    }

    /// Is `window_target` already the current window of ITS OWN session? `#{window_active}` is
    /// per-session (verified on 3.6a: each session's current window reads `1`, whichever session
    /// tmux would call current), and it resolves from a window target or a pane target alike.
    /// An unreadable answer is `false`, which falls through to the plain `select-window` this
    /// guards — the behaviour before the guard existed.
    fn window_is_current(&self, window_target: &str) -> bool {
        self.display(window_target, "#{window_active}")
            .is_ok_and(|v| v.trim() == "1")
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

    /// Deliver a caller-supplied string into a pane as literal characters, wrapped in the
    /// manifest's `prefix` and `suffix` key sequences. Three invocations at most, ordered
    /// prefix → text → suffix, and the broker holds the pane's single-flight lock across all of
    /// them.
    ///
    /// The middle one is the whole point: `-l` turns off named-key interpretation, so a message
    /// containing the word `Enter` types five characters instead of pressing Return, and `--`
    /// terminates the flags, so a message beginning with `-` is data rather than something tmux's
    /// own getopt reads. Neither is optional; both are asserted by the send-keys source guard.
    pub fn send_text(
        &self,
        pane_id: &str,
        prefix: &[String],
        text: &str,
        suffix: &[String],
    ) -> Result<(), TmuxError> {
        self.send_keys(pane_id, prefix)?;
        self.run(&["send-keys", "-t", pane_id, "-l", "--", text])?;
        self.send_keys(pane_id, suffix)
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
    ///
    /// Wrapped for tmux's DCS passthrough ([`passthrough`]), which is what actually carries an OSC
    /// out of a pane; see that function for why, and for the `allow-passthrough` requirement.
    pub fn osc_notify(&self, pane_id: &str, text: &str) {
        self.write_pane_tty(pane_id, &passthrough(&osc9(text)));
    }

    /// [`Self::osc_notify`]'s OSC 777 twin (`notify.osc_777`): the sequence Ghostty and WezTerm
    /// honour, which carries a title and a body rather than one string. Emitted alongside OSC 9, and
    /// an emulator that understands only one of the two renders exactly one notification.
    pub fn osc777_notify(&self, pane_id: &str, title: &str, body: &str) {
        self.write_pane_tty(pane_id, &passthrough(&osc777(title, body)));
    }

    /// Set the terminal's taskbar/tab progress indicator (`notify.osc_progress`): OSC 9;4, which
    /// Ghostty, WezTerm and Windows Terminal render on the tab itself, so it survives a minimized
    /// window in a way a transient banner does not. Written to the pane's tty like every other sink
    /// here, and emitted only on an edge, never per cycle.
    pub fn osc_progress(&self, pane_id: &str, progress: Progress) {
        self.write_pane_tty(pane_id, &passthrough(&osc9_4(progress)));
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

/// Build the OSC 777 notification sequence `ESC ] 777 ; notify ; <title> ; <body> BEL`. `;` is
/// dropped from both fields on top of the control-byte filter [`osc9`] applies: it is this
/// sequence's own field separator, so a `;` in the title would silently shift the body.
fn osc777(title: &str, body: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(title.len() + body.len() + 14);
    out.extend_from_slice(b"\x1b]777;notify;");
    out.extend(osc_field(title));
    out.push(b';');
    out.extend(osc_field(body));
    out.push(0x07);
    out
}

/// One OSC 777 field: printable bytes only, no `;`, bounded by [`OSC_TEXT_MAX`].
fn osc_field(text: &str) -> Vec<u8> {
    text.bytes()
        .filter(|b| *b >= 0x20 && *b != 0x7f && *b != b';')
        .take(OSC_TEXT_MAX)
        .collect()
}

/// What the OSC 9;4 progress indicator should show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Progress {
    /// State 3: work is running, with no percentage to report (a pulsing/indeterminate bar).
    Indeterminate,
    /// State 0: no work running, which removes the indicator.
    Clear,
}

impl Progress {
    /// The OSC 9;4 state digit.
    fn state(self) -> u8 {
        match self {
            Progress::Indeterminate => b'3',
            Progress::Clear => b'0',
        }
    }
}

/// Build the OSC 9;4 progress sequence `ESC ] 9 ; 4 ; <state> ; 0 BEL`. The trailing `0` is the
/// percentage field, which every state tma emits ignores.
fn osc9_4(progress: Progress) -> Vec<u8> {
    vec![
        0x1b,
        b']',
        b'9',
        b';',
        b'4',
        b';',
        progress.state(),
        b';',
        b'0',
        0x07,
    ]
}

/// Wrap an escape sequence in tmux's DCS passthrough: `ESC P tmux ; <seq, ESC doubled> ESC \`.
///
/// tmux parses pane output itself and forwards only the OSC codes it handles (title, colours,
/// clipboard); 9, 777 and 9;4 are not among them, so an unwrapped sequence written to a pane tty is
/// consumed by tmux and never reaches the emulator (verified against tmux 3.6a). Wrapped, it does,
/// provided the user has `set -g allow-passthrough on`. With that option off tmux drops the whole
/// DCS silently, printing nothing into the pane, so the wrapper is never worse than not wrapping.
fn passthrough(sequence: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(sequence.len() + 10);
    out.extend_from_slice(b"\x1bPtmux;");
    for byte in sequence {
        if *byte == 0x1b {
            out.push(0x1b); // an ESC inside the payload has to be doubled
        }
        out.push(*byte);
    }
    out.extend_from_slice(b"\x1b\\");
    out
}

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
    fn osc777_carries_a_title_and_a_body() {
        assert_eq!(
            osc777("claude", "blocked"),
            b"\x1b]777;notify;claude;blocked\x07".to_vec()
        );
        // `;` is this sequence's field separator, so it never survives into a field.
        assert_eq!(
            osc777("a;b", "c\x07d"),
            b"\x1b]777;notify;ab;cd\x07".to_vec()
        );
        let long = osc777(&"x".repeat(OSC_TEXT_MAX * 2), "s");
        assert_eq!(long.len(), OSC_TEXT_MAX + 16);
    }

    #[test]
    fn osc9_4_encodes_the_two_progress_states() {
        assert_eq!(
            osc9_4(Progress::Indeterminate),
            b"\x1b]9;4;3;0\x07".to_vec()
        );
        assert_eq!(osc9_4(Progress::Clear), b"\x1b]9;4;0;0\x07".to_vec());
    }

    #[test]
    fn passthrough_wraps_and_doubles_the_escape() {
        assert_eq!(
            passthrough(b"\x1b]9;4;3;0\x07"),
            b"\x1bPtmux;\x1b\x1b]9;4;3;0\x07\x1b\\".to_vec()
        );
        // Two escapes in the payload are both doubled; nothing else is touched.
        assert_eq!(
            passthrough(b"\x1ba\x1b"),
            b"\x1bPtmux;\x1b\x1ba\x1b\x1b\x1b\\".to_vec()
        );
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
