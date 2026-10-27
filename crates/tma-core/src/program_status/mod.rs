//! Pure OSC 7501 revision 0.4 boundary.
//!
//! Wire reference: <https://www.superlogical.com/rex/docs/build/program-status>.
//! Parsing borrows input and caller-owned scratch. Retaining a command is a separate,
//! explicit allocation after the entire frame has passed validation.

mod parse;
mod project;
mod store;

pub use project::{
    ProgramAuthority, ProgramResult, ProgramSelection, ResultDisposition, ResultGeneration,
    ResultIdentity, ResultKind, WorkCycle,
};
pub use store::{EpisodeId, LiveEpisode, RecordStore, StoredRecord};

pub use parse::{parse_frame, DecodeScratch, IgnoreReason, ParseOutcome, RejectReason, TextField};

pub const MAX_FRAME_BYTES: usize = 4096;
pub const MAX_KEY_BYTES: usize = 16;
pub const MAX_MSG_ENCODED_BYTES: usize = 2732;
pub const MAX_MSG_BYTES: usize = 2048;
pub const MAX_TITLE_ENCODED_BYTES: usize = 256;
pub const MAX_TITLE_BYTES: usize = 192;
pub const MAX_APP_BYTES: usize = 32;
pub const MAX_ID_BYTES: usize = 128;
pub const MAX_ID_SEGMENT_BYTES: usize = 32;
pub const MAX_ID_LEVELS: usize = 8;
pub const MAX_RECORDS: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueError {
    InvalidRecordId,
    InvalidAppId,
    InvalidText,
    TitleTooLong,
    InvalidProgress,
    InvalidPaneId,
    InvalidSessionId,
}

fn identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'+' | b'-')
}

/// An explicit root is distinct from an invalid or empty wire `id`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RecordIdRef<'a>(&'a str);

impl<'a> RecordIdRef<'a> {
    pub const ROOT: Self = Self("");

    pub fn parse(value: &'a str) -> Result<Self, ValueError> {
        if value.is_empty() || value.len() > MAX_ID_BYTES {
            return Err(ValueError::InvalidRecordId);
        }
        let mut levels = 0;
        for segment in value.split('/') {
            levels += 1;
            if segment.is_empty()
                || segment.len() > MAX_ID_SEGMENT_BYTES
                || !segment.bytes().all(identifier_byte)
                || levels > MAX_ID_LEVELS
            {
                return Err(ValueError::InvalidRecordId);
            }
        }
        Ok(Self(value))
    }

    pub const fn as_str(self) -> &'a str {
        self.0
    }

    pub fn is_root(self) -> bool {
        self.0.is_empty()
    }

    pub fn parent(self) -> Option<Self> {
        if self.is_root() {
            None
        } else {
            Some(Self(
                self.0.rsplit_once('/').map_or("", |(parent, _)| parent),
            ))
        }
    }

    /// Includes the addressed record itself, for subtree clearing.
    pub fn contains(self, other: Self) -> bool {
        self.is_root()
            || self == other
            || other
                .0
                .strip_prefix(self.0)
                .is_some_and(|tail| tail.starts_with('/'))
    }

    pub fn into_owned(self) -> RecordId {
        RecordId(self.0.to_owned())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RecordId(String);

impl RecordId {
    pub fn root() -> Self {
        Self(String::new())
    }

    pub fn parse(value: &str) -> Result<Self, ValueError> {
        RecordIdRef::parse(value).map(RecordIdRef::into_owned)
    }

    pub fn as_ref(&self) -> RecordIdRef<'_> {
        RecordIdRef(&self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AppIdRef<'a>(&'a str);

impl<'a> AppIdRef<'a> {
    pub fn parse(value: &'a str) -> Result<Self, ValueError> {
        if value.is_empty() || value.len() > MAX_APP_BYTES || !value.bytes().all(identifier_byte) {
            Err(ValueError::InvalidAppId)
        } else {
            Ok(Self(value))
        }
    }

    pub const fn as_str(self) -> &'a str {
        self.0
    }

    pub fn into_owned(self) -> AppId {
        AppId(self.0.to_owned())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AppId(String);

impl AppId {
    pub fn parse(value: &str) -> Result<Self, ValueError> {
        AppIdRef::parse(value).map(AppIdRef::into_owned)
    }

    pub fn as_ref(&self) -> AppIdRef<'_> {
        AppIdRef(&self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SingleLineRef<'a>(&'a str);

impl<'a> SingleLineRef<'a> {
    pub fn parse(value: &'a str) -> Result<Self, ValueError> {
        if value.len() > MAX_MSG_BYTES || value.chars().any(char::is_control) {
            Err(ValueError::InvalidText)
        } else {
            Ok(Self(value))
        }
    }

    pub const fn as_str(self) -> &'a str {
        self.0
    }

    pub fn into_owned(self) -> SingleLineText {
        SingleLineText(self.0.to_owned())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SingleLineText(String);

impl SingleLineText {
    pub fn parse(value: &str) -> Result<Self, ValueError> {
        SingleLineRef::parse(value).map(SingleLineRef::into_owned)
    }

    pub fn as_ref(&self) -> SingleLineRef<'_> {
        SingleLineRef(&self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Progress(u8);

impl Progress {
    pub const fn new(value: u8) -> Result<Self, ValueError> {
        if value <= 100 {
            Ok(Self(value))
        } else {
            Err(ValueError::InvalidProgress)
        }
    }

    pub const fn get(self) -> u8 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockedKind {
    Permission,
    Question,
    Auth,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgramStatus {
    Idle,
    Working {
        progress: Option<Progress>,
    },
    Blocked {
        kind: Option<BlockedKind>,
        progress: Option<Progress>,
    },
    Done,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordRef<'a> {
    pub id: RecordIdRef<'a>,
    pub status: ProgramStatus,
    pub app: Option<AppIdRef<'a>>,
    // A title has the narrower 192-byte cap; SingleLineRef alone permits message lengths.
    title: Option<SingleLineRef<'a>>,
    pub msg: Option<SingleLineRef<'a>>,
}

impl<'a> RecordRef<'a> {
    pub const fn new(id: RecordIdRef<'a>, status: ProgramStatus) -> Self {
        Self {
            id,
            status,
            app: None,
            title: None,
            msg: None,
        }
    }

    /// Check the title-specific bound before it can be retained.
    pub fn with_title(mut self, title: Option<SingleLineRef<'a>>) -> Result<Self, ValueError> {
        if title.is_some_and(|text| text.as_str().len() > MAX_TITLE_BYTES) {
            return Err(ValueError::TitleTooLong);
        }
        self.title = title;
        Ok(self)
    }

    pub const fn title(self) -> Option<SingleLineRef<'a>> {
        self.title
    }

    pub fn into_owned(self) -> Record {
        Record {
            id: self.id.into_owned(),
            status: self.status,
            app: self.app.map(AppIdRef::into_owned),
            title: self.title.map(SingleLineRef::into_owned),
            msg: self.msg.map(SingleLineRef::into_owned),
        }
    }
}

/// Only explicit apps are retained. Inheritance is recomputed by the record store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub id: RecordId,
    pub status: ProgramStatus,
    pub app: Option<AppId>,
    title: Option<SingleLineText>,
    pub msg: Option<SingleLineText>,
}

impl Record {
    pub fn title(&self) -> Option<SingleLineRef<'_>> {
        self.title.as_ref().map(SingleLineText::as_ref)
    }

    pub fn as_ref(&self) -> RecordRef<'_> {
        RecordRef {
            id: self.id.as_ref(),
            status: self.status,
            app: self.app.as_ref().map(AppId::as_ref),
            title: self.title.as_ref().map(SingleLineText::as_ref),
            msg: self.msg.as_ref().map(SingleLineText::as_ref),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgramCommandRef<'a> {
    Replace(RecordRef<'a>),
    Clear(RecordIdRef<'a>),
    Query,
}

impl ProgramCommandRef<'_> {
    pub fn into_owned(self) -> ProgramCommand {
        match self {
            Self::Replace(record) => ProgramCommand::Replace(record.into_owned()),
            Self::Clear(id) => ProgramCommand::Clear(id.into_owned()),
            Self::Query => ProgramCommand::Query,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProgramCommand {
    Replace(Record),
    Clear(RecordId),
    Query,
}

impl ProgramCommand {
    pub fn as_ref(&self) -> ProgramCommandRef<'_> {
        match self {
            Self::Replace(record) => ProgramCommandRef::Replace(record.as_ref()),
            Self::Clear(id) => ProgramCommandRef::Clear(id.as_ref()),
            Self::Query => ProgramCommandRef::Query,
        }
    }
}

macro_rules! numeric_id {
    ($name:ident, $inner:ty) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name($inner);

        impl $name {
            pub const fn new(value: $inner) -> Self {
                Self(value)
            }
            pub const fn get(self) -> $inner {
                self.0
            }
        }
    };
}

numeric_id!(PaneId, u32);
numeric_id!(SessionId, u32);
numeric_id!(ConnectionEpoch, u64);
numeric_id!(ObservationEpoch, u64);
numeric_id!(ObservationSequence, u64);

fn parse_tmux_id(value: &str, prefix: char) -> Option<u32> {
    let digits = value.strip_prefix(prefix)?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

impl PaneId {
    pub fn parse(value: &str) -> Result<Self, ValueError> {
        parse_tmux_id(value, '%')
            .map(Self)
            .ok_or(ValueError::InvalidPaneId)
    }
}

impl SessionId {
    pub fn parse(value: &str) -> Result<Self, ValueError> {
        parse_tmux_id(value, '$')
            .map(Self)
            .ok_or(ValueError::InvalidSessionId)
    }
}

/// Terminal history lifetime, separate from agent departure and observation loss.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalLifecycle {
    FullReset,
    ShellPrompt,
    AttachedProcessExit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObservationGap {
    Disconnected,
    OwnerChanged,
    Paused,
    MalformedTransport,
    FramingLoss,
    QueueOverflow,
    BudgetExceeded,
    WorkBudgetExceeded,
}

/// Queue payloads own only validated reports, never ordinary terminal output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProgramEvent {
    Command(ProgramCommand),
    Lifecycle(TerminalLifecycle),
    Gap(ObservationGap),
}

/// Sequence numbers order events within one connection epoch. Epochs never splice fragments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservedEvent {
    pub pane: PaneId,
    pub connection: ConnectionEpoch,
    pub sequence: ObservationSequence,
    pub event: ProgramEvent,
}
