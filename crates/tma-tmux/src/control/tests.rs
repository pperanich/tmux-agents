use super::decoder::{Decoded, Decoder, MAX_BLOCK_BYTES, MAX_BLOCK_LINES, MAX_HEADER};
use super::protocol::{Collector, MAX_PROGRAM_EVENTS, MAX_PROGRAM_PAYLOAD_BYTES};
use super::terminal::Scanner;
use super::*;
use tma_core::program_status::{
    ProgramCommand, ProgramEvent, ProgramStatus, TerminalLifecycle, MAX_FRAME_BYTES,
};

const REPORT: &[u8] = b"\x1b]7501;state=working:app=pi\x1b\\";

fn ready_decoder() -> Decoder {
    let mut decoder = Decoder::default();
    decoder.feed(b"%begin 1 1 0\n%end 1 1 0\n", true, |_| {});
    decoder
}

fn test_client(serial: u64) -> ControlClient {
    let mut child = std::process::Command::new("cat")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    ControlClient {
        _stdin: child.stdin.take().unwrap(),
        stdout: child.stdout.take().unwrap(),
        child,
        decoder: ready_decoder(),
        serial,
        attached: false,
        dead: false,
    }
}

fn encoded(payload: &[u8]) -> Vec<u8> {
    let mut bytes = b"%output %1 ".to_vec();
    for &byte in payload {
        if !(0x20..=0x7e).contains(&byte) || byte == b'\\' {
            bytes.extend_from_slice(format!("\\{byte:03o}").as_bytes());
        } else {
            bytes.push(byte);
        }
    }
    bytes.push(b'\n');
    bytes
}

struct Harness {
    decoder: Decoder,
    collector: Collector,
    activity: usize,
    fatal: bool,
}

impl Harness {
    fn new() -> Self {
        let mut harness = Self::unsynchronized();
        // Synthetic unit setup explicitly establishes safe terminal framing.
        harness.feed(&encoded(b"framing\x1b\\"));
        harness.activity = 0;
        harness
    }

    fn unsynchronized() -> Self {
        let mut collector = Collector::default();
        collector.reconcile(
            &[(SessionId::new(1), PaneId::new(1))],
            &BTreeMap::from([(SessionId::new(1), 1)]),
        );
        Self {
            decoder: ready_decoder(),
            collector,
            activity: 0,
            fatal: false,
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        self.decoder.feed(bytes, true, |item| {
            self.activity += usize::from(matches!(item, Decoded::Start(_)));
            self.fatal |= item == Decoded::Fatal;
            self.collector.decoded(SessionId::new(1), 1, item);
        });
    }

    fn observations(&mut self) -> Vec<tma_core::program_status::ObservedEvent> {
        self.collector.drain().observations
    }
}

#[test]
fn all_os_split_positions_and_generated_chunks_preserve_reports() {
    let bytes = encoded(REPORT);
    let mut baseline = Harness::new();
    baseline.feed(&bytes);
    let expected = baseline.observations();
    assert_eq!(expected.len(), 1);
    for split in 0..=bytes.len() {
        let mut h = Harness::new();
        h.feed(&bytes[..split]);
        h.feed(&bytes[split..]);
        assert_eq!(h.observations(), expected, "split {split}");
    }
    for seed in 1..=64u64 {
        let mut h = Harness::new();
        let mut random = seed;
        let mut offset = 0;
        while offset < bytes.len() {
            random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
            let end = (offset + 1 + ((random >> 32) as usize % 17)).min(bytes.len());
            h.feed(&bytes[offset..end]);
            offset = end;
        }
        assert_eq!(h.observations(), expected);
    }
}

#[test]
fn native_pi_fixture_spans_notifications_and_every_read_split() {
    let fixture =
        include_bytes!("../../../tma-core/fixtures/program_status/pi-1.3.0.split.control");
    // The pinned fixture uses pane zero; topology accepts that stable identity.
    for split in 0..=fixture.len() {
        let mut h = Harness::new();
        h.collector.reconcile(
            &[(SessionId::new(1), PaneId::new(0))],
            &BTreeMap::from([(SessionId::new(1), 1)]),
        );
        // Synthetic framing delimiter precedes the unchanged native Pi fixture.
        h.feed(b"%output %0 framing\\033\\134\n");
        h.feed(&fixture[..split]);
        h.feed(&fixture[split..]);
        let events = h.observations();
        assert_eq!(events.len(), 1, "split {split}");
        let ProgramEvent::Command(ProgramCommand::Replace(record)) = &events[0].event else {
            panic!("expected report")
        };
        assert_eq!(record.status, ProgramStatus::Idle);
        assert_eq!(record.app.as_ref().unwrap().as_ref().as_str(), "pi");
    }
}

#[test]
fn terminal_bytes_continue_across_every_notification_split() {
    for split in 0..=REPORT.len() {
        let mut h = Harness::new();
        h.feed(&encoded(&REPORT[..split]));
        h.feed(&encoded(&REPORT[split..]));
        assert_eq!(h.observations().len(), 1, "split {split}");
    }
}

#[test]
fn escaped_non_utf8_literal_backslash_and_long_ordinary_tail() {
    let mut h = Harness::new();
    h.feed(b"%output %1 ");
    for _ in 0..300 {
        h.feed(&[b'x'; 4096]);
        assert!(h.collector.pending_bytes() <= MAX_FRAME_BYTES);
    }
    h.feed(b"\\377\\134");
    let mut tail = encoded(REPORT);
    tail.drain(..b"%output %1 ".len());
    h.feed(&tail);
    assert_eq!(h.activity, 1);
    assert_eq!(h.observations().len(), 1);
}

#[test]
fn available_reads_stop_at_the_per_client_wake_budget() {
    use std::fs::OpenOptions;
    use std::io::{Seek, Write};
    use std::os::fd::OwnedFd;
    use std::time::SystemTime;

    // A regular fd supplies more than a wake's budget without pipe scheduling
    // making WouldBlock end the read early. Exercise the production read loop.
    let path = std::env::temp_dir().join(format!(
        "tma_control_budget_{}_{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    std::fs::remove_file(path).unwrap();
    file.write_all(b"%output %1 ").unwrap();
    file.write_all(&vec![b'x'; READ_CHUNK * MAX_CHUNKS_PER_READ * 2])
        .unwrap();
    file.rewind().unwrap();
    let mut offset = file.try_clone().unwrap();
    let mut client = test_client(1);
    client.stdout = ChildStdout::from(OwnedFd::from(file));
    let mut notes = Vec::new();
    client.read_available(&mut notes, None, SessionId::new(1));
    assert_eq!(offset.stream_position().unwrap(), 1024 * 1024);
    assert!(!client.dead);
    assert_eq!(notes, vec![Notification::Output { pane: "%1".into() }]);
}

#[test]
fn extended_headers_ignore_future_fields_and_backlog_age() {
    let payload = encoded(REPORT);
    for header in [
        b"%extended-output %1 123 : ".as_slice(),
        b"%extended-output %1 18446744073709551615 future=1 other : ",
    ] {
        let mut bytes = header.to_vec();
        bytes.extend_from_slice(&payload[b"%output %1 ".len()..]);
        for split in 0..=bytes.len() {
            let mut h = Harness::new();
            h.feed(&bytes[..split]);
            h.feed(&bytes[split..]);
            assert_eq!(h.observations().len(), 1);
        }
    }
}

#[test]
fn command_blocks_treat_apparent_notifications_as_data() {
    for end in ["%end", "%error"] {
        let mut bytes = b"%begin 1 2 0\n".to_vec();
        bytes.extend(encoded(REPORT));
        bytes.extend_from_slice(b"%pause %1\n%sessions-changed\n");
        bytes.extend_from_slice(format!("{end} 1 2 0\n").as_bytes());
        bytes.extend(encoded(REPORT));
        for split in 0..=bytes.len() {
            let mut h = Harness::new();
            h.feed(&bytes[..split]);
            h.feed(&bytes[split..]);
            assert_eq!(h.activity, 1);
            assert_eq!(h.observations().len(), 1);
            assert!(!h.fatal);
        }
    }
}

#[test]
fn malformed_lost_or_nested_command_framing_poison_client() {
    for prefix in [
        "%begin 1 nope 0\n",
        "%end 1 2 0\n",
        "%begin 1 2 0\n%end 1 3 0\n",
        "%begin 1 2 0\n%begin 1 3 0\n",
        "%begin 18446744073709551616 2 0\n",
    ] {
        let mut h = Harness::new();
        h.feed(prefix.as_bytes());
        h.feed(&encoded(REPORT));
        assert!(h.fatal, "{prefix}");
        let drain = h.collector.drain();
        assert!(drain.observations.is_empty());
        assert_eq!(drain.invalidations.len(), 1);
    }
}

#[test]
fn numeric_header_response_line_and_byte_limits_are_enforced() {
    let mut h = Harness::new();
    h.feed(b"%extended-output %1 1 ");
    h.feed(&vec![b'x'; MAX_HEADER]);
    assert!(h.fatal);
    let mut h = Harness::new();
    h.feed(b"%begin 1 2 0\n");
    for _ in 0..=MAX_BLOCK_LINES {
        h.feed(b"\n");
    }
    assert!(h.fatal);
    let mut h = Harness::new();
    h.feed(b"%begin 1 2 0\n");
    // Stay under the individual header bound and line limit while exceeding bytes.
    let line = [vec![b'x'; 512], vec![b'\n']].concat();
    for _ in 0..=(MAX_BLOCK_BYTES / line.len()) {
        h.feed(&line);
    }
    assert!(h.fatal);
}

#[test]
fn command_line_budget_includes_the_matching_end() {
    for body_lines in [MAX_BLOCK_LINES - 1, MAX_BLOCK_LINES] {
        let mut h = Harness::new();
        h.feed(b"%begin 1 2 0\n");
        for _ in 0..body_lines {
            h.feed(b"data\n");
        }
        h.feed(b"%end 1 2 0\n");
        h.feed(&encoded(REPORT));
        assert_eq!(h.fatal, body_lines == MAX_BLOCK_LINES);
        assert_eq!(
            h.observations().len(),
            usize::from(body_lines < MAX_BLOCK_LINES)
        );
    }
}

#[test]
fn malformed_octal_truncated_escape_and_eof_revoke_coverage() {
    for malformed in [b"\\4".as_slice(), b"\\08", b"\\03\n", b"\\\n"] {
        let mut h = Harness::new();
        h.feed(&encoded(REPORT));
        h.feed(b"%output %1 ");
        h.feed(malformed);
        let drain = h.collector.drain();
        assert_eq!(drain.invalidations.len(), 1);
        assert!(drain.observations.is_empty());
        assert_eq!(
            drain.invalidations[0].event,
            ProgramEvent::Gap(ObservationGap::MalformedTransport)
        );
    }
    let mut h = Harness::new();
    h.feed(b"%output %1 \\03");
    h.decoder
        .finish(|item| h.collector.decoded(SessionId::new(1), 1, item));
    assert_eq!(
        h.collector.drain().invalidations[0].event,
        ProgramEvent::Gap(ObservationGap::Disconnected)
    );
}

#[test]
fn pause_continue_and_unknown_loss_require_safe_resync_and_fresh_report() {
    let mut h = Harness::new();
    h.feed(&encoded(REPORT));
    h.feed(b"%pause %1\n");
    h.feed(&encoded(REPORT));
    h.feed(b"%continue %1\n");
    assert!(h.observations().is_empty());
    // First ST safely closes unknown framing; its report is sacrificed.
    h.feed(&encoded(REPORT));
    assert!(h.observations().is_empty());
    h.feed(&encoded(REPORT));
    assert_eq!(h.observations().len(), 1);
}

#[test]
fn terminal_strings_skip_nested_fake_reports_and_handle_cancellation() {
    for introducer in *b"P_^X]k" {
        let mut bytes = vec![0x1b, introducer];
        bytes.extend_from_slice(b"unrelated");
        bytes.extend_from_slice(REPORT);
        bytes.extend_from_slice(b"\x1b\\");
        bytes.extend_from_slice(REPORT);
        let mut h = Harness::new();
        h.feed(&encoded(&bytes));
        assert_eq!(h.observations().len(), 1, "introducer {introducer}");
    }
    for cancel in [0x18, 0x1a] {
        let mut bytes = b"\x1b]ignored\x1b".to_vec();
        bytes.push(cancel);
        bytes.extend_from_slice(REPORT);
        let mut h = Harness::new();
        h.feed(&encoded(&bytes));
        assert_eq!(h.observations().len(), 1);
    }
}

#[test]
fn dcs_escape_pairs_and_cancel_bytes_cannot_expose_embedded_reports() {
    // tmux 3.7b input_state_dcs_handler/escape consume both ESC ESC and
    // CAN/SUB as DCS data. Only the unescaped ST below closes this DCS.
    for data in [b"\x1b\x1b\\".as_slice(), b"\x18", b"\x1a"] {
        let mut bytes = b"\x1bPtmux;data".to_vec();
        bytes.extend_from_slice(data);
        bytes.extend_from_slice(b"\x1b]7501;state=done\x07\x1b\\");
        bytes.extend_from_slice(REPORT);
        let wire = encoded(&bytes);
        for split in 0..=wire.len() {
            let mut h = Harness::new();
            h.feed(&wire[..split]);
            h.feed(&wire[split..]);
            assert_eq!(h.observations().len(), 1, "data {data:?}, split {split}");
        }
    }
}

#[test]
fn cold_stream_sync_requires_known_dcs_phase_and_unescaped_st() {
    for prefix in [
        b"\x18".as_slice(),
        b"\x1a",
        b"\x1b\\",
        b"\x1b\x1b\\",
        b"ordinary\x1b\x1b\\",
    ] {
        let mut bytes = prefix.to_vec();
        bytes.extend_from_slice(b"\x1b]7501;state=done\x07");
        let wire = encoded(&bytes);
        for split in 0..=wire.len() {
            let mut h = Harness::unsynchronized();
            h.feed(&wire[..split]);
            h.feed(&wire[split..]);
            assert!(
                h.observations().is_empty(),
                "prefix {prefix:?}, split {split}"
            );
            h.feed(&encoded(b"framing\x1b\\"));
            h.feed(&encoded(REPORT));
            assert_eq!(h.observations().len(), 1);
        }
    }
    for escapes in 1..=4 {
        let mut bytes = b"ordinary".to_vec();
        bytes.extend(std::iter::repeat_n(0x1b, escapes));
        bytes.push(b'\\');
        bytes.extend_from_slice(b"\x1b]7501;state=done\x07");
        let mut h = Harness::unsynchronized();
        h.feed(&encoded(&bytes));
        assert_eq!(h.observations().len(), escapes % 2);
    }
}

#[test]
fn escape_ignored_bytes_do_not_hide_a_dcs_introducer() {
    for ignored in [0x00, 0x07, 0x17, 0x19, 0x1c, 0x1f, 0x7f, 0x80, 0xff] {
        let mut bytes = vec![0x1b, ignored, b'P', b'q'];
        bytes.extend_from_slice(b"\x1b]7501;state=done\x07\x1b\\");
        bytes.extend_from_slice(REPORT);
        let mut h = Harness::new();
        h.feed(&encoded(&bytes));
        assert_eq!(h.observations().len(), 1, "ignored {ignored}");
    }
}

#[test]
fn ignored_escape_bytes_do_not_manufacture_wire_osc_introducers() {
    for ignored in [0x00, 0x07, 0x17, 0x19, 0x1c, 0x1f, 0x7f, 0x80, 0xff] {
        let mut bytes = vec![0x1b, ignored];
        bytes.extend_from_slice(b"]7501;state=done\x07");
        let mut h = Harness::new();
        h.feed(&encoded(&bytes));
        assert!(h.observations().is_empty(), "ignored {ignored}");
        h.feed(&encoded(REPORT));
        assert_eq!(h.observations().len(), 1);
    }
}

#[test]
fn ignored_escape_bytes_do_not_manufacture_wire_st_terminators() {
    for ignored in [0x00, 0x17, 0x19, 0x1c, 0x1f, 0x7f, 0x80, 0xff] {
        let mut bytes = b"\x1b]7501;state=done:x=\x1b".to_vec();
        bytes.push(ignored);
        bytes.push(b'\\');
        let mut h = Harness::new();
        h.feed(&encoded(&bytes));
        assert!(h.observations().is_empty(), "ignored {ignored}");
        assert_eq!(h.collector.stats().rejected_frames, 1);
        h.feed(&encoded(REPORT));
        assert_eq!(h.observations().len(), 1);
    }
}

#[test]
fn skipped_strings_track_a_dcs_introduced_by_an_escape() {
    for outer in b"]_^Xk" {
        for ignored in [b"".as_slice(), b"\x00", b"\x7f", b"\x07", b"\xff"] {
            for cancel in [0x07, 0x18, 0x1a] {
                let mut bytes = vec![0x1b, *outer];
                bytes.extend_from_slice(b"unrelated\x1b");
                bytes.extend_from_slice(ignored);
                bytes.extend_from_slice(b"Pq");
                bytes.push(cancel);
                bytes.extend_from_slice(b"\x1b]7501;state=done\x07\x1b\\");
                bytes.extend_from_slice(REPORT);
                let wire = encoded(&bytes);
                for split in 0..=wire.len() {
                    let mut h = Harness::new();
                    h.feed(&wire[..split]);
                    h.feed(&wire[split..]);
                    assert_eq!(
                        h.observations().len(),
                        1,
                        "outer {outer}, ignored {ignored:?}, cancel {cancel}, split {split}"
                    );
                }
            }
        }
    }
}

#[test]
fn canceled_osc133_does_not_emit_a_shell_prompt_lifecycle() {
    for ending in [b"\x07".as_slice(), b"\x1b\\"] {
        let mut bytes = b"\x1b]133;A;\x1b".to_vec();
        bytes.extend_from_slice(ending);
        let mut h = Harness::new();
        h.feed(&encoded(&bytes));
        assert!(h.observations().is_empty(), "ending {ending:?}");
        h.feed(&encoded(REPORT));
        assert_eq!(h.observations().len(), 1);
    }
}

#[test]
fn lifecycle_ris_prompt_soft_reset_and_alternate_screen() {
    let mut h = Harness::new();
    h.feed(&encoded(
        b"\x1b[!p\x1b[?1049h\x1b[?1049l\x1bc\x1b]133;A;extra\x07",
    ));
    let events = h.observations();
    assert_eq!(events.len(), 2);
    assert_eq!(
        events[0].event,
        ProgramEvent::Lifecycle(TerminalLifecycle::FullReset)
    );
    assert_eq!(
        events[1].event,
        ProgramEvent::Lifecycle(TerminalLifecycle::ShellPrompt)
    );
}

#[test]
fn invalid_and_oversized_complete_frames_are_atomic_rejections() {
    let mut h = Harness::new();
    h.feed(&encoded(b"\x1b]7501;state=working:msg=AA==\x07"));
    let mut huge = b"\x1b]7501;state=idle:msg=".to_vec();
    huge.extend(vec![b'A'; MAX_FRAME_BYTES * 2]);
    huge.extend_from_slice(b"\x07");
    h.feed(&encoded(&huge));
    h.feed(&encoded(REPORT));
    let drain = h.collector.drain();
    assert!(drain.invalidations.is_empty());
    assert_eq!(drain.observations.len(), 1);
}

#[test]
fn bel_and_st_exact_wire_frame_size_boundaries() {
    for ending in [b"\x07".as_slice(), b"\x1b\\"] {
        for extra in [0, 1] {
            let mut scanner = Scanner::default();
            let mut frame = b"\x1b]7501;state=idle:unused=".to_vec();
            frame.resize(MAX_FRAME_BYTES - ending.len() + extra, b'a');
            frame.extend_from_slice(ending);
            let events: Vec<_> = frame
                .iter()
                .filter_map(|&byte| scanner.feed(byte))
                .collect();
            assert_eq!(
                events.len(),
                usize::from(extra == 0),
                "ending {ending:?}, extra {extra}"
            );
            assert_eq!(scanner.rejected, extra as u64);
        }
    }
}

#[test]
fn pending_escapes_before_bel_and_st_are_part_of_the_frame() {
    for ending in [b"\x07".as_slice(), b"\x1b\\"] {
        for escapes in 1..=3 {
            let mut frame = b"\x1b]7501;state=done".to_vec();
            frame.extend(std::iter::repeat_n(0x1b, escapes));
            frame.extend_from_slice(ending);
            let bytes = encoded(&frame);
            for split in 0..=bytes.len() {
                let mut h = Harness::new();
                h.feed(&bytes[..split]);
                h.feed(&bytes[split..]);
                let drain = h.collector.drain();
                assert!(
                    drain.observations.is_empty(),
                    "ending {ending:?}, escapes {escapes}, split {split}"
                );
                assert!(drain.invalidations.is_empty());
                assert_eq!(h.collector.stats().rejected_frames, 1);
                h.feed(&encoded(REPORT));
                assert_eq!(h.observations().len(), 1);
            }
        }
    }
}

#[test]
fn clear_completion_and_query_remain_ordered_and_observational() {
    let mut h = Harness::new();
    h.feed(&encoded(
        b"\x1b]7501;state=done\x07\x1b]7501;state=clear\x07\x1b]7501;?\x07",
    ));
    let events = h.observations();
    assert_eq!(events.len(), 3);
    assert!(matches!(
        events[0].event,
        ProgramEvent::Command(ProgramCommand::Replace(_))
    ));
    assert!(matches!(
        events[1].event,
        ProgramEvent::Command(ProgramCommand::Clear(_))
    ));
    assert_eq!(
        events[2].event,
        ProgramEvent::Command(ProgramCommand::Query)
    );
    assert!(events
        .windows(2)
        .all(|pair| pair[0].sequence < pair[1].sequence));
}

#[test]
fn saturated_queue_invalidates_out_of_band_then_keeps_post_gap_report() {
    let mut h = Harness::new();
    let report = encoded(REPORT);
    for _ in 0..=MAX_PROGRAM_EVENTS {
        h.feed(&report);
    }
    h.feed(&report);
    let drain = h.collector.drain();
    assert_eq!(drain.invalidations.len(), 1);
    assert_eq!(drain.observations.len(), 1);
    let gap = &drain.invalidations[0];
    assert_eq!(gap.event, ProgramEvent::Gap(ObservationGap::QueueOverflow));
    assert_eq!(gap.connection, drain.observations[0].connection);
    assert!(gap.sequence < drain.observations[0].sequence);
}

#[test]
fn decoded_payload_budget_counts_queue_and_partial_candidates() {
    let mut h = Harness::new();
    let mut frame = b"\x1b]7501;state=working:msg=".to_vec();
    frame.extend_from_slice("YWFh".repeat(682).as_bytes()); // 2046 'a' bytes.
    frame.push(0x07);
    let report = encoded(&frame);
    for _ in 0..600 {
        h.feed(&report);
    }
    assert!(h.collector.pending_bytes() <= MAX_PROGRAM_PAYLOAD_BYTES + MAX_FRAME_BYTES);
    assert!(h.collector.overflows > 0);
    let drain = h.collector.drain();
    assert!(!drain.invalidations.is_empty());
    assert_eq!(h.collector.pending_bytes(), 0);
    h.feed(&encoded(b"\x1b]7501;state=wor"));
    assert!(h.collector.pending_bytes() > 0);
}

#[test]
fn binding_boundary_discards_prebinding_events_and_partial_report() {
    for split in b"%output %1 ".len()..encoded(REPORT).len() {
        let mut h = Harness::new();
        let bytes = encoded(REPORT);
        // Split raw transport too, including inside octal ESC.
        let cut = split.min(bytes.len() - 1);
        h.feed(&bytes[..cut]);
        let boundary = h
            .collector
            .boundary(PaneId::new(1), h.decoder.in_payload(PaneId::new(1)))
            .unwrap();
        h.feed(&bytes[cut..]);
        h.feed(&encoded(REPORT));
        let drain = h.collector.drain();
        assert_eq!(drain.observations.len(), 1, "split {split}");
        assert_eq!(drain.observations[0].connection, boundary.connection);
        assert!(drain.observations[0].sequence > boundary.sequence_floor);
    }
}

#[test]
fn binding_quarantine_preserves_fully_observed_string_framing() {
    for quarantined in [
        b"\x1b]7501;state=done\x07".as_slice(),
        b"\x1b]title;unrelated\x07",
        b"\x1bPembedded \x1b]7501;state=done\x07\x1b\\",
    ] {
        let mut h = Harness::new();
        h.feed(b"%output %1 prebinding ordinary output");
        h.collector.boundary(PaneId::new(1), true);
        let bytes = encoded(quarantined);
        for byte in &bytes[b"%output %1 ".len()..] {
            h.feed(&[*byte]);
        }
        assert!(h.observations().is_empty());
        h.feed(&encoded(b"\x1b]7501;state=working:app=pi\x07"));
        assert_eq!(h.observations().len(), 1, "quarantined {quarantined:?}");
    }
}

#[test]
fn quarantined_notification_cannot_leave_an_eligible_partial_introducer() {
    for ending in [b"\x07".as_slice(), b"\x1b\\"] {
        let mut h = Harness::new();
        h.feed(b"%output %1 prebinding ordinary output");
        h.collector.boundary(PaneId::new(1), true);
        let bytes = encoded(b"\x1b");
        h.feed(&bytes[b"%output %1 ".len()..]);
        let mut suffix = b"]7501;state=done".to_vec();
        suffix.extend_from_slice(ending);
        h.feed(&encoded(&suffix));
        assert!(h.observations().is_empty(), "ending {ending:?}");
        h.feed(&encoded(b"\x1b\\"));
        h.feed(&encoded(REPORT));
        assert_eq!(h.observations().len(), 1);
    }
}

#[test]
fn binding_boundary_uses_only_the_selected_owners_payload() {
    let mut pool = ControlPool::default();
    pool.set_program_collection(true);
    pool.clients.insert("$1".into(), test_client(1));
    pool.clients.insert("$2".into(), test_client(2));
    pool.protocol.as_mut().unwrap().reconcile(
        &[
            (SessionId::new(1), PaneId::new(1)),
            (SessionId::new(2), PaneId::new(1)),
        ],
        &BTreeMap::from([(SessionId::new(1), 1), (SessionId::new(2), 2)]),
    );
    let mut notes = Vec::new();
    pool.clients.get_mut("$1").unwrap().ingest(
        &encoded(b"framing\x1b\\"),
        &mut notes,
        pool.protocol.as_mut(),
        SessionId::new(1),
    );
    // The linked client's stale fragment must have no effect on the selected owner.
    pool.clients.get_mut("$2").unwrap().ingest(
        b"%output %1 stale linked output",
        &mut notes,
        pool.protocol.as_mut(),
        SessionId::new(2),
    );
    let boundary = pool.program_boundary(PaneId::new(1)).unwrap();
    pool.clients.get_mut("$1").unwrap().ingest(
        &encoded(REPORT),
        &mut notes,
        pool.protocol.as_mut(),
        SessionId::new(1),
    );
    let drain = pool.drain_program();
    assert_eq!(drain.observations.len(), 1);
    assert_eq!(drain.observations[0].connection, boundary.connection);
    assert!(drain.observations[0].sequence > boundary.sequence_floor);
}

#[test]
fn binding_inside_unrelated_string_preserves_skip_framing() {
    for intro in [b"\x1bP".as_slice(), b"\x1b]title;"] {
        let mut h = Harness::new();
        h.feed(&encoded(intro));
        h.collector.boundary(PaneId::new(1), false);
        h.feed(&encoded(REPORT));
        assert!(h.observations().is_empty());
        h.feed(&encoded(REPORT));
        assert_eq!(h.observations().len(), 1);
    }
}

#[test]
fn linked_owners_are_deterministic_retained_and_never_splice() {
    let mut collector = Collector::default();
    let topology = [
        (SessionId::new(2), PaneId::new(1)),
        (SessionId::new(1), PaneId::new(1)),
    ];
    let clients = BTreeMap::from([(SessionId::new(2), 2), (SessionId::new(1), 1)]);
    collector.reconcile(&topology, &clients);
    let mut first = ready_decoder();
    let mut second = ready_decoder();
    second.feed(&encoded(REPORT), true, |item| {
        collector.decoded(SessionId::new(2), 2, item)
    });
    assert!(collector.drain().observations.is_empty());
    first.feed(&encoded(&REPORT[..10]), true, |item| {
        collector.decoded(SessionId::new(1), 1, item)
    });
    collector.reconcile(&[(SessionId::new(2), PaneId::new(1))], &clients);
    second.feed(&encoded(&REPORT[10..]), true, |item| {
        collector.decoded(SessionId::new(2), 2, item)
    });
    assert!(collector.drain().observations.is_empty());
    second.feed(&encoded(REPORT), true, |item| {
        collector.decoded(SessionId::new(2), 2, item)
    });
    assert_eq!(collector.drain().observations.len(), 1);
    // Readding a lower session does not steal a still-valid owner.
    collector.reconcile(&topology, &clients);
    first.feed(&encoded(REPORT), true, |item| {
        collector.decoded(SessionId::new(1), 1, item)
    });
    second.feed(&encoded(REPORT), true, |item| {
        collector.decoded(SessionId::new(2), 2, item)
    });
    assert_eq!(collector.drain().observations.len(), 1);
}

#[test]
fn replacement_client_gets_new_epoch_and_old_client_is_ignored() {
    let mut h = Harness::new();
    h.feed(&encoded(REPORT));
    let original = h.observations()[0].connection;
    h.collector.reconcile(
        &[(SessionId::new(1), PaneId::new(1))],
        &BTreeMap::from([(SessionId::new(1), 2)]),
    );
    h.feed(&encoded(REPORT)); // stale connection serial 1.
    assert!(h.observations().is_empty());
    let mut replacement = ready_decoder();
    for _ in 0..2 {
        replacement.feed(&encoded(REPORT), true, |item| {
            h.collector.decoded(SessionId::new(1), 2, item)
        });
    }
    let events = h.observations();
    assert_eq!(events.len(), 1);
    assert!(events[0].connection > original);
}

#[test]
fn topology_failure_suppresses_reports_and_pane_removal_stays_local() {
    let mut h = Harness::new();
    let clients = BTreeMap::from([(SessionId::new(1), 1)]);
    h.collector.reconcile(
        &[
            (SessionId::new(1), PaneId::new(1)),
            (SessionId::new(1), PaneId::new(2)),
        ],
        &clients,
    );
    h.feed(&encoded(REPORT));
    let original = h.observations()[0].connection;
    h.collector
        .reconcile(&[(SessionId::new(1), PaneId::new(1))], &clients);
    h.feed(&encoded(REPORT));
    let drain = h.collector.drain();
    assert!(!drain.all_invalidated);
    assert_eq!(drain.invalidations[0].pane, PaneId::new(2));
    assert_eq!(drain.observations[0].connection, original);
    h.collector.topology_failed();
    h.feed(&encoded(REPORT));
    assert!(h.observations().is_empty());
    h.collector
        .reconcile(&[(SessionId::new(1), PaneId::new(1))], &clients);
    h.feed(&encoded(REPORT));
    h.feed(&encoded(REPORT));
    assert_eq!(h.observations().len(), 1);
}

#[test]
fn departed_invalidation_bookkeeping_has_whole_scope_overflow_fallback() {
    let mut collector = Collector::default();
    let clients = BTreeMap::from([(SessionId::new(1), 1)]);
    for pane in 0..=(MAX_PROGRAM_EVENTS as u32 + 1) {
        collector.reconcile(&[(SessionId::new(1), PaneId::new(pane))], &clients);
    }
    let drain = collector.drain();
    assert!(drain.all_invalidated);
    assert!(drain.invalidations.len() <= MAX_PROGRAM_EVENTS + 1);
}

#[test]
fn off_path_discards_payload_without_protocol_storage_and_toggle_clears_state() {
    let mut pool = ControlPool::default();
    assert!(pool.protocol.is_none());
    let mut decoder = ready_decoder();
    let mut decoded_bytes = 0;
    decoder.feed(&encoded(REPORT), false, |item| {
        decoded_bytes += usize::from(matches!(item, Decoded::Byte(..)))
    });
    assert_eq!(decoded_bytes, 0);
    pool.set_program_collection(true);
    let collector = pool.protocol.as_mut().unwrap();
    collector.reconcile(
        &[(SessionId::new(1), PaneId::new(1))],
        &BTreeMap::from([(SessionId::new(1), 1)]),
    );
    decoder.feed(&encoded(b"framing\x1b\\"), true, |item| {
        collector.decoded(SessionId::new(1), 1, item)
    });
    decoder.feed(&encoded(REPORT), true, |item| {
        collector.decoded(SessionId::new(1), 1, item)
    });
    let old_epoch = pool.drain_program().observations[0].connection;
    pool.set_program_collection(false);
    assert_eq!(pool.pending_program_bytes(), 0);
    pool.set_program_collection(true);
    let collector = pool.protocol.as_mut().unwrap();
    collector.reconcile(
        &[(SessionId::new(1), PaneId::new(1))],
        &BTreeMap::from([(SessionId::new(1), 1)]),
    );
    let boundary = pool.program_boundary(PaneId::new(1)).unwrap();
    assert!(boundary.connection > old_epoch);
    assert!(pool.drain_program().observations.is_empty());
}

#[test]
fn initial_attach_frame_must_complete_before_output_is_trusted() {
    for initial in [
        b"".as_slice(),
        b"%begi 1 2 0\n",
        b"%begin 1 2 0\n",
        b"%begin 1 2 0\n%error 1 2 0\n",
    ] {
        let mut decoder = Decoder::default();
        let mut events = Vec::new();
        decoder.feed(initial, true, |item| events.push(item));
        decoder.feed(&encoded(REPORT), true, |item| events.push(item));
        assert!(!events
            .iter()
            .any(|item| matches!(item, Decoded::Start(_) | Decoded::Byte(..))));
    }
}

#[test]
fn long_command_response_body_is_streamed_without_notification_parsing() {
    let mut h = Harness::new();
    h.feed(b"%begin 1 2 0\n");
    h.feed(&vec![b'x'; MAX_HEADER * 4]);
    h.feed(b"\n%end 1 2 0\n");
    h.feed(&encoded(REPORT));
    assert!(!h.fatal);
    assert_eq!(h.observations().len(), 1);
}

#[test]
fn extended_header_limit_includes_the_payload_delimiter() {
    for extra in [0, 1] {
        let mut h = Harness::new();
        let mut header = b"%extended-output %1 0 ".to_vec();
        header.resize(MAX_HEADER - 3 + extra, b'x');
        header.extend_from_slice(b" : ");
        let bytes = encoded(REPORT);
        header.extend_from_slice(&bytes[b"%output %1 ".len()..]);
        h.feed(&header);
        assert_eq!(h.fatal, extra != 0);
        assert_eq!(h.observations().len(), usize::from(extra == 0));
    }
}

#[test]
fn collector_stats_keep_rejections_across_pane_departure_and_expose_overflow() {
    let mut h = Harness::new();
    h.feed(&encoded(b"\x1b]7501;state=working:msg=AA==\x07"));
    h.feed(&encoded(b"\x1b]7501;state="));
    let stats = h.collector.stats();
    assert_eq!(stats.rejected_frames, 1);
    assert_eq!(stats.tracked_panes, 1);
    assert!(stats.topology_ready);
    assert!(stats.pending_payload_bytes > 0);
    h.collector.reconcile(&[], &BTreeMap::new());
    let stats = h.collector.stats();
    assert_eq!(stats.rejected_frames, 1);
    assert_eq!(stats.tracked_panes, 0);
    assert_eq!(stats.pending_payload_bytes, 0);
    assert!(ControlPool::default().program_stats().is_none());
}

#[test]
fn exit_does_not_deliver_any_later_payload_from_the_dead_client() {
    let mut decoder = ready_decoder();
    let mut seen = Vec::new();
    let mut bytes = b"%exit detached\n".to_vec();
    bytes.extend(encoded(REPORT));
    decoder.feed(&bytes, true, |item| seen.push(item));
    assert_eq!(seen, vec![Decoded::Note(super::decoder::Note::Exit)]);
}

#[test]
fn cold_attach_and_reenable_wait_for_safe_framing_before_accepting_reports() {
    let mut pool = ControlPool::default();
    let mut decoder = ready_decoder();
    for _ in 0..2 {
        pool.set_program_collection(true);
        let collector = pool.protocol.as_mut().unwrap();
        collector.reconcile(
            &[(SessionId::new(1), PaneId::new(1))],
            &BTreeMap::from([(SessionId::new(1), 1)]),
        );
        // The unrelated DCS/OSC began before attach or while collection was off.
        // Its visible continuation includes an embedded fake BEL report.
        let mut continuation = b"unrelated string content \x1b]7501;state=done\x07".to_vec();
        continuation.extend_from_slice(b"\x1b\\");
        decoder.feed(&encoded(&continuation), true, |item| {
            collector.decoded(SessionId::new(1), 1, item)
        });
        assert!(collector.drain().observations.is_empty());
        decoder.feed(&encoded(REPORT), true, |item| {
            collector.decoded(SessionId::new(1), 1, item)
        });
        assert_eq!(collector.drain().observations.len(), 1);
        pool.set_program_collection(false);
        // Payload emitted while off is never scanned into the next interval.
        decoder.feed(&encoded(b"\x1bPpreexisting unrelated DCS"), false, |_| {});
    }
}

#[test]
fn runtime_allowance_counts_candidates_and_queues_and_lowering_revokes_locally() {
    let mut h = Harness::new();
    h.collector.reconcile(
        &[
            (SessionId::new(1), PaneId::new(1)),
            (SessionId::new(1), PaneId::new(2)),
        ],
        &BTreeMap::from([(SessionId::new(1), 1)]),
    );
    h.collector.set_payload_budget(100);
    h.feed(&encoded(REPORT));
    assert_eq!(h.collector.pending_bytes(), 2);
    h.feed(&encoded(b"\x1b]7501;state=working:app="));
    assert!(h.collector.pending_bytes() > 2);
    assert_eq!(
        h.collector.pending_bytes(),
        2 + b"7501;state=working:app=".len()
    );
    h.collector.set_payload_budget(1);
    assert_eq!(h.collector.pending_bytes(), 0);
    let drain = h.collector.drain();
    assert!(drain.observations.is_empty());
    assert_eq!(drain.invalidations.len(), 1);
    assert_eq!(drain.invalidations[0].pane, PaneId::new(1));
    assert_eq!(
        drain.invalidations[0].event,
        ProgramEvent::Gap(ObservationGap::BudgetExceeded)
    );
    h.collector.set_payload_budget(100);
    // Finish the suppressed pre-loss string, then a fresh report can recover.
    h.feed(&encoded(b"pi\x1b\\"));
    h.feed(&encoded(REPORT));
    assert_eq!(h.observations().len(), 1);
    assert_eq!(h.collector.pending_bytes(), 0);
}

#[test]
fn runtime_allowance_is_enforced_during_arrival_and_released_on_boundary_removal() {
    let mut h = Harness::new();
    h.collector.set_payload_budget(8);
    h.feed(&encoded(REPORT));
    let drain = h.collector.drain();
    assert!(drain.observations.is_empty());
    assert_eq!(
        drain.invalidations[0].event,
        ProgramEvent::Gap(ObservationGap::BudgetExceeded)
    );
    assert_eq!(h.collector.pending_bytes(), 0);
    h.collector.set_payload_budget(100);
    h.feed(&encoded(b"\x1b]7501;state=working"));
    assert!(h.collector.pending_bytes() > 0);
    h.collector.boundary(PaneId::new(1), false);
    assert_eq!(h.collector.pending_bytes(), 0);
    h.feed(&encoded(b"\x1b\\"));
    h.feed(&encoded(b"\x1b]7501;state=working"));
    assert!(h.collector.pending_bytes() > 0);
    h.collector.reconcile(&[], &BTreeMap::new());
    assert_eq!(h.collector.pending_bytes(), 0);
}

#[test]
fn budget_loss_survives_later_gaps_and_the_binding_baseline() {
    let mut h = Harness::new();
    h.collector.set_payload_budget(8);
    h.feed(&encoded(REPORT));
    let first = h.collector.stream(PaneId::new(1)).unwrap();
    assert_eq!(first.pending_gap, Some(ObservationGap::BudgetExceeded));
    h.collector
        .client_gap(SessionId::new(1), ObservationGap::Disconnected);
    let boundary = h.collector.boundary(PaneId::new(1), false).unwrap();
    assert_ne!(first.connection, boundary.connection);
    let drain = h.collector.drain();
    assert_eq!(drain.invalidations.len(), 1);
    assert_eq!(drain.invalidations[0].connection, boundary.connection);
    assert_eq!(drain.invalidations[0].sequence, boundary.sequence_floor);
    assert_eq!(
        drain.invalidations[0].event,
        ProgramEvent::Gap(ObservationGap::BudgetExceeded)
    );
    assert_eq!(
        h.collector.stream(PaneId::new(1)).unwrap().pending_gap,
        None
    );
}

#[test]
fn current_stream_reports_loss_without_treating_queued_reports_as_a_gap() {
    let mut h = Harness::new();
    let initial = h.collector.stream(PaneId::new(1)).unwrap();
    h.feed(&encoded(REPORT));
    assert_eq!(h.collector.stream(PaneId::new(1)), Some(initial));
    h.collector
        .client_gap(SessionId::new(1), ObservationGap::Disconnected);
    let disconnected = h.collector.stream(PaneId::new(1)).unwrap();
    assert_ne!(disconnected.connection, initial.connection);
    assert_eq!(disconnected.pending_gap, Some(ObservationGap::Disconnected));
    h.collector.topology_failed();
    assert_eq!(h.collector.stream(PaneId::new(1)), None);
    h.collector.reconcile(
        &[(SessionId::new(1), PaneId::new(1))],
        &BTreeMap::from([(SessionId::new(1), 1)]),
    );
    assert!(h.collector.stream(PaneId::new(1)).is_some());
    let topology: Vec<_> = (1..=MAX_PROGRAM_EVENTS as u32 + 2)
        .map(|pane| (SessionId::new(1), PaneId::new(pane)))
        .collect();
    h.collector
        .reconcile(&topology, &BTreeMap::from([(SessionId::new(1), 1)]));
    h.collector.reconcile(&[], &BTreeMap::new());
    h.collector.reconcile(
        &[(SessionId::new(1), PaneId::new(1))],
        &BTreeMap::from([(SessionId::new(1), 1)]),
    );
    assert_eq!(h.collector.stream(PaneId::new(1)), None);
}
