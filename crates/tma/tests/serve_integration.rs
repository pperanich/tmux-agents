//! `tma serve --stdio` acceptance: a real serve process on one end of two pipes, a scratch tmux
//! server and a private config dir on the other.
//!
//! Every test spawns `tma serve --stdio --device <id>` with stdin and stdout piped, writes NDJSON
//! request lines and reads NDJSON response lines off a background thread with a deadline. stderr
//! goes to a file under the scratch workdir, so a failure can quote what the host said about it.
//!
//! Two directories are pinned apart on purpose. `XDG_CONFIG_HOME` is `<workdir>/config`, so the
//! device store is `<workdir>/config/tma/devices.toml`; `XDG_RUNTIME_DIR` is `<workdir>/run`, so the
//! slot ledger and the connection registry are `<workdir>/run/tma/`. Sharing one directory would
//! make the "nothing under the runtime dir is a queue" scan meaningless.
//!
//! Panes are hand-stamped rather than detected, the way the `tma act --slot` suite does it: a
//! stamped pane is consumed without a manifest, so the fixture is the stamp and nothing here
//! depends on a capture rule matching a scratch shell.

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::Duration;

use serde_json::Value;
use tma_proto::{
    Binder, Budget, CardRequest, Cursor, Dispatch, EventRequest, Hello, ReceiptsRequest, Request,
    RequestFrame, SnapshotRequest, Subscribe, WindowRequest,
};
use tma_test_support::{wait_capture_contains, Scratch, POLL_CEILING, SHELL_PROMPT};

/// How long one response may take. Generous: a snapshot runs a whole detection cycle, and CI
/// serializes every tmux suite behind one lock.
const DEADLINE: Duration = Duration::from_secs(30);

/// How long a test waits before concluding that no frame is coming. Always paired with a positive
/// assertion elsewhere, so a slow machine cannot make the negative pass by accident.
const QUIET: Duration = Duration::from_millis(2000);

fn have_tmux() -> bool {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return false;
    }
    true
}

fn scratch(tag: &str) -> Scratch {
    Scratch::new_daemon(tag)
}

fn config_home(s: &Scratch) -> PathBuf {
    s.workdir.join("config")
}

fn runtime_dir(s: &Scratch) -> PathBuf {
    s.workdir.join("run").join("tma")
}

fn device_store(s: &Scratch) -> PathBuf {
    config_home(s).join("tma").join("devices.toml")
}

/// A `tma` command with the scratch's server, config dir and runtime dir pinned, so nothing any
/// test does can reach the developer's own tmux server or `~/.config/tma`.
fn tma_cmd(s: &Scratch) -> Command {
    let mut cmd = Command::new(tma_test_support::tma_bin());
    cmd.env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env("HOME", &s.workdir)
        .env("XDG_CONFIG_HOME", config_home(s))
        .env("XDG_RUNTIME_DIR", s.workdir.join("run"))
        .env("TMA_CONFIG", s.config_path());
    cmd
}

fn run_tma(s: &Scratch, args: &[&str]) -> Output {
    tma_cmd(s).args(args).output().expect("spawn tma")
}

fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

/// Pair a device through the CLI, so the record these tests serve is the one a user gets. `only`
/// pairs with `read` alone plus whatever `scopes` names: the watch-only device.
fn pair(s: &Scratch, name: &str, id: &str, scopes: &[&str], only: bool) {
    let mut args = vec!["device", "pair", name, "--id", id];
    for scope in scopes {
        args.push("--scope");
        args.push(scope);
    }
    if only {
        args.push("--only");
    }
    let out = run_tma(s, &args);
    assert!(out.status.success(), "pairing {name}: {}", stderr_of(&out));
}

/// Stamp `pane` as a fresh `blocked/permission` claude agent: approve's gate passes and the keys
/// path skips its freshness re-verify. The `tma act --slot` suite's fixture, so the wire path is
/// measured against the same one the CLI path is.
fn stamp_blocked_claude(s: &Scratch, pane: &str) {
    s.set_opt(pane, "@agent_name", "claude");
    s.set_opt(pane, "@agent_state", "blocked");
    s.set_opt(pane, "@agent_detail", "permission");
    s.set_opt(pane, "@agent_source", "capture");
    s.set_opt(pane, "@agent_pid", "4242");
    let now = tma_runtime::now_ms();
    // A real stamp dates its episode. Without it `episode_ms` is zero, which the wire reads as
    // "no expectation", and a binder test would pass by never checking anything.
    s.set_opt(pane, "@agent_since", &now.to_string());
    s.set_opt(pane, "@agent_stamped_at", &now.to_string());
}

fn capture(s: &Scratch, pane: &str) -> String {
    String::from_utf8_lossy(&s.tmux(&["capture-pane", "-p", "-t", pane]).stdout).to_string()
}

/// The approve key reached the pane exactly once: one `1` after the prompt and not two.
fn assert_one_keystroke(s: &Scratch, pane: &str) {
    let screen = capture(s, pane);
    assert!(
        screen.contains(&format!("{SHELL_PROMPT}1")),
        "the approve keystroke never reached the pane:\n{screen}"
    );
    assert!(
        !screen.contains(&format!("{SHELL_PROMPT}11")),
        "the pane received the keystroke twice:\n{screen}"
    );
}

fn assert_no_keystroke(s: &Scratch, pane: &str) {
    let screen = capture(s, pane);
    assert!(
        !screen.contains(&format!("{SHELL_PROMPT}1")),
        "a refused dispatch delivered a keystroke:\n{screen}"
    );
}

// ---- the harness --------------------------------------------------------------------------

static SERVE_LOGS: AtomicU32 = AtomicU32::new(0);

/// One live `tma serve --stdio` process on the far end of two pipes.
struct ServeHarness {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    /// Every line read, so a test can grep the whole connection's output rather than one frame.
    heard: Vec<String>,
    log: PathBuf,
}

impl ServeHarness {
    fn open(s: &Scratch, device: &str) -> ServeHarness {
        let n = SERVE_LOGS.fetch_add(1, Ordering::Relaxed);
        let log = s.workdir.join(format!("serve-{n}.log"));
        let stderr = std::fs::File::create(&log).expect("open the serve log");
        let mut child = tma_cmd(s)
            .args(["serve", "--stdio", "--device", device])
            .args(["--socket-name", &s.socket])
            .arg("--manifest-dir")
            .arg(s.manifest_dir())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(stderr))
            .spawn()
            .expect("spawn tma serve");
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { return };
                if tx.send(line).is_err() {
                    return;
                }
            }
        });
        ServeHarness {
            child,
            stdin: Some(stdin),
            lines,
            heard: Vec::new(),
            log,
        }
    }

    /// The host's own account of what it did, for a panic message.
    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_else(|e| format!("<unreadable: {e}>"))
    }

    fn write_frame(&mut self, frame: &RequestFrame) {
        let line = tma_proto::encode(frame).expect("encode a request");
        self.write_line(&line);
    }

    fn write_line(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("stdin is still open");
        writeln!(stdin, "{line}").expect("write a request line");
        stdin.flush().expect("flush the request");
    }

    /// The next frame, or a panic naming what the host logged.
    fn next(&mut self) -> Value {
        match self.next_within(DEADLINE) {
            Some(frame) => frame,
            None => panic!(
                "no response within {DEADLINE:?}\nserve log:\n{}",
                self.log()
            ),
        }
    }

    fn next_within(&mut self, deadline: Duration) -> Option<Value> {
        match self.lines.recv_timeout(deadline) {
            Ok(line) => {
                self.heard.push(line.clone());
                Some(serde_json::from_str(&line).unwrap_or_else(|e| {
                    panic!("stdout carried a line that is not a frame: {line:?} ({e})")
                }))
            }
            Err(RecvTimeoutError::Timeout) => None,
            // The child closed stdout: no frame is coming on this connection ever again.
            Err(RecvTimeoutError::Disconnected) => None,
        }
    }

    fn ask(&mut self, frame: &RequestFrame) -> Value {
        self.write_frame(frame);
        self.next()
    }

    /// The handshake, asserted successful, returning the `hello` frame.
    fn hello(&mut self, device: &str) -> Value {
        let frame = RequestFrame::new(
            "h",
            Request::Hello(Hello {
                app: "tma-test".to_string(),
                app_version: "0".to_string(),
                device: device.to_string(),
            }),
        );
        let response = self.ask(&frame);
        assert_eq!(
            response["t"],
            "hello",
            "the handshake was refused: {response}\nserve log:\n{}",
            self.log()
        );
        response
    }

    /// Close stdin without reaping: the EOF the protocol reads as "the caller hung up".
    fn close_stdin(&mut self) {
        self.stdin = None;
    }

    /// Read whatever else the host has to say, so a grep over the connection sees all of it.
    fn drain(&mut self) {
        while self.next_within(QUIET).is_some() {}
    }

    /// Everything this connection wrote, as one string.
    fn transcript(&self) -> String {
        self.heard.join("\n")
    }

    /// Close stdin and wait for the exit code.
    fn wait(&mut self) -> i32 {
        self.close_stdin();
        self.child
            .wait()
            .expect("reap the serve process")
            .code()
            .unwrap_or(-1)
    }
}

impl Drop for ServeHarness {
    fn drop(&mut self) {
        self.stdin = None;
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn snapshot_request(id: &str) -> RequestFrame {
    RequestFrame::new(id, Request::Snapshot(SnapshotRequest::default()))
}

fn dispatch(slot: &str, pane: &str, action: &str) -> Dispatch {
    Dispatch {
        slot: slot.to_string(),
        host: String::new(),
        pane: pane.to_string(),
        action: action.to_string(),
        binder: Binder::default(),
        text: None,
        answers: None,
        device: None,
    }
}

fn dispatch_request(id: &str, d: Dispatch) -> RequestFrame {
    RequestFrame::new(id, Request::Dispatch(d))
}

fn subscribe_request(id: &str) -> RequestFrame {
    RequestFrame::new(
        id,
        Request::Subscribe(Subscribe {
            events: true,
            selector: None,
        }),
    )
}

fn card_request(id: &str, pane: &str) -> RequestFrame {
    RequestFrame::new(
        id,
        Request::Card(CardRequest {
            pane: pane.to_string(),
        }),
    )
}

fn window_request(id: &str, pane: &str, last: u32, before: Option<Cursor>) -> RequestFrame {
    RequestFrame::new(
        id,
        Request::Window(WindowRequest {
            pane: pane.to_string(),
            last: Some(last),
            before,
            budget: Budget::default(),
        }),
    )
}

fn event_request(id: &str, pane: &str, cursor: Cursor) -> RequestFrame {
    RequestFrame::new(
        id,
        Request::Event(EventRequest {
            pane: pane.to_string(),
            cursor,
        }),
    )
}

/// The redacted E4 `PermissionRequest` payload claude 2.1.261 delivers, as the hook receives it.
/// What matters here is what it does NOT carry: no rendered label, so a card built from a record
/// written off it cannot be quoting a screen.
const HOOK_PAYLOAD: &str = r#"{
  "session_id": "7c74ac42-d330-4c0f-a7b1-ba9a918ba98e",
  "prompt_id": "20edf0d8-a22b-43de-94ab-4ecce0c78d11",
  "hook_event_name": "PermissionRequest",
  "tool_name": "Bash",
  "tool_input": {"command": "touch marker-leg1 && echo leg1-done"}
}"#;

/// Park a hook-lane request record under the SERVE process's runtime dir, where its
/// `PermissionRequest` hook would have left one. Rendered by the record's own writer, so the card
/// reads back exactly the bytes the lane produces; placed by hand because `hook_lane::write_request`
/// resolves the directory from its own process's `$XDG_RUNTIME_DIR`, and this process's is not the
/// serve process's.
fn park_hook_record(s: &Scratch, pane: &str, id: &str) -> PathBuf {
    let dir = runtime_dir(s).join("requests");
    std::fs::create_dir_all(&dir).expect("create the requests dir");
    let now = tma_runtime::now_ms();
    let record =
        tma_runtime::hook_lane::RequestRecord::from_payload(id, pane, HOOK_PAYLOAD, now, now);
    let path = dir.join(format!("{id}.json"));
    std::fs::write(&path, record.render()).expect("park the request record");
    path
}

/// Copy the committed claude corpus file into the scratch tree and stamp it onto `pane`, the way
/// claude's own hook payload does. `lines` truncates it: the first two lines end on a tool call
/// nothing resolved, which is the shape A-273 is about.
fn stamp_transcript(s: &Scratch, pane: &str, name: &str, lines: Option<usize>) -> PathBuf {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../tma-transcript/fixtures/stores/claude/2.1.236/claude-2.1.236-session.jsonl");
    let text = std::fs::read_to_string(&fixture).expect("read the committed claude fixture");
    let body = match lines {
        Some(n) => text.lines().take(n).fold(String::new(), |mut acc, line| {
            acc.push_str(line);
            acc.push('\n');
            acc
        }),
        None => text,
    };
    let dest = s.workdir.join(name);
    std::fs::write(&dest, body).expect("write the scratch transcript");
    s.set_opt(pane, "@agent_transcript", &dest.display().to_string());
    dest
}

fn kinds(frame: &Value) -> Vec<String> {
    frame["events"]
        .as_array()
        .expect("an events array")
        .iter()
        .map(|e| e["kind"].as_str().unwrap_or_default().to_string())
        .collect()
}

// ---- the handshake ------------------------------------------------------------------------

/// The connection is refused before anything is read, so an unpaired caller never gets far enough
/// to learn the fleet's shape. A typed frame rather than a dropped pipe: a client cannot tell a
/// hang up from a network fault.
#[test]
fn an_unknown_device_is_refused_with_a_typed_frame_and_exit_two() {
    if !have_tmux() {
        return;
    }
    let s = scratch("serve_unknown");
    let mut h = ServeHarness::open(&s, "SHA256:never-paired");
    let frame = h.next();
    assert_eq!(frame["t"], "error");
    assert_eq!(frame["code"], "scope-denied");
    assert_eq!(h.wait(), 2, "the process refuses to serve");
}

/// A-200. The hello answers the STORE's scopes, the host's reconcile interval and this build's
/// version. The client's own `device` claim is a log line and nothing more.
#[test]
fn the_handshake_answers_the_stores_scopes_not_the_requests() {
    if !have_tmux() {
        return;
    }
    let s = scratch("serve_hello");
    s.write_config("[serve]\nreconcile_interval_ms = 750\n");
    pair(&s, "phone", "SHA256:phone", &[], false);

    let mut h = ServeHarness::open(&s, "SHA256:phone");
    // The client names another device. The connection's own id is what authorizes it.
    let frame = h.hello("SHA256:somebody-else");
    assert_eq!(frame["schema"], 1);
    assert_eq!(frame["id"], "h");
    assert_eq!(frame["reconcile_interval_ms"], 750);
    assert_eq!(frame["tma_version"], env!("CARGO_PKG_VERSION"));
    assert!(
        frame["host"].as_str().is_some_and(|h| !h.is_empty()),
        "{frame}"
    );
    assert_eq!(
        frame["scopes"],
        serde_json::json!(["read", "act:answer", "act:steer"]),
        "the scopes are the store's: {frame}"
    );
}

/// A-106. A schema this build does not implement is a typed refusal, not a parse failure, and the
/// connection survives it so the device can downgrade instead of guessing why the pipe went quiet.
#[test]
fn a_schema_this_build_does_not_speak_is_refused_and_the_connection_survives() {
    if !have_tmux() {
        return;
    }
    let s = scratch("serve_schema");
    pair(&s, "phone", "SHA256:phone", &[], false);
    let mut h = ServeHarness::open(&s, "SHA256:phone");

    h.write_line(
        r#"{"schema":2,"id":"1","t":"hello","app":"future","app_version":"9","device":"SHA256:phone"}"#,
    );
    let refusal = h.next();
    assert_eq!(refusal["t"], "error");
    assert_eq!(refusal["code"], "unsupported-schema");
    assert_eq!(refusal["id"], "1");

    // Still serving: the refusal was about the frame, not about the connection.
    h.hello("SHA256:phone");
}

/// The protocol's own rule: the first frame of a session is the handshake, and it happens once.
#[test]
fn the_first_frame_must_be_the_handshake_and_there_is_only_one() {
    if !have_tmux() {
        return;
    }
    let s = scratch("serve_first");
    pair(&s, "phone", "SHA256:phone", &[], false);
    let mut h = ServeHarness::open(&s, "SHA256:phone");

    let refusal = h.ask(&snapshot_request("1"));
    assert_eq!(refusal["t"], "error");
    assert_eq!(refusal["code"], "bad-request");
    h.hello("SHA256:phone");

    let second = h.ask(&RequestFrame::new(
        "2",
        Request::Hello(Hello {
            app: "again".to_string(),
            app_version: "0".to_string(),
            device: "SHA256:phone".to_string(),
        }),
    ));
    assert_eq!(second["code"], "bad-request", "{second}");
}

// ---- reading ------------------------------------------------------------------------------

/// A-201. The wire row is the `tma ls --json` row minus `title`, asserted as set equality against
/// the local document rather than against a literal, so an additive key on either side has to be a
/// deliberate change on both.
#[test]
fn a_snapshot_row_is_the_ls_json_row_minus_title() {
    if !have_tmux() {
        return;
    }
    let s = scratch("serve_snapshot");
    let pane = s.new_pane();
    stamp_blocked_claude(&s, &pane);
    pair(&s, "phone", "SHA256:phone", &[], false);

    let local: Value = serde_json::from_str(&s.ls_json()).expect("ls --json parses");
    let local_row = local["agents"]
        .as_array()
        .and_then(|rows| rows.first())
        .unwrap_or_else(|| panic!("the stamp should make {pane} a row: {local}"))
        .clone();

    let mut h = ServeHarness::open(&s, "SHA256:phone");
    h.hello("SHA256:phone");
    let frame = h.ask(&snapshot_request("1"));
    assert_eq!(frame["t"], "snapshot", "{frame}");
    let wire_row = frame["agents"]
        .as_array()
        .and_then(|rows| rows.first())
        .unwrap_or_else(|| panic!("the snapshot should carry {pane}: {frame}"))
        .clone();

    let keys = |v: &Value| -> BTreeSet<String> {
        v.as_object().expect("an object").keys().cloned().collect()
    };
    let mut expected = keys(&local_row);
    assert!(expected.remove("title"), "ls --json carries a title");
    assert_eq!(keys(&wire_row), expected);
    assert_eq!(wire_row["pane"], local_row["pane"]);
    assert_eq!(wire_row["state"], "blocked");
    assert!(
        wire_row["stamped_at_ms"].is_number(),
        "the freshness anchor rides the wire row: {wire_row}"
    );
}

/// A-203, grep-class. A pane title carrying a right-to-left override and a nonce appears **zero**
/// times in the complete captured stdout of a driven session: handshake, snapshot, a subscription
/// with an edge, dispatch and receipts. A per-frame check would miss the frame nobody thought of.
#[test]
fn no_frame_of_a_whole_session_carries_a_pane_title() {
    if !have_tmux() {
        return;
    }
    let s = scratch("serve_title");
    s.write_config("[serve]\nreconcile_interval_ms = 250\n");
    let pane = s.new_pane();
    stamp_blocked_claude(&s, &pane);
    stamp_transcript(&s, &pane, "session.jsonl", None);
    let nonce = "zqxjkvbnonce7713";
    let title = format!("\u{202e}{nonce}");
    assert!(s
        .tmux(&["select-pane", "-t", &pane, "-T", &title])
        .status
        .success());
    pair(&s, "phone", "SHA256:phone", &[], false);

    let mut h = ServeHarness::open(&s, "SHA256:phone");
    h.hello("SHA256:phone");
    h.ask(&snapshot_request("1"));
    assert_eq!(h.ask(&subscribe_request("2"))["t"], "ack");

    // A second stamped pane, titled the same way, so an edge frame is built from a row whose title
    // is the thing under test.
    let out = s.tmux(&[
        "new-window",
        "-d",
        "-t",
        "s1",
        "-P",
        "-F",
        "#{pane_id}",
        "exec sleep 100000",
    ]);
    let second = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(second.starts_with('%'), "got {second:?}");
    assert!(s
        .tmux(&["select-pane", "-t", &second, "-T", &title])
        .status
        .success());
    stamp_blocked_claude(&s, &second);

    h.ask(&dispatch_request("3", dispatch("t1", &pane, "approve")));
    h.ask(&RequestFrame::new(
        "4",
        Request::Receipts(ReceiptsRequest::default()),
    ));
    // The card and transcript arms too: each answers about a pane whose title is the thing under
    // test, and a per-frame redaction check would miss whichever frame nobody thought of.
    let card = h.ask(&card_request("5", &pane));
    assert_eq!(card["t"], "card", "{card}\nserve log:\n{}", h.log());
    let window = h.ask(&window_request("6", &pane, 20, None));
    assert_eq!(window["t"], "window", "{window}");
    let cursor = window["events"].as_array().expect("an events array")[0]["cursor"]
        .as_str()
        .expect("a cursor")
        .to_string();
    assert_eq!(
        h.ask(&event_request("7", &pane, Cursor(cursor)))["t"],
        "event"
    );
    h.drain();

    let transcript = h.transcript();
    assert!(
        transcript.contains("\"t\":\"edge\""),
        "the session must actually have streamed an edge, or the grep proves nothing:\n{transcript}"
    );
    assert!(
        !transcript.contains(nonce),
        "a pane title reached the wire:\n{transcript}"
    );
    assert!(
        !transcript.contains("\"title\""),
        "a frame carries a title key:\n{transcript}"
    );
    assert!(
        !transcript.contains("\\u202e"),
        "the override escaped onto the wire:\n{transcript}"
    );
}

/// A-202. A subscription acks, then streams one edge per transition. The first cycle is the
/// stream's baseline and emits nothing, which is what makes a resumed stream replay nothing: a
/// device converges with `snapshot` and subscribes, and the second connection's baseline swallows
/// everything the first one already delivered.
#[test]
fn a_subscription_streams_edges_and_a_resumed_one_replays_nothing() {
    if !have_tmux() {
        return;
    }
    let s = scratch("serve_stream");
    s.write_config("[serve]\nreconcile_interval_ms = 250\n");
    let pane = s.new_pane();
    stamp_blocked_claude(&s, &pane);
    pair(&s, "phone", "SHA256:phone", &[], false);

    let mut first = ServeHarness::open(&s, "SHA256:phone");
    first.hello("SHA256:phone");
    assert_eq!(first.ask(&subscribe_request("sub"))["t"], "ack");

    // A pane that appears after the baseline is an edge with an open `from` end.
    let out = s.tmux(&[
        "new-window",
        "-d",
        "-t",
        "s1",
        "-P",
        "-F",
        "#{pane_id}",
        "exec sleep 100000",
    ]);
    let second = String::from_utf8_lossy(&out.stdout).trim().to_string();
    stamp_blocked_claude(&s, &second);

    let edge = loop {
        let frame = first
            .next_within(DEADLINE)
            .unwrap_or_else(|| panic!("no edge arrived\nserve log:\n{}", first.log()));
        if frame["t"] == "edge" {
            break frame;
        }
    };
    assert_eq!(
        edge["id"], "sub",
        "a streamed frame carries its subscribe's id"
    );
    assert_eq!(
        edge["from"], "",
        "a pane that appeared has an open from end"
    );
    assert_eq!(edge["pane"], second);
    drop(first);

    // The re-dial: converge with a snapshot, then subscribe again. Nothing is re-delivered.
    let mut resumed = ServeHarness::open(&s, "SHA256:phone");
    resumed.hello("SHA256:phone");
    let converged = resumed.ask(&snapshot_request("1"));
    let local: Value = serde_json::from_str(&s.ls_json()).expect("ls --json parses");
    let panes = |doc: &Value| -> BTreeSet<String> {
        doc["agents"]
            .as_array()
            .expect("an agents array")
            .iter()
            .map(|row| row["pane"].as_str().unwrap_or_default().to_string())
            .collect()
    };
    assert_eq!(
        panes(&converged),
        panes(&local),
        "the post-resume snapshot equals a freshly-taken ls --json"
    );
    assert_eq!(resumed.ask(&subscribe_request("sub2"))["t"], "ack");
    let replayed = resumed.next_within(QUIET);
    assert!(
        replayed.is_none(),
        "the resumed stream re-sent a transition the first one delivered: {replayed:?}"
    );
}

// ---- cards --------------------------------------------------------------------------------

/// A-525 over the wire. The record on disk is the whole difference between the two lanes: with one
/// parked the card is structured and `exact`, and removing it degrades the SAME pane to the screen
/// lane and `failed`, which is what tells the app to open the pane on the host instead.
#[test]
fn a_parked_hook_record_is_what_makes_the_card_structured() {
    if !have_tmux() {
        return;
    }
    let s = scratch("serve_card_hook");
    let pane = s.new_pane();
    stamp_blocked_claude(&s, &pane);
    let request_id = "d41d8cd98f00b204";
    s.set_opt(&pane, "@agent_permission_request", request_id);
    let record = park_hook_record(&s, &pane, request_id);
    pair(&s, "phone", "SHA256:phone", &[], false);

    let mut h = ServeHarness::open(&s, "SHA256:phone");
    h.hello("SHA256:phone");
    let hook = h.ask(&card_request("1", &pane));
    assert_eq!(hook["t"], "card", "{hook}\nserve log:\n{}", h.log());
    assert_eq!(hook["card"], "permission", "{hook}");
    assert_eq!(hook["lane"], "hook");
    assert_eq!(hook["extraction"], "exact");
    assert_eq!(hook["pane"], pane);
    assert_eq!(hook["agent"], "claude");

    let options = hook["options"].as_array().expect("an options array");
    assert_eq!(options.len(), 2, "{hook}");
    assert_eq!(options[0]["kind"], "allow-once");
    assert_eq!(options[1]["kind"], "reject-once");
    // The payload's own object, not a label anybody rendered.
    assert_eq!(hook["pending_call"]["tool"], "Bash");
    assert_eq!(
        hook["pending_call"]["input"]["command"], "touch marker-leg1 && echo leg1-done",
        "{hook}"
    );
    assert_eq!(hook["binder"]["expect_permission_request"], request_id);
    assert!(
        hook["binder"]["expect_episode_ms"].as_u64().unwrap_or(0) > 0,
        "the card quotes an episode a dispatch can bind to: {hook}"
    );

    // The hold expired, or the lane is off: the same pane, the same stamps, no record.
    std::fs::remove_file(&record).expect("unpark the record");
    let screen = h.ask(&card_request("2", &pane));
    assert_eq!(screen["lane"], "screen", "{screen}");
    assert_eq!(screen["extraction"], "failed");
    let names: Vec<&str> = screen["options"]
        .as_array()
        .expect("an options array")
        .iter()
        .map(|o| o["name"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(names, vec!["Approve", "Deny"], "the bundled labels");
    assert!(
        screen["options"]
            .as_array()
            .unwrap()
            .iter()
            .all(|o| o["option_id"].is_null()),
        "no index was read, so none is offered as a keycap: {screen}"
    );
}

/// A-207 to A-210. Every blocked detail but `permission` and a fetched `question` is something to
/// read: the variant carries a headline and has no field an approve control could be put in.
#[test]
fn a_plan_dialog_is_informational_and_offers_nothing() {
    if !have_tmux() {
        return;
    }
    let s = scratch("serve_card_plan");
    let pane = s.new_pane();
    stamp_blocked_claude(&s, &pane);
    s.set_opt(&pane, "@agent_detail", "plan");
    pair(&s, "phone", "SHA256:phone", &[], false);

    let mut h = ServeHarness::open(&s, "SHA256:phone");
    h.hello("SHA256:phone");
    let card = h.ask(&card_request("1", &pane));
    assert_eq!(card["t"], "card", "{card}\nserve log:\n{}", h.log());
    assert_eq!(card["card"], "informational", "{card}");
    assert_eq!(card["detail"], "plan");
    assert_eq!(card["headline"], "Plan approval");
    assert!(card["options"].is_null(), "nothing to fire: {card}");
}

/// A-273 over the wire, in both directions. The transcript ends on a tool call nothing resolved,
/// which is what a card reads to say what a pane is blocked ON; a pane the cycle calls `working`
/// still has no card, because blocked-ness comes from detection and from nowhere else.
#[test]
fn a_dangling_call_names_a_blocked_panes_prompt_and_never_makes_a_working_one_asked() {
    if !have_tmux() {
        return;
    }
    let s = scratch("serve_card_dangling");
    let pane = s.new_pane();
    stamp_blocked_claude(&s, &pane);
    stamp_transcript(&s, &pane, "dangling.jsonl", Some(2));
    pair(&s, "phone", "SHA256:phone", &[], false);

    let mut h = ServeHarness::open(&s, "SHA256:phone");
    h.hello("SHA256:phone");
    // Blocked: the tail is read, and the unresolved call is what the prompt is about.
    let blocked = h.ask(&card_request("1", &pane));
    assert_eq!(blocked["card"], "permission", "{blocked}");
    assert_eq!(
        blocked["pending_call"]["call_id"],
        "toolu_call01",
        "the transcript tail supplied the call: {blocked}\nserve log:\n{}",
        h.log()
    );
    assert_eq!(blocked["pending_call"]["tool"], "Bash");

    // The same pane, the same dangling call, working: an in-flight call is a slow tool as often as
    // a prompt, so it never makes a card.
    s.set_opt(&pane, "@agent_state", "working");
    s.set_opt(&pane, "@agent_detail", "");
    let working = h.ask(&card_request("2", &pane));
    assert_eq!(working["t"], "card", "{working}");
    assert_eq!(working["card"], "none", "{working}");
}

/// A pane this host does not have is a typed refusal on every arm that takes one, not an empty card
/// and not a dropped connection.
#[test]
fn a_card_or_window_for_an_unknown_pane_is_a_typed_error() {
    if !have_tmux() {
        return;
    }
    let s = scratch("serve_card_unknown");
    s.new_pane();
    pair(&s, "phone", "SHA256:phone", &[], false);

    let mut h = ServeHarness::open(&s, "SHA256:phone");
    h.hello("SHA256:phone");
    for (n, frame) in [
        card_request("1", "%9999"),
        window_request("2", "%9999", 10, None),
    ]
    .into_iter()
    .enumerate()
    {
        let refusal = h.ask(&frame);
        assert_eq!(refusal["t"], "error", "{n}: {refusal}");
        assert_eq!(refusal["code"], "not-found", "{n}: {refusal}");
    }
}

// ---- transcript ---------------------------------------------------------------------------

/// A window is a page of headers, newest first, and `older` walks back to the head of the file.
/// The transcript is the committed claude corpus, stamped the way claude's own hook payload stamps
/// it, so the page a device sees is measured against the same fixture the reader is pinned on.
#[test]
fn a_window_pages_newest_first_back_to_the_head() {
    if !have_tmux() {
        return;
    }
    let s = scratch("serve_window");
    let pane = s.new_pane();
    stamp_blocked_claude(&s, &pane);
    stamp_transcript(&s, &pane, "session.jsonl", None);
    pair(&s, "phone", "SHA256:phone", &[], false);

    let mut h = ServeHarness::open(&s, "SHA256:phone");
    h.hello("SHA256:phone");
    let first = h.ask(&window_request("1", &pane, 4, None));
    assert_eq!(first["t"], "window", "{first}\nserve log:\n{}", h.log());
    assert_eq!(first["pane"], pane);
    assert_eq!(first["agent"], "claude");
    assert_eq!(first["unknown"], 0, "every record of the corpus maps");
    assert_eq!(
        first["session"]["version"], "2.1.236",
        "the header rides the first page: {first}"
    );
    assert_eq!(
        kinds(&first),
        vec!["user_message", "compaction", "turn_boundary", "bookkeeping"],
        "newest first: {first}"
    );
    assert!(
        first["events"].as_array().unwrap()[0]["body"].is_null(),
        "a window read carries no bodies: {first}"
    );

    // Page back to the head. The corpus is twelve events over eleven records, so paging crosses a
    // record that carries more than one and must not skip its earlier halves.
    let mut seen = kinds(&first);
    let mut older = first["older"].as_str().map(|c| Cursor(c.to_string()));
    let mut page = 2;
    while let Some(cursor) = older.clone() {
        let next = h.ask(&window_request(&page.to_string(), &pane, 4, Some(cursor)));
        assert_eq!(next["t"], "window", "{next}");
        assert!(
            next["session"].is_null(),
            "the header rode page one: {next}"
        );
        seen.extend(kinds(&next));
        older = next["older"].as_str().map(|c| Cursor(c.to_string()));
        page += 1;
        assert!(page < 10, "paging never reached the head: {seen:?}");
    }
    assert_eq!(
        seen.len(),
        12,
        "the whole corpus, one page at a time: {seen:?}"
    );
    assert_eq!(
        seen.last().map(String::as_str),
        Some("user_message"),
        "the head of the file is the oldest event: {seen:?}"
    );
}

/// An `event` request fetches exactly one cursor's body, which is the whole reason a window carries
/// none: 200 headers cost kilobytes and the reader asks for the one message it wants to read.
#[test]
fn an_event_request_returns_one_body() {
    if !have_tmux() {
        return;
    }
    let s = scratch("serve_event");
    let pane = s.new_pane();
    stamp_blocked_claude(&s, &pane);
    stamp_transcript(&s, &pane, "session.jsonl", None);
    pair(&s, "phone", "SHA256:phone", &[], false);

    let mut h = ServeHarness::open(&s, "SHA256:phone");
    h.hello("SHA256:phone");
    let window = h.ask(&window_request("1", &pane, 20, None));
    let call = window["events"]
        .as_array()
        .expect("an events array")
        .iter()
        .find(|e| e["kind"] == "tool_call")
        .unwrap_or_else(|| panic!("the corpus has a tool call: {window}"));
    let cursor = call["cursor"].as_str().expect("a cursor").to_string();

    let event = h.ask(&event_request("2", &pane, Cursor(cursor.clone())));
    assert_eq!(event["t"], "event", "{event}\nserve log:\n{}", h.log());
    assert_eq!(event["cursor"], cursor, "the event answers its own cursor");
    assert_eq!(event["kind"], "tool_call");
    assert_eq!(event["body"]["kind"], "json");
    assert!(
        event["body"]["text"]
            .as_str()
            .is_some_and(|t| !t.is_empty()),
        "an event read populates the body: {event}"
    );

    // A cursor this host did not mint earns its own code, whose one right answer is to re-anchor.
    let forged = h.ask(&event_request("3", &pane, Cursor("t1.2.3.4.5".to_string())));
    assert_eq!(forged["t"], "error", "{forged}");
    assert_eq!(forged["code"], "cursor-invalid", "{forged}");
}

/// R20. A store the reader will not serve is a typed refusal carrying the reader's own sentence,
/// never an empty window: "nothing happened" is a different fact from "this cannot be shown".
#[test]
fn an_unreadable_store_is_a_typed_refusal_rather_than_an_empty_window() {
    if !have_tmux() {
        return;
    }
    let s = scratch("serve_window_store");
    let pane = s.new_pane();
    s.set_opt(&pane, "@agent_name", "cursor");
    s.set_opt(&pane, "@agent_state", "working");
    s.set_opt(&pane, "@agent_session", "sess01");
    let out = s.tmux(&[
        "new-window",
        "-d",
        "-t",
        "s1",
        "-P",
        "-F",
        "#{pane_id}",
        "exec sleep 100000",
    ]);
    let bare = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(bare.starts_with('%'), "got {bare:?}");
    s.set_opt(&bare, "@agent_name", "claude");
    s.set_opt(&bare, "@agent_state", "idle");
    pair(&s, "phone", "SHA256:phone", &[], false);

    let mut h = ServeHarness::open(&s, "SHA256:phone");
    h.hello("SHA256:phone");
    let refusal = h.ask(&window_request("1", &pane, 10, None));
    assert_eq!(refusal["t"], "error", "{refusal}\nserve log:\n{}", h.log());
    assert_eq!(refusal["code"], "unsupported", "{refusal}");
    assert!(
        refusal["message"]
            .as_str()
            .is_some_and(|m| m.contains("holes")),
        "the refusal says why this store is not served: {refusal}"
    );

    // A claude pane with nothing to read is a different refusal: there is no such transcript.
    let missing = h.ask(&window_request("2", &bare, 10, None));
    assert_eq!(missing["t"], "error", "{missing}");
    assert_eq!(missing["code"], "not-found", "{missing}");
}

// ---- dispatch -----------------------------------------------------------------------------

/// A-216 over the wire. The slot is claimed BEFORE the fire, so a repeat replays the receipt and
/// the pane receives the keystroke exactly once.
#[test]
fn a_repeat_dispatch_replays_its_receipt_and_sends_nothing() {
    if !have_tmux() {
        return;
    }
    let s = scratch("serve_slot");
    let pane = s.new_shell_pane();
    stamp_blocked_claude(&s, &pane);
    pair(&s, "phone", "SHA256:phone", &[], false);

    let mut h = ServeHarness::open(&s, "SHA256:phone");
    h.hello("SHA256:phone");
    let first = h.ask(&dispatch_request("1", dispatch("s1", &pane, "approve")));
    assert_eq!(first["t"], "receipt", "{first}\nserve log:\n{}", h.log());
    assert_eq!(first["outcome"], "sent", "{first}");
    assert_eq!(first["cached"], false);
    assert_eq!(first["exit_code"], 0);
    assert_eq!(
        first["device"], "SHA256:phone",
        "the receipt records who dispatched"
    );
    assert!(wait_capture_contains(
        &s.socket,
        &pane,
        &format!("{SHELL_PROMPT}1"),
        POLL_CEILING
    ));

    let second = h.ask(&dispatch_request("2", dispatch("s1", &pane, "approve")));
    assert_eq!(second["t"], "receipt");
    assert_eq!(second["cached"], true, "the replay marks itself: {second}");
    assert_eq!(second["outcome"], "sent");
    assert_eq!(second["at_ms"], first["at_ms"], "the claim's own instant");
    assert_one_keystroke(&s, &pane);
}

/// A-220. Two serve processes on one host share the ledger, because it is a file rather than
/// process state. A per-connection ledger passes every other test here and fails this one.
#[test]
fn two_connections_share_one_ledger() {
    if !have_tmux() {
        return;
    }
    let s = scratch("serve_shared");
    let pane = s.new_shell_pane();
    stamp_blocked_claude(&s, &pane);
    pair(&s, "phone", "SHA256:phone", &[], false);
    pair(&s, "tablet", "SHA256:tablet", &[], false);

    let mut a = ServeHarness::open(&s, "SHA256:phone");
    let mut b = ServeHarness::open(&s, "SHA256:tablet");
    a.hello("SHA256:phone");
    b.hello("SHA256:tablet");

    let fired = a.ask(&dispatch_request("1", dispatch("shared", &pane, "approve")));
    assert_eq!(fired["outcome"], "sent", "{fired}\n{}", a.log());
    assert_eq!(fired["cached"], false);
    assert!(wait_capture_contains(
        &s.socket,
        &pane,
        &format!("{SHELL_PROMPT}1"),
        POLL_CEILING
    ));

    let replay = b.ask(&dispatch_request("1", dispatch("shared", &pane, "approve")));
    assert_eq!(replay["cached"], true, "{replay}\n{}", b.log());
    assert_eq!(
        replay["device"], "SHA256:phone",
        "idempotency crosses devices: the second gets the first one's receipt"
    );
    assert_one_keystroke(&s, &pane);
}

/// A-283, the pocket disconnect. The response never reaches the client, and the client learns the
/// outcome on the next connection without dispatching anything to find out.
#[test]
fn a_dispatch_whose_response_was_lost_is_answered_by_receipts() {
    if !have_tmux() {
        return;
    }
    let s = scratch("serve_pocket");
    let pane = s.new_shell_pane();
    stamp_blocked_claude(&s, &pane);
    pair(&s, "phone", "SHA256:phone", &[], false);

    let mut lost = ServeHarness::open(&s, "SHA256:phone");
    lost.hello("SHA256:phone");
    lost.write_frame(&dispatch_request("1", dispatch("pocket", &pane, "approve")));
    // The read side goes away before the receipt lands: the host still fires, still writes the
    // ledger, and writes a response into a pipe nobody will ever read.
    assert_eq!(lost.wait(), 0, "the host exits cleanly on EOF");
    assert!(wait_capture_contains(
        &s.socket,
        &pane,
        &format!("{SHELL_PROMPT}1"),
        POLL_CEILING
    ));

    let mut redial = ServeHarness::open(&s, "SHA256:phone");
    redial.hello("SHA256:phone");
    let answer = redial.ask(&RequestFrame::new(
        "2",
        Request::Receipts(ReceiptsRequest {
            slot: Some("pocket".to_string()),
            since_ms: None,
        }),
    ));
    assert_eq!(answer["t"], "receipts", "{answer}");
    let receipts = answer["receipts"].as_array().expect("an array");
    assert_eq!(receipts.len(), 1, "{answer}");
    assert_eq!(receipts[0]["slot"], "pocket");
    assert_eq!(receipts[0]["outcome"], "sent");
    assert_eq!(receipts[0]["device"], "SHA256:phone");
    assert_eq!(receipts[0]["cached"], true, "a ledger read is a replay");
    assert_one_keystroke(&s, &pane);
}

/// §5.3 over the wire. The binder rides the dispatch frame and is checked under the pane lock: a
/// device that quotes an episode the pane has left is refused and nothing is sent.
#[test]
fn the_binder_travels_on_the_dispatch_frame() {
    if !have_tmux() {
        return;
    }
    let s = scratch("serve_binder");
    let pane = s.new_shell_pane();
    stamp_blocked_claude(&s, &pane);
    pair(&s, "phone", "SHA256:phone", &[], false);

    let mut h = ServeHarness::open(&s, "SHA256:phone");
    h.hello("SHA256:phone");

    // The episode a device would quote is the one on the row it acted on.
    let snapshot = h.ask(&snapshot_request("0"));
    let episode = snapshot["agents"][0]["episode_ms"]
        .as_u64()
        .unwrap_or_else(|| panic!("the row dates its episode: {snapshot}"));
    assert!(episode > 0, "a zero episode binds nothing: {snapshot}");

    let mut stale = dispatch("bind1", &pane, "approve");
    stale.binder = Binder {
        expect_episode_ms: episode - 1,
        expect_permission_request: None,
    };
    let refused = h.ask(&dispatch_request("1", stale));
    assert_eq!(refused["outcome"], "refused", "{refused}");
    assert_eq!(refused["reason"], "episode-changed");
    assert_no_keystroke(&s, &pane);

    // The instant the pane is actually in still fires.
    let mut bound = dispatch("bind2", &pane, "approve");
    bound.binder = Binder {
        expect_episode_ms: episode,
        expect_permission_request: None,
    };
    let sent = h.ask(&dispatch_request("2", bound));
    assert_eq!(sent["outcome"], "sent", "{sent}\nserve log:\n{}", h.log());
}

// ---- scopes -------------------------------------------------------------------------------

/// A-221. A hand-written client holding only `read` sends a well-formed dispatch. The refusal
/// happens before `broker::fire` and before the slot claim, so no keystroke reaches the pane and
/// no ledger entry is written.
#[test]
fn a_read_only_device_is_refused_before_anything_reaches_the_pane() {
    if !have_tmux() {
        return;
    }
    let s = scratch("serve_scope");
    let pane = s.new_shell_pane();
    stamp_blocked_claude(&s, &pane);
    pair(&s, "watcher", "SHA256:watcher", &[], true);

    let mut h = ServeHarness::open(&s, "SHA256:watcher");
    let hello = h.hello("SHA256:watcher");
    assert_eq!(hello["scopes"], serde_json::json!(["read"]), "{hello}");

    let refused = h.ask(&dispatch_request("1", dispatch("nope", &pane, "approve")));
    assert_eq!(refused["t"], "receipt", "{refused}");
    assert_eq!(refused["outcome"], "refused");
    assert_eq!(refused["reason"], "scope-denied");
    assert_eq!(refused["exit_code"], 4);
    assert_no_keystroke(&s, &pane);

    // The gate is ahead of the claim, so the slot was never spent.
    let ledger = std::fs::read_to_string(runtime_dir(&s).join("ledger.jsonl")).unwrap_or_default();
    assert!(
        !ledger.contains("\"slot\":\"nope\""),
        "a scope refusal must not claim the slot: {ledger}"
    );
}

/// A-253's shape. Steering needs `act:steer`, and only the CLI can grant it: the device asks for
/// nothing, and the store's next read is what widens the live connection.
#[test]
fn steering_needs_its_scope_and_only_the_cli_can_grant_it() {
    if !have_tmux() {
        return;
    }
    let s = scratch("serve_grant");
    let pane = s.new_shell_pane();
    stamp_blocked_claude(&s, &pane);
    pair(&s, "watcher", "SHA256:watcher", &[], true);

    let mut h = ServeHarness::open(&s, "SHA256:watcher");
    h.hello("SHA256:watcher");
    let mut steer = dispatch("st1", &pane, "steer");
    steer.text = Some("hello from a phone".to_string());
    let refused = h.ask(&dispatch_request("1", steer));
    assert_eq!(refused["reason"], "scope-denied", "{refused}");

    let granted = run_tma(&s, &["device", "grant", "watcher", "act:steer"]);
    assert!(granted.status.success(), "{}", stderr_of(&granted));

    // The same live connection: the store is re-read per request, so the grant lands without a
    // re-dial. The pane is `blocked`, so a text action still refuses, on a DIFFERENT reason.
    let mut steer = dispatch("st2", &pane, "steer");
    steer.text = Some("hello from a phone".to_string());
    let after = h.ask(&dispatch_request("2", steer));
    assert_ne!(
        after["reason"], "scope-denied",
        "the grant reached the live connection: {after}"
    );
    assert_no_keystroke(&s, &pane);
}

/// A-225. An exec action is unreachable from any device, and a payload with nowhere to go is a
/// protocol error rather than a value the host quietly drops.
#[test]
fn an_exec_action_and_a_mismatched_payload_are_typed_errors() {
    if !have_tmux() {
        return;
    }
    let s = scratch("serve_exec");
    let pane = s.new_shell_pane();
    stamp_blocked_claude(&s, &pane);
    pair(&s, "phone", "SHA256:phone", &[], false);

    let actions = config_home(&s).join("tma").join("actions");
    std::fs::create_dir_all(&actions).unwrap();
    std::fs::write(
        actions.join("runme.toml"),
        "min_engine_version = \"0.1\"\nname = \"runme\"\nlabel = \"Run me\"\n\
         kind = \"exec\"\nwhen = { state = [\"blocked\"] }\ncommand = \"/bin/true\"\n",
    )
    .unwrap();

    let mut h = ServeHarness::open(&s, "SHA256:phone");
    h.hello("SHA256:phone");

    let exec = h.ask(&dispatch_request("1", dispatch("x1", &pane, "runme")));
    assert_eq!(exec["t"], "error", "{exec}");
    assert_eq!(exec["code"], "scope-denied");

    let mut with_text = dispatch("x2", &pane, "approve");
    with_text.text = Some("not for a keys action".to_string());
    let payload = h.ask(&dispatch_request("2", with_text));
    assert_eq!(payload["t"], "error", "{payload}");
    assert_eq!(payload["code"], "bad-request");

    // A text action with no string is the same class of usage error.
    let missing = h.ask(&dispatch_request("3", dispatch("x3", &pane, "steer")));
    assert_eq!(missing["t"], "error", "{missing}");
    assert_eq!(missing["code"], "bad-request");

    // And an action nobody named in the scope table is refused even though it is bundled.
    let control = h.ask(&dispatch_request("4", dispatch("x4", &pane, "compact")));
    assert_eq!(control["t"], "receipt", "{control}");
    assert_eq!(control["reason"], "scope-denied");
    assert_no_keystroke(&s, &pane);
}

/// A-205 and A-223. Revocation reaches a live connection: the store is read per request, and the
/// absence of the RECORD ends the connection rather than degrading it to a read-only device.
/// A-222 rides along: no request type widens a scope, so the store's bytes are unchanged.
#[test]
fn a_revoked_device_is_refused_on_its_next_request_and_the_process_exits() {
    if !have_tmux() {
        return;
    }
    let s = scratch("serve_revoke");
    let pane = s.new_pane();
    stamp_blocked_claude(&s, &pane);
    pair(&s, "phone", "SHA256:phone", &[], false);

    let before = std::fs::read(device_store(&s)).expect("the store exists");
    let mut h = ServeHarness::open(&s, "SHA256:phone");
    h.hello("SHA256:phone");
    assert_eq!(h.ask(&snapshot_request("1"))["t"], "snapshot");
    h.ask(&RequestFrame::new(
        "2",
        Request::Receipts(ReceiptsRequest::default()),
    ));
    h.ask(&dispatch_request("3", dispatch("r1", &pane, "approve")));
    assert_eq!(
        std::fs::read(device_store(&s)).expect("the store exists"),
        before,
        "no request type widens a scope"
    );

    let revoked = run_tma(&s, &["device", "revoke", "phone"]);
    assert!(revoked.status.success(), "{}", stderr_of(&revoked));

    let refusal = h.ask(&snapshot_request("4"));
    assert_eq!(refusal["t"], "error", "{refusal}");
    assert_eq!(refusal["code"], "scope-denied");
    assert_eq!(refusal["id"], "4");
    assert_eq!(h.wait(), 2, "a revoked device is not a read-only device");
}

// ---- limits and hygiene -------------------------------------------------------------------

/// A-285. Each connection runs its own detection cycle, so connections cost tmux query throughput
/// and the cap is what bounds them. The next one is refused with a typed error, not starved.
#[test]
fn the_connection_beyond_the_cap_is_refused_with_a_typed_error() {
    if !have_tmux() {
        return;
    }
    let s = scratch("serve_cap");
    s.write_config("[serve]\nmax_connections = 2\n");
    pair(&s, "phone", "SHA256:phone", &[], false);

    let mut held = Vec::new();
    for _ in 0..2 {
        let mut h = ServeHarness::open(&s, "SHA256:phone");
        h.hello("SHA256:phone");
        held.push(h);
    }
    let mut refused = ServeHarness::open(&s, "SHA256:phone");
    let frame = refused.next();
    assert_eq!(frame["t"], "error", "{frame}");
    assert_eq!(frame["code"], "too-many-connections");
    assert_eq!(refused.wait(), 2);

    // A closed connection frees its slot for the next dial.
    drop(held.pop());
    let mut next = ServeHarness::open(&s, "SHA256:phone");
    next.hello("SHA256:phone");
}

/// A line the host cannot read earns a typed refusal on the same stream and the connection keeps
/// serving. A dropped pipe is what a client cannot tell from a network fault.
#[test]
fn an_unreadable_line_is_refused_and_the_connection_keeps_serving() {
    if !have_tmux() {
        return;
    }
    let s = scratch("serve_lines");
    // A pane, so the scratch server exists: the closing assertion is that a snapshot still answers,
    // and a snapshot against no server is a different failure wearing the same shape.
    s.new_pane();
    pair(&s, "phone", "SHA256:phone", &[], false);
    let mut h = ServeHarness::open(&s, "SHA256:phone");
    h.hello("SHA256:phone");

    for line in [
        "not json at all",
        r#"{"schema":1,"id":"9"}"#,
        r#"{"schema":1,"id":"9","t":"nonesuch"}"#,
        "",
    ] {
        h.write_line(line);
        let refusal = h.next();
        assert_eq!(refusal["t"], "error", "{line:?} -> {refusal}");
        assert_eq!(refusal["code"], "bad-request", "{line:?}");
    }

    // An overlong line is dropped whole and its newline consumed, so the next frame still answers.
    h.write_line(&format!(
        r#"{{"schema":1,"id":"1","t":"snapshot","pad":"{}"}}"#,
        "x".repeat(70_000)
    ));
    let refusal = h.next();
    assert_eq!(refusal["code"], "bad-request", "{refusal}");
    assert_eq!(h.ask(&snapshot_request("ok"))["t"], "snapshot");
}

/// A-206. Nothing on this path stores an unsent frame. After a session that refused a dispatch and
/// abandoned a subscription mid-stream, the runtime dir holds the ledger, its lock and an EMPTY
/// connection registry, and nothing else.
#[test]
fn a_finished_session_leaves_no_queue_or_spool_behind() {
    if !have_tmux() {
        return;
    }
    let s = scratch("serve_hygiene");
    let pane = s.new_pane();
    stamp_blocked_claude(&s, &pane);
    pair(&s, "watcher", "SHA256:watcher", &[], true);

    let mut h = ServeHarness::open(&s, "SHA256:watcher");
    h.hello("SHA256:watcher");
    h.ask(&snapshot_request("1"));
    // A refused dispatch and a subscription abandoned mid-stream: the two shapes that would want a
    // spool if anything here did.
    h.ask(&dispatch_request(
        "2",
        dispatch("refused", &pane, "approve"),
    ));
    h.write_frame(&subscribe_request("3"));
    assert_eq!(h.wait(), 0, "EOF is a clean exit");

    let dir = runtime_dir(&s);
    let entries: BTreeSet<String> = std::fs::read_dir(&dir)
        .expect("the runtime dir exists")
        .map(|e| {
            e.expect("an entry")
                .file_name()
                .to_string_lossy()
                .to_string()
        })
        .collect();
    let allowed: BTreeSet<String> = ["ledger.jsonl", "ledger.lock", "serve"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert!(
        entries.is_subset(&allowed),
        "the runtime dir grew something outside the allowlist: {entries:?}"
    );
    assert!(
        markers(&s).is_empty(),
        "a closed connection left {:?}",
        markers(&s)
    );
}

/// SIGTERM is a clean exit: the registry marker goes with the process rather than holding a slot
/// against the cap until something later notices the pid is gone.
#[test]
fn sigterm_exits_cleanly_and_releases_the_connection_slot() {
    if !have_tmux() {
        return;
    }
    let s = scratch("serve_sigterm");
    pair(&s, "phone", "SHA256:phone", &[], false);
    let mut h = ServeHarness::open(&s, "SHA256:phone");
    h.hello("SHA256:phone");
    assert_eq!(markers(&s).len(), 1, "a live connection is registered");

    let pid = h.child.id().to_string();
    assert!(Command::new("kill")
        .args(["-TERM", &pid])
        .status()
        .expect("kill")
        .success());
    let code = h.child.wait().expect("reap").code().unwrap_or(-1);
    assert_eq!(code, 0, "SIGTERM is a clean exit");
    assert!(
        markers(&s).is_empty(),
        "SIGTERM left {:?} behind",
        markers(&s)
    );
}

/// The connection registry's marker files. A registry, never a queue: two facts and no payload.
fn markers(s: &Scratch) -> Vec<String> {
    std::fs::read_dir(runtime_dir(s).join("serve"))
        .map(|rd| {
            rd.map(|e| {
                e.expect("an entry")
                    .file_name()
                    .to_string_lossy()
                    .to_string()
            })
            .collect()
        })
        .unwrap_or_default()
}

/// Every arm answers the id it was asked under, on the same connection and in order. Written as
/// hand-rolled lines rather than encoded frames, because a device's `card`/`window`/`event` frames
/// carry only the fields it has: the defaults have to hold on the host side too.
#[test]
fn every_request_type_answers_its_own_correlation_id() {
    if !have_tmux() {
        return;
    }
    let s = scratch("serve_ids");
    let pane = s.new_pane();
    stamp_blocked_claude(&s, &pane);
    stamp_transcript(&s, &pane, "session.jsonl", None);
    pair(&s, "phone", "SHA256:phone", &[], false);
    let mut h = ServeHarness::open(&s, "SHA256:phone");
    h.hello("SHA256:phone");

    for (id, t, line) in [
        (
            "1",
            "card",
            format!(r#"{{"schema":1,"id":"1","t":"card","pane":"{pane}"}}"#),
        ),
        (
            "2",
            "window",
            format!(r#"{{"schema":1,"id":"2","t":"window","pane":"{pane}"}}"#),
        ),
        (
            "3",
            "error",
            format!(r#"{{"schema":1,"id":"3","t":"event","pane":"{pane}","cursor":"c"}}"#),
        ),
    ] {
        h.write_line(&line);
        let answer = h.next();
        assert_eq!(answer["id"], id, "{line} -> {answer}");
        assert_eq!(
            answer["t"],
            t,
            "{line} -> {answer}\nserve log:\n{}",
            h.log()
        );
    }
}

/// `tma device` round trip through the CLI, which is the only surface that writes the store.
#[test]
fn the_device_cli_pairs_grants_revokes_and_lists() {
    let s = scratch("serve_device_cli");
    pair(&s, "phone", "SHA256:phone", &[], false);
    pair(&s, "watcher", "SHA256:watcher", &[], true);

    let listed = run_tma(&s, &["device", "list", "--json"]);
    assert!(listed.status.success(), "{}", stderr_of(&listed));
    let doc: Value = serde_json::from_str(&stdout_of(&listed)).expect("the document parses");
    assert_eq!(doc["schema"], 1);
    let devices = doc["devices"].as_array().expect("an array");
    assert_eq!(devices.len(), 2);
    assert_eq!(devices[0]["name"], "phone");
    assert_eq!(
        devices[0]["scopes"],
        serde_json::json!(["read", "act:answer", "act:steer"])
    );
    assert_eq!(devices[1]["scopes"], serde_json::json!(["read"]));

    let granted = run_tma(&s, &["device", "grant", "phone", "act:always"]);
    assert!(granted.status.success(), "{}", stderr_of(&granted));
    assert!(stdout_of(&granted).contains("act:always"));

    // A scope outside the vocabulary is a usage error naming the four, never a grant nobody holds.
    let typo = run_tma(&s, &["device", "grant", "phone", "act:everything"]);
    assert_eq!(typo.status.code(), Some(2), "{}", stderr_of(&typo));

    let revoked = run_tma(&s, &["device", "revoke", "watcher"]);
    assert!(revoked.status.success(), "{}", stderr_of(&revoked));
    assert!(
        stdout_of(&revoked).contains("authorized_keys"),
        "revoke names the line the user still has to remove: {}",
        stdout_of(&revoked)
    );
    let missing = run_tma(&s, &["device", "revoke", "watcher"]);
    assert_eq!(missing.status.code(), Some(3), "{}", stderr_of(&missing));

    let text = stdout_of(&run_tma(&s, &["device", "list"]));
    let lines: Vec<&str> = text.lines().map(|l| l.trim_end()).collect();
    assert_eq!(lines.len(), 1);
    assert_eq!(
        lines[0].split('\t').take(3).collect::<Vec<_>>(),
        vec![
            "phone",
            "SHA256:phone",
            "read,act:answer,act:steer,act:always"
        ]
    );
}
