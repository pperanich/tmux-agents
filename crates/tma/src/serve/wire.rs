//! The two halves of the pipe: one framed line out, one bounded line in.
//!
//! Nothing but frames reaches stdout. Logs go to stderr, because stdout is the protocol and a
//! stray `println!` there is a parse error on a phone.

use std::io::{self, BufRead, Write};
use std::sync::{Arc, Mutex};

use tma_proto::ResponseFrame;

/// The longest request line this host will read. A `dispatch` carrying the 4 KiB text cap plus its
/// binder and answers is an order of magnitude under it, so the cap bounds a client that never
/// sends a newline rather than trimming a legitimate frame.
pub(crate) const MAX_LINE_BYTES: usize = 64 * 1024;

/// What one read of the request stream produced.
pub(crate) enum Incoming {
    /// One line, without its newline.
    Line(String),
    /// The line ran past [`MAX_LINE_BYTES`]. It has been drained to its newline, so the next read
    /// starts on a frame boundary rather than treating the tail as a second request.
    TooLong,
    /// The bytes were not UTF-8. Drained the same way.
    NotUtf8,
    /// The writer closed its end.
    Eof,
}

/// Read one newline-terminated line, discarding anything past `cap` bytes and resynchronizing on
/// the newline. `BufRead::read_line` would buffer the whole overlong line first, which is the
/// memory the cap exists to bound.
pub(crate) fn next_line<R: BufRead>(reader: &mut R, cap: usize) -> io::Result<Incoming> {
    let mut buf: Vec<u8> = Vec::new();
    let mut over = false;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            // EOF mid-line. A trailing fragment with no newline is not a frame: handing the parser
            // half an object every time a connection dropped mid-write is worse than dropping it.
            return Ok(Incoming::Eof);
        }
        match available.iter().position(|b| *b == b'\n') {
            Some(at) => {
                if !over && buf.len() + at > cap {
                    over = true;
                }
                if !over {
                    buf.extend_from_slice(&available[..at]);
                }
                reader.consume(at + 1);
                if over {
                    return Ok(Incoming::TooLong);
                }
                return Ok(match String::from_utf8(buf) {
                    Ok(line) => Incoming::Line(line),
                    Err(_) => Incoming::NotUtf8,
                });
            }
            None => {
                let taken = available.len();
                if !over && buf.len() + taken > cap {
                    over = true;
                    // Nothing before the cap is usable either: a frame is the whole line.
                    buf = Vec::new();
                }
                if !over {
                    buf.extend_from_slice(available);
                }
                reader.consume(taken);
            }
        }
    }
}

/// The framed stdout, shared by the request loop and the subscription stream. One lock per line
/// plus a flush, so a streamed edge can never land inside a response.
#[derive(Clone)]
pub(crate) struct Wire {
    out: Arc<Mutex<io::Stdout>>,
}

impl Wire {
    pub(crate) fn new() -> Wire {
        Wire {
            out: Arc::new(Mutex::new(io::stdout())),
        }
    }

    /// Encode and write one frame. An encode failure is reported on stderr and swallowed: every
    /// frame this crate builds is plain data, and dropping the connection over one would turn a
    /// host bug into an unexplained hang up.
    pub(crate) fn send(&self, frame: &ResponseFrame) -> io::Result<()> {
        match tma_proto::encode(frame) {
            Ok(line) => self.send_line(&line),
            Err(err) => {
                eprintln!("tma serve: cannot encode a response frame: {err}");
                Ok(())
            }
        }
    }

    /// Write one already-encoded frame line.
    pub(crate) fn send_line(&self, line: &str) -> io::Result<()> {
        let mut out = match self.out.lock() {
            Ok(out) => out,
            // A panic in another frame writer poisoned the lock. The bytes it was writing are the
            // only casualty, so keep serving with the stream it left rather than dying too.
            Err(poisoned) => poisoned.into_inner(),
        };
        out.write_all(line.as_bytes())?;
        out.write_all(b"\n")?;
        out.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_all(input: &[u8], cap: usize) -> Vec<String> {
        let mut reader = io::BufReader::with_capacity(8, input);
        let mut out = Vec::new();
        loop {
            match next_line(&mut reader, cap).unwrap() {
                Incoming::Line(line) => out.push(line),
                Incoming::TooLong => out.push("<too-long>".to_string()),
                Incoming::NotUtf8 => out.push("<not-utf8>".to_string()),
                Incoming::Eof => return out,
            }
        }
    }

    #[test]
    fn lines_split_on_newlines_and_eof_ends_the_stream() {
        assert_eq!(read_all(b"a\nbb\n", 16), vec!["a", "bb"]);
        assert_eq!(read_all(b"", 16), Vec::<String>::new());
        // A blank line is a line: it earns a parse refusal rather than being silently skipped.
        assert_eq!(read_all(b"\n", 16), vec![""]);
    }

    /// The resynchronization is the point: an overlong line must consume its own newline, or its
    /// tail is read as a second request and a client that sent one frame gets two answers.
    #[test]
    fn an_overlong_line_is_dropped_whole_and_the_next_one_still_parses() {
        let input = format!("{}\nshort\n", "x".repeat(64));
        assert_eq!(read_all(input.as_bytes(), 16), vec!["<too-long>", "short"]);
    }

    #[test]
    fn invalid_utf8_is_refused_rather_than_lossily_decoded() {
        assert_eq!(read_all(b"\xff\xfe\nok\n", 16), vec!["<not-utf8>", "ok"]);
    }

    /// A fragment with no trailing newline is not a frame. Reading it as one would hand the parser
    /// half a JSON object every time a connection dropped mid-write.
    #[test]
    fn a_trailing_fragment_is_not_a_frame() {
        assert_eq!(read_all(b"a\n{\"id\"", 16), vec!["a"]);
    }
}
