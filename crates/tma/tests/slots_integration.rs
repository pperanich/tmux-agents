//! Idempotent dispatch (`tma act --slot`) and the ledger behind it, end to end against a scratch
//! `tmux -L` server and a private runtime dir.
//!
//! The oracle everywhere is the pane itself: the shell prompt is the fixed `SHELL_PROMPT`, and
//! claude's approve sequence is the single key `1`, so `tma> 1` in the capture is one delivered
//! keystroke and `tma> 11` is two. Every test that says "sends nothing" asserts that difference
//! rather than trusting an exit code.
//!
//! `XDG_RUNTIME_DIR` is the scratch workdir, so the ledger these tests read and truncate is theirs
//! and never the developer's own.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tma_runtime::slots::LEDGER_FILE;
use tma_test_support::{wait_capture_contains, Scratch, POLL_CEILING, SHELL_PROMPT};

fn have_tmux() -> bool {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return false;
    }
    true
}

/// A scratch whose workdir doubles as the private `XDG_RUNTIME_DIR` holding the ledger.
fn scratch(tag: &str) -> Scratch {
    Scratch::new_daemon(tag)
}

/// Run `tma act` against the scratch server. `Scratch::command` pins the runtime dir and an empty
/// `TMA_CONFIG`; `XDG_CONFIG_HOME` pins the user action dir empty, so only the bundled actions load.
fn act_cmd(s: &Scratch) -> Command {
    let mut cmd = s.command();
    cmd.arg("act")
        .args(["--socket-name", &s.socket])
        .arg("--manifest-dir")
        .arg(s.manifest_dir())
        .env("XDG_CONFIG_HOME", &s.workdir);
    cmd
}

fn act(s: &Scratch, args: &[&str]) -> Output {
    let mut cmd = act_cmd(s);
    cmd.args(args);
    cmd.output().expect("spawn tma act")
}

fn receipts(s: &Scratch, args: &[&str]) -> Output {
    s.command()
        .arg("receipts")
        .args(args)
        .output()
        .expect("spawn tma receipts")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

/// Stamp `pane` as a fresh `blocked/permission` claude agent: approve's gate passes and the keys
/// path skips its freshness re-verify.
fn stamp_blocked_claude(s: &Scratch, pane: &str) {
    s.set_opt(pane, "@agent_name", "claude");
    s.set_opt(pane, "@agent_state", "blocked");
    s.set_opt(pane, "@agent_detail", "permission");
    s.set_opt(pane, "@agent_source", "capture");
    s.set_opt(pane, "@agent_pid", "4242");
    refresh_stamp(s, pane);
}

/// Re-date the pane's stamp. A dispatch loop outruns the broker's 3 s freshness bound, and a
/// re-verify would correctly unmask a shell pane wearing a hand-written claude stamp.
fn refresh_stamp(s: &Scratch, pane: &str) {
    s.set_opt(
        pane,
        "@agent_stamped_at",
        &tma_runtime::now_ms().to_string(),
    );
}

fn capture(s: &Scratch, pane: &str) -> String {
    String::from_utf8_lossy(&s.tmux(&["capture-pane", "-p", "-t", pane]).stdout).to_string()
}

/// The pane received the approve key exactly once: the prompt carries one `1` and not two.
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

fn ledger_path(s: &Scratch) -> PathBuf {
    s.workdir.join("tma").join(LEDGER_FILE)
}

/// Shift every record back by `by_ms`, so a test can age the ledger without waiting a day.
fn age_ledger(path: &Path, by_ms: u64) {
    let text = std::fs::read_to_string(path).expect("read the ledger");
    let mut out = String::new();
    for line in text.lines() {
        let key = "\"at_ms\":";
        let at = line.find(key).expect("every record dates itself") + key.len();
        let end = at
            + line[at..]
                .find(|c: char| !c.is_ascii_digit())
                .expect("a number is followed by something");
        let value: u64 = line[at..end].parse().expect("epoch ms");
        out.push_str(&format!(
            "{}{}{}\n",
            &line[..at],
            value.saturating_sub(by_ms),
            &line[end..]
        ));
    }
    std::fs::write(path, out).expect("write the ledger");
}

/// Every slot id named in a `receipts --json` document, in document order.
fn slots_in(json: &str) -> Vec<String> {
    json.split("\"slot\":\"")
        .skip(1)
        .map(|rest| rest.split('"').next().unwrap_or_default().to_string())
        .collect()
}

/// Two identical dispatches: the pane receives the keystroke once and the second invocation
/// replays the first one's receipt.
///
/// `[MUT]` A ledger that claims AFTER firing fails here: both invocations would find the slot free,
/// both would fire, and the capture would read `tma> 11`. Watched failing by moving the claim below
/// the `fire` call in `act::fire_once`.
#[test]
fn a_repeat_dispatch_replays_the_receipt_and_sends_nothing() {
    if !have_tmux() {
        return;
    }
    let s = scratch("slot_once");
    let pane = s.new_shell_pane();
    stamp_blocked_claude(&s, &pane);

    let first = act(&s, &["approve", "--pane", &pane, "--slot", "s1", "--json"]);
    assert_eq!(first.status.code(), Some(0), "{}", stderr(&first));
    assert!(
        stdout(&first).contains(r#""outcome":"sent""#)
            && stdout(&first).contains(r#""cached":false"#),
        "the first dispatch fires and says so: {}",
        stdout(&first)
    );
    assert!(
        wait_capture_contains(&s.socket, &pane, &format!("{SHELL_PROMPT}1"), POLL_CEILING),
        "the approve keystroke should reach the pane"
    );

    let second = act(&s, &["approve", "--pane", &pane, "--slot", "s1", "--json"]);
    assert_eq!(
        second.status.code(),
        Some(0),
        "the replay carries the original exit code: {}",
        stderr(&second)
    );
    assert!(
        stdout(&second).contains(r#""cached":true"#),
        "the replay marks itself cached: {}",
        stdout(&second)
    );
    assert!(
        stderr(&second).contains("cached receipt for slot `s1`"),
        "the human line names the slot: {}",
        stderr(&second)
    );
    assert_one_keystroke(&s, &pane);
}

/// The read half of the pocket disconnect. The outcome of a dispatch whose response was lost is answerable without
/// dispatching anything to find it out.
#[test]
fn receipts_answer_a_lost_response_without_dispatching() {
    if !have_tmux() {
        return;
    }
    let s = scratch("slot_receipts");
    let pane = s.new_shell_pane();
    stamp_blocked_claude(&s, &pane);

    let fired = act(
        &s,
        &[
            "approve", "--pane", &pane, "--slot", "lost", "--device", "phone", "--json",
        ],
    );
    assert_eq!(fired.status.code(), Some(0), "{}", stderr(&fired));
    assert!(wait_capture_contains(
        &s.socket,
        &pane,
        &format!("{SHELL_PROMPT}1"),
        POLL_CEILING
    ));

    let text = receipts(&s, &["--slot", "lost"]);
    assert_eq!(text.status.code(), Some(0));
    let line = stdout(&text);
    let fields: Vec<&str> = line.trim_end().split('\t').collect();
    assert_eq!(
        fields.len(),
        8,
        "one tab-separated line per receipt: {line:?}"
    );
    assert_eq!(&fields[1..5], ["lost", pane.as_str(), "approve", "sent"]);
    assert_eq!(
        fields[7], "phone",
        "the dispatching device is on the record"
    );

    let json = stdout(&receipts(&s, &["--slot", "lost", "--json"]));
    assert!(json.starts_with(r#"{"schema":1,"receipts":["#), "{json}");
    assert!(json.contains(r#""outcome":"sent""#) && json.contains(r#""exit_code":0"#));
    // Asking twice is still a read: the pane never sees a second keystroke.
    assert_one_keystroke(&s, &pane);
    // A slot nobody dispatched has nothing to say.
    assert_eq!(stdout(&receipts(&s, &["--slot", "never"])), "");
}

/// `locked` is the one refusal that leaves the slot claimable, so a retry fires.
#[test]
fn a_locked_refusal_leaves_the_slot_claimable() {
    if !have_tmux() {
        return;
    }
    let s = scratch("slot_locked");
    let pane = s.new_shell_pane();
    stamp_blocked_claude(&s, &pane);
    // A live holder: our own pid, and an expiry an hour out, so the broker cannot reclaim it.
    let held = format!(
        "{}:{}:{}:approve",
        tma_runtime::now_ms() + 3_600_000,
        "f".repeat(32),
        std::process::id()
    );
    s.set_opt(&pane, "@agent_action", &held);

    let refused = act(&s, &["approve", "--pane", &pane, "--slot", "s2", "--json"]);
    assert_eq!(refused.status.code(), Some(5), "{}", stderr(&refused));
    assert!(
        stdout(&refused).contains(r#""reason":"locked""#),
        "{}",
        stdout(&refused)
    );
    assert_eq!(
        stdout(&receipts(&s, &["--slot", "s2"])),
        "",
        "a released slot leaves no receipt behind"
    );

    s.set_opt(&pane, "@agent_action", "");
    refresh_stamp(&s, &pane);
    let retry = act(&s, &["approve", "--pane", &pane, "--slot", "s2", "--json"]);
    assert_eq!(
        retry.status.code(),
        Some(0),
        "the retry must fire, not replay: {}",
        stderr(&retry)
    );
    assert!(
        stdout(&retry).contains(r#""outcome":"sent""#)
            && stdout(&retry).contains(r#""cached":false"#),
        "{}",
        stdout(&retry)
    );
    assert!(wait_capture_contains(
        &s.socket,
        &pane,
        &format!("{SHELL_PROMPT}1"),
        POLL_CEILING
    ));
}

/// Every refusal class writes a terminal receipt, and a retry replays it rather than trying
/// again. `error` is covered by the module's own unit test, since a broker failure needs no tmux.
#[test]
fn every_refusal_class_writes_a_terminal_receipt() {
    if !have_tmux() {
        return;
    }
    let s = scratch("slot_refusals");
    let blocked = s.new_shell_pane();
    stamp_blocked_claude(&s, &blocked);
    let idle = s.new_shell_pane();
    stamp_blocked_claude(&s, &idle);
    s.set_opt(&idle, "@agent_state", "idle");
    let wrong = s.new_shell_pane();
    stamp_blocked_claude(&s, &wrong);
    // pi has no approve row in any state, so approve on a pi pane is the wrong-agent refusal.
    s.set_opt(&wrong, "@agent_name", "pi");

    let cases: [(&str, Vec<&str>, i32, &str); 5] = [
        ("gated", vec!["approve", "--pane", &idle], 4, "gated"),
        (
            "vanished",
            vec!["approve", "--pane", "%999"],
            3,
            "pane-gone",
        ),
        ("wrong", vec!["approve", "--pane", &wrong], 4, "wrong-agent"),
        (
            "cover",
            vec!["compact", "--pane", &blocked],
            4,
            "no-coverage",
        ),
        (
            "episode",
            vec!["approve", "--pane", &blocked, "--expect-episode-ms", "1"],
            4,
            "episode-changed",
        ),
    ];
    for (slot, args, code, reason) in cases {
        for pane in [&blocked, &idle, &wrong] {
            refresh_stamp(&s, pane);
        }
        let mut first = args.clone();
        first.extend(["--slot", slot, "--json"]);
        let out = act(&s, &first);
        assert_eq!(out.status.code(), Some(code), "{slot}: {}", stderr(&out));
        assert!(
            stdout(&out).contains(&format!(r#""reason":"{reason}""#)),
            "{slot}: expected {reason}, got {}",
            stdout(&out)
        );
        assert!(
            stdout(&out).contains(r#""cached":false"#),
            "{slot} fired once"
        );

        let replay = act(&s, &first);
        assert_eq!(
            replay.status.code(),
            Some(code),
            "{slot}: the replay repeats the exit code: {}",
            stderr(&replay)
        );
        assert!(
            stdout(&replay).contains(r#""cached":true"#)
                && stdout(&replay).contains(&format!(r#""reason":"{reason}""#)),
            "{slot}: the replay must be the same receipt: {}",
            stdout(&replay)
        );
    }
    // No refusal reached the pane, so no refusal left a keystroke on it.
    let screen = capture(&s, &blocked);
    assert!(
        !screen.contains(&format!("{SHELL_PROMPT}1")),
        "a refused dispatch sent keys:\n{screen}"
    );
}

/// Fifty dispatches mixing successes with every refusal class reconcile against the ledger:
/// as many receipts as dispatches, every receipt names a dispatched slot, no slot twice.
#[test]
fn fifty_dispatches_reconcile_against_their_receipts() {
    if !have_tmux() {
        return;
    }
    let s = scratch("slot_reconcile");
    let blocked = s.new_shell_pane();
    stamp_blocked_claude(&s, &blocked);
    let idle = s.new_shell_pane();
    stamp_blocked_claude(&s, &idle);
    s.set_opt(&idle, "@agent_state", "idle");

    let mut dispatched: Vec<String> = Vec::new();
    for i in 0..50u32 {
        let slot = format!("d{i}");
        refresh_stamp(&s, &blocked);
        refresh_stamp(&s, &idle);
        let out = match i % 4 {
            0 => act(&s, &["approve", "--pane", &blocked, "--slot", &slot]),
            1 => act(&s, &["approve", "--pane", &idle, "--slot", &slot]),
            2 => act(&s, &["approve", "--pane", "%999", "--slot", &slot]),
            _ => act(
                &s,
                &[
                    "approve",
                    "--pane",
                    &blocked,
                    "--expect-episode-ms",
                    "1",
                    "--slot",
                    &slot,
                ],
            ),
        };
        assert!(
            out.status.code() != Some(2),
            "{slot} was a usage error: {}",
            stderr(&out)
        );
        dispatched.push(slot);
    }

    let json = stdout(&receipts(&s, &["--json"]));
    let mut written = slots_in(&json);
    assert_eq!(
        written.len(),
        dispatched.len(),
        "one receipt per dispatch: {json}"
    );
    for slot in &written {
        assert!(dispatched.contains(slot), "{slot} was never dispatched");
    }
    written.sort();
    let before = written.len();
    written.dedup();
    assert_eq!(written.len(), before, "a slot cannot carry two receipts");
}

/// Two processes dispatching one slot at the same time: one fires, both get an answer, and
/// one of the answers is the other's receipt. The ledger is a shared file, so this is a
/// cross-process claim and not a cross-thread one.
#[test]
fn two_concurrent_dispatches_on_one_slot_fire_once() {
    if !have_tmux() {
        return;
    }
    let s = scratch("slot_race");
    let pane = s.new_shell_pane();
    stamp_blocked_claude(&s, &pane);

    let spawn = || {
        let mut cmd = act_cmd(&s);
        cmd.args(["approve", "--pane", &pane, "--slot", "race", "--json"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        cmd.spawn().expect("spawn tma act")
    };
    let (a, b) = (spawn(), spawn());
    let outs = [
        a.wait_with_output().expect("first result"),
        b.wait_with_output().expect("second result"),
    ];

    for out in &outs {
        assert_eq!(out.status.code(), Some(0), "{}", stderr(out));
    }
    let cached = outs
        .iter()
        .filter(|o| stdout(o).contains(r#""cached":true"#))
        .count();
    assert_eq!(
        cached,
        1,
        "exactly one of the two answers must be a replay: {} / {}",
        stdout(&outs[0]),
        stdout(&outs[1])
    );
    assert!(wait_capture_contains(
        &s.socket,
        &pane,
        &format!("{SHELL_PROMPT}1"),
        POLL_CEILING
    ));
    assert_one_keystroke(&s, &pane);
}

/// The TTL floor is 24 h: an entry 23 h old still answers for its slot, and one past the
/// floor is claimable again. Nothing evicts on age alone.
#[test]
fn a_receipt_answers_for_a_day_and_not_beyond_it() {
    if !have_tmux() {
        return;
    }
    let s = scratch("slot_ttl");
    let pane = s.new_shell_pane();
    stamp_blocked_claude(&s, &pane);

    assert_eq!(
        act(&s, &["approve", "--pane", &pane, "--slot", "ttl"])
            .status
            .code(),
        Some(0)
    );
    assert!(wait_capture_contains(
        &s.socket,
        &pane,
        &format!("{SHELL_PROMPT}1"),
        POLL_CEILING
    ));

    age_ledger(&ledger_path(&s), 23 * 60 * 60 * 1000);
    let replay = act(&s, &["approve", "--pane", &pane, "--slot", "ttl", "--json"]);
    assert!(
        stdout(&replay).contains(r#""cached":true"#),
        "an entry 23 h old is still cached: {}",
        stdout(&replay)
    );
    assert_one_keystroke(&s, &pane);

    // Two more hours puts it past the floor, and the slot is a fresh dispatch again.
    age_ledger(&ledger_path(&s), 2 * 60 * 60 * 1000);
    refresh_stamp(&s, &pane);
    let refired = act(&s, &["approve", "--pane", &pane, "--slot", "ttl", "--json"]);
    assert!(
        stdout(&refired).contains(r#""cached":false"#),
        "past the TTL the slot dispatches again: {}",
        stdout(&refired)
    );
    assert!(wait_capture_contains(
        &s.socket,
        &pane,
        &format!("{SHELL_PROMPT}11"),
        POLL_CEILING
    ));
}

/// A torn record refuses with the ledger's typed error and dispatches nothing. Reading a
/// truncated tail as "no such slot" is exactly the double-fire the ledger exists to prevent.
#[test]
fn a_torn_ledger_refuses_rather_than_dispatching() {
    if !have_tmux() {
        return;
    }
    let s = scratch("slot_torn");
    let pane = s.new_shell_pane();
    stamp_blocked_claude(&s, &pane);

    assert_eq!(
        act(&s, &["approve", "--pane", &pane, "--slot", "torn"])
            .status
            .code(),
        Some(0)
    );
    assert!(wait_capture_contains(
        &s.socket,
        &pane,
        &format!("{SHELL_PROMPT}1"),
        POLL_CEILING
    ));

    let path = ledger_path(&s);
    let text = std::fs::read_to_string(&path).expect("the ledger exists");
    assert!(text.ends_with('\n'), "records are newline terminated");
    std::fs::write(&path, &text[..text.len() - 8]).expect("truncate mid-record");

    refresh_stamp(&s, &pane);
    let out = act(
        &s,
        &["approve", "--pane", &pane, "--slot", "other", "--json"],
    );
    assert_eq!(out.status.code(), Some(1), "a torn ledger is a failure");
    assert!(
        stderr(&out).contains("torn at line 1"),
        "the error names the record: {}",
        stderr(&out)
    );
    assert!(
        stdout(&out).is_empty(),
        "a refusal to read prints no result"
    );
    assert_one_keystroke(&s, &pane);
    assert_eq!(
        receipts(&s, &["--json"]).status.code(),
        Some(1),
        "the reader refuses the same file"
    );
}

/// The dispatching device is recorded and is not part of the key: another device's
/// dispatch inside the same slot replays the first device's receipt.
#[test]
fn the_device_is_recorded_but_is_not_part_of_the_key() {
    if !have_tmux() {
        return;
    }
    let s = scratch("slot_device");
    let pane = s.new_shell_pane();
    stamp_blocked_claude(&s, &pane);

    let first = act(
        &s,
        &[
            "approve", "--pane", &pane, "--slot", "dev", "--device", "phone-a", "--json",
        ],
    );
    assert_eq!(first.status.code(), Some(0), "{}", stderr(&first));
    assert!(wait_capture_contains(
        &s.socket,
        &pane,
        &format!("{SHELL_PROMPT}1"),
        POLL_CEILING
    ));

    refresh_stamp(&s, &pane);
    let second = act(
        &s,
        &[
            "approve", "--pane", &pane, "--slot", "dev", "--device", "phone-b", "--json",
        ],
    );
    assert!(
        stdout(&second).contains(r#""cached":true"#),
        "device B must replay device A's receipt: {}",
        stdout(&second)
    );
    assert_one_keystroke(&s, &pane);
    let json = stdout(&receipts(&s, &["--slot", "dev", "--json"]));
    assert!(
        json.contains(r#""device":"phone-a""#) && !json.contains("phone-b"),
        "the receipt keeps the device that actually dispatched: {json}"
    );
}

/// `--device` without `--slot` records nothing, and neither flag belongs on a fan-out: both are
/// usage errors at parse time rather than a silently ignored value.
#[test]
fn the_flags_refuse_the_combinations_that_would_mean_nothing() {
    for args in [
        vec!["act", "approve", "--pane", "%1", "--device", "phone"],
        vec!["act", "approve", "--all", "--slot", "s1"],
        vec![
            "act", "approve", "--all", "--slot", "s1", "--device", "phone",
        ],
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_tma"))
            .args(&args)
            .output()
            .expect("tma runs");
        assert_eq!(
            out.status.code(),
            Some(2),
            "{args:?} should be a usage error"
        );
    }
}
