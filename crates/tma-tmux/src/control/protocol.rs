//! Selected streams and the bounded, owned handoff to runtime.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use tma_core::program_status::{
    ConnectionEpoch, ObservationGap, ObservationSequence, ObservedEvent, PaneId, ProgramCommand,
    ProgramEvent, SessionId,
};

use super::decoder::Decoded;
use super::terminal::Scanner;

/// At most 1024 immediate events and 1 MiB of decoded owned payload. Both limits
/// are independent of ordinary output volume. Overflow revokes a pane through
/// its topology slot, outside the saturated queue, before fresh events can pass.
pub const MAX_PROGRAM_EVENTS: usize = 1024;
pub const MAX_PROGRAM_PAYLOAD_BYTES: usize = 1024 * 1024;

/// The fresh eligible interval returned after process binding. Only events with
/// this connection and a sequence strictly above `sequence_floor` qualify.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObservationBoundary {
    pub connection: ConnectionEpoch,
    pub sequence_floor: ObservationSequence,
}

/// Current selected stream, independently of when runtime last drained it.
/// Queued observations preserve this connection; a loss or handoff changes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProgramStreamState {
    pub connection: ConnectionEpoch,
    pub pending_gap: Option<ObservationGap>,
}

/// Gaps precede every observation in a drain. A gap carries the new epoch and
/// floor; observations after it are retained and may establish fresh authority.
/// Per-pane event order is preserved, including clear and completion commands.
#[derive(Default, Debug)]
pub struct ProgramDrain {
    /// Revokes all previously drained authority if departed-pane bookkeeping
    /// overflows. Routine departure invalidates only the departed pane.
    /// Apply before pane invalidations and observations, even if no pane remains.
    pub all_invalidated: bool,
    pub invalidations: Vec<ObservedEvent>,
    pub observations: Vec<ObservedEvent>,
}

/// Read-only bounded collector diagnostics. Counts contain no terminal text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProgramStats {
    pub tracked_panes: usize,
    pub queued_events: usize,
    pub pending_gaps: usize,
    pub pending_payload_bytes: usize,
    pub rejected_frames: u64,
    pub queue_overflows: u64,
    pub topology_ready: bool,
}

struct PaneStream {
    owner: SessionId,
    client: u64,
    epoch: ConnectionEpoch,
    sequence: u64,
    scanner: Scanner,
    pending_gap: Option<ObservedEvent>,
    paused: bool,
    // Binding during a notification must not qualify a pre-boundary escape or
    // report completed later in that same notification. Still scan for framing.
    quarantine: bool,
}

pub(super) struct Collector {
    panes: BTreeMap<PaneId, PaneStream>,
    queue: VecDeque<ObservedEvent>,
    queued_bytes: usize,
    candidate_bytes: usize,
    payload_budget: usize,
    epoch: u64,
    topology_ready: bool,
    all_invalidated: bool,
    departed: BTreeMap<PaneId, ObservedEvent>,
    pub(super) overflows: u64,
    retired_rejections: u64,
}

impl Default for Collector {
    fn default() -> Self {
        Self {
            panes: BTreeMap::new(),
            queue: VecDeque::new(),
            queued_bytes: 0,
            candidate_bytes: 0,
            payload_budget: usize::MAX,
            epoch: 0,
            topology_ready: false,
            all_invalidated: false,
            departed: BTreeMap::new(),
            overflows: 0,
            retired_rejections: 0,
        }
    }
}

pub(super) fn payload_bytes(event: &ProgramEvent) -> usize {
    match event {
        ProgramEvent::Command(ProgramCommand::Replace(record)) => {
            record.id.as_ref().as_str().len()
                + record
                    .app
                    .as_ref()
                    .map_or(0, |app| app.as_ref().as_str().len())
                + record.title().map_or(0, |text| text.as_str().len())
                + record
                    .msg
                    .as_ref()
                    .map_or(0, |text| text.as_ref().as_str().len())
        }
        ProgramEvent::Command(ProgramCommand::Clear(id)) => id.as_ref().as_str().len(),
        _ => 0,
    }
}

impl Collector {
    /// Runtime owns the fleet policy and subtracts its retained stores. Lowering
    /// the allowance immediately revokes whole affected panes, never just rows.
    pub(super) fn set_payload_budget(&mut self, available: usize) {
        self.payload_budget = available;
        while self.pending_bytes() > available {
            let victim = self
                .panes
                .iter()
                .map(|(&pane, stream)| {
                    let bytes = stream.scanner.pending_bytes()
                        + self
                            .queue
                            .iter()
                            .filter(|event| event.pane == pane)
                            .map(|event| payload_bytes(&event.event))
                            .sum::<usize>();
                    (bytes, pane)
                })
                .filter(|(bytes, _)| *bytes > 0)
                .max();
            let Some((_, pane)) = victim else {
                break;
            };
            self.invalidate(pane, ObservationGap::BudgetExceeded, false);
        }
    }
    pub(super) fn with_epoch(epoch: u64) -> Self {
        Self {
            epoch,
            ..Self::default()
        }
    }

    pub(super) fn last_epoch(&self) -> u64 {
        self.epoch
    }

    pub(super) fn owner(&self, pane: PaneId) -> Option<(SessionId, u64)> {
        self.panes
            .get(&pane)
            .map(|stream| (stream.owner, stream.client))
    }

    pub(super) fn stream(&self, pane: PaneId) -> Option<ProgramStreamState> {
        if !self.topology_ready || self.all_invalidated {
            return None;
        }
        self.panes.get(&pane).map(|stream| ProgramStreamState {
            connection: stream.epoch,
            pending_gap: stream
                .pending_gap
                .as_ref()
                .and_then(|event| match event.event {
                    ProgramEvent::Gap(reason) => Some(reason),
                    _ => None,
                }),
        })
    }

    fn next_epoch(&mut self) -> ConnectionEpoch {
        self.epoch = self
            .epoch
            .checked_add(1)
            .expect("connection epoch exhausted");
        ConnectionEpoch::new(self.epoch)
    }

    /// On a failed topology read, stop all protocol delivery until a successful
    /// read. The known slots retain bounded out-of-band invalidations. A fresh
    /// read resumes observation, never restores an earlier report's authority.
    pub(super) fn topology_failed(&mut self) {
        self.topology_ready = false;
        let panes: Vec<_> = self.panes.keys().copied().collect();
        for pane in panes {
            self.invalidate(pane, ObservationGap::FramingLoss, true);
        }
    }

    pub(super) fn reconcile(
        &mut self,
        topology: &[(SessionId, PaneId)],
        clients: &BTreeMap<SessionId, u64>,
    ) {
        let mut eligible: BTreeMap<PaneId, BTreeSet<SessionId>> = BTreeMap::new();
        for &(session, pane) in topology {
            if clients.contains_key(&session) {
                eligible.entry(pane).or_default().insert(session);
            }
        }
        let removed: Vec<_> = self
            .panes
            .keys()
            .filter(|pane| !eligible.contains_key(pane))
            .copied()
            .collect();
        for pane in removed {
            self.invalidate(pane, ObservationGap::OwnerChanged, false);
            let mut stream = self.panes.remove(&pane).expect("known pane");
            self.retired_rejections += stream.scanner.rejected;
            if self.departed.len() >= MAX_PROGRAM_EVENTS && !self.departed.contains_key(&pane) {
                // Only pathological topology churn without a consumer drain
                // reaches this fallback. Routine pane departure stays local.
                self.overflows += 1;
                self.all_invalidated = true;
                self.departed.clear();
                self.queue.clear();
                self.queued_bytes = 0;
                let known: Vec<_> = self.panes.keys().copied().collect();
                for known_pane in known {
                    self.invalidate(known_pane, ObservationGap::QueueOverflow, false);
                }
            } else if let Some(gap) = stream.pending_gap.take() {
                self.departed.insert(pane, gap);
            }
        }
        for (pane, sessions) in &eligible {
            let owner = self
                .panes
                .get(pane)
                .filter(|stream| sessions.contains(&stream.owner))
                .map_or_else(
                    || *sessions.first().expect("nonempty sessions"),
                    |stream| stream.owner,
                );
            let client = clients[&owner];
            if let Some(stream) = self.panes.get(pane) {
                if stream.owner == owner && stream.client == client {
                    continue;
                }
                self.invalidate(*pane, ObservationGap::OwnerChanged, true);
                let stream = self.panes.get_mut(pane).expect("existing pane");
                stream.owner = owner;
                stream.client = client;
                stream.paused = false;
                stream.quarantine = false;
            } else {
                let epoch = self.next_epoch();
                self.panes.insert(
                    *pane,
                    PaneStream {
                        owner,
                        client,
                        epoch,
                        sequence: 0,
                        scanner: Scanner::unsynchronized(),
                        pending_gap: None,
                        paused: false,
                        quarantine: false,
                    },
                );
            }
        }
        self.topology_ready = true;
    }

    pub(super) fn client_gap(&mut self, session: SessionId, reason: ObservationGap) {
        let panes: Vec<_> = self
            .panes
            .iter()
            .filter(|(_, stream)| stream.owner == session)
            .map(|(pane, _)| *pane)
            .collect();
        for pane in panes {
            self.invalidate(pane, reason, true);
        }
    }

    fn discard_queued(&mut self, pane: PaneId) {
        self.queue.retain(|event| event.pane != pane);
        self.queued_bytes = self
            .queue
            .iter()
            .map(|event| payload_bytes(&event.event))
            .sum();
    }

    pub(super) fn invalidate(&mut self, pane: PaneId, reason: ObservationGap, lost: bool) {
        if !self.panes.contains_key(&pane) {
            return;
        }
        self.discard_queued(pane);
        let epoch = self.next_epoch();
        let stream = self.panes.get_mut(&pane).expect("known pane");
        self.candidate_bytes -= stream.scanner.pending_bytes();
        // Runtime must release retained payload even if another boundary arrives
        // before it drains this loss. Keep the budget cause at the latest epoch.
        let reason =
            if stream.pending_gap.as_ref().is_some_and(|event| {
                event.event == ProgramEvent::Gap(ObservationGap::BudgetExceeded)
            }) {
                ObservationGap::BudgetExceeded
            } else {
                reason
            };
        stream.epoch = epoch;
        stream.sequence = stream
            .sequence
            .checked_add(1)
            .expect("observation sequence exhausted");
        if lost {
            stream.scanner.loss();
        } else {
            stream.scanner.boundary();
        }
        stream.pending_gap = Some(ObservedEvent {
            pane,
            connection: epoch,
            sequence: ObservationSequence::new(stream.sequence),
            event: ProgramEvent::Gap(reason),
        });
    }

    pub(super) fn boundary(
        &mut self,
        pane: PaneId,
        in_payload: bool,
    ) -> Option<ObservationBoundary> {
        self.invalidate(pane, ObservationGap::OwnerChanged, false);
        let stream = self.panes.get_mut(&pane)?;
        stream.quarantine = in_payload;
        Some(ObservationBoundary {
            connection: stream.epoch,
            sequence_floor: ObservationSequence::new(stream.sequence),
        })
    }

    pub(super) fn decoded(&mut self, session: SessionId, client: u64, item: Decoded) {
        let pane = match item {
            Decoded::Gap(None, reason) => {
                self.client_gap(session, reason);
                return;
            }
            Decoded::Start(pane)
            | Decoded::Byte(pane, _)
            | Decoded::End(pane)
            | Decoded::Pause(pane)
            | Decoded::Continue(pane)
            | Decoded::Gap(Some(pane), _) => pane,
            _ => return,
        };
        if !self.topology_ready {
            return;
        }
        let Some(stream) = self.panes.get_mut(&pane) else {
            return;
        };
        if stream.owner != session || stream.client != client {
            return;
        }
        match item {
            Decoded::Pause(_) | Decoded::Continue(_) => {
                stream.paused = matches!(item, Decoded::Pause(_));
                self.invalidate(pane, ObservationGap::Paused, true);
            }
            Decoded::Gap(_, reason) => self.invalidate(pane, reason, true),
            Decoded::Byte(_, byte) if !stream.paused => {
                let previous = stream.scanner.pending_bytes();
                if stream.quarantine {
                    stream.scanner.feed_quarantined(byte);
                    self.candidate_bytes =
                        self.candidate_bytes - previous + stream.scanner.pending_bytes();
                } else {
                    let event = stream.scanner.feed(byte);
                    self.candidate_bytes =
                        self.candidate_bytes - previous + stream.scanner.pending_bytes();
                    if self.pending_bytes() > self.payload_budget {
                        self.invalidate(pane, ObservationGap::BudgetExceeded, false);
                    } else if let Some(event) = event {
                        self.enqueue(pane, event);
                    }
                }
            }
            Decoded::End(_) if stream.quarantine => {
                self.candidate_bytes -= stream.scanner.pending_bytes();
                stream.scanner.boundary();
                stream.quarantine = false;
            }
            _ => {}
        }
    }

    fn enqueue(&mut self, pane: PaneId, event: ProgramEvent) {
        let bytes = payload_bytes(&event);
        let available = self
            .payload_budget
            .checked_sub(self.pending_bytes())
            .expect("pending program payload exceeds allowance");
        if bytes > available {
            self.invalidate(pane, ObservationGap::BudgetExceeded, false);
            return;
        }
        if self.queue.len() >= MAX_PROGRAM_EVENTS
            || self.queued_bytes + bytes > MAX_PROGRAM_PAYLOAD_BYTES
        {
            self.overflows += 1;
            self.invalidate(pane, ObservationGap::QueueOverflow, false);
            return;
        }
        let stream = self.panes.get_mut(&pane).expect("known pane");
        stream.sequence = stream
            .sequence
            .checked_add(1)
            .expect("observation sequence exhausted");
        self.queue.push_back(ObservedEvent {
            pane,
            connection: stream.epoch,
            sequence: ObservationSequence::new(stream.sequence),
            event,
        });
        self.queued_bytes += bytes;
    }

    pub(super) fn pending_bytes(&self) -> usize {
        self.queued_bytes + self.candidate_bytes
    }

    pub(super) fn stats(&self) -> ProgramStats {
        ProgramStats {
            tracked_panes: self.panes.len(),
            queued_events: self.queue.len(),
            pending_gaps: usize::from(self.all_invalidated)
                + self.departed.len()
                + self
                    .panes
                    .values()
                    .filter(|stream| stream.pending_gap.is_some())
                    .count(),
            pending_payload_bytes: self.pending_bytes(),
            rejected_frames: self.retired_rejections
                + self
                    .panes
                    .values()
                    .map(|stream| stream.scanner.rejected)
                    .sum::<u64>(),
            queue_overflows: self.overflows,
            topology_ready: self.topology_ready,
        }
    }

    pub(super) fn drain(&mut self) -> ProgramDrain {
        let mut invalidations: Vec<_> = std::mem::take(&mut self.departed).into_values().collect();
        invalidations.extend(
            self.panes
                .values_mut()
                .filter_map(|stream| stream.pending_gap.take()),
        );
        self.queued_bytes = 0;
        ProgramDrain {
            all_invalidated: std::mem::take(&mut self.all_invalidated),
            invalidations,
            observations: self.queue.drain(..).collect(),
        }
    }
}
