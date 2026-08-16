//! `tma wait` push fan-out: register bounded subscribers and broadcast a one-byte wake, dropping any
//! slow/dead peer never-wait so the loop never stalls.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

use rustix::event::PollFlags;

use tma_runtime::ipc::{PUSH, SUB_ACK};

/// Bound on concurrent `tma wait` push subscribers, capping the retained-fd set against a buggy or
/// hostile client. A subscribe past the cap is declined cleanly (the client reads EOF and polls).
const MAX_SUBSCRIBERS: usize = 16;

/// Register a `tma wait` push subscriber, or decline cleanly (drop `stream`; the client reads EOF and
/// polls) at the cap or on a failed [`SUB_ACK`]. On success the stream is non-blocking (never-wait PUSH).
pub(super) fn register_subscriber(subscribers: &mut Vec<UnixStream>, mut stream: UnixStream) {
    if subscribers.len() >= MAX_SUBSCRIBERS {
        return; // at the cap: decline (drop), the client polls, never an error
    }
    if stream.write_all(&[SUB_ACK]).is_err() {
        return; // client vanished between connect and ack: drop
    }
    if stream.set_nonblocking(true).is_err() {
        return;
    }
    subscribers.push(stream);
}

/// Broadcast a one-byte [`PUSH`] wake to every subscriber, never-wait: a non-blocking write that
/// cannot take the byte (`WouldBlock` full buffer, or a dead peer) DROPS the subscriber rather than
/// stall the loop. Dropping loses nothing (a full buffer already holds a wake; a dropped waiter
/// polls). Rust ignores SIGPIPE, so a write to a closed peer errors here, never signals.
pub(super) fn push_subscribers(subscribers: &mut Vec<UnixStream>) {
    subscribers.retain_mut(|s| matches!(s.write(&[PUSH]), Ok(1)));
}

/// Reap subscribers whose poll fd reported hangup/error, EOF, or stray data. `base` is the first
/// subscriber fd's index in `fds`, and the caller guarantees `subscribers` still matches that slice
/// (reaping runs before any new subscribe). Returns the count removed, to flag the gauge.
pub(super) fn reap_closed_subscribers(
    subscribers: &mut Vec<UnixStream>,
    revents: &[PollFlags],
    base: usize,
) -> usize {
    let before = subscribers.len();
    let hangup = PollFlags::HUP | PollFlags::ERR;
    let mut i = 0;
    subscribers.retain_mut(|s| {
        let re = revents[base + i];
        i += 1;
        if re.intersects(hangup) {
            return false; // definite close/error: the waiter is gone
        }
        if re.contains(PollFlags::IN) {
            // Readable: a well-behaved waiter never writes, so EOF (Ok(0)) means it closed ⇒ drop.
            // ANY payload byte is a protocol violation and is DROPPED too, since keeping it would
            // leave the fd POLLIN-ready forever and busy-spin the loop. Non-blocking, so no stall.
            let mut buf = [0u8; 64];
            return match s.read(&mut buf) {
                Ok(0) => false,                                                   // EOF: gone
                Ok(_) => false, // stray data: protocol violation, drop (no busy-spin)
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => true, // spurious: keep
                Err(_) => false, // hard error: gone
            };
        }
        true
    });
    before - subscribers.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::io::AsRawFd;

    /// The reap branches. EOF, stray data (the busy-spin fix), and POLLHUP/POLLERR all drop; a
    /// spurious `POLLIN` that reads `WouldBlock` and a subscriber with no event both stay.
    #[test]
    fn reap_drops_eof_stray_hangup_keeps_wouldblock_and_quiet() {
        // 0 EOF, 1 stray-data, 2 spurious-POLLIN (WouldBlock keep), 3 POLLHUP, 4 no-event keep.
        let (s0, p0) = UnixStream::pair().unwrap();
        let (s1, mut p1) = UnixStream::pair().unwrap();
        let (s2, _p2) = UnixStream::pair().unwrap();
        let (s3, _p3) = UnixStream::pair().unwrap();
        let (s4, _p4) = UnixStream::pair().unwrap();
        for s in [&s0, &s1, &s2, &s3, &s4] {
            s.set_nonblocking(true).unwrap();
        }
        // 0: shutdown before drop — a concurrent test's fork can briefly hold a dup of p0
        // (pre-exec/pre-CLOEXEC), so close alone does not guarantee an immediate EOF.
        p0.shutdown(std::net::Shutdown::Both).unwrap();
        drop(p0);
        p1.write_all(b"x").unwrap(); // 1: a stray byte is now readable
        let keep_wouldblock = s2.as_raw_fd();
        let keep_quiet = s4.as_raw_fd();

        let revents = vec![
            PollFlags::IN,      // 0: EOF (readable)
            PollFlags::IN,      // 1: stray data (readable)
            PollFlags::IN,      // 2: spurious POLLIN (reads WouldBlock)
            PollFlags::HUP,     // 3: hangup
            PollFlags::empty(), // 4: no event
        ];
        let mut subs = vec![s0, s1, s2, s3, s4];
        let removed = reap_closed_subscribers(&mut subs, &revents, 0);
        assert_eq!(removed, 3, "EOF + stray-data + POLLHUP are reaped");
        assert_eq!(
            subs.len(),
            2,
            "the WouldBlock and no-event subscribers survive"
        );
        let survivors: Vec<i32> = subs.iter().map(|s| s.as_raw_fd()).collect();
        assert!(survivors.contains(&keep_wouldblock));
        assert!(survivors.contains(&keep_quiet));
    }
}
