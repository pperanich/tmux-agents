//! Incremental tmux control framing. Payloads are never retained here.

use tma_core::program_status::{ObservationGap, PaneId};

// IDs fit u32, metadata fit u64 (20 decimal digits). Standard output needs < 32
// bytes; extended headers get 1 KiB for future fields, independently of payload.
pub(super) const MAX_OUTPUT_HEADER: usize = 64;
pub(super) const MAX_HEADER: usize = 1024;
pub(super) const MAX_COMMAND_HEADER: usize = 96;
// An attach response is tiny. Bound an unexpectedly unclosed response in bytes
// and lines, without buffering it; poisoning requires replacing the client.
pub(super) const MAX_BLOCK_BYTES: usize = 1024 * 1024;
pub(super) const MAX_BLOCK_LINES: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Note {
    SessionsChanged,
    Lifecycle,
    Exit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Decoded {
    Start(PaneId),
    Byte(PaneId, u8),
    End(PaneId),
    Pause(PaneId),
    Continue(PaneId),
    Gap(Option<PaneId>, ObservationGap),
    Fatal,
    Note(Note),
}

#[derive(Default)]
enum Line {
    #[default]
    Header,
    Payload {
        pane: PaneId,
        octal: Option<(u8, u16)>,
    },
    Skip,
    BlockData,
    Poisoned,
}

#[derive(Default)]
pub(super) struct Decoder {
    line: Line,
    header: Vec<u8>,
    block: Option<[u64; 3]>,
    block_bytes: usize,
    block_lines: usize,
    attached: bool,
}

fn number(bytes: &[u8]) -> Option<u64> {
    if bytes.is_empty() || bytes.len() > 20 || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(bytes).ok()?.parse().ok()
}

fn pane(bytes: &[u8]) -> Option<PaneId> {
    PaneId::parse(std::str::from_utf8(bytes).ok()?).ok()
}

fn frame(header: &[u8]) -> Option<[u64; 3]> {
    let mut words = header.split(|b| *b == b' ');
    words.next()?;
    let result = [
        number(words.next()?)?,
        number(words.next()?)?,
        number(words.next()?)?,
    ];
    words.next().is_none().then_some(result)
}

impl Decoder {
    fn fatal(&mut self, emit: &mut impl FnMut(Decoded)) {
        emit(Decoded::Gap(None, ObservationGap::FramingLoss));
        emit(Decoded::Fatal);
        self.line = Line::Poisoned;
        self.header.clear();
    }

    pub(super) fn in_payload(&self, pane: PaneId) -> bool {
        matches!(self.line, Line::Payload { pane: p, .. } if p == pane)
    }

    pub(super) fn feed(&mut self, bytes: &[u8], protocol: bool, mut emit: impl FnMut(Decoded)) {
        for &byte in bytes {
            if matches!(self.line, Line::Poisoned) {
                return;
            }
            if self.block.is_some() {
                self.block_bytes += 1;
                if self.block_bytes > MAX_BLOCK_BYTES {
                    self.fatal(&mut emit);
                    return;
                }
            }
            match &mut self.line {
                Line::Payload { pane, octal } => {
                    let id = *pane;
                    if byte == b'\n' {
                        if protocol && octal.is_some() {
                            emit(Decoded::Gap(Some(id), ObservationGap::MalformedTransport));
                        }
                        emit(Decoded::End(id));
                        self.line = Line::Header;
                    } else if protocol {
                        if let Some((digits, value)) = octal {
                            if !(b'0'..=b'7').contains(&byte) || (*digits == 0 && byte > b'3') {
                                emit(Decoded::Gap(Some(id), ObservationGap::MalformedTransport));
                                emit(Decoded::End(id));
                                self.line = Line::Skip;
                            } else {
                                *value = *value * 8 + u16::from(byte - b'0');
                                *digits += 1;
                                if *digits == 3 {
                                    emit(Decoded::Byte(id, *value as u8));
                                    *octal = None;
                                }
                            }
                        } else if byte == b'\\' {
                            *octal = Some((0, 0));
                        } else {
                            emit(Decoded::Byte(id, byte));
                        }
                    }
                }
                Line::Skip => {
                    if byte == b'\n' {
                        self.line = Line::Header;
                    }
                }
                Line::BlockData => {
                    if byte == b'\n' {
                        self.block_lines += 1;
                        if self.block_lines > MAX_BLOCK_LINES {
                            self.fatal(&mut emit);
                        } else {
                            self.line = Line::Header;
                        }
                    }
                }
                Line::Header => {
                    if byte == b'\n' {
                        self.complete_header(&mut emit);
                        self.header.clear();
                    } else {
                        self.header.push(byte);
                        if self.block.is_some()
                            && ![b"%begin ".as_slice(), b"%end ", b"%error "]
                                .iter()
                                .any(|prefix| {
                                    prefix.starts_with(&self.header)
                                        || self.header.starts_with(prefix)
                                })
                        {
                            // Command response data can be arbitrarily long up
                            // to the separate block budget. Retain no body bytes.
                            self.header.clear();
                            self.line = Line::BlockData;
                            continue;
                        }
                        let command = self.block.is_some()
                            || [b"%begin ".as_slice(), b"%end ", b"%error "]
                                .iter()
                                .any(|prefix| self.header.starts_with(prefix));
                        let limit = if command {
                            MAX_COMMAND_HEADER
                        } else if self.header.starts_with(b"%output ") {
                            MAX_OUTPUT_HEADER
                        } else {
                            MAX_HEADER
                        };
                        if self.header.len() > limit {
                            self.fatal(&mut emit);
                        } else if self.block.is_none()
                            && byte == b' '
                            && self.start_payload(&mut emit)
                        {
                            self.header.clear();
                        }
                    }
                }
                Line::Poisoned => unreachable!(),
            }
        }
    }

    fn start_payload(&mut self, emit: &mut impl FnMut(Decoded)) -> bool {
        let mut words = self.header[..self.header.len() - 1].split(|b| *b == b' ');
        let tag = words.next().unwrap_or_default();
        if !matches!(tag, b"%output" | b"%extended-output") {
            return false;
        }
        let Some(id) = words.next().and_then(pane) else {
            return false;
        };
        if tag == b"%extended-output" {
            if words.next().and_then(number).is_none() {
                return false;
            }
            if words.next_back() != Some(&b":"[..]) {
                return false;
            }
        } else if words.next().is_some() {
            return false;
        }
        // The pool only sends attach. Its matching response block must be
        // observed before pane output can establish any coverage. A lost or
        // malformed initial begin cannot turn command data into authority.
        if !self.attached {
            self.fatal(emit);
            return true;
        }
        self.line = Line::Payload {
            pane: id,
            octal: None,
        };
        emit(Decoded::Start(id));
        true
    }

    fn complete_header(&mut self, emit: &mut impl FnMut(Decoded)) {
        let tag = self.header.split(|b| *b == b' ').next().unwrap_or_default();
        if let Some(expected) = self.block {
            self.block_lines += 1;
            if self.block_lines > MAX_BLOCK_LINES {
                self.fatal(emit);
                return;
            }
            if matches!(tag, b"%end" | b"%error") {
                if frame(&self.header) == Some(expected) {
                    self.block = None;
                    if !self.attached && tag == b"%error" {
                        emit(Decoded::Note(Note::Exit));
                        self.line = Line::Poisoned;
                    } else {
                        self.attached = true;
                    }
                    self.block_bytes = 0;
                    self.block_lines = 0;
                } else {
                    self.fatal(emit);
                }
            } else if tag == b"%begin" {
                self.fatal(emit);
            }
            // Everything else is command response data, even apparent notifications.
            return;
        }
        match tag {
            b"%begin" => match frame(&self.header) {
                Some(key) => {
                    self.block = Some(key);
                    self.block_bytes = 0;
                    self.block_lines = 0;
                }
                None => self.fatal(emit),
            },
            b"%end" | b"%error" => self.fatal(emit),
            b"%output" | b"%extended-output" => {
                emit(Decoded::Gap(None, ObservationGap::MalformedTransport));
            }
            b"%pause" | b"%continue" => {
                let id = self.header.split(|b| *b == b' ').nth(1).and_then(pane);
                if let Some(id) = id {
                    emit(if tag == b"%pause" {
                        Decoded::Pause(id)
                    } else {
                        Decoded::Continue(id)
                    });
                } else {
                    emit(Decoded::Gap(None, ObservationGap::MalformedTransport));
                }
            }
            b"%sessions-changed" | b"%session-renamed" | b"%session-changed" => {
                emit(Decoded::Note(Note::SessionsChanged));
            }
            b"%window-add"
            | b"%window-close"
            | b"%window-pane-changed"
            | b"%window-renamed"
            | b"%layout-change"
            | b"%unlinked-window-add"
            | b"%unlinked-window-close"
            | b"%unlinked-window-renamed"
            | b"%pane-mode-changed"
            | b"%session-window-changed" => {
                emit(Decoded::Note(Note::Lifecycle));
            }
            b"%exit" => {
                emit(Decoded::Note(Note::Exit));
                self.line = Line::Poisoned;
            }
            _ => {}
        }
    }

    pub(super) fn finish(&mut self, mut emit: impl FnMut(Decoded)) {
        // EOF always loses coverage, including EOF inside an octal escape or block.
        emit(Decoded::Gap(None, ObservationGap::Disconnected));
        self.line = Line::Poisoned;
        self.header.clear();
    }
}
