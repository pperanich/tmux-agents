//! A-114: the host's vocabularies against `tma-proto`'s.
//!
//! The wire copies four token sets and one key set out of the host. Exhaustive tests inside
//! `tma-proto` cannot notice a token the *host* grew, which is exactly the blind spot: a new
//! `RefusalReason` would ship with no proto arm and nothing would fail. These assertions run on the
//! host side, where both halves are in scope, in the shape the host already uses for its own guards.
//!
//! This is a pure data check with no tmux and no spawned binary.

use std::collections::BTreeSet;
use std::path::Path;

use tma_core::{AgentState, Detail, RefusalReason, TextRefusal};
use tma_runtime::broker::{Gone, Outcome, Refusal};
use tma_runtime::json::JsonWriter;
use tma_runtime::origin::Origin;
use tma_runtime::slots::FIRED_UNKNOWN;
use tma_ui::surfaces::{write_row_fields, RowSurface};

fn set<'a>(tokens: impl IntoIterator<Item = &'a str>) -> BTreeSet<String> {
    tokens.into_iter().map(str::to_string).collect()
}

/// Every `outcome` token the host can print. The match is what makes this exhaustive: a new
/// [`Outcome`] variant fails to compile here rather than shipping with no proto arm.
fn host_outcome_tokens() -> BTreeSet<String> {
    let every = [
        Outcome::Sent,
        Outcome::Replied,
        Outcome::Exited(0),
        Outcome::Spawned,
        Outcome::Timeout,
        Outcome::Refused(Refusal::Locked),
        Outcome::Vanished(Gone::Pane),
        Outcome::Error(String::new()),
    ];
    for outcome in &every {
        match outcome {
            Outcome::Sent
            | Outcome::Replied
            | Outcome::Exited(_)
            | Outcome::Spawned
            | Outcome::Timeout
            | Outcome::Refused(_)
            | Outcome::Vanished(_)
            | Outcome::Error(_) => {}
        }
    }
    set(every.iter().map(Outcome::token))
}

/// Every `reason` token the host can print: the gate's four, the broker's own verdicts, the two
/// vanished targets, and the ledger's indeterminate dispatch.
fn host_reason_tokens() -> BTreeSet<String> {
    let gate = [
        RefusalReason::WrongAgent,
        RefusalReason::NoCoverage,
        RefusalReason::RequiresUnmet,
        RefusalReason::Gated,
    ];
    for reason in gate {
        match reason {
            RefusalReason::WrongAgent
            | RefusalReason::NoCoverage
            | RefusalReason::RequiresUnmet
            | RefusalReason::Gated => {}
        }
    }
    let payload = [
        TextRefusal::Empty,
        TextRefusal::TooLong,
        TextRefusal::ControlBytes,
        TextRefusal::Sigil,
    ];
    for refusal in payload {
        match refusal {
            TextRefusal::Empty
            | TextRefusal::TooLong
            | TextRefusal::ControlBytes
            | TextRefusal::Sigil => {}
        }
    }
    let broker = [
        Refusal::Locked,
        Refusal::EpisodeChanged,
        Refusal::RequestGone,
    ];
    for refusal in broker {
        match refusal {
            Refusal::Gate(_)
            | Refusal::Locked
            | Refusal::EpisodeChanged
            | Refusal::RequestGone
            | Refusal::Payload(_) => {}
        }
    }
    let gone = [Gone::Pane, Gone::Request];
    for target in gone {
        match target {
            Gone::Pane | Gone::Request => {}
        }
    }

    let mut tokens = set(gate.iter().copied().map(RefusalReason::token));
    tokens.extend(set(payload
        .iter()
        .copied()
        .map(|r| Refusal::Payload(r).token())));
    tokens.extend(set(broker.iter().copied().map(Refusal::token)));
    tokens.extend(set(gone.iter().copied().map(Gone::token)));
    tokens.insert(FIRED_UNKNOWN.to_string());
    tokens
}

/// A-114: the proto's `outcome` set equals the host's. Not a superset: a token the device can
/// receive but the host cannot send is a lie about what the wire carries.
#[test]
fn the_proto_outcome_vocabulary_equals_the_hosts() {
    assert_eq!(
        set(tma_proto::Outcome::TOKENS.iter().copied()),
        host_outcome_tokens()
    );
}

/// A-114: the proto's `reason` set is a superset of the host's, and the excess is named.
///
/// The excess is deliberate and short: `scope-denied` is the remote-authorization refusal, which
/// only a serve loop can produce and no host code path emits yet. Anything else appearing here is a
/// token this crate invented, which is what the assertion is for.
#[test]
fn the_proto_reason_vocabulary_covers_the_hosts() {
    let proto = set(tma_proto::Reason::TOKENS.iter().copied());
    let host = host_reason_tokens();

    let missing: Vec<_> = host.difference(&proto).collect();
    assert!(
        missing.is_empty(),
        "the host can print reasons the wire has no arm for: {missing:?}"
    );
    assert_eq!(
        proto.difference(&host).collect::<Vec<_>>(),
        vec!["scope-denied"],
        "the wire carries a reason no host path produces, and it is not the one that is expected to"
    );
}

/// A-114 for the state pair: the closed vocabulary matches exactly.
#[test]
fn the_proto_state_vocabulary_equals_the_hosts() {
    let every = [
        AgentState::Idle,
        AgentState::Working,
        AgentState::Blocked,
        AgentState::Unknown,
    ];
    for state in every {
        match state {
            AgentState::Idle | AgentState::Working | AgentState::Blocked | AgentState::Unknown => {}
        }
    }
    assert_eq!(
        set(tma_proto::State::TOKENS.iter().copied()),
        set(every.iter().copied().map(AgentState::token))
    );
}

/// A-114 for the detail tokens, which are associated constants rather than variants, so an
/// exhaustive match cannot reach them. The host's own source is the inventory instead: a constant
/// added to `tma_core::Detail` with no proto arm fails here.
#[test]
fn the_proto_detail_vocabulary_equals_the_hosts() {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../tma-core/src/state.rs");
    let text = std::fs::read_to_string(&source).expect("tma-core's state module is readable");
    let declared: BTreeSet<String> = text
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            // `pub const PERMISSION: &'static str = "permission";`
            let rest = line.strip_prefix("pub const ")?;
            let value = rest.split_once("= \"")?.1;
            Some(value.strip_suffix("\";")?.to_string())
        })
        .collect();

    assert!(
        declared.contains(Detail::PERMISSION) && declared.len() >= 8,
        "the scan of {} stopped matching: {declared:?}",
        source.display()
    );
    assert_eq!(
        set(tma_proto::Detail::TOKENS.iter().copied()),
        declared,
        "the host's detail vocabulary and the wire's known arms have diverged"
    );
}

/// The transcript event labels the wire mirrors, against the reader's own.
#[test]
fn the_proto_event_labels_equal_the_readers() {
    use tma_transcript::{EventKind, ResultStatus, SessionMeta, TurnKind};

    let every = [
        EventKind::SessionMeta(SessionMeta::default()),
        EventKind::UserMessage {
            bytes: 0,
            attachments: 0,
        },
        EventKind::AssistantText { bytes: 0 },
        EventKind::Thinking {
            bytes: 0,
            redacted: false,
        },
        EventKind::ToolCall {
            name: String::new(),
            call_id: None,
            arg_keys: Vec::new(),
            bytes: 0,
        },
        EventKind::ToolResult {
            call_id: None,
            status: ResultStatus::Ok,
            bytes: 0,
        },
        EventKind::PermissionRequest {
            tool: String::new(),
            call_id: None,
        },
        EventKind::TurnBoundary {
            kind: TurnKind::Start,
            reason: None,
        },
        EventKind::Usage {
            input: None,
            output: None,
            total: None,
            context_window: None,
            cost_usd: None,
        },
        EventKind::SubagentRef {
            child_id: String::new(),
            external_file: false,
        },
        EventKind::Compaction {
            kind: String::new(),
        },
        EventKind::Attachment {
            kind: String::new(),
            bytes: 0,
        },
        EventKind::Bookkeeping {
            type_name: String::new(),
        },
        EventKind::Unknown {
            type_name: String::new(),
        },
    ];
    let reader = set(every.iter().map(EventKind::label));

    // The proto's labels come out of its own serialization, so this compares the wire rather than a
    // table beside it.
    let wire: BTreeSet<String> = proto_event_labels();
    assert_eq!(
        wire, reader,
        "an event kind is labelled differently on the wire"
    );

    assert_eq!(
        set([
            ResultStatus::Ok,
            ResultStatus::Error,
            ResultStatus::Pending,
            ResultStatus::Unknown
        ]
        .iter()
        .copied()
        .map(ResultStatus::as_str)),
        set(tma_proto::ResultStatus::TOKENS.iter().copied())
    );
    assert_eq!(
        set([TurnKind::Start, TurnKind::End]
            .iter()
            .copied()
            .map(TurnKind::as_str)),
        set(tma_proto::TurnKind::TOKENS.iter().copied())
    );
}

/// Serialize one of each proto event kind and read the `kind` token back out.
fn proto_event_labels() -> BTreeSet<String> {
    use tma_proto::EventKind as P;

    let every = [
        P::SessionMeta(tma_proto::SessionMeta::default()),
        P::UserMessage {
            bytes: 0,
            attachments: 0,
        },
        P::AssistantText { bytes: 0 },
        P::Thinking {
            bytes: 0,
            redacted: false,
        },
        P::ToolCall {
            name: String::new(),
            call_id: None,
            arg_keys: Vec::new(),
            bytes: 0,
        },
        P::ToolResult {
            call_id: None,
            status: tma_proto::ResultStatus::Ok,
            bytes: 0,
        },
        P::PermissionRequest {
            tool: String::new(),
            call_id: None,
        },
        P::TurnBoundary {
            boundary: tma_proto::TurnKind::Start,
            reason: None,
        },
        P::Usage {
            input: None,
            output: None,
            total: None,
            context_window: None,
            cost_usd: None,
        },
        P::SubagentRef {
            child_id: String::new(),
            external_file: false,
        },
        P::Compaction {
            compaction: String::new(),
        },
        P::Attachment {
            attachment: String::new(),
            bytes: 0,
        },
        P::Bookkeeping {
            type_name: String::new(),
        },
        P::Unknown {
            type_name: String::new(),
        },
    ];
    every
        .iter()
        .map(|kind| {
            let line = tma_proto::encode(kind).expect("serialize");
            let value: serde_json::Value = serde_json::from_str(&line).expect("parse");
            value["kind"].as_str().expect("a kind token").to_string()
        })
        .collect()
}

/// The snapshot row's key set is the host's protocol row's, exactly.
///
/// The strongest of these guards: it compares the wire this crate emits against the writer that
/// actually serves it, so a key added to the host row without a proto field, or the reverse, fails
/// here. `title` is the key the protocol surface drops, and its absence is asserted from both ends.
#[test]
fn the_proto_row_key_set_equals_the_host_protocol_rows() {
    let host = host_protocol_row_keys();
    let proto = proto_row_keys();

    assert!(
        !host.contains("title") && !proto.contains("title"),
        "a pane title is agent-supplied text and rides no frame leaving the machine"
    );
    assert_eq!(
        proto, host,
        "tma_proto::FleetRow and RowSurface::Protocol disagree on the row's keys"
    );
}

fn proto_row_keys() -> BTreeSet<String> {
    let row = tma_proto::FleetRow {
        pane: "%1".to_string(),
        agent: "claude".to_string(),
        state: tma_proto::State::Blocked,
        detail: Some(tma_proto::Detail::Permission),
        since: 1,
        since_ms: 1000,
        episode_ms: 1000,
        locator: "s:1.0".to_string(),
        attention: true,
        done: false,
        session: Some(String::new()),
        transcript: Some(String::new()),
        permission_request: Some(String::new()),
        stamped_at_ms: Some(0),
        context: Some(0),
        context_at_ms: Some(0),
        muted: false,
        tokens: Some(0),
        quota: Some(tma_proto::Quota {
            pct: 0,
            window: String::new(),
            resets_at_ms: Some(0),
        }),
        cost_usd: Some(0.0),
        repo: Some(String::new()),
        branch: Some(String::new()),
        worktree: Some(false),
        pending_tool: Some(String::new()),
        pending_call: Some(String::new()),
        pending_summary: Some(String::new()),
        server: String::new(),
        host: String::new(),
    };
    json_keys(&tma_proto::encode(&row).expect("serialize"))
}

fn host_protocol_row_keys() -> BTreeSet<String> {
    use tma_core::{AgentRow, PendingCall, QuotaLabel, RepoLabel};

    // Every optional populated, so a key cannot be missing because its value was absent.
    let row = AgentRow {
        pane_id: "%1".to_string(),
        agent: "claude".to_string(),
        state: AgentState::Blocked,
        detail: Some(Detail::PERMISSION.to_string()),
        since: 1,
        turn_at: 1,
        session: "s".to_string(),
        window_index: 1,
        pane_index: 0,
        title: "SENTINEL".to_string(),
        attention: true,
        agent_session: Some(String::new()),
        transcript: Some(String::new()),
        permission_request: Some(String::new()),
        stamped_at: Some(0),
        context_pct: Some(0),
        context_at: Some(0),
        tokens: Some(0),
        quota: Some(QuotaLabel {
            pct: 0,
            window: String::new(),
            resets_at_ms: Some(0),
        }),
        cost_usd: Some(0.0),
        muted: false,
        model: Some(String::new()),
        cwd: Some(String::new()),
        repo: Some(RepoLabel {
            name: String::new(),
            branch: String::new(),
            worktree: false,
        }),
        pending: Some(PendingCall {
            tool: String::new(),
            call: String::new(),
            summary: String::new(),
        }),
    };

    let mut j = JsonWriter::new();
    j.begin_object();
    write_row_fields(
        &mut j,
        &row,
        &Origin {
            server: String::new(),
            host: String::new(),
        },
        RowSurface::Protocol,
    );
    j.end_object();
    json_keys(&j.finish())
}

/// The object keys of a JSON document, at every depth. Parsed rather than scanned, because a value
/// containing a literal `"title":` would put those bytes in the output whatever the writer did.
fn json_keys(json: &str) -> BTreeSet<String> {
    fn walk(value: &serde_json::Value, keys: &mut BTreeSet<String>) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, child) in map {
                    keys.insert(key.clone());
                    walk(child, keys);
                }
            }
            serde_json::Value::Array(items) => items.iter().for_each(|i| walk(i, keys)),
            _ => {}
        }
    }
    let value: serde_json::Value = serde_json::from_str(json).expect("a row is JSON");
    let mut keys = BTreeSet::new();
    walk(&value, &mut keys);
    keys
}
