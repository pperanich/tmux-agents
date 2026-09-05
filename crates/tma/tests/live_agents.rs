//! The `[live-gated]` lane: `tma act` fired at a REAL agent's REAL permission dialog.
//!
//! These are the only tests that prove a bundled action row, because every row is a keystroke into
//! somebody else's TUI and a synthetic pane cannot answer for it. They are ordinary tests that skip
//! green unless `TMA_LIVE_AGENTS=1` is set; CI never sets it. See CONTRIBUTING.md, "Live-gated
//! tests", for the credential each agent needs in its scratch home before it will start.
//!
//! Every agent here runs with its own configuration home pinned under the scratch directory
//! ([`live_agent_env`]), so an approve fired at a live dialog cannot write a persistent grant into
//! the developer's real configuration.
//!
//! Both agents draw nothing until a client is attached, so each test attaches the harness's PTY
//! client before it looks at the screen.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use tma_test_support::{
    empty_config_path, have_agent, live_agent_env, python3_available, wait_until, AttachOutcome,
    Scratch,
};

/// A live agent's own ceiling, well past [`tma_test_support::POLL_CEILING`]: a cold TUI start plus a
/// model round trip is seconds to tens of seconds, and a lapse here means the turn never happened.
const LIVE_CEILING: Duration = Duration::from_secs(180);

/// How often [`Live::press_until`] re-sends its key. A TUI still painting its first frame drops
/// keys silently, so a drive that sends one and waits wedges until the ceiling.
const RETRY_STEP: Duration = Duration::from_millis(1500);

/// The pane geometry the sessions start at. The attached PTY client governs the real size (80x24),
/// so every needle below is chosen to survive that width.
const PANE_SIZE: (&str, &str) = ("80", "24");

/// A scratch tmux server plus the two directories a live drive needs: a `home` the agent's own
/// config is pinned at, and a `repo` it runs in.
struct Live {
    s: Scratch,
    home: PathBuf,
    repo: PathBuf,
}

impl Live {
    fn new(tag: &str) -> Live {
        let s = Scratch::new(tag);
        let home = s.workdir.join("home");
        let repo = s.workdir.join("repo");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&repo).unwrap();
        Live { s, home, repo }
    }

    /// Start `agent`'s launcher in a fresh detached session rooted at the scratch repo, with the
    /// agent's home pinned, and attach a PTY client. Returns the pane id, or [`None`] when the
    /// attach helper's `python3` is missing (an environment skip, not a failure).
    fn start(&mut self, agent: &str, launcher: &str, args: &[&str]) -> Option<String> {
        let env: Vec<String> = live_agent_env(agent, &self.home)
            .into_iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        let repo = self.repo.display().to_string();
        let mut cmd: Vec<&str> = vec![
            "new-session",
            "-d",
            "-s",
            "s1",
            "-x",
            PANE_SIZE.0,
            "-y",
            PANE_SIZE.1,
            "-c",
            &repo,
        ];
        for pair in &env {
            cmd.push("-e");
            cmd.push(pair);
        }
        cmd.push("-e");
        cmd.push("TERM=xterm-256color");
        cmd.push(launcher);
        cmd.extend_from_slice(args);
        let out = self.s.tmux(&cmd);
        assert!(
            out.status.success(),
            "start {launcher}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let pane = self.s.get("s1", "#{pane_id}");
        assert!(pane.starts_with('%'), "unexpected pane id {pane:?}");

        match self.s.attach_client("s1") {
            AttachOutcome::Attached => Some(pane),
            AttachOutcome::NoPython => {
                eprintln!("skipping: python3 unavailable for the PTY attach");
                None
            }
            AttachOutcome::Failed => {
                panic!("PTY client failed to attach after python3 ran (regression, not env)")
            }
        }
    }

    fn capture(&self, pane: &str) -> String {
        let out = self.s.tmux(&["capture-pane", "-p", "-t", pane]);
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Poll the pane's screen until `needle` appears, panicking with the final screen when it does
    /// not: a live failure is unreproducible, so the capture has to travel with it.
    fn await_screen(&self, pane: &str, needle: &str, what: &str) {
        if !wait_until(LIVE_CEILING, || self.capture(pane).contains(needle)) {
            panic!(
                "{what}: never saw {needle:?}\n--- screen ---\n{}",
                self.capture(pane)
            );
        }
    }

    fn send(&self, pane: &str, key: &str) {
        assert!(self
            .s
            .tmux(&["send-keys", "-t", pane, key])
            .status
            .success());
    }

    /// Press `key` until `needle` is on screen (and `gone`, when given, is not), re-sending every
    /// [`RETRY_STEP`] and reading the screen before every press. A single blind send is how a live
    /// drive wedges: a TUI still painting drops the keystroke, and nothing says so.
    ///
    /// `gone` is what makes the wait a readiness check rather than a sighting. codex draws its full
    /// composer and THEN overlays the trust prompt, so "the composer is on screen" is true a beat
    /// before the pane can take a prompt; naming the overlay is what closes that window.
    fn press_until(&self, pane: &str, key: &str, needle: &str, gone: Option<&str>, what: &str) {
        let mut last: Option<Instant> = None;
        let ok = wait_until(LIVE_CEILING, || {
            let screen = self.capture(pane);
            if screen.contains(needle) && gone.map(|g| !screen.contains(g)).unwrap_or(true) {
                return true;
            }
            if last.map(|t| t.elapsed() >= RETRY_STEP).unwrap_or(true) {
                self.send(pane, key);
                last = Some(Instant::now());
            }
            false
        });
        assert!(
            ok,
            "{what}: pressing {key} never produced {needle:?}\n--- screen ---\n{}",
            self.capture(pane)
        );
    }

    /// Type `text` into the composer and submit it. The text goes in literally (`-l`), so a `/` or
    /// `-` in a prompt is never read as a key name; `Enter` is then pressed until the composer's
    /// `placeholder` is back, which is what says the prompt left the composer rather than sitting
    /// in it unsent.
    fn submit(&self, pane: &str, text: &str, echo: &str, placeholder: &str) {
        assert!(self
            .s
            .tmux(&["send-keys", "-t", pane, "-l", text])
            .status
            .success());
        self.await_screen(
            pane,
            echo,
            "the composer must echo the prompt before it is submitted",
        );
        self.press_until(
            pane,
            "Enter",
            placeholder,
            None,
            "the prompt must be submitted",
        );
    }

    /// Run `tma <args>` against the scratch server with the BUNDLED manifests and actions (no
    /// `--manifest-dir`: the point is to fire what ships) and the developer's own config and action
    /// dir pinned out.
    fn tma(&self, args: &[&str]) -> Output {
        Command::new(self.s.bin())
            .args(args)
            .arg("--socket-name")
            .arg(&self.s.socket)
            .env("TMA_CONFIG", empty_config_path())
            .env("XDG_CONFIG_HOME", &self.s.workdir)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .output()
            .expect("spawn tma")
    }

    /// Drive detection until the pane is stamped `blocked/permission`. `tma act --pane` names its
    /// target and so never runs a cycle of its own; `tma ls` is what stamps.
    fn await_blocked_permission(&self, pane: &str) {
        let stamped = wait_until(LIVE_CEILING, || {
            self.tma(&["ls"]);
            self.s.pane_option(pane, "@agent_state") == "blocked"
                && self.s.pane_option(pane, "@agent_detail") == "permission"
        });
        assert!(
            stamped,
            "the dialog must be detected as blocked/permission (state={:?} detail={:?})\n\
             --- screen ---\n{}",
            self.s.pane_option(pane, "@agent_state"),
            self.s.pane_option(pane, "@agent_detail"),
            self.capture(pane)
        );
    }

    /// Fire `action` at `pane` and assert it was sent.
    fn act(&self, action: &str, pane: &str) {
        let out = self.tma(&["act", action, "--pane", pane]);
        assert_eq!(
            out.status.code(),
            Some(0),
            "{action} must fire: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// The prompt every drive uses. Naming the command verbatim keeps the model from paraphrasing it
/// into something the sandbox would allow unasked.
fn touch_prompt(marker: &std::path::Path) -> String {
    format!("run this exact shell command: touch {}", marker.display())
}

/// The fragment of [`touch_prompt`] the composer echo is anchored on: short enough to survive the
/// composer's own wrapping of a long path.
const PROMPT_ECHO: &str = "run this exact shell";

/// Each agent's empty-composer placeholder. Its return is what says a prompt was submitted rather
/// than left sitting in the composer, and a stray `Enter` into an empty composer costs nothing.
const CODEX_COMPOSER: &str = "Ask Codex to do anything";
/// codex's first-run trust overlay, which it draws OVER the composer.
const CODEX_TRUST: &str = "Do you trust";
const GEMINI_COMPOSER: &str = "Type your message";

/// A-508. **The regression test for the v0.5.0 `Enter` to `y` change.** codex's approve option
/// prints its own accelerator (`1. Yes, proceed (y)`), while `Enter` confirms whatever the `›`
/// marker happens to be resting on, which nothing in tma's read path can know. So the marker is
/// moved OFF the approve option before the fire: `y` must still approve.
#[test]
fn codex_approve_sends_y_from_a_moved_selection_cursor() {
    if !have_agent("codex") || !python3_available() {
        return;
    }
    let mut live = Live::new("live_codex_approve");
    // `read-only` plus `on-request` is what makes a `touch` raise the dialog at all: under the
    // default workspace-write sandbox codex writes into a scratch temp dir unasked.
    let Some(pane) = live.start("codex", "codex", &["-s", "read-only", "-a", "on-request"]) else {
        return;
    };
    // codex asks to trust an unfamiliar directory. Answering it writes the grant into the SCRATCH
    // `CODEX_HOME` and nowhere else, which is the point of the pin.
    live.press_until(
        &pane,
        "Enter",
        CODEX_COMPOSER,
        Some(CODEX_TRUST),
        "codex startup",
    );

    let marker = live.s.workdir.join("codex_approve_probe");
    live.submit(&pane, &touch_prompt(&marker), PROMPT_ECHO, CODEX_COMPOSER);
    live.await_screen(&pane, "Yes, proceed", "codex permission dialog");

    // Walk the `›` marker onto option 3 ("No"), which is what makes `Enter` the wrong key. `Down`
    // wraps at the end of the list, so pressing until the marker is there cannot overshoot.
    live.press_until(
        &pane,
        "Down",
        "› 3.",
        None,
        "codex's selection cursor must leave option 1",
    );
    live.await_blocked_permission(&pane);
    live.act("approve", &pane);

    assert!(
        wait_until(LIVE_CEILING, || marker.exists()),
        "the approved command must run\n--- screen ---\n{}",
        live.capture(&pane)
    );
    assert!(
        wait_until(LIVE_CEILING, || !live
            .capture(&pane)
            .contains("Yes, proceed")),
        "the dialog must be gone\n--- screen ---\n{}",
        live.capture(&pane)
    );
}

/// A-304. gemini's dialog is a numbered list whose reject option prints `(esc)`, so both answers
/// are digits: `1` allows once, `3` rejects. One pane, two turns, so reaching the second dialog is
/// itself proof that the first one resolved.
#[test]
fn gemini_approve_sends_1_and_deny_sends_3() {
    if !have_agent("gemini") || !python3_available() {
        return;
    }
    let mut live = Live::new("live_gemini");
    // `--skip-trust` trusts the workspace for this session only, so the drive answers no dialog it
    // did not come to test and writes no trusted-folder entry even into the scratch home.
    let Some(pane) = live.start("gemini", "gemini", &["--skip-trust"]) else {
        return;
    };
    live.await_screen(&pane, GEMINI_COMPOSER, "gemini composer");

    let approved = live.s.workdir.join("gemini_approve_probe");
    live.submit(
        &pane,
        &touch_prompt(&approved),
        PROMPT_ECHO,
        GEMINI_COMPOSER,
    );
    live.await_screen(&pane, "Allow execution", "gemini permission dialog");
    live.await_blocked_permission(&pane);
    live.act("approve", &pane);
    assert!(
        wait_until(LIVE_CEILING, || approved.exists()),
        "the approved command must run\n--- screen ---\n{}",
        live.capture(&pane)
    );

    let denied = live.s.workdir.join("gemini_deny_probe");
    live.submit(&pane, &touch_prompt(&denied), PROMPT_ECHO, GEMINI_COMPOSER);
    live.await_screen(
        &pane,
        "Allow execution",
        "gemini's second permission dialog",
    );
    live.await_blocked_permission(&pane);
    live.act("deny", &pane);
    live.await_screen(&pane, "Request cancelled", "gemini's cancellation notice");
    assert!(
        !denied.exists(),
        "the denied command must not run\n--- screen ---\n{}",
        live.capture(&pane)
    );
}
