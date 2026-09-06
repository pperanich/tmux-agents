//! `tma serve --stdio`: one remote connection, one process.
//!
//! NDJSON requests on stdin, NDJSON responses and events on stdout, logs on stderr. There is no
//! listening socket, no TLS, no async runtime and no bearer token, because the SSH channel is the
//! transport: sshd authenticates the caller and the forced command passes `--device <id>`.
//!
//! **Serve trusts that argument and nothing in the stream.** The hello names a device too; that
//! field is the client's own claim and is used for a log line, never for authorization. Every scope
//! decision reads the host's device store.
//!
//! Three orderings are the design and are easy to lose:
//!
//! 1. **The device store is re-read per request and per publish**, memoized on the file's identity
//!    tuple. Absence of the *record* ends the connection: a revoked device is not a `read`-only
//!    device, and a subscription is a push with no request to gate.
//! 2. **The scope gate runs before the slot claim, which runs before `broker::fire`.** The scope
//!    says *this device may approve*; the slot says *this approval has not already been sent*; the
//!    binder, inside the broker and under the pane lock, says *the thing you approved is still on
//!    screen*. None of the three substitutes for another.
//! 3. **`force` is hardcoded `false` and no exec action is reachable.** A device never skips the
//!    `when` gate, and an exec action sends no keystrokes and so has no freshness guard at all.
//!    `tests/serve_source_guard.rs` reads this file for both.
//!
//! Exit codes: `0` on EOF or SIGTERM, `2` when the connection is refused (unknown device, revoked
//! device, or the connection cap), `1` when the host could not start.

mod conn;
mod rows;
mod wire;

use std::cell::Cell;
use std::io::{self, BufReader};
use std::path::PathBuf;
use std::process::ExitCode;

use tma_core::stamp::opt;
use tma_core::{ActionKind, ActionManifest, AgentRow};
use tma_proto::{
    CardRequest, Dispatch, ErrorCode, ErrorFrame, EventRequest, HelloOk, Outcome as WireOutcome,
    Reason, ReceiptsRequest, Request, RequestFrame, Response, ResponseFrame, Scope, Snapshot,
    SnapshotRequest, Subscribe, WindowRequest,
};
use tma_runtime::broker::{self, BrokerIo, Outcome, PaneFacts, Refusal};
use tma_runtime::card::{self, CardInputs, PendingQuestion};
use tma_runtime::config::Config;
use tma_runtime::cycle::{self, CycleReport};
use tma_runtime::device::{Cache, Store};
use tma_runtime::manifests::LoadedManifest;
use tma_runtime::origin::Origin;
use tma_runtime::serve_transcript::{self, PaneTranscript};
use tma_runtime::slots::{self, Claim, Ledger, ReceiptFilter};
use tma_runtime::subscribe::{run_stream, StreamParams, Tick};
use tma_runtime::{actions, hook_lane, ipc, repo, seen};
use tma_tmux::tmux::{Server, Tmux, TmuxError};
use tma_transcript as tx;
use tma_transcript::discovery::{self, StoreRoots};

use self::conn::{Connection, Refused};
use self::wire::{next_line, Incoming, Wire, MAX_LINE_BYTES};

/// Everything `tma serve` needs, assembled by the bin's dispatch from the CLI args and config.
pub(crate) struct ServeOpts {
    pub stdio: bool,
    pub device: String,
    pub server: Server,
    pub manifest_dir: Option<PathBuf>,
    pub config: Config,
    pub config_path: Option<PathBuf>,
}

pub(crate) fn run(opts: ServeOpts) -> ExitCode {
    let wire = Wire::new();
    if !opts.stdio {
        eprintln!("tma: serve speaks the protocol over stdio; pass --stdio");
        return ExitCode::from(2);
    }

    let store = match Store::at_config_dir() {
        Ok(store) => store,
        Err(err) => return refuse(&wire, ErrorCode::Internal, &err.to_string(), 1),
    };
    let mut cache = Cache::default();
    // The connection is refused before anything is read, so an unknown device never gets as far as
    // a handshake it could learn the fleet's shape from.
    let device = match cache.get(&store, &opts.device) {
        Ok(Some(device)) => device,
        Ok(None) => {
            return refuse(
                &wire,
                ErrorCode::ScopeDenied,
                "this device is not paired with this host; run `tma device pair` on the host",
                2,
            )
        }
        Err(err) => return refuse(&wire, ErrorCode::Internal, &err.to_string(), 1),
    };

    let runtime_dir = ipc::runtime_dir();
    let connection = match Connection::open(
        &runtime_dir,
        &opts.device,
        opts.config.serve.max_connections,
    ) {
        Ok(connection) => connection,
        Err(Refused::Full { max }) => {
            return refuse(
                &wire,
                ErrorCode::TooManyConnections,
                &format!("this host already has {max} serve connections open"),
                2,
            )
        }
        Err(Refused::Io(err)) => return refuse(&wire, ErrorCode::Internal, &err.to_string(), 1),
    };
    install_sigterm(connection.marker());

    let manifests = match crate::cli_support::load_manifests_or_exit(
        opts.manifest_dir.as_deref(),
        &opts.config.agent_overrides,
    ) {
        Ok(manifests) => manifests,
        Err(code) => return code,
    };
    let action_set = match actions::load(None) {
        Ok(set) => set,
        Err(err) => return refuse(&wire, ErrorCode::Internal, &err.to_string(), 1),
    };
    let ledger = match Ledger::at_runtime_dir() {
        Ok(ledger) => ledger,
        Err(err) => return refuse(&wire, ErrorCode::Internal, &err.to_string(), 1),
    };
    let tmux = Tmux::connect(&opts.server);
    let origin = Origin::resolve(&tmux);

    let mut session = Session {
        device_id: opts.device,
        scopes: device.scopes,
        store,
        cache,
        wire: wire.clone(),
        tmux,
        origin,
        manifests,
        action_set,
        ledger,
        server: opts.server,
        config: opts.config,
        manifest_dir: opts.manifest_dir,
        config_path: opts.config_path,
        marker: connection.marker(),
        reader: tx::Reader::new(),
        greeted: false,
        streaming: false,
    };

    let mut stdin = BufReader::new(io::stdin());
    loop {
        match next_line(&mut stdin, MAX_LINE_BYTES) {
            // EOF: the caller hung up. That is the ordinary end of a connection, not a failure.
            Ok(Incoming::Eof) => return ExitCode::SUCCESS,
            Ok(Incoming::Line(line)) => {
                if let Flow::Stop(code) = session.on_line(&line) {
                    return code;
                }
            }
            Ok(Incoming::TooLong) => session.reply_untyped(
                ErrorCode::BadRequest,
                &format!("a request line may not exceed {MAX_LINE_BYTES} bytes"),
            ),
            Ok(Incoming::NotUtf8) => {
                session.reply_untyped(ErrorCode::BadRequest, "a request line must be UTF-8")
            }
            // The pipe broke under us. Nothing left to answer to.
            Err(err) => {
                eprintln!("tma serve: reading the request stream: {err}");
                return ExitCode::SUCCESS;
            }
        }
    }
}

/// How many transcript events a card reads to infer the call a pane is blocked on. A page, not a
/// window: the correlation only has to reach back past the newest unresolved call.
const TAIL_EVENTS: usize = 20;

/// Whether the request loop keeps going.
enum Flow {
    Continue,
    Stop(ExitCode),
}

/// Write one typed refusal and exit with `code`. The frame carries id `"0"`: the refusal happens
/// before any request was read, so there is no correlation id to echo.
fn refuse(wire: &Wire, code: ErrorCode, message: &str, exit: u8) -> ExitCode {
    eprintln!("tma serve: {message}");
    let _ = wire.send(&ResponseFrame::new(
        "0",
        Response::Error(ErrorFrame::new(code, message)),
    ));
    ExitCode::from(exit)
}

/// Exit cleanly on SIGTERM: drop this connection's registry marker and stop. A handler thread
/// rather than a flag, because `BufRead` retries on `EINTR` and would swallow the interruption.
fn install_sigterm(marker: PathBuf) {
    let Ok(mut signals) = signal_hook::iterator::Signals::new([signal_hook::consts::SIGTERM])
    else {
        return;
    };
    std::thread::spawn(move || {
        if signals.forever().next().is_some() {
            let _ = std::fs::remove_file(&marker);
            std::process::exit(0);
        }
    });
}

/// One connection's state. Single-threaded apart from the subscription stream, which owns its own
/// tmux handle and writes through the shared [`Wire`].
struct Session {
    device_id: String,
    /// Refreshed from the store before every request, never trusted from one request to the next.
    scopes: Vec<Scope>,
    store: Store,
    cache: Cache,
    wire: Wire,
    tmux: Tmux,
    origin: Origin,
    manifests: Vec<LoadedManifest>,
    action_set: Vec<ActionManifest>,
    ledger: Ledger,
    server: Server,
    config: Config,
    manifest_dir: Option<PathBuf>,
    config_path: Option<PathBuf>,
    /// This connection's registry marker, for the paths that exit without unwinding.
    marker: PathBuf,
    /// One transcript reader for the life of the connection, so its stat memo and its held
    /// OpenCode database handle survive between requests. A reader per request would reconnect to
    /// the database on every page, which is what makes the writing agent's own commits fail (E2).
    reader: tx::Reader,
    greeted: bool,
    streaming: bool,
}

impl Session {
    fn on_line(&mut self, line: &str) -> Flow {
        let frame: RequestFrame = match tma_proto::decode(line) {
            Ok(frame) => frame,
            Err(err) => {
                // A line that does not parse still deserves a typed refusal on the same stream: a
                // dropped connection is what a client cannot tell from a network fault.
                eprintln!("tma serve: unreadable request: {err}");
                self.reply_untyped(ErrorCode::BadRequest, "this frame could not be read");
                return Flow::Continue;
            }
        };

        // R48: the authorization record is re-read per request. Absence of the record ends the
        // connection; a store that momentarily cannot be read refuses this one request instead,
        // because an I/O error is not a revocation and must not read as one.
        match self.cache.get(&self.store, &self.device_id) {
            Ok(Some(device)) => self.scopes = device.scopes,
            Ok(None) => {
                self.send(
                    &frame.id,
                    error(ErrorCode::ScopeDenied, "this device was revoked"),
                );
                eprintln!("tma serve: the device record is gone; closing the connection");
                return Flow::Stop(ExitCode::from(2));
            }
            Err(err) => {
                self.send(&frame.id, error(ErrorCode::Internal, &err.to_string()));
                return Flow::Continue;
            }
        }

        if !self.greeted {
            // The version claim is judged before the shape, and the first frame must be the
            // handshake. Both rules live in the protocol crate so host and app cannot disagree.
            return match frame.accept_hello() {
                Ok(hello) => {
                    if hello.device != self.device_id {
                        eprintln!(
                            "tma serve: the hello names device {:?} and the connection is {:?}; \
                             the connection's own id is authoritative",
                            hello.device, self.device_id
                        );
                    }
                    eprintln!("tma serve: {} {} connected", hello.app, hello.app_version);
                    self.greeted = true;
                    let ok = HelloOk {
                        host: self.origin.host.clone(),
                        tma_version: env!("CARGO_PKG_VERSION").to_string(),
                        // The effective interval, after the floor, so the number the device is told
                        // is the cadence the host actually keeps to.
                        reconcile_interval_ms: self.config.serve.reconcile_interval().as_millis()
                            as u64,
                        scopes: self.scopes.clone(),
                    };
                    self.send(&frame.id, Response::Hello(ok));
                    Flow::Continue
                }
                Err(refusal) => {
                    self.send(&frame.id, Response::Error(refusal));
                    Flow::Continue
                }
            };
        }
        if frame.schema != tma_proto::SCHEMA {
            self.send(
                &frame.id,
                error(
                    ErrorCode::UnsupportedSchema,
                    &format!(
                        "this host speaks protocol schema {}; the frame claims {}",
                        tma_proto::SCHEMA,
                        frame.schema
                    ),
                ),
            );
            return Flow::Continue;
        }

        let response = match &frame.body {
            Request::Hello(_) => error(ErrorCode::BadRequest, "the handshake happens once"),
            Request::Snapshot(req) => self.on_snapshot(req),
            Request::Subscribe(req) => self.on_subscribe(&frame.id, req),
            Request::Dispatch(req) => self.on_dispatch(req),
            Request::Receipts(req) => self.on_receipts(req),
            Request::Card(req) => self.on_card(req),
            Request::Window(req) => self.on_window(req),
            Request::Event(req) => self.on_event(req),
        };
        self.send(&frame.id, response);
        Flow::Continue
    }

    // ---- read -------------------------------------------------------------------------------

    fn on_snapshot(&mut self, req: &SnapshotRequest) -> Response {
        match self.cycle() {
            Ok(rows) => {
                let selector = rows::selector(req.selector.as_ref());
                let mut rows = rows;
                selector.retain(&mut rows);
                match rows
                    .iter()
                    .map(|row| rows::fleet_row(row, &self.origin))
                    .collect::<Result<Vec<_>, _>>()
                {
                    Ok(agents) => Response::Snapshot(Snapshot { agents }),
                    Err(err) => error(ErrorCode::Internal, &format!("row: {err}")),
                }
            }
            Err(err) => error(ErrorCode::Internal, &err),
        }
    }

    /// One detection cycle, annotated and with the ordered-input clear DEFERRED. Deferred because a
    /// device reads `done` to decide something: an inline clear would retract the flag before the
    /// frame carrying it is written, so the completion is never reported at all.
    fn cycle(&mut self) -> Result<Vec<AgentRow>, String> {
        let report = cycle::run_cycle_with(
            &self.tmux,
            &self.manifests,
            &self.config.fold_config(),
            cycle::SeenClear::Deferred,
        )
        // A gone server is reported rather than answered with an empty fleet: "nothing is running"
        // and "I cannot see" are different facts, and only one of them is worth retrying.
        .map_err(|err| match err {
            tma_tmux::tmux::TmuxError::ServerGone => {
                "no tmux server is running on this host".to_string()
            }
            err => err.to_string(),
        })?;
        let mut rows = report.rows;
        repo::annotate_rows(&mut rows);
        if !report.deferred_seen.is_empty() {
            seen::clear_seen(&self.tmux, &report.deferred_seen);
        }
        Ok(rows)
    }

    fn on_receipts(&mut self, req: &ReceiptsRequest) -> Response {
        let filter = ReceiptFilter {
            slot: req.slot.as_deref(),
            since_ms: req.since_ms,
        };
        match self.ledger.receipts(&filter) {
            Ok(entries) => Response::Receipts(tma_proto::Receipts {
                receipts: entries.iter().filter_map(wire_receipt).collect(),
            }),
            Err(err) => error(ErrorCode::Internal, &err.to_string()),
        }
    }

    // ---- card and transcript ------------------------------------------------------------------

    /// The card for one pane: the pane's own read, then whatever else can say what it is asking.
    ///
    /// Everything after the pane read is best-effort by construction. A missing hook record, an
    /// endpoint that does not answer and an unreadable transcript each subtract detail from the
    /// card and none of them refuses it, because a device that gets no frame cannot even open the
    /// pane on the host. Only the pane read itself can fail the request.
    fn on_card(&mut self, req: &CardRequest) -> Response {
        let facts = match self.read_pane(&req.pane) {
            Ok(Some(facts)) => facts,
            Ok(None) => return error(ErrorCode::NotFound, "no pane by that id on this host"),
            Err(err) => return error(ErrorCode::Internal, &err.to_string()),
        };
        let agent = facts.agent.clone().unwrap_or_default();
        let record = facts
            .permission_request
            .as_deref()
            .and_then(hook_lane::read_request);
        let question = self.fetch_question(&facts);
        // The tail is read last of the three, so the reader's borrow ends before the card is built.
        let tail = self.transcript_tail(&req.pane, &facts);

        let card = card::build_card(&CardInputs {
            pane: &req.pane,
            agent: &agent,
            state: Some(facts.state),
            detail: facts.detail.as_deref(),
            episode_ms: facts.episode_ms,
            permission_request: facts.permission_request.as_deref(),
            pending_tool: facts.pending_tool.as_deref(),
            pending_call: facts.pending_call.as_deref(),
            approve: label(&self.action_set, "approve", &agent),
            deny: label(&self.action_set, "deny", &agent),
            api_transport: answers_over_http(&self.action_set, &agent),
            hook_record: record.as_ref(),
            question: question.as_ref(),
            tail: &tail,
        });
        Response::Card(card)
    }

    /// One page of transcript headers, newest first. `last` and the budget are clamped inside
    /// [`serve_transcript::window`], which is where the host's own ceilings live.
    fn on_window(&mut self, req: &WindowRequest) -> Response {
        let (agent, source) = match self.transcript_source(&req.pane) {
            Ok(pair) => pair,
            Err(frame) => return Response::Error(frame),
        };
        let facts = PaneTranscript {
            pane: &req.pane,
            agent: &agent,
            source: &source,
        };
        match serve_transcript::window(&mut self.reader, &facts, req) {
            Ok(window) => Response::Window(window),
            Err(frame) => Response::Error(frame),
        }
    }

    /// One event with its body, addressed by a cursor this host minted.
    fn on_event(&mut self, req: &EventRequest) -> Response {
        let (agent, source) = match self.transcript_source(&req.pane) {
            Ok(pair) => pair,
            Err(frame) => return Response::Error(frame),
        };
        let facts = PaneTranscript {
            pane: &req.pane,
            agent: &agent,
            source: &source,
        };
        match serve_transcript::event(&mut self.reader, &facts, req) {
            Ok(event) => Response::Event(event),
            Err(frame) => Response::Error(frame),
        }
    }

    /// The pane's agent and the file its conversation lives in. A reader refusal (an unsupported
    /// store, a pane with no transcript) travels as its own typed frame rather than as an empty
    /// window, which would read as "nothing happened".
    fn transcript_source(&self, pane: &str) -> Result<(String, tx::Source), ErrorFrame> {
        let record = self
            .tmux
            .list_panes()
            .map_err(|err| ErrorFrame::new(ErrorCode::Internal, err.to_string()))?
            .into_iter()
            .find(|rec| rec.pane_id == pane)
            .ok_or_else(|| {
                ErrorFrame::new(ErrorCode::NotFound, "no pane by that id on this host")
            })?;
        let facts = discovery::PaneFacts {
            agent: record.options.get(opt::NAME).cloned().unwrap_or_default(),
            session: record.options.get(opt::SESSION).cloned(),
            transcript: record.options.get(opt::TRANSCRIPT).cloned(),
            cwd: record.cwd.as_ref().map(Into::into),
        };
        let source = self
            .discover(&facts)
            .map_err(|e| serve_transcript::refusal(&e))?;
        Ok((facts.agent, source))
    }

    /// The pane's transcript, resolved against this host's real store roots.
    fn discover(&self, facts: &discovery::PaneFacts) -> Result<tx::Source, tx::Refusal> {
        let roots = StoreRoots::from_env().ok_or(tx::Refusal::NoTranscript)?;
        discovery::discover(facts, &roots)
    }

    /// The newest events for the card's `pending_call` inference: headers, a small page, and no
    /// refusal path. This is a hint about what a blocked pane is blocked on, not the transcript
    /// surface, which `window` serves under the device's own budget.
    fn transcript_tail(&mut self, pane: &str, facts: &PaneFacts) -> Vec<tx::Event> {
        let discovered = discovery::PaneFacts {
            agent: facts.agent.clone().unwrap_or_default(),
            session: facts.session.clone(),
            // The one stamp the broker's read does not carry, and the cheap half of discovery: with
            // it, resolving the file is a `stat` on a string tma already has.
            transcript: self
                .tmux
                .get_pane_option(pane, opt::TRANSCRIPT)
                .ok()
                .flatten(),
            cwd: (!facts.cwd.is_empty()).then(|| PathBuf::from(&facts.cwd)),
        };
        let Ok(source) = self.discover(&discovered) else {
            return Vec::new();
        };
        self.reader
            .window(&source, &tx::WindowRequest::new(TAIL_EVENTS))
            .map(|page| page.events)
            .unwrap_or_default()
    }

    /// The pending question the agent's own server is holding, for a pane blocked on one.
    ///
    /// Gated on the pane's stamps rather than on an agent name: `@agent_question_request` is
    /// written by the one plugin that has questions to report, and an endpoint is the agent saying
    /// it has an HTTP surface at all. No stamps, no fetch, and so no socket on the ordinary card.
    fn fetch_question(&self, facts: &PaneFacts) -> Option<PendingQuestion> {
        if facts.detail.as_deref() != Some("question") {
            return None;
        }
        let endpoint = facts.api_endpoint.as_deref().filter(|e| !e.is_empty())?;
        facts
            .question_request
            .as_deref()
            .filter(|id| !id.is_empty())?;
        PendingQuestion::fetch(endpoint, facts.session.as_deref(), card::QUESTION_TIMEOUT)
    }

    /// The pane's stamped facts, through the same read a dispatch gates on, so a card and the
    /// dispatch it invites are describing one pane rather than two reads of it.
    fn read_pane(&self, pane: &str) -> Result<Option<PaneFacts>, TmuxError> {
        let cfg = self.config.fold_config();
        broker::TmuxBroker {
            tmux: &self.tmux,
            manifests: &self.manifests,
            cfg: &cfg,
            api_bases: &self.config.api,
            server: self.server.clone(),
            notify_command: None,
        }
        .read_pane(pane)
    }

    // ---- stream -----------------------------------------------------------------------------

    fn on_subscribe(&mut self, id: &str, req: &Subscribe) -> Response {
        if self.streaming {
            return error(
                ErrorCode::BadRequest,
                "this connection already has a subscription; open a second connection instead",
            );
        }
        self.streaming = true;
        self.spawn_stream(id.to_string(), req);
        // The device converges with a `snapshot` and then subscribes: the first cycle establishes
        // the stream's baseline and emits no edges, so a resumed stream never replays a transition
        // the previous one already delivered.
        Response::Ack
    }

    fn spawn_stream(&self, id: String, req: &Subscribe) {
        let events = req.events;
        let selector = rows::selector(req.selector.as_ref());
        let params = StreamParams {
            interval: self.config.serve.reconcile_interval(),
            // Edges are edge-triggered by construction, and a snapshot stream repeats itself
            // without this. Either way a quiet fleet stays quiet.
            changes_only: true,
            config_path: self.config_path.clone(),
            manifest_dir: self.manifest_dir.clone(),
        };
        let server = self.server.clone();
        let config = self.config.clone();
        let origin = self.origin.clone();
        let store = self.store.clone();
        let device_id = self.device_id.clone();
        let wire = self.wire.clone();
        let manifest_dir = self.manifest_dir.clone();
        let marker = self.marker.clone();

        std::thread::spawn(move || {
            // Its OWN tmux handle, its own manifest set and its own cycle: two connections are two
            // detection loops, which is what the connection cap exists to bound. The set is loaded
            // here rather than shared because `run_stream` hot-reloads its own pair on every tick.
            let manifests = match tma_runtime::manifests::load(
                manifest_dir.as_deref(),
                &config.agent_overrides,
            ) {
                Ok(set) => set.manifests,
                Err(err) => {
                    eprintln!("tma serve: the stream could not load manifests: {err}");
                    return;
                }
            };
            let tmux = Tmux::connect(&server);
            let revoked = Cell::new(false);
            let mut cache = Cache::default();
            let mut prev: Option<Vec<AgentRow>> = None;
            let mut last: Option<String> = None;
            let render = |report: &CycleReport, _tick: Tick| -> Vec<String> {
                // R48's other half: a push has no request to gate, so the record is re-read here
                // too. An unreadable store is logged and the stream continues, because the CLI's
                // atomic rename never produces one and a transient I/O error is not a revocation.
                match cache.get(&store, &device_id) {
                    Ok(Some(_)) => {}
                    Ok(None) => {
                        revoked.set(true);
                        return vec![encoded(
                            &id,
                            error(ErrorCode::ScopeDenied, "this device was revoked"),
                        )];
                    }
                    Err(err) => eprintln!("tma serve: {err}"),
                }
                let mut rows = report.rows.clone();
                repo::annotate_rows(&mut rows);
                selector.retain(&mut rows);
                if !events {
                    return snapshot_line(&id, &rows, &origin, &mut last);
                }
                let Some(before) = prev.replace(rows) else {
                    return Vec::new();
                };
                let after = prev.as_deref().unwrap_or_default();
                let at_ms = tma_runtime::now_ms();
                tma_core::diff_rows(&before, after)
                    .iter()
                    .filter_map(|e| rows::edge(e, at_ms).ok())
                    .map(|e| encoded(&id, Response::Edge(e)))
                    .collect()
            };
            let emit = |line: &str| -> io::Result<()> {
                wire.send_line(line)?;
                if revoked.get() {
                    // The refusal frame has landed; end the stream on the same publish that saw the
                    // revocation rather than one cycle later.
                    return Err(io::Error::from(io::ErrorKind::BrokenPipe));
                }
                Ok(())
            };
            run_stream(&tmux, config, manifests, params, render, emit);
            if revoked.get() {
                eprintln!("tma serve: the device record is gone; closing the connection");
                // `exit` skips this connection's `Drop`, so the registry marker goes by hand.
                let _ = std::fs::remove_file(&marker);
                std::process::exit(2);
            }
        });
    }

    // ---- dispatch ---------------------------------------------------------------------------

    fn on_dispatch(&mut self, req: &Dispatch) -> Response {
        if !req.host.is_empty() && !self.origin.host.is_empty() && req.host != self.origin.host {
            return error(
                ErrorCode::NotFound,
                "that dispatch names another host; this one is not it",
            );
        }
        let Some(action) = actions::find(&self.action_set, &req.action) else {
            return error(ErrorCode::NotFound, "no action by that name on this host");
        };
        // N10: an exec action puts no keystrokes into a pane, so it is the one class with no
        // freshness guard at all. Unreachable from a device, by kind rather than by name.
        if !action.kind.sends_keystrokes() {
            return error(
                ErrorCode::ScopeDenied,
                "that action runs a command rather than answering a pane; \
                 no device scope reaches it",
            );
        }
        // The payload rules the CLI enforces, on this surface too. A value with nowhere to go is
        // refused rather than accepted and silently dropped.
        if let Some(usage) = payload_error(action, req) {
            return error(ErrorCode::BadRequest, usage);
        }
        if req.answers.is_some() {
            return error(
                ErrorCode::Unsupported,
                "this build has no question action to answer; \
                 the opencode question ops are unimplemented here",
            );
        }

        // U10: the scope gate, ahead of the claim and ahead of every tmux call.
        let Some(needed) = required_scope(&action.name) else {
            return self.refused(
                req,
                "that action is outside the scope vocabulary a device may dispatch",
            );
        };
        if !self.scopes.contains(&needed) {
            return self.refused(
                req,
                &format!("this device is not granted {}", needed.token()),
            );
        }

        // U13: claim BEFORE firing, so a replay returns the cached receipt and sends nothing.
        let now = tma_runtime::now_ms();
        let guard = match self.ledger.claim(
            &req.slot,
            &req.pane,
            &action.name,
            // The connection's device, never the frame's: the client states one for its own
            // bookkeeping and the host records who it actually authenticated.
            Some(&self.device_id),
            now,
        ) {
            Ok(Claim::Hit(receipt)) => return self.cached(req, &receipt),
            Ok(Claim::Claimed(guard)) => guard,
            Err(err) => return error(ErrorCode::Internal, &err.to_string()),
        };

        let result = broker::fire(
            &self.tmux,
            &self.manifests,
            &self.config.fold_config(),
            &self.config.api,
            broker::DetachCtx::default(),
            action,
            &req.pane,
            broker::FireArgs {
                // N8: a device never skips the `when` gate. Hardcoded here and nowhere settable.
                force: false,
                args: &[],
                text: req.text.as_deref(),
                answers: req.answers.as_deref(),
                // The `[act] log`'s `source` vocabulary has no device token, and the slot ledger
                // already records the dispatching device, which ARCHITECTURE §4.2.6 names as the
                // only place "which device approved this" is answerable.
                audit: broker::audit::AuditCtx::default(),
                // The binder's zero is "no expectation": a device that read a real row always
                // quotes a real instant, and `Binder::default` is what a client sends when it has
                // nothing to bind to.
                expect_episode_ms: Some(req.binder.expect_episode_ms).filter(|ms| *ms != 0),
                expect_permission_request: req.binder.expect_permission_request.as_deref(),
            },
        );

        let stored = slots::Receipt::from_act_result(&result);
        // `locked` is the one refusal that changed nothing and passes on a retry, so it releases
        // the claim. Every other outcome writes a terminal receipt, `error` included: the broker
        // failed somewhere it cannot prove the keystroke did not land.
        let resolved = if matches!(result.outcome, Outcome::Refused(Refusal::Locked)) {
            guard.release()
        } else {
            guard.write(stored.clone())
        };
        if let Err(err) = resolved {
            // The dispatch already happened; a ledger that could not record it is worth saying and
            // is not worth turning a delivered action into a failure.
            eprintln!("tma serve: {err}");
        }
        Response::Receipt(receipt_frame(req, &stored, &self.device_id, now, false))
    }

    /// A scope refusal: a receipt naming `scope-denied`, and no ledger entry. The gate is ahead of
    /// the claim (ARCHITECTURE §4.4), and it is deterministic, so a replay of the slot earns the
    /// same refusal without a keystroke, which is the property a terminal receipt would be buying.
    fn refused(&self, req: &Dispatch, message: &str) -> Response {
        eprintln!(
            "tma serve: refused `{}` on {}: {message}",
            req.action, req.pane
        );
        Response::Receipt(tma_proto::Receipt {
            slot: req.slot.clone(),
            pane: req.pane.clone(),
            action: req.action.clone(),
            outcome: WireOutcome::Refused,
            reason: Some(Reason::ScopeDenied),
            // The gate refusals' exit code: the world has to change before this fire can land.
            exit_code: 4,
            cached: false,
            device: Some(self.device_id.clone()),
            at_ms: tma_runtime::now_ms(),
        })
    }

    /// A replayed receipt. The ledger is re-read for the claim's own instant and the device that
    /// earned it, because a cached receipt that dated itself now would misreport when it happened.
    fn cached(&self, req: &Dispatch, receipt: &slots::Receipt) -> Response {
        let recorded = self
            .ledger
            .receipts(&ReceiptFilter {
                slot: Some(&req.slot),
                since_ms: None,
            })
            .ok()
            .and_then(|entries| entries.into_iter().next());
        let (at_ms, device) = match &recorded {
            Some(entry) => (entry.at_ms, entry.device.clone()),
            // Evicted between the claim and this read. Rare enough to be worth naming rather than
            // papering over, and the receipt itself is still the truth.
            None => (tma_runtime::now_ms(), Some(self.device_id.clone())),
        };
        let mut frame = receipt_frame(req, receipt, "", at_ms, true);
        frame.device = device;
        Response::Receipt(frame)
    }

    // ---- writing ----------------------------------------------------------------------------

    fn send(&self, id: &str, response: Response) {
        if let Err(err) = self.wire.send(&ResponseFrame::new(id, response)) {
            eprintln!("tma serve: writing a response: {err}");
        }
    }

    /// Answer a line whose correlation id could not be recovered. `"0"` rather than a guess: a
    /// client matches on the id it sent, and an invented one would answer the wrong request.
    fn reply_untyped(&self, code: ErrorCode, message: &str) {
        self.send("0", error(code, message));
    }
}

/// Which U10 scope covers an action, or `None` for one the record does not name.
///
/// `None` is a refusal, not a pass: the closed vocabulary N8 promises is exactly this table, and an
/// action outside it (a user-authored one, or `compact`, which is a control-plane verb rather than
/// an answer) must not become reachable from a phone by being added to the actions directory.
fn required_scope(action: &str) -> Option<Scope> {
    match action {
        "approve" | "deny" | "question_reply" | "question_reject" => Some(Scope::ActAnswer),
        "approve_always" => Some(Scope::ActAlways),
        "steer" | "steer_now" | "interrupt" | "deny_with_message" => Some(Scope::ActSteer),
        _ => None,
    }
}

/// The payload rule for this action's kind, as a sentence, or `None` when the frame fits. The same
/// rule `tma act` enforces: a value with nowhere to go is refused rather than silently dropped.
fn payload_error(action: &ActionManifest, req: &Dispatch) -> Option<&'static str> {
    match (action.kind, req.text.is_some()) {
        (ActionKind::Text, false) => Some("that action delivers a caller's string and needs one"),
        (ActionKind::Keys, true) => {
            Some("that action's sequence comes from the manifest and takes no text payload")
        }
        _ => None,
    }
}

/// The bundled label a card offers for `action` on `agent`, or `None` when the loaded set does not
/// cover that agent: a card never offers a control this host could not fire.
fn label<'a>(set: &'a [ActionManifest], action: &str, agent: &str) -> Option<&'a str> {
    let manifest = actions::find(set, action)?;
    manifest
        .applies_to(agent)
        .then_some(manifest.label.as_str())
}

/// Whether this agent's `approve` travels over its own HTTP surface instead of as keystrokes. A
/// fact about the transport, which is all the card's `api` lane claims.
fn answers_over_http(set: &[ActionManifest], agent: &str) -> bool {
    actions::find(set, "approve").is_some_and(|a| a.api_for(agent).is_some())
}

fn error(code: ErrorCode, message: &str) -> Response {
    Response::Error(ErrorFrame::new(code, message))
}

/// One response as its wire line, for the stream, which writes lines rather than frames.
fn encoded(id: &str, response: Response) -> String {
    tma_proto::encode(&ResponseFrame::new(id, response)).unwrap_or_default()
}

/// The snapshot stream's line for one cycle, suppressed when it repeats the last one.
fn snapshot_line(
    id: &str,
    rows: &[AgentRow],
    origin: &Origin,
    last: &mut Option<String>,
) -> Vec<String> {
    let Ok(agents) = rows
        .iter()
        .map(|row| rows::fleet_row(row, origin))
        .collect::<Result<Vec<_>, _>>()
    else {
        return Vec::new();
    };
    let line = encoded(id, Response::Snapshot(Snapshot { agents }));
    if last.as_deref() == Some(line.as_str()) {
        return Vec::new();
    }
    *last = Some(line.clone());
    vec![line]
}

/// A ledger record as its wire receipt. `cached` is always true here: a `receipts` read is a replay
/// by construction, never a dispatch that just happened.
fn wire_receipt(entry: &slots::Entry) -> Option<tma_proto::Receipt> {
    let receipt = entry.receipt.as_ref()?;
    Some(tma_proto::Receipt {
        slot: entry.slot.clone(),
        pane: entry.pane.clone(),
        action: entry.action.clone(),
        outcome: WireOutcome::from_token(&receipt.outcome),
        reason: receipt.reason.as_deref().map(Reason::from_token),
        exit_code: receipt.exit_code,
        cached: true,
        device: entry.device.clone(),
        at_ms: entry.at_ms,
    })
}

/// A just-resolved dispatch as its wire receipt.
fn receipt_frame(
    req: &Dispatch,
    receipt: &slots::Receipt,
    device: &str,
    at_ms: u64,
    cached: bool,
) -> tma_proto::Receipt {
    tma_proto::Receipt {
        slot: req.slot.clone(),
        pane: req.pane.clone(),
        action: req.action.clone(),
        outcome: WireOutcome::from_token(&receipt.outcome),
        reason: receipt.reason.as_deref().map(Reason::from_token),
        exit_code: receipt.exit_code,
        cached,
        device: (!device.is_empty()).then(|| device.to_string()),
        at_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The U10 table, token for token. A new action name is not automatically dispatchable: it has
    /// to be given a class here, which is what keeps the remote vocabulary closed.
    #[test]
    fn the_scope_map_is_the_u10_table() {
        for action in ["approve", "deny", "question_reply", "question_reject"] {
            assert_eq!(required_scope(action), Some(Scope::ActAnswer), "{action}");
        }
        assert_eq!(required_scope("approve_always"), Some(Scope::ActAlways));
        for action in ["steer", "steer_now", "interrupt", "deny_with_message"] {
            assert_eq!(required_scope(action), Some(Scope::ActSteer), "{action}");
        }
        // `compact` is a bundled action and a control-plane verb: it is not an answer, not a steer,
        // and reaching `/compact` from a phone is outside what any scope grants.
        assert_eq!(required_scope("compact"), None);
        assert_eq!(required_scope("anything-a-user-wrote"), None);
    }

    /// `read` covers no dispatch at all. Stated as a test because the scope is implicit everywhere
    /// else, and "implicit" is how a read-only device ends up with a button.
    #[test]
    fn read_covers_no_dispatch() {
        let every = ["approve", "approve_always", "steer", "interrupt"];
        for action in every {
            assert_ne!(required_scope(action), Some(Scope::Read), "{action}");
        }
    }
}
