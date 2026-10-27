//! Private persistence protocol. These values do not extend the public state/source grammar.
//!
//! A lease proves collector liveness, not record eligibility. Runtime must independently
//! recheck the process ancestry and current observation stream before publication.

use std::collections::HashMap;

use crate::program_status::{
    EpisodeId, ProgramResult, RecordId, ResultGeneration, ResultIdentity, ResultKind, WorkCycle,
};
use crate::snapshot::{ProcessBirth, ProcessIdentity};
use crate::stamp::{ReadResult, StampedState};

pub mod opt {
    pub const COLLECTOR: &str = "@tma_program_collector";
    pub const ORIGIN: &str = "@tma_program_origin";
    pub const OWNER: &str = "@tma_program_owner";
    pub const COMMIT: &str = "@tma_program_commit";
    pub const RESULT: &str = "@tma_program_result";
    pub const BLOCKED_HISTORY: &str = "@tma_blocked_history";
    pub const INVALIDATED: &str = "@tma_result_invalidated";
    pub const ACK: &str = "@tma_program_ack";
    pub const NOTIFIED: &str = "@tma_program_notified";
    pub const RECEIPT: &str = "@tma_write_receipt";
    pub const ATTENTION_EPISODE: &str = "@tma_attention_episode";
    pub const EPISODE_NOTIFIED: &str = "@tma_episode_notified";
    pub const METADATA_UPDATE: &str = "@tma_metadata_update";
    pub const MARKER_CLAIM: &str = "@tma_marker_claim";
    pub const METADATA_OWNER: &str = "@tma_metadata_owner";
    pub const PANE_KEYS: &[&str] = &[
        ORIGIN,
        OWNER,
        COMMIT,
        RESULT,
        INVALIDATED,
        BLOCKED_HISTORY,
        ACK,
        NOTIFIED,
        RECEIPT,
        METADATA_OWNER,
        ATTENTION_EPISODE,
        EPISODE_NOTIFIED,
        MARKER_CLAIM,
        METADATA_UPDATE,
    ];
}

pub const LEASE_RENEW_MS: u64 = 5_000;
pub const LEASE_DURATION_MS: u64 = 15_000;
pub const LEASE_FUTURE_MS: u64 = 17_000;
pub const CLOCK_SLACK_MS: u64 = 2_000;

const MAX_PROCESS_BYTES: usize = 10 + 3 + 20 + 1 + 6;
const MAX_CHAIN_BYTES: usize = 256 * (MAX_PROCESS_BYTES + 1) - 1;
const MAX_OWNER_BYTES: usize = 32 + 1 + 20 + 1 + 20 + 1 + MAX_CHAIN_BYTES;
const MAX_RESULT_BYTES: usize =
    MAX_OWNER_BYTES + crate::program_status::MAX_ID_BYTES + 20 + 20 + 5 + 4;

/// Kernel-random 128-bit identity, encoded without tmux format or command metacharacters.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Token([u8; 32]);

impl Token {
    pub fn parse(value: &str) -> Option<Self> {
        (value.len() == 32
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
        .then(|| Self(value.as_bytes().try_into().expect("length checked above")))
    }

    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.0).expect("Token contains validated ASCII hex")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollectorLease {
    pub token: Token,
    pub expires_at: u64,
}

impl CollectorLease {
    pub fn encode(&self) -> String {
        format!("{}:{}", self.expires_at, self.token.as_str())
    }

    pub fn parse(raw: &str) -> Option<Self> {
        if raw.len() > 20 + 1 + 32 {
            return None;
        }
        let (expiry, token) = raw.split_once(':')?;
        Some(Self {
            token: Token::parse(token)?,
            expires_at: number(expiry)?,
        })
    }

    pub fn live_at(&self, now: u64) -> bool {
        self.expires_at > now && self.expires_at <= now.saturating_add(LEASE_FUTURE_MS)
    }
}

fn number(value: &str) -> Option<u64> {
    if value.is_empty() || value.len() > 20 || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: u64 = value.parse().ok()?;
    (n.to_string() == value).then_some(n)
}

fn process_fields(process: ProcessIdentity) -> String {
    match process.birth {
        ProcessBirth::LinuxStartTicks(ticks) => format!("{}:l:{ticks}", process.pid),
        ProcessBirth::MacOsStart {
            seconds,
            microseconds,
        } => {
            format!("{}:m:{seconds}:{microseconds}", process.pid)
        }
    }
}

fn parse_process(fields: &mut std::str::Split<'_, char>) -> Option<ProcessIdentity> {
    let pid = u32::try_from(number(fields.next()?)?).ok()?;
    if pid == 0 {
        return None;
    }
    let birth = match fields.next()? {
        "l" => ProcessBirth::LinuxStartTicks(number(fields.next()?)?),
        "m" => {
            let seconds = number(fields.next()?)?;
            let microseconds = u32::try_from(number(fields.next()?)?).ok()?;
            if microseconds >= 1_000_000 {
                return None;
            }
            ProcessBirth::MacOsStart {
                seconds,
                microseconds,
            }
        }
        _ => return None,
    };
    Some(ProcessIdentity { pid, birth })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessChain(Vec<ProcessIdentity>);

impl ProcessChain {
    pub fn new(chain: Vec<ProcessIdentity>) -> Option<Self> {
        if chain.is_empty() || chain.len() > 256 || chain.iter().any(|p| p.pid == 0
            || matches!(p.birth, ProcessBirth::MacOsStart { microseconds, .. } if microseconds >= 1_000_000))
            || chain.iter().enumerate().any(|(i, p)| chain[..i].iter().any(|prior| prior.pid == p.pid)) {
            return None;
        }
        Some(Self(chain))
    }
    pub fn as_slice(&self) -> &[ProcessIdentity] {
        &self.0
    }
    pub fn agent(&self) -> ProcessIdentity {
        self.0[self.0.len() - 1]
    }
    fn encode(&self) -> String {
        self.0
            .iter()
            .copied()
            .map(process_fields)
            .collect::<Vec<_>>()
            .join("~")
    }
    fn parse(raw: &str) -> Option<Self> {
        if raw.len() > MAX_CHAIN_BYTES {
            return None;
        }
        let chain = raw
            .split('~')
            .take(257)
            .map(|p| {
                let mut f = p.split(':');
                let process = parse_process(&mut f)?;
                f.next().is_none().then_some(process)
            })
            .collect::<Option<Vec<_>>>()?;
        Self::new(chain)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaneOwner {
    pub collector: Token,
    pub episode: EpisodeId,
    /// Complete pane-root-to-agent lineage, including every intermediate birth.
    pub processes: ProcessChain,
    pub stream_epoch: u64,
}

impl PaneOwner {
    pub fn encode(&self) -> String {
        format!(
            "{}:{}:{}:{}",
            self.collector.as_str(),
            self.episode.get(),
            self.stream_epoch,
            self.processes.encode()
        )
    }

    pub fn parse(raw: &str) -> Option<Self> {
        if raw.len() > MAX_OWNER_BYTES {
            return None;
        }
        let mut f = raw.splitn(4, ':');
        let collector = Token::parse(f.next()?)?;
        let episode = EpisodeId::new(number(f.next()?)?);
        let stream_epoch = number(f.next()?)?;
        let processes = ProcessChain::parse(f.next()?)?;
        if episode.get() == 0 || stream_epoch == 0 {
            return None;
        }
        Some(Self {
            collector,
            episode,
            processes,
            stream_epoch,
        })
    }
}

/// Last-write marker binds the unique write id to the owner it actually committed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitMarker {
    pub write: Token,
    pub owner: PaneOwner,
}

impl CommitMarker {
    pub fn encode(&self) -> String {
        format!("{}|{}", self.write.as_str(), self.owner.encode())
    }

    pub fn parse(raw: &str) -> Option<Self> {
        if raw.len() > 32 + 1 + MAX_OWNER_BYTES {
            return None;
        }
        let (write, owner) = raw.split_once('|')?;
        Some(Self {
            write: Token::parse(write)?,
            owner: PaneOwner::parse(owner)?,
        })
    }
}

/// Historical normalized result. This alone never restores live authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PersistedResult {
    pub owner: PaneOwner,
    pub identity: ResultIdentity,
    pub cycle: WorkCycle,
    pub kind: ResultKind,
}

impl PersistedResult {
    pub fn new(owner: PaneOwner, result: &ProgramResult) -> Option<Self> {
        (owner.episode == result.identity.owner).then(|| Self {
            owner,
            identity: result.identity.clone(),
            cycle: result.cycle,
            kind: result.kind,
        })
    }

    pub fn encode(&self) -> String {
        format!(
            "{}|{}|{}|{}|{}",
            self.owner.encode(),
            self.identity.record.as_ref().as_str(),
            self.identity.generation.get(),
            self.cycle.get(),
            match self.kind {
                ResultKind::Completion => "done",
                ResultKind::Failure => "error",
            }
        )
    }

    pub fn parse(raw: &str) -> Option<Self> {
        if raw.len() > MAX_RESULT_BYTES {
            return None;
        }
        let mut f = raw.split('|');
        let owner = PaneOwner::parse(f.next()?)?;
        let record = match f.next()? {
            "" => RecordId::root(),
            r => RecordId::parse(r).ok()?,
        };
        let generation = ResultGeneration::new(number(f.next()?)?);
        let cycle = WorkCycle::new(number(f.next()?)?);
        let kind = match f.next()? {
            "done" => ResultKind::Completion,
            "error" => ResultKind::Failure,
            _ => return None,
        };
        if f.next().is_some() || generation.get() == 0 {
            return None;
        }
        Some(Self {
            identity: ResultIdentity {
                owner: owner.episode,
                record,
                generation,
            },
            owner,
            cycle,
            kind,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotificationClaim {
    pub token: Token,
    pub result: PersistedResult,
}

impl NotificationClaim {
    pub fn encode(&self) -> String {
        format!("{}|{}", self.token.as_str(), self.result.encode())
    }
    pub fn parse(raw: &str) -> Option<Self> {
        if raw.len() > 32 + 1 + MAX_RESULT_BYTES {
            return None;
        }
        let (token, result) = raw.split_once('|')?;
        Some(Self {
            token: Token::parse(token)?,
            result: PersistedResult::parse(result)?,
        })
    }
}

/// Only normalized consumption markers are transferred during independently validated recovery.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResultHistory {
    Retain,
    Reset,
    Rebind { acknowledged: bool, notified: bool },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HistoryInvalidation {
    All,
    Subtree(RecordId),
}
impl HistoryInvalidation {
    pub fn affects(&self, result: &PersistedResult) -> bool {
        match self {
            Self::All => true,
            Self::Subtree(id) => id.as_ref().contains(result.identity.record.as_ref()),
        }
    }
}

/// Nonterminal history survives collector release, without proving current authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockedHistory {
    pub processes: ProcessChain,
    pub episode: Token,
    pub acknowledged: bool,
    pub record: RecordId,
}
impl BlockedHistory {
    pub fn identity(&self) -> String {
        format!(
            "blocked|{}|{}",
            self.processes.encode(),
            self.episode.as_str()
        )
    }
    pub fn encode(&self) -> String {
        format!(
            "{}|{}|{}|{}",
            self.processes.encode(),
            self.episode.as_str(),
            u8::from(self.acknowledged),
            self.record.as_ref().as_str()
        )
    }
    pub fn parse(raw: &str) -> Option<Self> {
        if raw.len() > MAX_CHAIN_BYTES + 36 + crate::program_status::MAX_ID_BYTES {
            return None;
        }
        let mut fields = raw.split('|');
        let processes = ProcessChain::parse(fields.next()?)?;
        let episode = Token::parse(fields.next()?)?;
        let acknowledged = match fields.next()? {
            "0" => false,
            "1" => true,
            _ => return None,
        };
        let record = match fields.next()? {
            "" => RecordId::root(),
            record => RecordId::parse(record).ok()?,
        };
        if fields.next().is_some() {
            return None;
        }
        Some(Self {
            processes,
            episode,
            acknowledged,
            record,
        })
    }
}

/// Stable presentation episode, independent of transaction receipts and duplicate commits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ObservationIdentity {
    Result(PersistedResult),
    Blocked {
        owner: PaneOwner,
        episode: Token,
    },
    Legacy {
        blocked: Option<BlockedHistory>,
        episode: Option<Token>,
        pid: u32,
        state: crate::AgentState,
        source: crate::Provenance,
        since: u64,
        turn_at: u64,
    },
}

impl ObservationIdentity {
    pub fn encode(&self) -> String {
        match self {
            Self::Result(result) => format!("result|{}", result.encode()),
            Self::Blocked { owner, episode } => {
                format!("blocked|{}|{}", owner.processes.encode(), episode.as_str())
            }
            Self::Legacy {
                blocked: Some(history),
                ..
            } => history.identity(),
            Self::Legacy {
                episode: Some(episode),
                ..
            } => format!("legacy|{}", episode.as_str()),
            Self::Legacy {
                episode: None,
                pid,
                state,
                source: _,
                since,
                turn_at,
                ..
            } => format!("legacy||{pid}|{}|{since}|{turn_at}", state.token()),
        }
    }
}

/// Bounded hook-dispatch witness. It fences the later handler read, which then captures the
/// existing typed observation. It grants no authority and carries no public row JSON field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttentionRevision {
    session: crate::program_status::SessionId,
    pane: crate::program_status::PaneId,
    receipt: Option<Token>,
    episode: Option<Token>,
    commit: Option<Token>,
    origin: bool,
    state: crate::AgentState,
    source: crate::Provenance,
    pid: u32,
    since: u64,
    turn: u64,
}

impl AttentionRevision {
    pub fn parse(raw: &str) -> Option<Self> {
        if raw.len() > 256 {
            return None;
        }
        let mut fields = raw.split('|');
        let session = crate::program_status::SessionId::parse(fields.next()?).ok()?;
        let pane = crate::program_status::PaneId::parse(fields.next()?).ok()?;
        let optional_token = |raw: &str| {
            if raw.is_empty() {
                Some(None)
            } else {
                Token::parse(raw).map(Some)
            }
        };
        let receipt = optional_token(fields.next()?)?;
        let episode = optional_token(fields.next()?)?;
        let commit = optional_token(fields.next()?)?;
        let origin = match fields.next()? {
            "" => false,
            "1" => true,
            _ => return None,
        };
        let state = fields.next()?.parse().ok()?;
        let source = match fields.next()? {
            "" => crate::Provenance::Capture,
            raw => raw.parse().ok()?,
        };
        let epoch = |raw: &str| if raw.is_empty() { Some(0) } else { number(raw) };
        let pid = u32::try_from(epoch(fields.next()?)?).ok()?;
        let since = epoch(fields.next()?)?;
        let turn = epoch(fields.next()?)?;
        if fields.next()? != "1" || fields.next().is_some() {
            return None;
        }
        Some(Self {
            session,
            pane,
            receipt,
            episode,
            commit,
            origin,
            state,
            source,
            pid,
            since,
            turn,
        })
    }

    pub const fn pane(&self) -> crate::program_status::PaneId {
        self.pane
    }
    pub const fn session(&self) -> crate::program_status::SessionId {
        self.session
    }

    /// The complete COMMIT is still validated at readback. Its first 32 bytes are the unique
    /// write token, keeping the shell witness small even with maximum process ancestry.
    pub fn capture_format() -> String {
        Self::capture_fields(false).join("|")
    }

    /// tmux quotes only flat lookups with q, not nested expanded strings. Quote each bounded
    /// component separately and concatenate literal shell-quoted separators.
    pub fn capture_shell_format() -> String {
        Self::capture_fields(true).join("'|'")
    }

    fn capture_fields(shell: bool) -> Vec<String> {
        let lookup = |key| {
            if shell {
                format!("#{{q:{key}}}")
            } else {
                format!("#{{{key}}}")
            }
        };
        let bounded = |key, max, chars| {
            format!(
                "#{{?#{{&&:#{{==:#{{m/r:[^{chars}],#{{{key}}}}},0}},#{{e|<=:#{{n:{key}}},{max}}}}},{},!}}",
                lookup(key)
            )
        };
        let prefix = format!("#{{=32:{}}}", opt::COMMIT);
        let commit = format!(
            "#{{?#{{==:#{{{key}}},}},,#{{?#{{&&:#{{==:#{{m/r:[^0-9a-f],{prefix}}},0}},#{{&&:#{{==:#{{n:{prefix}}},32}},#{{e|<=:#{{n:{key}}},{max}}}}}}},#{{=32{quote}:{key}}},!}}}}",
            key = opt::COMMIT, max = 33 + MAX_OWNER_BYTES, quote = if shell { ";q" } else { "" },
        );
        vec![
            lookup("session_id"),
            lookup("pane_id"),
            bounded(opt::RECEIPT, 32, "0-9a-f"),
            bounded(opt::ATTENTION_EPISODE, 32, "0-9a-f"),
            commit,
            bounded(opt::ORIGIN, 1, "0-9"),
            bounded(crate::stamp::opt::STATE, 7, "a-z"),
            bounded(crate::stamp::opt::SOURCE, 8, "a-z"),
            bounded(crate::stamp::opt::PID, 10, "0-9"),
            bounded(crate::stamp::opt::SINCE, 20, "0-9"),
            bounded(crate::stamp::opt::TURN_AT, 20, "0-9"),
            bounded(crate::stamp::opt::ATTENTION, 1, "0-9"),
        ]
    }

    pub fn matches(&self, options: &HashMap<String, String>) -> bool {
        let value = |key: &str| options.get(key).map_or("", String::as_str);
        let token = |expected: &Option<Token>, raw: &str| {
            raw == expected.as_ref().map_or("", Token::as_str)
        };
        let commit_matches = if value(opt::COMMIT).is_empty() {
            self.commit.is_none()
        } else {
            CommitMarker::parse(value(opt::COMMIT))
                .is_some_and(|marker| Some(marker.write) == self.commit)
        };
        let epoch = |key| {
            if value(key).is_empty() {
                Some(0)
            } else {
                number(value(key))
            }
        };
        token(&self.receipt, value(opt::RECEIPT))
            && token(&self.episode, value(opt::ATTENTION_EPISODE))
            && commit_matches
            && value(opt::ORIGIN) == if self.origin { "1" } else { "" }
            && value(crate::stamp::opt::STATE) == self.state.token()
            && (value(crate::stamp::opt::SOURCE) == self.source.token()
                || value(crate::stamp::opt::SOURCE).is_empty()
                    && self.source == crate::Provenance::Capture)
            && epoch(crate::stamp::opt::PID) == Some(u64::from(self.pid))
            && epoch(crate::stamp::opt::SINCE) == Some(self.since)
            && epoch(crate::stamp::opt::TURN_AT) == Some(self.turn)
            && value(crate::stamp::opt::ATTENTION) == "1"
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetadataEvidence {
    Context(u64),
    Quota(u64),
}
impl MetadataEvidence {
    pub fn key(self) -> &'static str {
        match self {
            Self::Context(_) => crate::stamp::opt::CONTEXT_AT,
            Self::Quota(_) => crate::stamp::opt::QUOTA_AT,
        }
    }
    pub fn at(self) -> u64 {
        match self {
            Self::Context(at) | Self::Quota(at) => at,
        }
    }
}

/// An advisory update supersedes an older yielding suffix, even for the same metadata owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MetadataUpdate {
    pub token: Token,
    pub evidence: Option<MetadataEvidence>,
}
impl MetadataUpdate {
    pub fn encode(&self) -> String {
        let (lane, at) = match self.evidence {
            Some(MetadataEvidence::Context(at)) => ("c", at),
            Some(MetadataEvidence::Quota(at)) => ("q", at),
            None => ("n", 0),
        };
        format!("{}|{lane}|{at}", self.token.as_str())
    }
    pub fn parse(raw: &str) -> Option<Self> {
        if raw.len() > 56 {
            return None;
        }
        let mut fields = raw.split('|');
        let token = Token::parse(fields.next()?)?;
        let lane = fields.next()?;
        let at = number(fields.next()?)?;
        if fields.next().is_some() {
            return None;
        }
        let evidence = match lane {
            "c" => Some(MetadataEvidence::Context(at)),
            "q" => Some(MetadataEvidence::Quota(at)),
            "n" if at == 0 => None,
            _ => return None,
        };
        Some(Self { token, evidence })
    }
}

/// Fields with a process/session lifetime separate from state arbitration.
pub const METADATA_KEYS: &[&str] = &[
    crate::stamp::opt::SESSION,
    crate::stamp::opt::TRANSCRIPT,
    crate::stamp::opt::SUBAGENTS,
    crate::stamp::opt::CONTEXT_PCT,
    crate::stamp::opt::CONTEXT_AT,
    crate::stamp::opt::TOKENS,
    crate::stamp::opt::TOKENS_AT,
    crate::stamp::opt::QUOTA_PCT,
    crate::stamp::opt::QUOTA_AT,
    crate::stamp::opt::QUOTA_WINDOW,
    crate::stamp::opt::QUOTA_RESETS_AT,
    crate::stamp::opt::COST_USD,
    crate::stamp::opt::MODEL,
    crate::stamp::opt::API_ENDPOINT,
    crate::stamp::opt::PERMISSION_REQUEST,
    crate::stamp::opt::QUESTION_REQUEST,
    crate::stamp::opt::PENDING_TOOL,
    crate::stamp::opt::PENDING_CALL,
    crate::stamp::opt::PENDING_SUMMARY,
    crate::stamp::opt::ACT_REPEAT,
];

/// Independent hook/API metadata lifetime, separate from state publication arbitration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MetadataOwner {
    pub process: ProcessIdentity,
    pub session: String,
}

impl MetadataOwner {
    pub fn encode(&self) -> String {
        let hex: String = self.session.bytes().map(|b| format!("{b:02x}")).collect();
        format!("{}|{hex}", process_fields(self.process))
    }
    pub fn parse(raw: &str) -> Option<Self> {
        if raw.len() > MAX_PROCESS_BYTES + 1 + 2048 {
            return None;
        }
        let (process, session) = raw.split_once('|')?;
        let mut f = process.split(':');
        let process = parse_process(&mut f)?;
        if f.next().is_some()
            || session.is_empty()
            || session.len() > 2048
            || session.len() % 2 != 0
            || !session
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return None;
        }
        let bytes = (0..session.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&session[i..i + 2], 16).ok())
            .collect::<Option<Vec<_>>>()?;
        let session = String::from_utf8(bytes).ok()?;
        if session.chars().any(char::is_control) {
            return None;
        }
        Some(Self { process, session })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProjectionRead {
    Legacy(Option<ReadResult>),
    Live {
        stamp: StampedState,
        owner: PaneOwner,
        commit: CommitMarker,
    },
    /// Keep the history available to recovery code, but never treat its hook token as authority.
    Stale,
}

/// Parsed once at the options boundary. Results and their consumption markers are historical
/// facts; only `projection` can establish the separate current-lease validity fact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProgramMetadata {
    pub program_origin: bool,
    pub origin_complete: bool,
    pub owner: Option<PaneOwner>,
    pub commit: Option<CommitMarker>,
    pub lease: Option<CollectorLease>,
    pub result: Option<PersistedResult>,
    pub invalidated: Option<PersistedResult>,
    pub acknowledged: Option<PersistedResult>,
    pub notified: Option<NotificationClaim>,
}

impl ProgramMetadata {
    pub fn from_options(options: &HashMap<String, String>) -> Self {
        Self {
            program_origin: [opt::ORIGIN, opt::OWNER, opt::COMMIT]
                .iter()
                .any(|key| options.get(*key).is_some_and(|v| !v.is_empty())),
            origin_complete: options.get(opt::ORIGIN).map(String::as_str) == Some("1"),
            owner: options.get(opt::OWNER).and_then(|s| PaneOwner::parse(s)),
            commit: options
                .get(opt::COMMIT)
                .and_then(|s| CommitMarker::parse(s)),
            lease: options
                .get(opt::COLLECTOR)
                .and_then(|s| CollectorLease::parse(s)),
            result: options
                .get(opt::RESULT)
                .and_then(|s| PersistedResult::parse(s)),
            invalidated: options
                .get(opt::INVALIDATED)
                .and_then(|raw| PersistedResult::parse(raw)),
            acknowledged: options
                .get(opt::ACK)
                .and_then(|s| PersistedResult::parse(s)),
            notified: options
                .get(opt::NOTIFIED)
                .and_then(|s| NotificationClaim::parse(s)),
        }
    }

    /// Presentation identity from validated persisted truth. Public row JSON cannot construct
    /// this proof; UI consumers carry it privately and verify it again before acknowledgement.
    pub fn attention_observation(
        &self,
        options: &HashMap<String, String>,
        now: u64,
    ) -> Option<(StampedState, ObservationIdentity)> {
        let value = |key: &str| options.get(key).map_or("", String::as_str);
        match self.projection(options, now).ok()? {
            ProjectionRead::Stale => None,
            ProjectionRead::Live { stamp, owner, .. } => {
                let identity = if stamp.state == crate::AgentState::Blocked {
                    ObservationIdentity::Blocked {
                        owner,
                        episode: Token::parse(value(opt::ATTENTION_EPISODE))?,
                    }
                } else if stamp.state == crate::AgentState::Idle && stamp.attention {
                    let result = self.result.clone()?;
                    if result.owner != owner {
                        return None;
                    }
                    ObservationIdentity::Result(result)
                } else {
                    return None;
                };
                Some((stamp, identity))
            }
            ProjectionRead::Legacy(Some(stamp)) => {
                let stamp = stamp.into_inner();
                let blocked =
                    BlockedHistory::parse(value(opt::BLOCKED_HISTORY)).filter(|history| {
                        stamp.state == crate::AgentState::Blocked
                            && value(opt::ATTENTION_EPISODE) == history.episode.as_str()
                    });
                let identity = ObservationIdentity::Legacy {
                    blocked,
                    episode: Token::parse(value(opt::ATTENTION_EPISODE)),
                    pid: stamp.pid,
                    state: stamp.state,
                    source: stamp.source,
                    since: stamp.since,
                    turn_at: stamp.turn_at,
                };
                Some((stamp, identity))
            }
            ProjectionRead::Legacy(None) => None,
        }
    }

    pub fn projection(
        &self,
        options: &HashMap<String, String>,
        now: u64,
    ) -> Result<ProjectionRead, crate::GrammarError> {
        if !self.program_origin {
            return StampedState::from_options(options).map(ProjectionRead::Legacy);
        }
        let (Some(owner), Some(commit), Some(lease)) = (&self.owner, &self.commit, &self.lease)
        else {
            return Ok(ProjectionRead::Stale);
        };
        if !self.origin_complete
            || &commit.owner != owner
            || lease.token != owner.collector
            || !lease.live_at(now)
        {
            return Ok(ProjectionRead::Stale);
        }
        match StampedState::from_options(options)? {
            Some(ReadResult::Settled(stamp))
                if stamp.pid == owner.processes.agent().pid
                    && stamp.source == crate::Provenance::Hook =>
            {
                Ok(ProjectionRead::Live {
                    stamp,
                    owner: owner.clone(),
                    commit: commit.clone(),
                })
            }
            _ => Ok(ProjectionRead::Stale),
        }
    }
}

/// Decode once with the server lease and reader clock at the options boundary.
pub fn read_projection(
    options: &HashMap<String, String>,
    now: u64,
) -> Result<ProjectionRead, crate::GrammarError> {
    ProgramMetadata::from_options(options).projection(options, now)
}

/// Public tuple with current protocol authority checked. Old valid program reports remain live
/// while their collector lease is healthy; stale private origins cannot become legacy hooks.
pub fn current_stamp(
    options: &HashMap<String, String>,
    now: u64,
) -> Result<Option<ReadResult>, crate::GrammarError> {
    match read_projection(options, now)? {
        ProjectionRead::Legacy(read) => Ok(read),
        ProjectionRead::Live { stamp, .. } => Ok(Some(ReadResult::Settled(stamp))),
        ProjectionRead::Stale => Ok(None),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeaseAction {
    Wait,
    Renew { expires_at: u64 },
    Revoke,
}

/// The monotonic and wall clocks sampled at one lease IO boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LeaseTime {
    pub monotonic_ms: u64,
    pub epoch_ms: u64,
}

/// Scheduling state advances only after a confirmed lease write. Revocation is terminal:
/// callers release all panes and start a fresh collector token before trying again.
#[derive(Clone, Debug)]
pub struct LeaseSchedule {
    last: LeaseTime,
    revoked: bool,
}

impl LeaseSchedule {
    pub fn committed(at: LeaseTime) -> Self {
        Self {
            last: at,
            revoked: false,
        }
    }
    pub fn action(&mut self, at: LeaseTime) -> LeaseAction {
        let elapsed = at.monotonic_ms.checked_sub(self.last.monotonic_ms);
        let wall = at.epoch_ms.checked_sub(self.last.epoch_ms);
        if self.revoked
            || elapsed.is_none()
            || wall.is_none()
            || elapsed.unwrap_or(0) >= LEASE_DURATION_MS
            || elapsed.unwrap_or(0).abs_diff(wall.unwrap_or(0)) > CLOCK_SLACK_MS
        {
            self.revoked = true;
            return LeaseAction::Revoke;
        }
        if elapsed.unwrap_or(0) < LEASE_RENEW_MS {
            LeaseAction::Wait
        } else {
            LeaseAction::Renew {
                expires_at: at.epoch_ms.saturating_add(LEASE_DURATION_MS),
            }
        }
    }
    /// Confirm only a successful write whose expiry used `submission.epoch_ms`. Validate
    /// continuity at IO completion, then keep the original deadline anchor, not a later one.
    pub fn confirm(&mut self, submission: LeaseTime, completion: LeaseTime) -> bool {
        let elapsed = completion.monotonic_ms.checked_sub(submission.monotonic_ms);
        let wall = completion.epoch_ms.checked_sub(submission.epoch_ms);
        if elapsed.is_none()
            || wall.is_none()
            || elapsed.unwrap_or(0).abs_diff(wall.unwrap_or(0)) > CLOCK_SLACK_MS
        {
            self.revoked = true;
            return false;
        }
        if self.action(completion) == LeaseAction::Revoke {
            return false;
        }
        if matches!(self.action(submission), LeaseAction::Renew { .. }) {
            self.last = submission;
            true
        } else {
            false
        }
    }
}
