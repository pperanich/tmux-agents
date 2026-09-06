//! One pane's facts to one [`tma_proto::Card`]: what the pane is asking, typed so the wrong
//! affordance cannot be expressed.
//!
//! The rule the whole module turns on is that **blocked-ness comes from detection and from nowhere
//! else**. A dangling tool call in a transcript says a call is in flight, which a slow tool, a
//! permission prompt and a crashed process all produce; it is read here only to say what the pane
//! is blocked *on*, never that it is blocked. [`build_card`] returns [`Card::None`] for any
//! pane whose stamped state is not `blocked`, before it looks at anything else.
//!
//! Three lanes produce a permission card, and the card is tagged with the one it came from so a
//! reviewer can tell a structured card from a scraped one. [`Lane::Hook`] is the only lane that
//! carries the agent's own tool input as an object: the hook payload arrives unwrapped, which is
//! the whole reason that lane exists. The other two synthesize their options from the bundled
//! `approve`/`deny` labels and report [`Extraction::Failed`], because the dialog extractor does
//! not exist in this release for any agent, so the app degrades to open-on-host rather than
//! drawing a control over a label nobody read.

use std::time::Duration;

use serde_json::Value;

use tma_core::AgentState;
use tma_proto::{
    Binder, Card, Detail, Extraction, Lane, OptionKind, PendingCall, PermissionCard,
    PermissionOption, Question, QuestionCard,
};
use tma_transcript::{Event, EventKind};

use crate::hook_lane::RequestRecord;

/// The labels claude's own permission dialog shows for its one-shot options. Synthesized rather
/// than read, because on the hook lane there is no dialog text to read (ARCHITECTURE §1.9).
const HOOK_ALLOW_LABEL: &str = "Yes";
const HOOK_REJECT_LABEL: &str = "No";

/// How long [`PendingQuestion::fetch`] waits on the agent's own server. Short on purpose: the serve
/// loop answers one request at a time, so a hung localhost server would stall every other request,
/// and a card with no question set degrades to informational rather than to a frame that never comes.
pub const QUESTION_TIMEOUT: Duration = Duration::from_millis(750);

/// The endpoint's pending-question path. A v1 path, like every other op on this lane.
const QUESTION_PATH: &str = "/question";

/// Everything the serve loop can gather for one pane, and nothing it would have to go to tmux or
/// to a manifest file for. Assembled once per `card` request; [`build_card`] is pure over it, which
/// is what keeps the card path free of the manifest load the design forbids after the handshake.
#[derive(Clone, Debug, Default)]
pub struct CardInputs<'a> {
    pub pane: &'a str,
    /// `@agent_name`.
    pub agent: &'a str,
    /// `@agent_state`. Anything but [`AgentState::Blocked`] yields [`Card::None`].
    pub state: Option<AgentState>,
    /// `@agent_detail`, which is what types the card.
    pub detail: Option<&'a str>,
    /// `max(@agent_since, @agent_turn_at)`, quoted back on the dispatch binder.
    pub episode_ms: u64,
    /// `@agent_permission_request`.
    pub permission_request: Option<&'a str>,
    /// `@agent_pending_tool` and `@agent_pending_call`. The sibling `@agent_pending_summary` is
    /// agent-supplied prose that was rendered for a status line, so it is deliberately not read:
    /// the card carries the call's own identity or nothing.
    pub pending_tool: Option<&'a str>,
    pub pending_call: Option<&'a str>,
    /// The `approve` and `deny` labels for this agent, when the loaded action set covers it. The
    /// screen and api lanes offer exactly these two and never invent a third.
    pub approve: Option<&'a str>,
    pub deny: Option<&'a str>,
    /// The agent answers a permission over its own HTTP surface rather than with keystrokes, i.e.
    /// the loaded `approve` action resolves an `[api]` transport for it. Tags the lane only; the
    /// options are still the bundled labels.
    pub api_transport: bool,
    /// The parked hook-lane request record for `permission_request`, when one is on disk.
    pub hook_record: Option<&'a RequestRecord>,
    /// The pending question the agent's own API reported, for a pane blocked on one.
    pub question: Option<&'a PendingQuestion>,
    /// The transcript tail, newest first, bounded by the caller. Read for `pending_call` only.
    pub tail: &'a [Event],
}

/// One pending question, as opencode's `GET /question` returns it. The question set is the wire's
/// own type, so what the device renders is the server's text and not a re-modelling of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingQuestion {
    pub id: String,
    pub session_id: String,
    pub questions: Vec<Question>,
}

impl PendingQuestion {
    /// Parse the pending question for `session` out of a `GET /question` body, `None` when the
    /// server has none for it.
    ///
    /// Both shapes are accepted because the endpoint's empty answer is an array and its populated
    /// answer was observed as one object (17-owed §1.5); a device must not lose a question to that
    /// difference. One server serves every session on the machine, so a caller that knows its
    /// pane's `@agent_session` passes it and gets only its own.
    pub fn from_json(body: &str, session: Option<&str>) -> Option<PendingQuestion> {
        let value: Value = serde_json::from_str(body).ok()?;
        let entries = match &value {
            Value::Array(items) => items.as_slice(),
            Value::Object(_) => std::slice::from_ref(&value),
            _ => return None,
        };
        entries.iter().find_map(|entry| {
            let question = Self::one(entry)?;
            match session {
                Some(want) if question.session_id != want => None,
                _ => Some(question),
            }
        })
    }

    /// Ask `endpoint` for `session`'s pending question. The one I/O in this module, kept beside the
    /// parse it feeds rather than in the loop that calls it: [`build_card`] stays pure over what
    /// comes back, and every failure (unreachable, slow, a body this cannot read) is the same
    /// `None`, because a question the host could not fetch is a card with nothing to answer.
    pub fn fetch(
        endpoint: &str,
        session: Option<&str>,
        timeout: Duration,
    ) -> Option<PendingQuestion> {
        let body = crate::http::get_text(endpoint, QUESTION_PATH, timeout).ok()?;
        PendingQuestion::from_json(&body, session)
    }

    fn one(entry: &Value) -> Option<PendingQuestion> {
        let id = entry.get("id")?.as_str()?.to_string();
        let questions: Vec<Question> =
            serde_json::from_value(entry.get("questions")?.clone()).ok()?;
        Some(PendingQuestion {
            id,
            session_id: entry
                .get("sessionID")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            questions,
        })
    }
}

/// The card for one pane.
///
/// The order is the contract: state first, then detail. A pane that detection does not call
/// `blocked` has no card whatever its transcript, its stamps or its agent's API say.
pub fn build_card(facts: &CardInputs) -> Card {
    if facts.state != Some(AgentState::Blocked) {
        return Card::None;
    }
    let detail = Detail::from_token(facts.detail.unwrap_or_default());
    match detail {
        Detail::Permission => permission_card(facts),
        // A question with no fetched question set is something to read and nothing to answer: the
        // agent has an API that could serve it and this host did not reach it, or (claude) there is
        // no such API at all.
        Detail::Question => match facts.question {
            Some(question) => question_card(facts, question),
            None => informational(detail),
        },
        other => informational(other),
    }
}

/// A permission card, on whichever lane can answer it.
fn permission_card(facts: &CardInputs) -> Card {
    let card = match facts.hook_record {
        Some(record) => hook_card(facts, record),
        None => screen_card(facts),
    };
    Card::Permission(card)
}

/// The hook lane: everything comes from the parked request record, so nothing was extracted and
/// [`Extraction::Exact`] is true by construction rather than by measurement.
///
/// The record is written by claude's `PermissionRequest` hook and by nothing else today, so the
/// lane is claude's in practice; the gate here is the record, not the agent name, because a second
/// agent that parks one has earned the same card.
fn hook_card(facts: &CardInputs, record: &RequestRecord) -> PermissionCard {
    PermissionCard {
        pane: facts.pane.to_string(),
        agent: facts.agent.to_string(),
        lane: Lane::Hook,
        // Exactly two, and no `AllowAlways`: the hook lane has no second-interaction surface for
        // the second-interaction rule to gate, so approve_always does not reach it in v1 (ARCHITECTURE §1.9).
        options: vec![
            option(OptionKind::AllowOnce, HOOK_ALLOW_LABEL, Some(&record.id)),
            option(OptionKind::RejectOnce, HOOK_REJECT_LABEL, Some(&record.id)),
        ],
        extraction: Extraction::Exact,
        pending_call: Some(PendingCall {
            tool: record.tool_name.clone(),
            // The payload's own object, reparsed rather than rendered. A rendered line wraps at a
            // phone width and the wrap is not invertible, which is what this lane exists to avoid.
            input: serde_json::from_str(&record.tool_input).ok(),
            call_id: Some(record.id.clone()),
        }),
        binder: Binder {
            expect_episode_ms: facts.episode_ms,
            expect_permission_request: Some(record.id.clone()),
        },
    }
}

/// The unstructured lanes: the options are the two bundled actions this host would fire, and the
/// dialog itself was never read.
fn screen_card(facts: &CardInputs) -> PermissionCard {
    let mut options = Vec::new();
    if let Some(label) = facts.approve {
        options.push(option(OptionKind::AllowOnce, label, None));
    }
    if let Some(label) = facts.deny {
        options.push(option(OptionKind::RejectOnce, label, None));
    }
    PermissionCard {
        pane: facts.pane.to_string(),
        agent: facts.agent.to_string(),
        // `api` says the reply travels over the agent's own HTTP surface, which is a fact about
        // the transport and not a claim that the dialog was read.
        lane: if facts.api_transport {
            Lane::Api
        } else {
            Lane::Screen
        },
        options,
        // Never anything else in this release: dialog option extraction's extractor does not exist yet for any agent, so
        // no option label here is the dialog's own line and the app must open the pane on the host
        // rather than render a control over text nobody parsed.
        extraction: Extraction::Failed,
        pending_call: screen_pending_call(facts),
        binder: Binder {
            expect_episode_ms: facts.episode_ms,
            expect_permission_request: facts.permission_request.map(str::to_string),
        },
    }
}

/// What the prompt is about, on a lane that carries no payload: the pane's own stamped trio first
/// (it was written from the agent's hook payload), then the transcript's newest unresolved call.
fn screen_pending_call(facts: &CardInputs) -> Option<PendingCall> {
    if let Some(tool) = facts.pending_tool.filter(|t| !t.is_empty()) {
        return Some(PendingCall {
            tool: tool.to_string(),
            input: None,
            call_id: facts
                .pending_call
                .filter(|c| !c.is_empty())
                .map(str::to_string),
        });
    }
    dangling_call(facts.tail)
}

/// The newest tool call in `tail` (newest first) that no later result resolved.
///
/// Only a call carrying a `call_id` can be reported: without one there is nothing to correlate a
/// result against, and naming a call the host guessed at is worse than naming none.
fn dangling_call(tail: &[Event]) -> Option<PendingCall> {
    let mut resolved: Vec<&str> = Vec::new();
    for event in tail {
        match &event.kind {
            EventKind::ToolResult {
                call_id: Some(id), ..
            } => resolved.push(id),
            EventKind::ToolCall {
                name,
                call_id: Some(id),
                ..
            } if !resolved.contains(&id.as_str()) => {
                return Some(PendingCall {
                    tool: name.clone(),
                    input: None,
                    call_id: Some(id.clone()),
                })
            }
            _ => {}
        }
    }
    None
}

/// A question card. The question set is the agent's own, verbatim: this host neither trims it nor
/// re-orders its options, because the label is what the reply quotes back.
fn question_card(facts: &CardInputs, question: &PendingQuestion) -> Card {
    Card::Question(QuestionCard {
        pane: facts.pane.to_string(),
        agent: facts.agent.to_string(),
        request_id: question.id.clone(),
        questions: question.questions.clone(),
        binder: Binder {
            expect_episode_ms: facts.episode_ms,
            expect_permission_request: facts.permission_request.map(str::to_string),
        },
    })
}

/// Something to read and nothing to fire. The variant has no field an approve control could be
/// expressed in, which is the plan-dialog bug class made unrepresentable.
fn informational(detail: Detail) -> Card {
    let headline = match &detail {
        Detail::Permission => "Permission",
        Detail::Plan => "Plan approval",
        Detail::Trust => "Workspace trust",
        Detail::Question => "Question",
        Detail::Error => "Error",
        Detail::RateLimit => "Rate limited",
        Detail::Background => "Background task",
        Detail::Compacting => "Compacting",
        // A token minted after this build, `awaiting-text` included: the pane is blocked and this
        // host cannot type the dialog, which is exactly what an informational card says.
        Detail::Other(_) => "Waiting for you",
    }
    .to_string();
    Card::Informational { detail, headline }
}

fn option(kind: OptionKind, name: &str, id: Option<&str>) -> PermissionOption {
    PermissionOption {
        kind,
        name: name.to_string(),
        option_id: id.map(str::to_string),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tma_transcript::{Cursor, FileId, ResultStatus};

    /// The live hook experiment's leg-1 `PermissionRequest` payloads, redacted, as claude 2.1.261 delivered them
    /// (`research/captures/claude/hooks/e4-leg1-*-PermissionRequest.json`). What is not here is the
    /// point: no rendered label anywhere, so a card built from one cannot be quoting a screen.
    const E4_BASH: &str = r#"{
  "session_id": "7c74ac42-d330-4c0f-a7b1-ba9a918ba98e",
  "transcript_path": "<TRANSCRIPT>",
  "cwd": "<PROJECT>",
  "prompt_id": "20edf0d8-a22b-43de-94ab-4ecce0c78d11",
  "permission_mode": "default",
  "hook_event_name": "PermissionRequest",
  "tool_name": "Bash",
  "tool_input": {
    "command": "touch <SCRATCH>/marker-leg1 && echo leg1-done",
    "description": "Create marker file and print confirmation"
  },
  "permission_suggestions": [
    { "type": "setMode", "mode": "acceptEdits", "destination": "session" }
  ]
}"#;

    const E4_WRITE: &str = r#"{
  "session_id": "7c74ac42-d330-4c0f-a7b1-ba9a918ba98e",
  "transcript_path": "<TRANSCRIPT>",
  "cwd": "<PROJECT>",
  "prompt_id": "81ee2c39-0097-4cb2-bdee-3913af01ef5e",
  "permission_mode": "default",
  "hook_event_name": "PermissionRequest",
  "tool_name": "Write",
  "tool_input": {
    "file_path": "<PROJECT>/notes.txt",
    "content": "hello-e4\n"
  }
}"#;

    /// The live `GET /question` body, redacted (17-owed §1.5).
    const OC_QUESTION: &str = r#"[{
  "id": "que_7f3a",
  "sessionID": "ses_1122",
  "questions": [
    {
      "question": "Which colour?",
      "header": "Colour",
      "options": [
        { "label": "Red", "description": "the warm one" },
        { "label": "Blue" }
      ],
      "multiple": false
    }
  ],
  "tool": { "messageID": "msg_1", "callID": "call_1" }
}]"#;

    fn blocked<'a>(agent: &'a str, detail: &'a str) -> CardInputs<'a> {
        CardInputs {
            pane: "%1",
            agent,
            state: Some(AgentState::Blocked),
            detail: Some(detail),
            episode_ms: 1_757_030_400_000,
            approve: Some("Approve"),
            deny: Some("Deny"),
            ..CardInputs::default()
        }
    }

    fn record(payload: &str) -> RequestRecord {
        RequestRecord::from_payload("d41d8cd98f00b204", "%1", payload, 1_757_030_400_000, 7)
    }

    fn cursor(offset: u64) -> Cursor {
        Cursor {
            file: FileId { dev: 1, ino: 2 },
            size: 4096,
            offset,
            part: 0,
        }
    }

    fn event(offset: u64, kind: EventKind) -> Event {
        Event {
            cursor: cursor(offset),
            ts: None,
            kind,
            preview: None,
            body: None,
        }
    }

    fn call(id: &str) -> EventKind {
        EventKind::ToolCall {
            name: "Bash".to_string(),
            call_id: Some(id.to_string()),
            arg_keys: vec!["command".to_string()],
            bytes: 24,
        }
    }

    fn result(id: &str) -> EventKind {
        EventKind::ToolResult {
            call_id: Some(id.to_string()),
            status: ResultStatus::Ok,
            bytes: 8,
        }
    }

    fn permission(card: Card) -> PermissionCard {
        match card {
            Card::Permission(p) => p,
            other => panic!("expected a permission card, got {other:?}"),
        }
    }

    /// The card half of the never-infer rule: a pane detection does not call blocked has no card, whatever its
    /// transcript holds. The tail here is one unresolved call, the exact shape that would tempt a
    /// reader into inferring a prompt.
    #[test]
    fn a_pane_that_is_not_blocked_has_no_card() {
        let tail = [event(100, call("toolu_1"))];
        for state in [
            Some(AgentState::Working),
            Some(AgentState::Idle),
            Some(AgentState::Unknown),
            None,
        ] {
            let facts = CardInputs {
                state,
                tail: &tail,
                ..blocked("claude", "permission")
            };
            assert_eq!(build_card(&facts), Card::None, "{state:?}");
        }
    }

    /// The hook lane's card is structured by construction. Both experiment legs, because a record
    /// written for one call must never read as the other.
    #[test]
    fn a_hook_record_yields_an_exact_two_option_card() {
        for (payload, tool, needle) in [
            (E4_BASH, "Bash", "marker-leg1"),
            (E4_WRITE, "Write", "notes.txt"),
        ] {
            let rec = record(payload);
            let facts = CardInputs {
                hook_record: Some(&rec),
                permission_request: Some(&rec.id),
                ..blocked("claude", "permission")
            };
            let card = permission(build_card(&facts));
            assert_eq!(card.lane, Lane::Hook);
            assert_eq!(card.extraction, Extraction::Exact);

            let kinds: Vec<&OptionKind> = card.options.iter().map(|o| &o.kind).collect();
            assert_eq!(kinds, vec![&OptionKind::AllowOnce, &OptionKind::RejectOnce]);
            let names: Vec<&str> = card.options.iter().map(|o| o.name.as_str()).collect();
            assert_eq!(names, vec!["Yes", "No"]);
            assert!(
                !card
                    .options
                    .iter()
                    .any(|o| o.kind == OptionKind::AllowAlways),
                "no always-grant reaches this lane in v1"
            );

            let pending = card.pending_call.expect("the record names the call");
            assert_eq!(pending.tool, tool);
            let input = pending.input.expect("the payload's own object survives");
            assert!(
                serde_json::to_string(&input).unwrap().contains(needle),
                "the tool input is the payload's, not a label: {input}"
            );
            assert_eq!(
                card.binder.expect_permission_request.as_deref(),
                Some(&*rec.id)
            );
            assert_eq!(card.binder.expect_episode_ms, 1_757_030_400_000);
        }
    }

    /// With no record parked the same pane degrades: the lane says the dialog was not read, and
    /// the extraction says the app must open the pane on the host.
    #[test]
    fn no_record_degrades_to_the_screen_lane() {
        let facts = CardInputs {
            permission_request: Some("per_1"),
            ..blocked("claude", "permission")
        };
        let card = permission(build_card(&facts));
        assert_eq!(card.lane, Lane::Screen);
        assert_eq!(card.extraction, Extraction::Failed);
        assert_eq!(card.pending_call, None, "nothing said what the call was");
        let names: Vec<&str> = card.options.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(names, vec!["Approve", "Deny"]);
        assert!(
            card.options.iter().all(|o| o.option_id.is_none()),
            "no index was read, so none is offered as a keycap"
        );
    }

    /// An agent whose reply travels over HTTP is tagged `api`, and still reports `failed`: the
    /// transport is structured, the dialog read was not.
    #[test]
    fn an_api_agent_is_tagged_api_and_still_unextracted() {
        let facts = CardInputs {
            api_transport: true,
            permission_request: Some("per_1"),
            ..blocked("opencode", "permission")
        };
        let card = permission(build_card(&facts));
        assert_eq!(card.lane, Lane::Api);
        assert_eq!(card.extraction, Extraction::Failed);
    }

    /// An agent the action set does not cover offers nothing rather than a control the host could
    /// not fire.
    #[test]
    fn an_uncovered_agent_gets_no_options() {
        let facts = CardInputs {
            approve: None,
            deny: None,
            ..blocked("pi", "permission")
        };
        assert!(permission(build_card(&facts)).options.is_empty());
    }

    /// The transcript's newest unresolved call fills `pending_call`, and a tail whose calls
    /// all settled fills nothing.
    #[test]
    fn the_tail_supplies_the_pending_call_when_nothing_else_does() {
        // Newest first: the older call was answered, the newer one was not.
        let dangling = [
            event(300, call("toolu_new")),
            event(200, result("toolu_old")),
            event(100, call("toolu_old")),
        ];
        let facts = CardInputs {
            tail: &dangling,
            ..blocked("claude", "permission")
        };
        let pending = permission(build_card(&facts))
            .pending_call
            .expect("the newest call is unresolved");
        assert_eq!(pending.call_id.as_deref(), Some("toolu_new"));
        assert_eq!(pending.tool, "Bash");
        assert_eq!(pending.input, None, "a header carries no tool input");

        let settled = [
            event(200, result("toolu_old")),
            event(100, call("toolu_old")),
        ];
        let facts = CardInputs {
            tail: &settled,
            ..blocked("claude", "permission")
        };
        assert_eq!(permission(build_card(&facts)).pending_call, None);
    }

    /// A call with no id cannot be correlated, so it is never reported: naming a call the host
    /// guessed at is worse than naming none.
    #[test]
    fn an_uncorrelatable_call_is_not_reported() {
        let tail = [event(
            100,
            EventKind::ToolCall {
                name: "Bash".to_string(),
                call_id: None,
                arg_keys: Vec::new(),
                bytes: 4,
            },
        )];
        let facts = CardInputs {
            tail: &tail,
            ..blocked("claude", "permission")
        };
        assert_eq!(permission(build_card(&facts)).pending_call, None);
    }

    /// The pane's own stamped trio outranks the transcript: it was written from the agent's hook
    /// payload for this prompt, where the tail is whatever the file happens to end with.
    #[test]
    fn the_stamped_trio_outranks_the_tail() {
        let tail = [event(100, call("toolu_tail"))];
        let facts = CardInputs {
            pending_tool: Some("Write"),
            pending_call: Some("toolu_stamped"),
            tail: &tail,
            ..blocked("claude", "permission")
        };
        let pending = permission(build_card(&facts)).pending_call.unwrap();
        assert_eq!(
            (pending.tool.as_str(), pending.call_id.as_deref()),
            ("Write", Some("toolu_stamped"))
        );
    }

    /// Every other blocked detail is informational, and the variant has no field
    /// an approve control could be expressed in. `awaiting-text` is not a token this build knows,
    /// which is the degradation the plan asks for rather than a special case.
    #[test]
    fn every_other_blocked_detail_is_informational() {
        for (detail, headline) in [
            ("plan", "Plan approval"),
            ("trust", "Workspace trust"),
            ("question", "Question"),
            ("awaiting-text", "Waiting for you"),
            ("error", "Error"),
            ("rate_limit", "Rate limited"),
            ("", "Waiting for you"),
        ] {
            match build_card(&blocked("claude", detail)) {
                Card::Informational {
                    detail: d,
                    headline: h,
                } => {
                    assert_eq!(d, Detail::from_token(detail));
                    assert_eq!(h, headline);
                }
                other => panic!("{detail:?} produced {other:?}"),
            }
        }
    }

    /// The question card carries the agent's own set, verbatim: the label is what a reply quotes
    /// back, so a trimmed or re-ordered option would answer a different question.
    #[test]
    fn a_fetched_question_becomes_a_question_card() {
        let question = PendingQuestion::from_json(OC_QUESTION, Some("ses_1122"))
            .expect("the body holds this session's question");
        let facts = CardInputs {
            question: Some(&question),
            ..blocked("opencode", "question")
        };
        let Card::Question(card) = build_card(&facts) else {
            panic!("expected a question card");
        };
        assert_eq!(card.request_id, "que_7f3a");
        assert_eq!(card.questions.len(), 1);
        let q = &card.questions[0];
        assert_eq!(q.question, "Which colour?");
        assert_eq!(q.header, "Colour");
        let labels: Vec<&str> = q.options.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(labels, vec!["Red", "Blue"]);
        assert_eq!(
            q.options[1].description, "",
            "an absent description is empty"
        );
        assert!(!q.multiple);
        // `custom` was absent from the live payload, and absent reads as false: this host does not
        // offer a free-text box the tool may not accept.
        assert!(!q.custom);
    }

    /// A question this host could not fetch is something to read, not an unanswerable control.
    #[test]
    fn a_question_with_no_fetched_set_is_informational() {
        assert!(matches!(
            build_card(&blocked("opencode", "question")),
            Card::Informational { .. }
        ));
    }

    /// The fetch is the parse plus one bounded GET, and every way it can fail is the same `None`:
    /// a card the host could not populate must degrade, never raise.
    #[test]
    fn a_fetch_reads_the_endpoints_question_and_swallows_every_failure() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a scratch port");
        let base = format!("http://{}", listener.local_addr().unwrap());
        let served = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("one connection");
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{OC_QUESTION}",
                    OC_QUESTION.len()
                )
                .as_bytes(),
            );
            String::from_utf8_lossy(&buf).to_string()
        });
        let fetched = PendingQuestion::fetch(&base, Some("ses_1122"), Duration::from_secs(2))
            .expect("the server holds this session's question");
        assert_eq!(fetched.id, "que_7f3a");
        let request = served.join().expect("the server thread");
        assert!(request.starts_with("GET /question HTTP/1.1"), "{request}");

        // A dead port: no question, and no error to handle at the call site.
        let closed = TcpListener::bind("127.0.0.1:0").unwrap();
        let gone = format!("http://{}", closed.local_addr().unwrap());
        drop(closed);
        assert_eq!(
            PendingQuestion::fetch(&gone, None, Duration::from_millis(300)),
            None
        );
    }

    /// One server serves every session on the machine, so a pane takes only its own question. A
    /// bare object is accepted beside the array because the endpoint was observed emitting both.
    #[test]
    fn a_question_is_matched_to_its_session() {
        assert_eq!(
            PendingQuestion::from_json(OC_QUESTION, Some("ses_other")),
            None
        );
        let bare = OC_QUESTION.trim_start_matches('[').trim_end_matches(']');
        let parsed = PendingQuestion::from_json(bare, None).expect("a bare object parses");
        assert_eq!(parsed.id, "que_7f3a");
        assert_eq!(PendingQuestion::from_json("[]", None), None);
        assert_eq!(PendingQuestion::from_json("not json", None), None);
    }
}
