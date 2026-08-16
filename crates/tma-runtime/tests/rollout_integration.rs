//! The Codex rollout pull path end to end on a scratch `tmux -L` server: a synthetic rollout file under a
//! temp `CODEX_HOME` is discovered from the pane's `@agent_session`, tailed, and its newest
//! `token_count` reading stamped onto `@agent_context_pct` (with the best-effort `@agent_model`),
//! read back via `show-options`. A second poll of the unchanged file is skipped by the memo (one
//! stat, no second read) — the "quiet-pane steady state is one stat call" acceptance.

use std::path::Path;

use tma_runtime::manifests;
use tma_runtime::rollout::{poll_context_tails, RolloutTail};
use tma_test_support::{self as common, Scratch};
use tma_tmux::tmux::Tmux;

/// A codex manifest carrying the file-tail context channel, so `poll_context_tails` recognizes the
/// pane as a Codex file-tail agent.
const CODEX_MANIFEST: &str = r#"min_engine_version = "0.1"
[identity]
process_names = ["codex"]
[capture]
visible = []
[telemetry.context]
channel = "file-tail"
format = "codex-rollout-jsonl"
"#;

const SID: &str = "019f99c3-7c57-7963-98e9-f496a7978257";

/// A rollout with two token_count records (the newest is 57%) plus a turn_context carrying the model.
fn rollout_body() -> String {
    [
        r#"{"type":"turn_context","payload":{"cwd":"<CWD>","model":"gpt-5-codex","effort":"high"}}"#,
        r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":88010,"total_tokens":90150},"last_token_usage":{"total_tokens":1},"model_context_window":272000}}}"#,
        r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":151234,"total_tokens":156054},"last_token_usage":{"total_tokens":1},"model_context_window":272000}}}"#,
    ]
    .join("\n")
        + "\n"
}

fn write_rollout(codex_home: &Path) {
    let day = codex_home.join("sessions/2026/07/28");
    std::fs::create_dir_all(&day).unwrap();
    let file = day.join(format!("rollout-2026-07-28T18-03-01-{SID}.jsonl"));
    std::fs::write(file, rollout_body()).unwrap();
}

#[test]
fn codex_rollout_tail_stamps_gauge_and_model_and_memo_skips_unchanged() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("rollout");
    s.write_manifest("codex.toml", CODEX_MANIFEST);
    let codex_home = s.workdir.join("codex");
    write_rollout(&codex_home);

    let pane = s.new_pane();
    // The hook channel registers the owning session id; the file-tail path keys discovery off it.
    s.set_opt(&pane, "@agent_name", "codex");
    s.set_opt(&pane, "@agent_session", SID);

    let manifests = manifests::load(Some(&s.manifest_dir()), &[])
        .expect("load manifests")
        .manifests;
    let tmux = Tmux::new(Some(s.socket.clone()));
    let now = tma_runtime::now_ms();

    let panes = tmux.list_panes().unwrap();
    let mut tail = RolloutTail::new();
    poll_context_tails(&tmux, &panes, &manifests, &mut tail, &codex_home, now);

    // The newest record (156054 / 272000 => 57%) is stamped, and the model label is read.
    assert_eq!(
        s.pane_option(&pane, "@agent_context_pct"),
        "57",
        "the newest token_count record stamps the gauge"
    );
    assert_ne!(
        s.pane_option(&pane, "@agent_context_at"),
        "",
        "the marker is written"
    );
    assert_eq!(
        s.pane_option(&pane, "@agent_model"),
        "gpt-5-codex",
        "the best-effort model label is stamped"
    );
    assert_eq!(tail.read_calls(), 1, "one read on the first poll");
    assert_eq!(tail.discover_calls(), 1, "one discovery walk");

    // A second cycle over the unchanged file: the memo skips the read (one more stat, no more reads).
    let panes = tmux.list_panes().unwrap();
    poll_context_tails(&tmux, &panes, &manifests, &mut tail, &codex_home, now + 1);
    assert_eq!(
        tail.read_calls(),
        1,
        "an unchanged rollout is not re-read (memo skip)"
    );
    assert_eq!(tail.stat_calls(), 2, "steady state is one stat per cycle");
    assert_eq!(tail.discover_calls(), 1, "the path cache avoids re-walking");
}
