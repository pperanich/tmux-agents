use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use base64::{alphabet, Engine as _};

use super::{
    AppIdRef, BlockedKind, ProgramCommandRef, ProgramStatus, Progress, RecordIdRef, RecordRef,
    SingleLineRef, MAX_APP_BYTES, MAX_FRAME_BYTES, MAX_ID_BYTES, MAX_ID_LEVELS,
    MAX_ID_SEGMENT_BYTES, MAX_KEY_BYTES, MAX_MSG_BYTES, MAX_MSG_ENCODED_BYTES, MAX_TITLE_BYTES,
    MAX_TITLE_ENCODED_BYTES,
};

const BASE64_PADDED: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::RequireCanonical),
);
const BASE64_UNPADDED: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::RequireNone),
);

/// Fixed-size decode storage reused across reports, including overwritten duplicate text.
/// The extra message byte lets the decoder distinguish decoded-size overflow from bad base64.
pub struct DecodeScratch {
    msg: [u8; MAX_MSG_BYTES + 1],
    title: [u8; MAX_TITLE_BYTES],
}

impl Default for DecodeScratch {
    fn default() -> Self {
        Self {
            msg: [0; MAX_MSG_BYTES + 1],
            title: [0; MAX_TITLE_BYTES],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextField {
    Message,
    Title,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RejectReason {
    FrameTooLong,
    InvalidFrame,
    KeyTooLong,
    EncodedTextTooLong(TextField),
    DecodedTextTooLong(TextField),
    InvalidBase64(TextField),
    InvalidUtf8(TextField),
    ControlText(TextField),
    AppTooLong,
    IdTooLong,
    IdSegmentTooLong,
    IdTooDeep,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IgnoreReason {
    MissingState,
    UnknownState,
    InvalidId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseOutcome<'a> {
    Command(ProgramCommandRef<'a>),
    Ignored(IgnoreReason),
}

fn trim(bytes: &[u8]) -> &[u8] {
    bytes.trim_ascii()
}

fn value_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b',' | b'+' | b'/' | b'=' | b'-')
}

fn decode_text(
    value: &[u8],
    output: &mut [u8],
    field: TextField,
    max: usize,
) -> Result<usize, RejectReason> {
    let engine = if value.last() == Some(&b'=') {
        &BASE64_PADDED
    } else {
        &BASE64_UNPADDED
    };
    let len = engine
        .decode_slice(value, output)
        .map_err(|_| RejectReason::InvalidBase64(field))?;
    if len > max {
        return Err(RejectReason::DecodedTextTooLong(field));
    }
    let text = std::str::from_utf8(&output[..len]).map_err(|_| RejectReason::InvalidUtf8(field))?;
    if text.chars().any(char::is_control) {
        return Err(RejectReason::ControlText(field));
    }
    Ok(len)
}

/// Parse exactly one complete OSC frame, including its introducer and BEL or ST terminator.
///
/// No retained allocation or store mutation occurs here. Every pair is checked before a command
/// is returned, even when the final state ignores that field or a duplicate overwrites it.
/// Ordinary malformed pairs are skipped. An addressed but invalid ID ignores the report, so it
/// can never become a root mutation. Rejection is local to this complete frame, not a stream gap.
pub fn parse_frame<'a>(
    frame: &'a [u8],
    scratch: &'a mut DecodeScratch,
) -> Result<ParseOutcome<'a>, RejectReason> {
    if frame.len() > MAX_FRAME_BYTES {
        return Err(RejectReason::FrameTooLong);
    }
    let body = frame
        .strip_prefix(b"\x1b]7501;")
        .ok_or(RejectReason::InvalidFrame)?;
    let body = body
        .strip_suffix(b"\x1b\\")
        .or_else(|| body.strip_suffix(b"\x07"))
        .ok_or(RejectReason::InvalidFrame)?;
    if body.iter().any(|byte| matches!(byte, 0x1b | 0x07)) {
        return Err(RejectReason::InvalidFrame);
    }
    if body == b"?" {
        return Ok(ParseOutcome::Command(ProgramCommandRef::Query));
    }

    let mut state = None;
    let mut id = RecordIdRef::ROOT;
    let mut invalid_id = false;
    let mut kind = None;
    let mut progress = None;
    let mut app = None;
    let mut msg_len = None;
    let mut title_len = None;

    for pair in body.split(|byte| *byte == b':') {
        let Some(equal) = pair.iter().position(|byte| *byte == b'=') else {
            continue;
        };
        let key = trim(&pair[..equal]);
        let value = trim(&pair[equal + 1..]);
        if key.len() > MAX_KEY_BYTES {
            return Err(RejectReason::KeyTooLong);
        }
        // Limits precede malformed-pair skipping, including overwritten duplicates.
        match key {
            b"msg" if value.len() > MAX_MSG_ENCODED_BYTES => {
                return Err(RejectReason::EncodedTextTooLong(TextField::Message))
            }
            b"title" if value.len() > MAX_TITLE_ENCODED_BYTES => {
                return Err(RejectReason::EncodedTextTooLong(TextField::Title))
            }
            b"app" if value.len() > MAX_APP_BYTES => return Err(RejectReason::AppTooLong),
            b"id" => {
                if value.len() > MAX_ID_BYTES {
                    return Err(RejectReason::IdTooLong);
                }
                let mut levels = 0;
                for segment in value.split(|byte| *byte == b'/') {
                    levels += 1;
                    if segment.len() > MAX_ID_SEGMENT_BYTES {
                        return Err(RejectReason::IdSegmentTooLong);
                    }
                }
                if levels > MAX_ID_LEVELS {
                    return Err(RejectReason::IdTooDeep);
                }
                match std::str::from_utf8(value)
                    .ok()
                    .and_then(|value| RecordIdRef::parse(value).ok())
                {
                    Some(parsed) => {
                        id = parsed;
                        invalid_id = false;
                    }
                    None => invalid_id = true,
                }
            }
            _ => {}
        }
        if key.is_empty()
            || !key.iter().all(u8::is_ascii_lowercase)
            || !value.iter().copied().all(value_byte)
        {
            continue;
        }
        match key {
            b"state" => state = Some(value),
            b"kind" => {
                kind = match value {
                    b"permission" => Some(BlockedKind::Permission),
                    b"question" => Some(BlockedKind::Question),
                    b"auth" => Some(BlockedKind::Auth),
                    _ => None,
                }
            }
            b"progress" => {
                progress = if !value.is_empty() && value.iter().all(u8::is_ascii_digit) {
                    // Bound numeric work by the frame cap; leading zeroes are legal integers.
                    value
                        .iter()
                        .try_fold(0_u8, |n, digit| {
                            n.checked_mul(10)?.checked_add(digit - b'0')
                        })
                        .and_then(|n| Progress::new(n).ok())
                } else {
                    None
                };
            }
            b"app" => {
                app = std::str::from_utf8(value)
                    .ok()
                    .and_then(|value| AppIdRef::parse(value).ok())
            }
            b"msg" => {
                msg_len = Some(decode_text(
                    value,
                    &mut scratch.msg,
                    TextField::Message,
                    MAX_MSG_BYTES,
                )?)
            }
            b"title" => {
                title_len = Some(decode_text(
                    value,
                    &mut scratch.title,
                    TextField::Title,
                    MAX_TITLE_BYTES,
                )?)
            }
            _ => {}
        }
    }

    if invalid_id {
        return Ok(ParseOutcome::Ignored(IgnoreReason::InvalidId));
    }
    let status = match state {
        None => return Ok(ParseOutcome::Ignored(IgnoreReason::MissingState)),
        Some(b"clear") => return Ok(ParseOutcome::Command(ProgramCommandRef::Clear(id))),
        Some(b"idle") => ProgramStatus::Idle,
        Some(b"working") => ProgramStatus::Working { progress },
        Some(b"blocked") => ProgramStatus::Blocked { kind, progress },
        Some(b"done") => ProgramStatus::Done,
        Some(b"error") => ProgramStatus::Error,
        Some(_) => return Ok(ParseOutcome::Ignored(IgnoreReason::UnknownState)),
    };
    // Validated above; keep the safe conversion here rather than unchecked UTF-8.
    let msg = msg_len.map(|len| {
        SingleLineRef(std::str::from_utf8(&scratch.msg[..len]).expect("validated UTF-8"))
    });
    let title = title_len.map(|len| {
        SingleLineRef(std::str::from_utf8(&scratch.title[..len]).expect("validated UTF-8"))
    });
    Ok(ParseOutcome::Command(ProgramCommandRef::Replace(
        RecordRef {
            id,
            status,
            app,
            title,
            msg,
        },
    )))
}
