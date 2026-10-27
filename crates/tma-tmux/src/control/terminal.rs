//! Terminal framing, independent of tmux notification and OS read boundaries.

use tma_core::program_status::{
    parse_frame, DecodeScratch, ParseOutcome, ProgramEvent, TerminalLifecycle, MAX_FRAME_BYTES,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StringKind {
    Osc,
    Dcs,
    Apc,
    Pm,
    Sos,
    Rename,
}

#[derive(Clone, Copy)]
enum DcsPhase {
    Unknown,
    Data,
    Escape,
}

#[derive(Default)]
enum State {
    #[default]
    Ground,
    Escape,
    DirtyEscape,
    Csi,
    String {
        kind: StringKind,
        escape: bool,
        suppressed: bool,
    },
    // tmux DCS consumes ESC pairs and treats CAN/SUB as data. At cold attach,
    // even an initial ESC-backslash may finish a pair begun before observation.
    // A non-ESC byte establishes its possible DCS phase before ST can be trusted.
    Lost(DcsPhase),
}

#[derive(Default)]
pub(super) struct Scanner {
    state: State,
    candidate: Vec<u8>,
    scratch: DecodeScratch,
    oversized: bool,
    pub(super) rejected: u64,
}

fn escape_ignored(byte: u8) -> bool {
    matches!(byte, 0x00..=0x17 | 0x19 | 0x1c..=0x1f | 0x7f..=0xff)
}

impl Scanner {
    /// An attach/mode transition does not reveal the pane's previous terminal
    /// string state. Require an unambiguous ST before recognizing OSC.
    pub(super) fn unsynchronized() -> Self {
        Self {
            state: State::Lost(DcsPhase::Unknown),
            ..Self::default()
        }
    }

    /// Discard candidates without turning an open DCS/OSC into ground. A binding
    /// boundary in an escape also suppresses the construct it was introducing.
    pub(super) fn boundary(&mut self) {
        self.suppress_candidate();
        if matches!(self.state, State::Escape | State::DirtyEscape) {
            self.state = State::Lost(DcsPhase::Escape);
        }
    }

    fn suppress_candidate(&mut self) {
        self.candidate.clear();
        self.oversized = false;
        if let State::String { suppressed, .. } = &mut self.state {
            *suppressed = true;
        }
    }

    pub(super) fn feed_quarantined(&mut self, byte: u8) {
        // These bytes are all observed, so keep tracking their string framing.
        // Only the escape already in progress at the binding boundary is unknown.
        let _ = self.feed(byte);
        self.suppress_candidate();
    }

    pub(super) fn loss(&mut self) {
        self.boundary();
        self.state = State::Lost(DcsPhase::Unknown);
    }

    pub(super) fn pending_bytes(&self) -> usize {
        self.candidate.len()
    }

    pub(super) fn feed(&mut self, byte: u8) -> Option<ProgramEvent> {
        if matches!(byte, 0x18 | 0x1a)
            && !matches!(
                self.state,
                State::String {
                    kind: StringKind::Dcs,
                    ..
                } | State::Lost(_)
            )
        {
            self.state = State::Ground;
            self.candidate.clear();
            return None;
        }
        match &mut self.state {
            State::Ground => {
                if byte == 0x1b {
                    self.state = State::Escape;
                }
            }
            State::Escape | State::DirtyEscape => {
                let suppressed = matches!(self.state, State::DirtyEscape);
                self.state = match byte {
                    b']' | b'P' | b'_' | b'^' | b'X' | b'k' => {
                        let kind = match byte {
                            b']' => StringKind::Osc,
                            // Conservatively skip the header like payload too,
                            // rather than trusting cancellation before its final.
                            b'P' => StringKind::Dcs,
                            b'_' => StringKind::Apc,
                            b'^' => StringKind::Pm,
                            b'k' => StringKind::Rename,
                            _ => StringKind::Sos,
                        };
                        self.candidate.clear();
                        self.oversized = false;
                        State::String {
                            kind,
                            escape: false,
                            suppressed,
                        }
                    }
                    b'[' => State::Csi,
                    byte if escape_ignored(byte) => State::DirtyEscape,
                    0x1b => State::Escape,
                    b'c' => {
                        self.state = State::Ground;
                        return Some(ProgramEvent::Lifecycle(TerminalLifecycle::FullReset));
                    }
                    _ => State::Ground,
                };
            }
            State::Csi => {
                if (0x40..=0x7e).contains(&byte) {
                    self.state = State::Ground;
                } else if byte == 0x1b {
                    self.state = State::Escape;
                }
            }
            State::String {
                kind,
                escape,
                suppressed,
            } => {
                if *kind != StringKind::Dcs && *escape && byte == b'P' {
                    // tmux cancels these strings at ESC. Keep nested OSC opaque,
                    // but track a possible DCS so BEL/CAN/SUB cannot free it.
                    self.state = State::String {
                        kind: StringKind::Dcs,
                        escape: false,
                        suppressed: true,
                    };
                    self.candidate.clear();
                    self.oversized = false;
                    return None;
                }
                if (*escape && byte == b'\\') || (*kind == StringKind::Osc && byte == 0x07) {
                    let st = *escape && byte == b'\\';
                    let pending_escape = *escape && !st;
                    let event = if *kind == StringKind::Osc && !*suppressed {
                        if st {
                            self.candidate.pop();
                        }
                        self.complete(st)
                    } else {
                        None
                    };
                    self.state = if pending_escape {
                        State::DirtyEscape
                    } else {
                        State::Ground
                    };
                    self.candidate.clear();
                    return event;
                }
                if *kind == StringKind::Osc && !*suppressed {
                    Self::push(&mut self.candidate, &mut self.oversized, byte);
                }
                *escape = if *kind == StringKind::Dcs {
                    byte == 0x1b && !*escape
                } else {
                    byte == 0x1b || (*escape && escape_ignored(byte))
                };
            }
            State::Lost(phase) => match (*phase, byte) {
                (DcsPhase::Escape, b'\\') => self.state = State::Ground,
                (DcsPhase::Unknown, 0x1b) => {}
                (DcsPhase::Data, 0x1b) => *phase = DcsPhase::Escape,
                _ => *phase = DcsPhase::Data,
            },
        }
        None
    }

    fn push(candidate: &mut Vec<u8>, oversized: &mut bool, byte: u8) {
        if candidate.len() < MAX_FRAME_BYTES {
            candidate.push(byte);
        } else {
            *oversized = true;
        }
    }

    fn complete(&mut self, st: bool) -> Option<ProgramEvent> {
        if !self.oversized
            && !self.candidate.contains(&0x1b)
            && (self.candidate == b"133;A" || self.candidate.starts_with(b"133;A;"))
        {
            return Some(ProgramEvent::Lifecycle(TerminalLifecycle::ShellPrompt));
        }
        if !self.candidate.starts_with(b"7501;") {
            return None;
        }
        // Include the actual terminator length in the B1 wire-frame limit.
        if self.oversized || self.candidate.len() + (if st { 4 } else { 3 }) > MAX_FRAME_BYTES {
            self.rejected += 1;
            return None;
        }
        self.candidate.splice(..0, [0x1b, b']']);
        if st {
            self.candidate.extend_from_slice(b"\x1b\\");
        } else {
            self.candidate.push(0x07);
        }
        match parse_frame(&self.candidate, &mut self.scratch) {
            Ok(ParseOutcome::Command(command)) => Some(ProgramEvent::Command(command.into_owned())),
            Ok(ParseOutcome::Ignored(_)) | Err(_) => {
                self.rejected += 1;
                None
            }
        }
    }
}
