//! The envelope: one NDJSON line per message in both directions.
//!
//! `{"schema":1,"id":"…","t":"…", …}`. The schema rides the envelope and nothing inside repeats it,
//! so one line carries exactly one version claim and a frame cannot contradict itself. A response
//! carries the id of the request it answers; a streamed frame carries the id of the subscribe that
//! opened it.

use serde::{Deserialize, Serialize};

use crate::card::{Card, CardRequest};
use crate::dispatch::{Dispatch, Receipt, Receipts, ReceiptsRequest};
use crate::error::{ErrorCode, ErrorFrame};
use crate::fleet::{Edge, Snapshot, SnapshotRequest, Subscribe};
use crate::hello::{Hello, HelloOk};
use crate::transcript::{EventHeader, EventRequest, Window, WindowRequest};
use crate::SCHEMA;

/// One line from the device.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RequestFrame {
    pub schema: u32,
    /// The device's own correlation id, echoed on every response to this request.
    pub id: String,
    #[serde(flatten)]
    pub body: Request,
}

impl RequestFrame {
    pub fn new(id: impl Into<String>, body: Request) -> RequestFrame {
        RequestFrame {
            schema: SCHEMA,
            id: id.into(),
            body,
        }
    }

    /// Read the handshake, refusing a schema this build does not implement with a typed error.
    ///
    /// The refusal is deliberately not a parse failure: a frame naming a future schema still parses,
    /// because the version claim is a field and not a shape, and a host that answers "I do not speak
    /// 2" is what lets a device downgrade instead of guessing why the pipe went quiet.
    pub fn accept_hello(&self) -> Result<&Hello, ErrorFrame> {
        // The version claim is judged first: a frame's shape means nothing under a schema this
        // build does not implement.
        if self.schema != SCHEMA {
            return Err(ErrorFrame::new(
                ErrorCode::UnsupportedSchema,
                format!(
                    "this host speaks protocol schema {SCHEMA}; the client asked for {}",
                    self.schema
                ),
            ));
        }
        let Request::Hello(hello) = &self.body else {
            return Err(ErrorFrame::new(
                ErrorCode::BadRequest,
                "the first frame of a session must be `hello`",
            ));
        };
        Ok(hello)
    }
}

/// What a device asks for.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "kebab-case")]
pub enum Request {
    Hello(Hello),
    Snapshot(SnapshotRequest),
    Subscribe(Subscribe),
    Card(CardRequest),
    Window(WindowRequest),
    Event(EventRequest),
    Dispatch(Dispatch),
    Receipts(ReceiptsRequest),
}

/// One line from the host.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResponseFrame {
    pub schema: u32,
    /// The id of the request this answers.
    pub id: String,
    #[serde(flatten)]
    pub body: Response,
}

impl ResponseFrame {
    pub fn new(id: impl Into<String>, body: Response) -> ResponseFrame {
        ResponseFrame {
            schema: SCHEMA,
            id: id.into(),
            body,
        }
    }
}

/// What the host answers with. Every request has exactly one success shape and one failure shape,
/// and the failure shape is always [`Response::Error`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "kebab-case")]
pub enum Response {
    Hello(HelloOk),
    Snapshot(Snapshot),
    /// One transition, from a subscription in events mode.
    Edge(Edge),
    Card(Card),
    Window(Window),
    Event(EventHeader),
    Receipt(Receipt),
    Receipts(Receipts),
    /// The request was accepted and has no body of its own.
    Ack,
    Error(ErrorFrame),
}
