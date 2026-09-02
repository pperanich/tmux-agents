//! Context telemetry end to end on an isolated scratch `tmux -L` server: a statusline-shaped
//! payload driven through the `tma event context` intake stamps `@agent_context_pct`, read back via
//! `ls --json`; a wrong-session payload is dropped by the ownership filter; and an out-of-order
//! observation is rejected by the evidence-time write guard (driven at the adapter for explicit times).

use std::process::Output;

use common::Scratch;
use tma_test_support as common;

use tma_tmux::stamp;
use tma_tmux::tmux::Tmux;

const OWNER: &str = "3f1c8d2e-9a44-4b17-9c0e-2b6a1d7e4f88";
const SUBAGENT: &str = "0b03d2a0-d44c-4c51-8de3-57f2c043e737";

/// A custom hook+telemetry manifest: a `Boot` lifecycle registers the pane (recording the owner
/// session), and a `[telemetry.context]` block declaring `format`. `process_names` matches the pane's
/// `sleep` so the `ls` identity walk keeps the registration.
fn faux_manifest(format: &str) -> String {
    FAUX_MANIFEST.replace("{FORMAT}", format)
}

const FAUX_MANIFEST: &str = r#"min_engine_version = "0.1"

[identity]
process_names = ["sleep"]

[hooks]
covers = ["idle", "lifecycle"]

[[hooks.map]]
event = "Boot"
claim = { lifecycle = "start" }

[capture]

[telemetry.context]
channel = "event"
format = "{FORMAT}"
"#;

fn scratch() -> Scratch {
    let s = Scratch::new("context");
    s.write_manifest("faux.toml", &faux_manifest("claude-statusline-json"));
    s
}

/// The same scratch on the Cursor parser: a channel that reports an absolute token count.
fn cursor_scratch() -> Scratch {
    let s = Scratch::new("context-tokens");
    s.write_manifest("faux.toml", &faux_manifest("cursor-statusline-json"));
    s
}

/// Fire the shared `tma event` intake for this suite's agent (`faux`). See [`Scratch::event`].
fn event(s: &Scratch, kind: &str, pane: &str, payload: &str) -> Output {
    s.event("faux", kind, pane, payload)
}

/// A Claude statusline-shaped payload with `session_id` and `used_percentage`.
fn statusline(session: &str, pct: u32) -> String {
    format!(
        r#"{{"session_id":"{session}","context_window":{{"used_percentage":{pct},"context_window_size":200000}}}}"#
    )
}

/// The same payload carrying the `version` and `total_input_tokens` the count gate reads.
fn statusline_versioned(session: &str, pct: u32, version: &str, tokens: u64) -> String {
    format!(
        r#"{{"session_id":"{session}","version":"{version}","context_window":{{"used_percentage":{pct},"total_input_tokens":{tokens},"context_window_size":200000}}}}"#
    )
}

#[test]
fn context_stamps_through_intake_and_reads_back_via_ls_json() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = scratch();
    let pane = s.new_pane();

    // Register the pane (owner session recorded), then push a statusline reading for that owner.
    let boot = event(&s, "Boot", &pane, &format!(r#"{{"session_id":"{OWNER}"}}"#));
    assert!(boot.status.success());
    assert_eq!(s.get(&pane, "#{@agent_session}"), OWNER);

    let out = event(&s, "context", &pane, &statusline(OWNER, 78));
    assert!(out.status.success(), "context intake must exit 0");
    assert_eq!(
        s.get(&pane, "#{@agent_context_pct}"),
        "78",
        "the owner's reading is stamped"
    );
    assert_ne!(
        s.get(&pane, "#{@agent_context_at}"),
        "",
        "the marker is written"
    );

    // The additive `context` key rides `ls --json`.
    let json = s.ls_json();
    assert!(
        json.contains("\"context\":78"),
        "ls --json exposes the gauge: {json}"
    );
    assert!(
        json.contains("\"session\":"),
        "ls --json exposes the owning session: {json}"
    );
}

#[test]
fn a_footprint_channel_stamps_the_absolute_count_beside_the_gauge() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = cursor_scratch();
    let pane = s.new_pane();
    s.event(
        "faux",
        "Boot",
        &pane,
        &format!(r#"{{"session_id":"{OWNER}"}}"#),
    );

    // Cursor's payload carries the numerator of its own gauge: 130000 / 200000 = 65%.
    let payload = format!(
        r#"{{"session_id":"{OWNER}","context_window":{{"total_input_tokens":130000,"context_window_size":200000}}}}"#
    );
    let out = s.event("faux", "context", &pane, &payload);
    assert!(out.status.success(), "context intake must exit 0");
    assert_eq!(s.get(&pane, "#{@agent_context_pct}"), "65");
    assert_eq!(
        s.get(&pane, "#{@agent_tokens}"),
        "130000",
        "the absolute rides the same intake as the gauge"
    );
    assert_eq!(
        s.get(&pane, "#{@agent_tokens_at}"),
        s.get(&pane, "#{@agent_context_at}"),
        "one observation, one evidence time"
    );

    let json = s.ls_json();
    assert!(
        json.contains("\"tokens\":130000"),
        "ls --json exposes the count: {json}"
    );
}

#[test]
fn the_claude_count_rides_only_a_post_fix_payload() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = scratch();
    let pane = s.new_pane();
    s.event(
        "faux",
        "Boot",
        &pane,
        &format!(r#"{{"session_id":"{OWNER}"}}"#),
    );

    // A payload with no version (and no absolute): the gauge alone.
    s.event("faux", "context", &pane, &statusline(OWNER, 78));
    assert_eq!(s.get(&pane, "#{@agent_context_pct}"), "78");
    assert_eq!(s.get(&pane, "#{@agent_tokens}"), "");
    assert!(
        s.ls_json().contains("\"tokens\":null"),
        "the JSON row nulls the count rather than dropping the key"
    );

    // Pre-2.1.132: the percent still reads plausible, but `total_input_tokens` is the corrupt
    // cumulative field, so no count is stamped.
    s.event(
        "faux",
        "context",
        &pane,
        &statusline_versioned(OWNER, 41, "2.1.131", 82_000),
    );
    assert_eq!(s.get(&pane, "#{@agent_context_pct}"), "41");
    assert_eq!(s.get(&pane, "#{@agent_tokens}"), "");

    // From the fix on, the count rides beside the gauge.
    s.event(
        "faux",
        "context",
        &pane,
        &statusline_versioned(OWNER, 55, "2.1.212", 110_000),
    );
    assert_eq!(s.get(&pane, "#{@agent_context_pct}"), "55");
    assert_eq!(s.get(&pane, "#{@agent_tokens}"), "110000");
    assert!(s.ls_json().contains("\"tokens\":110000"));

    // Downgrading (or any payload the gate rejects) clears the stored count instead of leaving a
    // stale number beside a fresh gauge.
    s.event(
        "faux",
        "context",
        &pane,
        &statusline_versioned(OWNER, 60, "2.1.131", 120_000),
    );
    assert_eq!(s.get(&pane, "#{@agent_context_pct}"), "60");
    assert_eq!(s.get(&pane, "#{@agent_tokens}"), "");
}

#[test]
fn wrong_session_context_stamp_is_rejected() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = scratch();
    let pane = s.new_pane();
    event(&s, "Boot", &pane, &format!(r#"{{"session_id":"{OWNER}"}}"#));

    // The owner stamps 78, then a subagent (foreign session) tries to report its own 15.
    event(&s, "context", &pane, &statusline(OWNER, 78));
    assert_eq!(s.get(&pane, "#{@agent_context_pct}"), "78");
    event(&s, "context", &pane, &statusline(SUBAGENT, 15));
    assert_eq!(
        s.get(&pane, "#{@agent_context_pct}"),
        "78",
        "a subagent's own session must not clobber the owner's gauge"
    );
}

#[test]
fn out_of_order_context_stamp_is_rejected() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = scratch();
    let pane = s.new_pane();
    let tmux = Tmux::new(Some(s.socket.clone()));

    let panes = tmux.list_panes().unwrap();
    let guarded = stamp::guarded_writes_supported(&tmux, &panes);
    assert!(guarded, "the scratch server supports -F conditional writes");

    // A newer observation (evidence 200, 55%) lands first.
    stamp::apply_context(&tmux, &panes, &pane, Some(55), Some(110_000), 200, guarded).unwrap();
    assert_eq!(s.get(&pane, "#{@agent_context_pct}"), "55");
    assert_eq!(s.get(&pane, "#{@agent_context_at}"), "200");
    assert_eq!(s.get(&pane, "#{@agent_tokens}"), "110000");
    assert_eq!(s.get(&pane, "#{@agent_tokens_at}"), "200");

    // A reordered older observation (evidence 100, 40%) must be suppressed by the `not older` guard.
    let panes = tmux.list_panes().unwrap();
    stamp::apply_context(&tmux, &panes, &pane, Some(40), Some(80_000), 100, guarded).unwrap();
    assert_eq!(
        s.get(&pane, "#{@agent_context_pct}"),
        "55",
        "the stale reading must not walk the gauge backward"
    );
    assert_eq!(s.get(&pane, "#{@agent_context_at}"), "200");
    assert_eq!(
        s.get(&pane, "#{@agent_tokens}"),
        "110000",
        "the count rides the same guard as the gauge"
    );

    // An equal-or-newer observation (evidence 200, 60%) is accepted (`not older`).
    let panes = tmux.list_panes().unwrap();
    stamp::apply_context(&tmux, &panes, &pane, Some(60), None, 200, guarded).unwrap();
    assert_eq!(s.get(&pane, "#{@agent_context_pct}"), "60");
    assert_eq!(
        s.get(&pane, "#{@agent_tokens}"),
        "",
        "an accepted percent-only observation clears the previous count"
    );
}

/// A statusline payload carrying both blocks: the quota trio and the cost land on their own lane
/// while the context gauge lands on its own, and each field comes from its own object.
#[test]
fn the_quota_and_cost_stamp_beside_the_gauge_and_reach_ls_json() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = scratch();
    let pane = s.new_pane();
    s.event(
        "faux",
        "Boot",
        &pane,
        &format!(r#"{{"session_id":"{OWNER}"}}"#),
    );

    // `used_percentage` appears in four objects here. The gauge must be `context_window`'s 8, and
    // the quota the highest rate-limit window (seven_day at 41), not whichever came first.
    let payload = format!(
        r#"{{"session_id":"{OWNER}","version":"2.1.251","rate_limits":{{"five_hour":{{"used_percentage":23.5,"resets_at":1788425600}},"seven_day":{{"used_percentage":41.2,"resets_at":1788857600}}}},"cost":{{"total_cost_usd":3.4972,"total_duration_ms":45000}},"model":{{"id":"claude-opus-5","display_name":"Opus"}},"context_window":{{"used_percentage":8,"total_input_tokens":15500,"context_window_size":200000}}}}"#
    );
    let out = s.event("faux", "context", &pane, &payload);
    assert!(out.status.success(), "context intake must exit 0");

    assert_eq!(s.get(&pane, "#{@agent_context_pct}"), "8");
    assert_eq!(s.get(&pane, "#{@agent_quota_pct}"), "41");
    assert_eq!(s.get(&pane, "#{@agent_quota_window}"), "7d");
    // The unit trap: `resets_at` is epoch SECONDS on the wire and epoch ms in the store.
    assert_eq!(
        s.get(&pane, "#{@agent_quota_resets_at}"),
        "1788857600000",
        "the seconds the vendor sent are stamped as ms"
    );
    assert_eq!(s.get(&pane, "#{@agent_cost_usd}"), "3.50");
    assert_ne!(
        s.get(&pane, "#{@agent_quota_at}"),
        "",
        "the marker is written"
    );
    // The statusline's `model` is an object, so this is a label the registration path cannot reach.
    assert_eq!(s.get(&pane, "#{@agent_model}"), "claude-opus-5");

    let json = s.ls_json();
    assert!(
        json.contains(r#""quota":{"pct":41,"window":"7d","resets_at_ms":1788857600000}"#),
        "ls --json carries the quota object: {json}"
    );
    assert!(json.contains(r#""cost_usd":3.50"#), "and the cost: {json}");
    assert!(
        json.contains(r#""context":8"#),
        "the gauge is untouched: {json}"
    );
}

/// A payload with no `rate_limits` block leaves a stored quota standing: the block is absent for
/// API-key auth and before the first API response, so its absence says nothing about the account.
#[test]
fn a_payload_without_rate_limits_leaves_the_stored_quota_alone() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = scratch();
    let pane = s.new_pane();
    s.event(
        "faux",
        "Boot",
        &pane,
        &format!(r#"{{"session_id":"{OWNER}"}}"#),
    );

    let with_quota = format!(
        r#"{{"session_id":"{OWNER}","rate_limits":{{"five_hour":{{"used_percentage":77}}}},"context_window":{{"used_percentage":8}}}}"#
    );
    s.event("faux", "context", &pane, &with_quota);
    assert_eq!(s.get(&pane, "#{@agent_quota_pct}"), "77");

    // A later gauge-only payload moves the gauge and leaves the quota where it was.
    s.event("faux", "context", &pane, &statusline(OWNER, 55));
    assert_eq!(s.get(&pane, "#{@agent_context_pct}"), "55");
    assert_eq!(
        s.get(&pane, "#{@agent_quota_pct}"),
        "77",
        "an absent block is no observation, not a clear"
    );
}

/// A subagent's own statusline must not stamp the parent pane's quota either: the ownership filter
/// covers the whole payload, not just the gauge.
#[test]
fn a_foreign_session_stamps_no_quota() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = scratch();
    let pane = s.new_pane();
    s.event(
        "faux",
        "Boot",
        &pane,
        &format!(r#"{{"session_id":"{OWNER}"}}"#),
    );

    let foreign = format!(
        r#"{{"session_id":"{SUBAGENT}","rate_limits":{{"five_hour":{{"used_percentage":99}}}},"cost":{{"total_cost_usd":9.99}}}}"#
    );
    let out = s.event("faux", "context", &pane, &foreign);
    assert!(out.status.success());
    assert_eq!(s.get(&pane, "#{@agent_quota_pct}"), "");
    assert_eq!(s.get(&pane, "#{@agent_cost_usd}"), "");
}
