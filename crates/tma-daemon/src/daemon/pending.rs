//! Parked inbound connections: a frame whose bytes have not all arrived on accept joins the poll set
//! with its own read buffer and an absolute `FRAME_DEADLINE` drop deadline, so one slow client never
//! serializes the accept path (which would starve control-fd reads, quiet edges, and notify dispatch
//! for up to 2 s per stalled connection). The overwhelmingly common case (a real client's whole frame
//! already buffered) completes inline on accept and never parks. Frame decoding lives in
//! `tma_runtime::ipc` ([`parse_inbound`]); this module owns only the buffering, the deadline, and the
//! bounded set (kill-oldest at the cap, like the subscriber/notify bounds).

use std::io::{Read, Write};
use std::os::unix::io::{AsFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use tma_runtime::ipc::{self, Inbound, ParseStatus, FRAME_DEADLINE, NAK};

/// Bound on connections parked mid-frame. A well-behaved client's frame is in the socket buffer on
/// accept and completes inline, so a nonempty pending set means slow/stalled peers; capping it bounds
/// the retained-fd and buffer cost against a buggy or hostile local client.
const MAX_PENDING: usize = 32;

/// A connection whose frame did not arrive whole on accept: the non-blocking stream, the bytes read
/// so far, and the absolute deadline (`accept + FRAME_DEADLINE`) at which it is dropped.
pub(super) struct Pending {
    stream: UnixStream,
    buf: Vec<u8>,
    deadline: Instant,
}

/// The outcome of one non-blocking read+parse against a connection.
pub(super) enum Advance {
    /// A whole frame arrived; dispatch this `Inbound` on the connection's stream (moved out by the
    /// caller via [`Pending::into_stream`]).
    Complete(Inbound),
    /// More bytes are needed and the peer is still open; keep the connection parked.
    Park,
    /// EOF mid-frame, a malformed frame, or an I/O error; the caller NAK+drops.
    Drop,
}

impl Pending {
    /// Wrap a freshly accepted `stream` (set non-blocking) and attempt the first read+parse, so the
    /// caller dispatches inline on [`Advance::Complete`] (the common case: the whole frame is already
    /// buffered), parks on [`Advance::Park`], or NAK+drops on [`Advance::Drop`]. `None` only if the
    /// stream cannot be made non-blocking (dropped).
    pub(super) fn accept(stream: UnixStream, now: Instant) -> Option<(Pending, Advance)> {
        stream.set_nonblocking(true).ok()?;
        let mut pending = Pending {
            stream,
            buf: Vec::new(),
            deadline: now + FRAME_DEADLINE,
        };
        let advance = pending.advance();
        Some((pending, advance))
    }

    /// Drain whatever is currently readable (non-blocking) into the buffer, then re-parse. Never
    /// blocks: reads until `WouldBlock`, then classifies via [`parse_inbound`].
    pub(super) fn advance(&mut self) -> Advance {
        let open = match read_available(&mut self.stream, &mut self.buf) {
            Ok(open) => open,
            Err(_) => return Advance::Drop, // hard read error
        };
        match ipc::parse_inbound(&self.buf) {
            ParseStatus::Complete(inbound) => Advance::Complete(inbound),
            ParseStatus::Invalid => Advance::Drop,
            ParseStatus::NeedMore if open => Advance::Park,
            ParseStatus::NeedMore => Advance::Drop, // EOF mid-frame
        }
    }

    /// Whether this connection's deadline has passed as of `now` (drop it if so).
    pub(super) fn is_due(&self, now: Instant) -> bool {
        self.deadline <= now
    }

    /// Consume the wrapper, yielding the stream so a completed frame can be dispatched on it.
    pub(super) fn into_stream(self) -> UnixStream {
        self.stream
    }

    /// NAK then drop: a malformed/expired connection gets the same one-byte NAK a legacy `tma event`
    /// client falls back on, best-effort (the peer may have already closed).
    pub(super) fn nak_and_drop(mut self) {
        let _ = self.stream.write_all(&[NAK]);
    }
}

/// Borrow the parked connection's fd so it can join the serve loop's poll set (`PollFd::new`).
impl AsFd for Pending {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.stream.as_fd()
    }
}

/// Park a slow connection, evicting the OLDEST parked one first when at [`MAX_PENDING`] (kill-oldest:
/// the oldest is closest to its own deadline and least likely to still complete). The evicted one is
/// NAKed so a legacy client falls back to a direct stamp.
pub(super) fn admit(pending: &mut Vec<Pending>, conn: Pending) {
    if pending.len() >= MAX_PENDING {
        pending.remove(0).nak_and_drop();
    }
    pending.push(conn);
}

/// The soonest parked deadline expressed as a duration from `now` (zero if already past), or `None`
/// when nothing is parked. The serve loop clamps its poll timeout to this so a stalled peer sending
/// no further bytes is still dropped on time.
pub(super) fn nearest_deadline(pending: &[Pending], now: Instant) -> Option<Duration> {
    pending
        .iter()
        .map(|p| p.deadline.saturating_duration_since(now))
        .min()
}

/// Drain all currently-readable bytes into `buf` without blocking. `Ok(true)` when the peer is still
/// open (a `WouldBlock` ended the drain), `Ok(false)` on EOF, `Err` on a hard read error.
fn read_available(stream: &mut UnixStream, buf: &mut Vec<u8>) -> std::io::Result<bool> {
    let mut chunk = [0u8; 4096];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => return Ok(false), // EOF
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(true),
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Construct a parked connection directly from a socketpair end for the set-management tests
    /// (bypassing `accept`'s non-blocking flip, which the pair already carries).
    fn parked(stream: UnixStream, deadline: Instant) -> Pending {
        Pending {
            stream,
            buf: Vec::new(),
            deadline,
        }
    }

    /// At the cap, admitting one more evicts the oldest (index 0) and keeps the set bounded, so a
    /// flood of stalled connections cannot grow the retained-fd set without limit.
    #[test]
    fn admit_evicts_the_oldest_at_the_cap() {
        let now = Instant::now();
        let mut pending: Vec<Pending> = Vec::new();
        let mut oldest_fd = None;
        for i in 0..MAX_PENDING {
            let (a, _b) = UnixStream::pair().unwrap();
            if i == 0 {
                use std::os::unix::io::AsRawFd;
                oldest_fd = Some(a.as_raw_fd());
            }
            pending.push(parked(a, now + Duration::from_secs(2)));
        }
        assert_eq!(pending.len(), MAX_PENDING);

        // One past the cap: the set stays at MAX_PENDING and the oldest fd is gone.
        let (extra, _e) = UnixStream::pair().unwrap();
        admit(&mut pending, parked(extra, now + Duration::from_secs(2)));
        assert_eq!(
            pending.len(),
            MAX_PENDING,
            "the set stays bounded at the cap"
        );
        use std::os::unix::io::AsRawFd;
        let survivors: Vec<i32> = pending.iter().map(|p| p.stream.as_raw_fd()).collect();
        assert!(
            !survivors.contains(&oldest_fd.unwrap()),
            "the oldest parked connection was evicted"
        );
    }

    /// `nearest_deadline` reports the soonest deadline as a from-`now` duration (zero once past), and
    /// `None` for an empty set, so the loop only shrinks its poll timeout when something is parked.
    #[test]
    fn nearest_deadline_is_the_soonest_from_now() {
        let now = Instant::now();
        assert!(nearest_deadline(&[], now).is_none(), "empty ⇒ no clamp");

        let (a, _b) = UnixStream::pair().unwrap();
        let (c, _d) = UnixStream::pair().unwrap();
        let pending = vec![
            parked(a, now + Duration::from_millis(1500)),
            parked(c, now + Duration::from_millis(500)),
        ];
        let d = nearest_deadline(&pending, now).unwrap();
        assert!(
            d <= Duration::from_millis(500) && d >= Duration::from_millis(400),
            "reports the soonest (~500ms), got {d:?}"
        );

        // A deadline already in the past saturates to zero (poll returns at once, then it is dropped).
        let (e, _f) = UnixStream::pair().unwrap();
        let past = vec![parked(e, now - Duration::from_secs(1))];
        assert_eq!(nearest_deadline(&past, now), Some(Duration::ZERO));
    }

    /// A parked connection whose deadline has passed reports `is_due`, so the loop drops it even
    /// though it never became readable.
    #[test]
    fn is_due_fires_at_and_past_the_deadline() {
        let now = Instant::now();
        let (a, _b) = UnixStream::pair().unwrap();
        let conn = parked(a, now);
        assert!(conn.is_due(now), "at the deadline ⇒ due");
        assert!(
            conn.is_due(now + Duration::from_millis(1)),
            "past the deadline ⇒ due"
        );
        let (c, _d) = UnixStream::pair().unwrap();
        let future = parked(c, now + Duration::from_secs(2));
        assert!(!future.is_due(now), "before the deadline ⇒ not due");
    }

    /// A completed frame delivered across two reads: the first read parks (partial), the second
    /// completes. Drives the parked-connection advance path end to end over a real socketpair.
    #[test]
    fn advance_parks_on_a_partial_frame_then_completes() {
        use tma_runtime::ipc::Frame;
        let (mut writer, reader) = UnixStream::pair().unwrap();
        reader.set_nonblocking(true).unwrap();
        let mut conn = parked(reader, Instant::now() + Duration::from_secs(2));

        // Encode a valid frame, then feed only its first half: the connection must park.
        let frame = ipc::encode_frame("%3", "claude", "Stop", "{}");
        let half = frame.len() / 2;
        writer.write_all(&frame[..half]).unwrap();
        assert!(
            matches!(conn.advance(), Advance::Park),
            "a partial frame with the peer still open must park"
        );

        // Feed the rest: the connection completes with the decoded frame.
        writer.write_all(&frame[half..]).unwrap();
        match conn.advance() {
            Advance::Complete(Inbound::Event(Frame { pane, kind, .. })) => {
                assert_eq!(pane, "%3");
                assert_eq!(kind, "Stop");
            }
            _ => panic!("the completing read must yield the whole frame"),
        }
    }

    /// A peer that connects, writes nothing, and closes is an EOF mid-frame: advance drops it (no
    /// frame, so the loop must not keep it parked).
    #[test]
    fn advance_drops_on_eof_before_any_frame() {
        let (writer, reader) = UnixStream::pair().unwrap();
        reader.set_nonblocking(true).unwrap();
        let mut conn = parked(reader, Instant::now() + Duration::from_secs(2));
        drop(writer); // immediate EOF
                      // The kernel may briefly report EWOULDBLOCK before the hangup is visible, so a
                      // Park here is retried rather than failed.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match conn.advance() {
                Advance::Park if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                outcome => {
                    assert!(matches!(outcome, Advance::Drop));
                    break;
                }
            }
        }
    }
}
