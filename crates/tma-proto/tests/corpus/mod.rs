//! The corpus itself: one constructed value per committed vector.
//!
//! Every string leaf is either a closed-vocabulary token or `x`-fill. Vectors are synthesized,
//! never captured, and `tests/vectors.rs` enforces that rather than trusting it.
//!
//! Most files hold one frame. A few hold a JSON array of them, where a vocabulary needs every one
//! of its tokens covered and a frame can only carry one at a time.
//!
//! Regenerate the files after a deliberate wire change:
//!
//! ```text
//! TMA_PROTO_BLESS=1 cargo test -p tma-proto --test vectors
//! ```

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::json;

use tma_proto::*;

/// One committed vector: the file, the bytes it holds, and what it covers.
pub(crate) struct Vector {
    pub file: &'static str,
    /// The pretty form, which is what the file holds: the wire is compact, but key ORDER is what
    /// this corpus pins and both forms carry it identically.
    pub json: String,
    /// The compact wire line, which is what a byte budget measures.
    pub wire: String,
    /// Parse the file back into its own type and re-emit it, for the round-trip leg.
    pub reparse: fn(&str) -> Result<String, String>,
    /// The types and `Enum::Variant` names this vector exercises.
    pub covers: &'static [&'static str],
}

impl Vector {
    fn new<T: Serialize + DeserializeOwned>(
        file: &'static str,
        value: &T,
        covers: &'static [&'static str],
    ) -> Vector {
        Vector {
            file,
            json: pretty(value),
            wire: tma_proto::encode(value).expect("a vector value serializes"),
            reparse: reparse::<T>,
            covers,
        }
    }
}

/// The corpus form: pretty, newline-terminated, so a reviewer can read a diff.
pub(crate) fn pretty<T: Serialize>(value: &T) -> String {
    let mut text = serde_json::to_string_pretty(value).expect("a vector value serializes");
    text.push('\n');
    text
}

fn reparse<T: Serialize + DeserializeOwned>(text: &str) -> Result<String, String> {
    let value: T = serde_json::from_str(text).map_err(|e| e.to_string())?;
    Ok(pretty(&value))
}

const X8: &str = "xxxxxxxx";
const X12: &str = "xxxxxxxxxxxx";
const X16: &str = "xxxxxxxxxxxxxxxx";
const UUID: &str = "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx";

fn binder() -> Binder {
    Binder {
        expect_episode_ms: 1_757_030_400_000,
        expect_permission_request: Some(X12.to_string()),
    }
}

/// A row with every key populated, so nothing is absent by accident.
fn full_row(pane: &str, state: State, detail: Option<Detail>) -> FleetRow {
    FleetRow {
        pane: pane.to_string(),
        agent: "claude".to_string(),
        state,
        detail,
        since: 1_757_030_400,
        since_ms: 1_757_030_400_000,
        episode_ms: 1_757_030_460_000,
        locator: "xxxx:1.0".to_string(),
        attention: true,
        done: false,
        session: Some(UUID.to_string()),
        transcript: Some("/xxxxx/xxxxxxxx/xxxxxxxx".to_string()),
        permission_request: Some(X12.to_string()),
        stamped_at_ms: Some(1_757_030_461_000),
        context: Some(42),
        context_at_ms: Some(1_757_030_461_000),
        muted: false,
        tokens: Some(96_000),
        quota: Some(Quota {
            pct: 61,
            window: "5h".to_string(),
            resets_at_ms: Some(1_757_048_400_000),
        }),
        cost_usd: Some(0.25),
        repo: Some(X8.to_string()),
        branch: Some(X12.to_string()),
        worktree: Some(true),
        pending_tool: Some("xxxx".to_string()),
        pending_call: Some(X12.to_string()),
        pending_summary: Some(X16.to_string()),
        server: "/xxx/xxxx-501/xxxxxxx".to_string(),
        host: X8.to_string(),
    }
}

/// A row with every optional key null, which is the shape a screen-only pane produces.
fn bare_row(pane: &str, state: State) -> FleetRow {
    FleetRow {
        detail: None,
        session: None,
        transcript: None,
        permission_request: None,
        stamped_at_ms: None,
        context: None,
        context_at_ms: None,
        tokens: None,
        quota: None,
        cost_usd: None,
        repo: None,
        branch: None,
        worktree: None,
        pending_tool: None,
        pending_call: None,
        pending_summary: None,
        ..full_row(pane, state, None)
    }
}

fn receipt(slot: &str, outcome: Outcome, reason: Option<Reason>, exit_code: i32) -> Receipt {
    Receipt {
        slot: slot.to_string(),
        pane: "%1".to_string(),
        action: "approve".to_string(),
        outcome,
        reason,
        exit_code,
        cached: false,
        device: Some(X16.to_string()),
        at_ms: 1_757_030_462_000,
    }
}

fn permission_card(lane: Lane, extraction: Extraction, options: Vec<PermissionOption>) -> Card {
    Card::Permission(PermissionCard {
        pane: "%1".to_string(),
        agent: "claude".to_string(),
        lane,
        options,
        extraction,
        pending_call: Some(PendingCall {
            tool: "xxxx".to_string(),
            input: Some(json!({ "xxxxxxx": X16 })),
            call_id: Some(X12.to_string()),
        }),
        binder: binder(),
    })
}

fn option(kind: OptionKind, name: &str, option_id: Option<&str>) -> PermissionOption {
    PermissionOption {
        kind,
        name: name.to_string(),
        option_id: option_id.map(str::to_string),
    }
}

fn header(cursor: &str, kind: EventKind, preview: Option<&str>) -> EventHeader {
    EventHeader {
        cursor: Cursor(cursor.to_string()),
        kind,
        ts: Some("2026-09-05T00:00:00.000Z".to_string()),
        preview: preview.map(str::to_string),
        body: None,
    }
}

/// The `MAXIMAL` notification payload: the longest value this crate will carry per field, pinned so
/// the byte bound is measured against a committed file rather than against whatever the corpus
/// holds the week the test runs. Regenerate it deliberately and say so in the commit message.
fn maximal_notify() -> NotifyPayload {
    NotifyPayload {
        // A hostname's own 255-byte ceiling, and the longest agent name plus room to grow.
        host: "x".repeat(255),
        pane: "%999999999".to_string(),
        agent: "x".repeat(64),
        state: State::Blocked,
        episode_ms: u64::MAX,
    }
}

pub(crate) fn corpus() -> Vec<Vector> {
    vec![
        Vector::new(
            "hello-request.json",
            &RequestFrame::new(
                "1",
                Request::Hello(Hello {
                    app: X8.to_string(),
                    app_version: "0.1.0".to_string(),
                    device: X16.to_string(),
                }),
            ),
            &["RequestFrame", "Request::Hello", "Hello"],
        ),
        Vector::new(
            "hello-response.json",
            &ResponseFrame::new(
                "1",
                Response::Hello(HelloOk {
                    host: X8.to_string(),
                    tma_version: "0.5.13".to_string(),
                    reconcile_interval_ms: 5_000,
                    scopes: vec![
                        Scope::Read,
                        Scope::ActAnswer,
                        Scope::ActAlways,
                        Scope::ActSteer,
                    ],
                }),
            ),
            &[
                "ResponseFrame",
                "Response::Hello",
                "HelloOk",
                "Scope::Read",
                "Scope::ActAnswer",
                "Scope::ActAlways",
                "Scope::ActSteer",
            ],
        ),
        Vector::new(
            "errors.json",
            &vec![
                ResponseFrame::new(
                    "1",
                    Response::Error(ErrorFrame::new(
                        ErrorCode::UnsupportedSchema,
                        "this host speaks protocol schema 1; the client asked for 2",
                    )),
                ),
                ResponseFrame::new(
                    "2",
                    Response::Error(ErrorFrame::new(
                        ErrorCode::BadRequest,
                        "a keys action takes no text payload",
                    )),
                ),
                ResponseFrame::new(
                    "3",
                    Response::Error(ErrorFrame::new(ErrorCode::NotFound, "no such pane")),
                ),
                ResponseFrame::new(
                    "4",
                    Response::Error(ErrorFrame::new(
                        ErrorCode::CursorInvalid,
                        "this cursor no longer addresses the file (it was rewritten, truncated or replaced)",
                    )),
                ),
                ResponseFrame::new(
                    "5",
                    Response::Error(ErrorFrame::new(
                        ErrorCode::ScopeDenied,
                        "this device is not granted act:steer",
                    )),
                ),
                ResponseFrame::new(
                    "6",
                    Response::Error(ErrorFrame::new(
                        ErrorCode::Unsupported,
                        "opencode keeps its transcript in SQLite; that reader is a separate workstream",
                    )),
                ),
                ResponseFrame::new(
                    "7",
                    Response::Error(ErrorFrame::new(ErrorCode::Internal, "the host failed")),
                ),
                ResponseFrame::new(
                    "8",
                    Response::Error(ErrorFrame::new(
                        ErrorCode::Other(X12.to_string()),
                        "a code this build has never heard of, kept verbatim",
                    )),
                ),
                ResponseFrame::new(
                    "8",
                    Response::Error(ErrorFrame::new(
                        ErrorCode::TooManyConnections,
                        "this host already has 4 serve connections open",
                    )),
                ),
            ],
            &[
                "Response::Error",
                "ErrorFrame",
                "ErrorCode::UnsupportedSchema",
                "ErrorCode::BadRequest",
                "ErrorCode::NotFound",
                "ErrorCode::CursorInvalid",
                "ErrorCode::ScopeDenied",
                "ErrorCode::TooManyConnections",
                "ErrorCode::Unsupported",
                "ErrorCode::Internal",
                "ErrorCode::Other",
            ],
        ),
        Vector::new(
            "snapshot.json",
            &ResponseFrame::new(
                "2",
                Response::Snapshot(Snapshot {
                    agents: vec![
                        full_row("%1", State::Blocked, Some(Detail::Permission)),
                        full_row("%2", State::Blocked, Some(Detail::Plan)),
                        full_row("%3", State::Blocked, Some(Detail::Trust)),
                        full_row("%4", State::Blocked, Some(Detail::Question)),
                        full_row("%5", State::Working, Some(Detail::Compacting)),
                        full_row("%6", State::Working, Some(Detail::Background)),
                        full_row("%7", State::Idle, Some(Detail::Error)),
                        full_row("%8", State::Idle, Some(Detail::RateLimit)),
                        // A detail token minted after this build: kept, not collapsed.
                        full_row("%9", State::Blocked, Some(Detail::Other(X12.to_string()))),
                        bare_row("%10", State::Unknown),
                    ],
                }),
            ),
            &[
                "Response::Snapshot",
                "Snapshot",
                "FleetRow",
                "Quota",
                "State::Idle",
                "State::Working",
                "State::Blocked",
                "State::Unknown",
                "Detail::Permission",
                "Detail::Plan",
                "Detail::Trust",
                "Detail::Question",
                "Detail::Error",
                "Detail::RateLimit",
                "Detail::Background",
                "Detail::Compacting",
                "Detail::Other",
            ],
        ),
        Vector::new(
            "snapshot-request.json",
            &RequestFrame::new(
                "2",
                Request::Snapshot(SnapshotRequest {
                    selector: Some(Selector {
                        session: Some(X8.to_string()),
                        repo: Some(X8.to_string()),
                        branch: Some(X12.to_string()),
                        agent: Some("claude".to_string()),
                        state: vec![
                            StateFilter::Idle,
                            StateFilter::Working,
                            StateFilter::Blocked,
                            StateFilter::Unknown,
                            StateFilter::Done,
                        ],
                    }),
                }),
            ),
            &[
                "Request::Snapshot",
                "SnapshotRequest",
                "Selector",
                "StateFilter::Idle",
                "StateFilter::Working",
                "StateFilter::Blocked",
                "StateFilter::Unknown",
                "StateFilter::Done",
            ],
        ),
        Vector::new(
            "edge.json",
            &vec![
                ResponseFrame::new(
                    "3",
                    Response::Edge(Edge {
                        at_ms: 1_757_030_462_000,
                        pane: "%1".to_string(),
                        agent: "claude".to_string(),
                        from: Some(State::Working),
                        to: Some(State::Blocked),
                        detail: Some(Detail::Permission),
                        locator: "xxxx:1.0".to_string(),
                        repo: Some(X8.to_string()),
                        branch: Some(X12.to_string()),
                    }),
                ),
                // The open ends of a pane's life: `""`, never `unknown`.
                ResponseFrame::new(
                    "3",
                    Response::Edge(Edge {
                        at_ms: 1_757_030_463_000,
                        pane: "%2".to_string(),
                        agent: "codex".to_string(),
                        from: None,
                        to: Some(State::Idle),
                        detail: None,
                        locator: "xxxx:1.1".to_string(),
                        repo: None,
                        branch: None,
                    }),
                ),
                ResponseFrame::new(
                    "3",
                    Response::Edge(Edge {
                        at_ms: 1_757_030_464_000,
                        pane: "%3".to_string(),
                        agent: "codex".to_string(),
                        from: Some(State::Idle),
                        to: None,
                        detail: None,
                        locator: "xxxx:1.2".to_string(),
                        repo: None,
                        branch: None,
                    }),
                ),
            ],
            &["Response::Edge", "Edge"],
        ),
        Vector::new(
            "subscribe.json",
            &RequestFrame::new(
                "3",
                Request::Subscribe(Subscribe {
                    events: true,
                    selector: Some(Selector {
                        agent: Some("claude".to_string()),
                        ..Selector::default()
                    }),
                }),
            ),
            &["Request::Subscribe", "Subscribe"],
        ),
        Vector::new(
            "ack.json",
            &ResponseFrame::new("3", Response::Ack),
            &["Response::Ack"],
        ),
        Vector::new(
            "card-request.json",
            &RequestFrame::new(
                "4",
                Request::Card(CardRequest {
                    pane: "%1".to_string(),
                }),
            ),
            &["Request::Card", "CardRequest"],
        ),
        Vector::new(
            "card-permission-screen.json",
            &ResponseFrame::new(
                "4",
                Response::Card(permission_card(
                    Lane::Screen,
                    Extraction::Exact,
                    vec![
                        option(OptionKind::AllowOnce, "1. Yes", Some("1")),
                        option(
                            OptionKind::AllowAlways,
                            "2. Yes, and don't ask again this session",
                            Some("2"),
                        ),
                        option(OptionKind::RejectOnce, "3. No, and tell Claude what to do differently", Some("3")),
                        option(OptionKind::RejectAlways, "4. No, and don't ask again", Some("4")),
                        // Rendered, never offered: an unknown kind has no control.
                        option(
                            OptionKind::Other(X12.to_string()),
                            "5. Run Everything",
                            Some("5"),
                        ),
                    ],
                )),
            ),
            &[
                "Response::Card",
                "Card::Permission",
                "PermissionCard",
                "PermissionOption",
                "PendingCall",
                "Binder",
                "Lane::Screen",
                "Extraction::Exact",
                "OptionKind::AllowOnce",
                "OptionKind::AllowAlways",
                "OptionKind::RejectOnce",
                "OptionKind::RejectAlways",
                "OptionKind::Other",
            ],
        ),
        Vector::new(
            "card-permission-hook.json",
            &ResponseFrame::new(
                "4",
                Response::Card(permission_card(
                    Lane::Hook,
                    // Exact by construction: nothing was extracted, the call arrived as data.
                    Extraction::Exact,
                    vec![
                        option(OptionKind::AllowOnce, "Yes", Some(X12)),
                        option(OptionKind::RejectOnce, "No", Some(X12)),
                    ],
                )),
            ),
            &["Lane::Hook"],
        ),
        Vector::new(
            "card-permission-api.json",
            &ResponseFrame::new(
                "4",
                Response::Card(permission_card(
                    Lane::Api,
                    Extraction::Wrapped,
                    vec![
                        option(OptionKind::AllowOnce, "Yes", Some(X12)),
                        option(OptionKind::RejectOnce, "No", Some(X12)),
                    ],
                )),
            ),
            &["Lane::Api", "Extraction::Wrapped"],
        ),
        // Cursor prints no index, so the wire carries none. A position the host invented
        // must never reach the device as a keycap the user could type.
        Vector::new(
            "card-permission-cursor.json",
            &ResponseFrame::new(
                "4",
                Response::Card(permission_card(
                    Lane::Screen,
                    Extraction::Failed,
                    vec![
                        option(OptionKind::AllowOnce, "(y) Yes", None),
                        option(OptionKind::AllowAlways, "(tab) Yes, and don't ask again", None),
                        option(OptionKind::Other(X12.to_string()), "(shift+tab) Run Everything", None),
                        option(OptionKind::RejectOnce, "(esc or n) No", None),
                    ],
                )),
            ),
            &["Extraction::Failed"],
        ),
        Vector::new(
            "card-question.json",
            &ResponseFrame::new(
                "4",
                Response::Card(Card::Question(QuestionCard {
                    pane: "%1".to_string(),
                    agent: "opencode".to_string(),
                    request_id: X12.to_string(),
                    questions: vec![Question {
                        question: X16.to_string(),
                        header: X8.to_string(),
                        options: vec![
                            QuestionOption {
                                label: X12.to_string(),
                                description: X16.to_string(),
                            },
                            QuestionOption {
                                label: X8.to_string(),
                                description: String::new(),
                            },
                        ],
                        multiple: false,
                        custom: true,
                    }],
                    binder: binder(),
                })),
            ),
            &[
                "Card::Question",
                "QuestionCard",
                "Question",
                "QuestionOption",
            ],
        ),
        Vector::new(
            "card-informational.json",
            &ResponseFrame::new(
                "4",
                Response::Card(Card::Informational {
                    detail: Detail::Plan,
                    headline: X16.to_string(),
                }),
            ),
            &["Card::Informational"],
        ),
        Vector::new(
            "card-none.json",
            &ResponseFrame::new("4", Response::Card(Card::None)),
            &["Card::None"],
        ),
        Vector::new(
            "dispatch-keys.json",
            &RequestFrame::new(
                "5",
                Request::Dispatch(Dispatch {
                    slot: UUID.to_string(),
                    host: X8.to_string(),
                    pane: "%1".to_string(),
                    action: "approve".to_string(),
                    binder: binder(),
                    text: None,
                    answers: None,
                    device: Some(X16.to_string()),
                }),
            ),
            &["Request::Dispatch", "Dispatch"],
        ),
        // The older writer: `device` postdates this frame, so it is simply absent.
        Vector::new(
            "dispatch-text.json",
            &RequestFrame::new(
                "5",
                Request::Dispatch(Dispatch {
                    slot: UUID.to_string(),
                    host: X8.to_string(),
                    pane: "%1".to_string(),
                    action: "steer".to_string(),
                    binder: Binder {
                        expect_episode_ms: 1_757_030_400_000,
                        expect_permission_request: None,
                    },
                    text: Some(X16.to_string()),
                    answers: None,
                    device: None,
                }),
            ),
            &[],
        ),
        Vector::new(
            "dispatch-answers.json",
            &RequestFrame::new(
                "5",
                Request::Dispatch(Dispatch {
                    slot: UUID.to_string(),
                    host: X8.to_string(),
                    pane: "%1".to_string(),
                    action: "answer".to_string(),
                    binder: binder(),
                    text: None,
                    answers: Some(vec![vec![X12.to_string()], vec![X8.to_string()]]),
                    device: Some(X16.to_string()),
                }),
            ),
            &[],
        ),
        Vector::new(
            "receipt.json",
            &ResponseFrame::new(
                "5",
                Response::Receipt(receipt(UUID, Outcome::Sent, None, 0)),
            ),
            &["Response::Receipt", "Receipt"],
        ),
        Vector::new(
            "receipts-request.json",
            &RequestFrame::new(
                "6",
                Request::Receipts(ReceiptsRequest {
                    slot: Some(UUID.to_string()),
                    since_ms: Some(1_757_030_400_000),
                }),
            ),
            &["Request::Receipts", "ReceiptsRequest"],
        ),
        // The whole outcome and reason vocabulary in one frame, which is what makes a host token
        // with no proto arm a missing vector rather than a silent hole.
        Vector::new(
            "receipts.json",
            &ResponseFrame::new(
                "6",
                Response::Receipts(Receipts {
                    receipts: vec![
                        receipt("xxxx-0001", Outcome::Sent, None, 0),
                        receipt("xxxx-0002", Outcome::Replied, None, 0),
                        receipt("xxxx-0003", Outcome::Exited, None, 2),
                        receipt("xxxx-0004", Outcome::Spawned, None, 0),
                        receipt("xxxx-0005", Outcome::Timeout, None, 124),
                        receipt("xxxx-0006", Outcome::Other(X12.to_string()), None, 1),
                        receipt(
                            "xxxx-0007",
                            Outcome::Refused,
                            Some(Reason::WrongAgent),
                            4,
                        ),
                        receipt(
                            "xxxx-0008",
                            Outcome::Refused,
                            Some(Reason::NoCoverage),
                            4,
                        ),
                        receipt(
                            "xxxx-0009",
                            Outcome::Refused,
                            Some(Reason::RequiresUnmet),
                            4,
                        ),
                        receipt("xxxx-0010", Outcome::Refused, Some(Reason::Gated), 4),
                        // The one refusal a retry can clear, so the slot stays open.
                        receipt("xxxx-0011", Outcome::Refused, Some(Reason::Locked), 5),
                        receipt(
                            "xxxx-0012",
                            Outcome::Refused,
                            Some(Reason::EpisodeChanged),
                            4,
                        ),
                        receipt(
                            "xxxx-0013",
                            Outcome::Refused,
                            Some(Reason::RequestGone),
                            4,
                        ),
                        receipt("xxxx-0014", Outcome::Refused, Some(Reason::Sigil), 4),
                        receipt(
                            "xxxx-0015",
                            Outcome::Refused,
                            Some(Reason::ControlBytes),
                            4,
                        ),
                        receipt("xxxx-0016", Outcome::Refused, Some(Reason::TooLong), 4),
                        receipt("xxxx-0017", Outcome::Refused, Some(Reason::Empty), 4),
                        receipt(
                            "xxxx-0018",
                            Outcome::Refused,
                            Some(Reason::ScopeDenied),
                            4,
                        ),
                        receipt("xxxx-0019", Outcome::Vanished, Some(Reason::PaneGone), 3),
                        // The dispatch may have landed. A retry replays this rather than firing.
                        receipt(
                            "xxxx-0020",
                            Outcome::Error,
                            Some(Reason::FiredUnknown),
                            1,
                        ),
                        receipt(
                            "xxxx-0021",
                            Outcome::Refused,
                            Some(Reason::Other(X12.to_string())),
                            4,
                        ),
                    ],
                }),
            ),
            &[
                "Response::Receipts",
                "Receipts",
                "Outcome::Sent",
                "Outcome::Replied",
                "Outcome::Exited",
                "Outcome::Spawned",
                "Outcome::Timeout",
                "Outcome::Refused",
                "Outcome::Vanished",
                "Outcome::Error",
                "Outcome::Other",
                "Reason::WrongAgent",
                "Reason::NoCoverage",
                "Reason::RequiresUnmet",
                "Reason::Gated",
                "Reason::Locked",
                "Reason::EpisodeChanged",
                "Reason::RequestGone",
                "Reason::PaneGone",
                "Reason::Sigil",
                "Reason::ControlBytes",
                "Reason::TooLong",
                "Reason::Empty",
                "Reason::FiredUnknown",
                "Reason::ScopeDenied",
                "Reason::Other",
            ],
        ),
        Vector::new(
            "window-request.json",
            &RequestFrame::new(
                "7",
                Request::Window(WindowRequest {
                    pane: "%1".to_string(),
                    last: Some(200),
                    before: Some(Cursor("t1.1234.beef.1000.200.0".to_string())),
                    budget: Budget::default(),
                }),
            ),
            &["Request::Window", "WindowRequest", "Budget", "Cursor"],
        ),
        Vector::new(
            "window.json",
            &ResponseFrame::new(
                "7",
                Response::Window(Window {
                    pane: "%1".to_string(),
                    agent: "claude".to_string(),
                    session: Some(SessionMeta {
                        agent: "claude".to_string(),
                        session_id: Some(UUID.to_string()),
                        cwd: Some("/xxxxx/xxxxxxxx".to_string()),
                        version: Some("2.1.236".to_string()),
                        model: Some(X12.to_string()),
                    }),
                    older: Some(Cursor("t1.1234.beef.1000.100.0".to_string())),
                    budget_truncated: true,
                    unknown: 0,
                    events: vec![
                        header(
                            "t1.1234.beef.1000.100.0",
                            EventKind::SessionMeta(SessionMeta {
                                agent: "claude".to_string(),
                                session_id: Some(UUID.to_string()),
                                cwd: Some("/xxxxx/xxxxxxxx".to_string()),
                                version: Some("2.1.236".to_string()),
                                model: Some(X12.to_string()),
                            }),
                            None,
                        ),
                        header(
                            "t1.1234.beef.1000.110.0",
                            EventKind::UserMessage {
                                bytes: 128,
                                attachments: 1,
                            },
                            Some(X16),
                        ),
                        header(
                            "t1.1234.beef.1000.120.0",
                            EventKind::AssistantText { bytes: 512 },
                            Some(X16),
                        ),
                        header(
                            "t1.1234.beef.1000.130.0",
                            EventKind::Thinking {
                                bytes: 64,
                                redacted: true,
                            },
                            None,
                        ),
                        header(
                            "t1.1234.beef.1000.140.0",
                            EventKind::ToolCall {
                                name: "xxxx".to_string(),
                                call_id: Some(X12.to_string()),
                                arg_keys: vec![X8.to_string(), X12.to_string()],
                                bytes: 96,
                            },
                            Some(X16),
                        ),
                        header(
                            "t1.1234.beef.1000.150.0",
                            EventKind::ToolResult {
                                call_id: Some(X12.to_string()),
                                status: ResultStatus::Ok,
                                bytes: 2048,
                            },
                            Some(X16),
                        ),
                        header(
                            "t1.1234.beef.1000.160.0",
                            EventKind::ToolResult {
                                call_id: Some(X12.to_string()),
                                status: ResultStatus::Error,
                                bytes: 64,
                            },
                            None,
                        ),
                        header(
                            "t1.1234.beef.1000.170.0",
                            EventKind::ToolResult {
                                call_id: None,
                                status: ResultStatus::Pending,
                                bytes: 0,
                            },
                            None,
                        ),
                        header(
                            "t1.1234.beef.1000.180.0",
                            EventKind::ToolResult {
                                call_id: None,
                                status: ResultStatus::Unknown,
                                bytes: 0,
                            },
                            None,
                        ),
                        header(
                            "t1.1234.beef.1000.190.0",
                            EventKind::PermissionRequest {
                                tool: "xxxx".to_string(),
                                call_id: Some(X12.to_string()),
                            },
                            None,
                        ),
                        header(
                            "t1.1234.beef.1000.200.0",
                            EventKind::TurnBoundary {
                                boundary: TurnKind::Start,
                                reason: None,
                            },
                            None,
                        ),
                        header(
                            "t1.1234.beef.1000.210.0",
                            EventKind::TurnBoundary {
                                boundary: TurnKind::End,
                                reason: Some(X8.to_string()),
                            },
                            None,
                        ),
                        header(
                            "t1.1234.beef.1000.220.0",
                            EventKind::Usage {
                                input: Some(96_000),
                                output: Some(1_200),
                                total: Some(97_200),
                                context_window: Some(200_000),
                                cost_usd: Some(0.25),
                            },
                            None,
                        ),
                        header(
                            "t1.1234.beef.1000.230.0",
                            EventKind::SubagentRef {
                                child_id: UUID.to_string(),
                                external_file: true,
                            },
                            None,
                        ),
                        header(
                            "t1.1234.beef.1000.240.0",
                            EventKind::Compaction {
                                compaction: X8.to_string(),
                            },
                            None,
                        ),
                        header(
                            "t1.1234.beef.1000.250.0",
                            EventKind::Attachment {
                                attachment: X8.to_string(),
                                bytes: 4096,
                            },
                            None,
                        ),
                        header(
                            "t1.1234.beef.1000.260.0",
                            EventKind::Bookkeeping {
                                type_name: X8.to_string(),
                            },
                            None,
                        ),
                        header(
                            "t1.1234.beef.1000.270.0",
                            EventKind::Unknown {
                                type_name: X8.to_string(),
                            },
                            None,
                        ),
                    ],
                }),
            ),
            &[
                "Response::Window",
                "Window",
                "SessionMeta",
                "EventHeader",
                "EventKind::SessionMeta",
                "EventKind::UserMessage",
                "EventKind::AssistantText",
                "EventKind::Thinking",
                "EventKind::ToolCall",
                "EventKind::ToolResult",
                "EventKind::PermissionRequest",
                "EventKind::TurnBoundary",
                "EventKind::Usage",
                "EventKind::SubagentRef",
                "EventKind::Compaction",
                "EventKind::Attachment",
                "EventKind::Bookkeeping",
                "EventKind::Unknown",
                "ResultStatus::Ok",
                "ResultStatus::Error",
                "ResultStatus::Pending",
                "ResultStatus::Unknown",
                "TurnKind::Start",
                "TurnKind::End",
            ],
        ),
        Vector::new(
            "event-request.json",
            &RequestFrame::new(
                "8",
                Request::Event(EventRequest {
                    pane: "%1".to_string(),
                    cursor: Cursor("t1.1234.beef.1000.140.0".to_string()),
                }),
            ),
            &["Request::Event", "EventRequest"],
        ),
        Vector::new(
            "events.json",
            &vec![
                ResponseFrame::new(
                    "8",
                    Response::Event(EventHeader {
                        body: Some(Body {
                            kind: BodyKind::Text,
                            text: X16.to_string(),
                        }),
                        ..header(
                            "t1.1234.beef.1000.120.0",
                            EventKind::AssistantText { bytes: 512 },
                            Some(X16),
                        )
                    }),
                ),
                ResponseFrame::new(
                    "8",
                    Response::Event(EventHeader {
                        body: Some(Body {
                            kind: BodyKind::Json,
                            text: "{\"xxxxxxx\":\"xxxxxxxxxxxxxxxx\"}".to_string(),
                        }),
                        ..header(
                            "t1.1234.beef.1000.140.0",
                            EventKind::ToolCall {
                                name: "xxxx".to_string(),
                                call_id: Some(X12.to_string()),
                                arg_keys: vec![X8.to_string()],
                                bytes: 96,
                            },
                            Some(X16),
                        )
                    }),
                ),
            ],
            &[
                "Response::Event",
                "Body",
                "BodyKind::Text",
                "BodyKind::Json",
            ],
        ),
        Vector::new(
            "notify.json",
            &NotifyPayload {
                host: X8.to_string(),
                pane: "%1".to_string(),
                agent: "claude".to_string(),
                state: State::Blocked,
                episode_ms: 1_757_030_460_000,
            },
            &["NotifyPayload"],
        ),
        Vector::new("notify-MAXIMAL.json", &maximal_notify(), &[]),
    ]
}
