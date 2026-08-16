//! The `@agent_action` single-flight pane lock. One pane option holds
//! `<expiry>:<nonce>:<pid>:<name>`, and the whole protocol is server-side conditional writes so
//! there is no client-side read-decide-write anywhere: a guarded state split across two options
//! could not be acquired atomically, because tmux expands each command's formats at that command's
//! own execution time.
//!
//! - **Acquire** is one `set-option -pF` write ([`ACQUIRE_GUARD`]): set the new value when the
//!   stored one is empty/absent or its leading expiry field is numerically past `NOW`. Because a
//!   `-pF` set always "succeeds", the winner is decided by a mandatory nonce read-back.
//! - **Clear** sets the value to empty *nonce-conditionally* (tmux has no conditional unset); empty
//!   and absent read identically, and the acquire guard's first arm covers both. An unconditional
//!   clear would be an ABA hole against a reclaimed lock.
//! - **Rewrite** replaces the value nonce-conditionally (same nonce, new pid): the detached
//!   supervisor's lock-custody handoff.
//!
//! The nonce-conditional predicate is a tmux fnmatch (`#{m:*:<nonce>:*,…}`), not an `s///`
//! field-extraction: a tmux `s` pattern cannot contain a colon (the first colon after the modifier
//! is consumed as the modifier/argument separator, silently mangling the result), so pulling the
//! nonce field out with substitution is not expressible. fnmatch tests the nonce is present as a
//! complete colon-delimited field, which survives the supervisor's pid rewrite (the nonce is kept).
//!
//! Read-back correctness rides entirely on nonce uniqueness, so the 128-bit entropy from the
//! kernel CSPRNG is load-bearing, not incidental.

use std::io::Read;

use tma_core::render::StampCommand;
use tma_core::stamp::opt;

use crate::tmux::{Tmux, TmuxError};

/// The expiry-field extraction, pinned verbatim: "strip from the first non-digit".
/// Never a pattern beginning with `:` (tmux consumes a leading `:` as the modifier/format
/// separator, silently disabling the reclaim arm), and the `s/` target is the nested
/// `#{@agent_action}` form (a bare name in target position expands to empty).
pub const EXPIRY_EXTRACT: &str = "#{s/[^0-9].*//:#{@agent_action}}";

/// The single-flight acquire guard, validated verbatim against a live tmux 3.6a server.
/// `NOW` is the writer-supplied epoch-ms clock and `NEW` the `<expiry>:<nonce>:<pid>:<name>`
/// value; both are interpolated by [`acquire`]. It sets `NEW` when `@agent_action` is empty/absent
/// (`#{==:…,}`, the empty string compares as less-than) or its leading expiry is numerically past
/// `NOW` (`e|<`); otherwise it holds the stored value. A corrupt value with no leading digits
/// extracts to empty and is therefore treated as expired, recovering a mangled lock. Field values
/// never contain commas, so the `?`-argument separators are safe by construction.
pub const ACQUIRE_GUARD: &str = "#{?#{||:#{==:#{@agent_action},},#{e|<:#{s/[^0-9].*//:#{@agent_action}},NOW}},NEW,#{@agent_action}}";

/// The nonce-presence predicate for the nonce-conditional clear/rewrite, as an fnmatch template
/// (`NONCE` interpolated): truthy iff the stored value carries this invocation's nonce as a
/// complete colon-delimited field (`*:<nonce>:*`). fnmatch rather than `s///` extraction because a
/// tmux `s` pattern cannot contain a colon (see the module docs). A 128-bit nonce makes a spurious
/// match negligible, and the pattern survives the supervisor's pid rewrite (the nonce is kept).
pub const NONCE_MATCH: &str = "#{m:*:NONCE:*,#{@agent_action}}";

/// A parsed lock value: `<expiry_ms>:<nonce>:<pid>:<name>`. The `nonce` is the release key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LockValue {
    /// Absolute expiry in epoch ms: the invocation's deadline plus slack. Reclaim is a numeric
    /// comparison against this, with no lookup of the held action's (maybe hot-reloaded) manifest.
    pub expiry_ms: u64,
    /// 128-bit nonce as 32 lowercase hex chars; read-back correctness rides on its uniqueness.
    pub nonce: String,
    /// Holder pid, for the reclaim liveness pre-check and `tma debug` eyes.
    pub pid: u32,
    /// Action name, a safe machine token (no comma or format metacharacter).
    pub name: String,
}

impl LockValue {
    /// Encode to the on-option string `<expiry_ms>:<nonce>:<pid>:<name>`.
    pub fn encode(&self) -> String {
        format!(
            "{}:{}:{}:{}",
            self.expiry_ms, self.nonce, self.pid, self.name
        )
    }

    /// Parse a stored value; `None` for anything that is not the four-field grammar (including an
    /// empty value or an empty nonce, both of which read as "no live lock").
    pub fn parse(raw: &str) -> Option<LockValue> {
        // splitn(4): the name is the free-form remainder, but a safe-token name never carries a
        // colon anyway, so the tail is a single field in practice.
        let mut it = raw.splitn(4, ':');
        let expiry_ms = it.next()?.parse().ok()?;
        let nonce = it.next()?.to_string();
        let pid = it.next()?.parse().ok()?;
        let name = it.next()?.to_string();
        if nonce.is_empty() {
            return None;
        }
        Some(LockValue {
            expiry_ms,
            nonce,
            pid,
            name,
        })
    }
}

/// The outcome of an [`acquire`] attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Acquire {
    /// Won the lock; carries the value written (whose `nonce` releases it).
    Acquired(LockValue),
    /// Another invocation won the CAS race (the read-back nonce differs). The broker exits 5.
    Contended,
}

/// Lock protocol errors: a tmux failure, or the kernel CSPRNG read that seeds the nonce.
#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error(transparent)]
    Tmux(#[from] TmuxError),
    #[error("cannot read a 128-bit lock nonce from /dev/urandom: {0}")]
    Rng(#[source] std::io::Error),
}

/// A fresh 128-bit nonce as 32 lowercase hex chars, from the kernel CSPRNG (`/dev/urandom`, the
/// portable-unix source; rustix's `getrandom` is Linux-only). Uniqueness is load-bearing, so a read
/// failure is surfaced rather than papered over with a weak fallback.
fn fresh_nonce() -> Result<String, std::io::Error> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    let mut hex = String::with_capacity(32);
    for b in bytes {
        // `from_digit(_, 16)` yields lowercase; the two nibbles are always in range.
        hex.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        hex.push(char::from_digit((b & 0x0f) as u32, 16).unwrap());
    }
    Ok(hex)
}

/// Build the `set-option -p -F` command that writes `value` (a guarded `-F` format) to
/// `@agent_action` on `pane_id`.
fn set_action_guarded(pane_id: &str, value: &str) -> StampCommand {
    StampCommand {
        argv: vec![
            "set-option".into(),
            "-p".into(),
            "-F".into(),
            "-t".into(),
            pane_id.into(),
            opt::ACTION.into(),
            value.into(),
        ],
    }
}

/// The acquire `-F` value: [`ACQUIRE_GUARD`] with `NOW`/`NEW` interpolated. Split out so the
/// interpolation is unit-testable without a server. `NOW` is replaced first so the digit string it
/// leaves can never contain the `NEW` sentinel; `new_value` is lowercase and comma-free.
fn acquire_value(now_ms: u64, new_value: &str) -> String {
    ACQUIRE_GUARD
        .replace("NOW", &now_ms.to_string())
        .replace("NEW", new_value)
}

/// The nonce-conditional `-F` value: set `then` when the stored value still carries `nonce`, else
/// hold the stored value. `then` is empty for a clear, the new encoded value for a rewrite. `NONCE`
/// is interpolated before `THEN` so a rewrite's encoded value (which itself contains the nonce) is
/// inserted verbatim without a second pass.
fn nonce_conditional_value(nonce: &str, then: &str) -> String {
    // `#{?#{m:*:<nonce>:*,#{@agent_action}},<then>,#{@agent_action}}`, built by replacement to
    // avoid brace-escaping the nested tmux format.
    "#{?PREDICATE,THEN,#{@agent_action}}"
        .replace("PREDICATE", &NONCE_MATCH.replace("NONCE", nonce))
        .replace("THEN", then)
}

/// Whether `pid` names a live process (`kill(pid, 0)`): the reclaim liveness pre-check.
/// `ESRCH` (no such process) is the only "dead" verdict; success and `EPERM` (exists, not ours) both
/// mean alive, and pid `0` or an out-of-range raw pid is treated as dead (no holder to protect).
pub fn pid_alive(pid: u32) -> bool {
    let Ok(raw) = i32::try_from(pid) else {
        return false;
    };
    match rustix::process::Pid::from_raw(raw) {
        Some(p) => !matches!(
            rustix::process::test_kill_process(p),
            Err(rustix::io::Errno::SRCH)
        ),
        None => false,
    }
}

/// Acquire the single-flight lock on `pane_id`. `now_ms` is the broker's clock and `expiry_ms` the
/// absolute deadline-plus-slack to stamp; the nonce is generated here. One `-pF` conditional write
/// then a mandatory nonce read-back decides the winner (a `-pF` set always "succeeds"). No
/// client-side read-decide-write: the guard reads only pre-write state.
pub fn acquire(
    tmux: &Tmux,
    pane_id: &str,
    now_ms: u64,
    expiry_ms: u64,
    pid: u32,
    name: &str,
) -> Result<Acquire, LockError> {
    // Reclaim liveness pre-check: a wall-clock-expired lock whose holder is still alive
    // is NOT reclaimed. Wall clocks and process timers diverge across suspend, so expiry alone would
    // reclaim from a supervisor whose child is still running. Advisory — the CAS below decides every
    // other case — so a read failure here is non-fatal and falls through to the guard.
    if let Ok(Some(stored)) = tmux.get_pane_option(pane_id, opt::ACTION) {
        if let Some(held) = LockValue::parse(&stored) {
            if held.expiry_ms < now_ms && pid_alive(held.pid) {
                return Ok(Acquire::Contended);
            }
        }
    }

    let nonce = fresh_nonce().map_err(LockError::Rng)?;
    let value = LockValue {
        expiry_ms,
        nonce,
        pid,
        name: name.to_string(),
    };
    let cmd = set_action_guarded(pane_id, &acquire_value(now_ms, &value.encode()));
    tmux.apply(std::slice::from_ref(&cmd))?;

    // Mandatory read-back: compare the stored nonce, not the whole value, so a later same-nonce
    // rewrite never invalidates a holder's own view.
    let stored = tmux.get_pane_option(pane_id, opt::ACTION)?;
    match stored.as_deref().and_then(LockValue::parse) {
        Some(held) if held.nonce == value.nonce => Ok(Acquire::Acquired(value)),
        _ => Ok(Acquire::Contended),
    }
}

/// Release the lock nonce-conditionally: set `@agent_action` to empty iff it still carries `nonce`,
/// else leave it untouched. Fire-and-forget on every synchronous exit path; empty and absent read
/// identically, so a cleared lock re-acquires via the guard's first arm. An unconditional clear
/// would be an ABA hole (a slow holder wiping the lock a reclaimer already took).
pub fn clear(tmux: &Tmux, pane_id: &str, nonce: &str) -> Result<(), LockError> {
    let cmd = set_action_guarded(pane_id, &nonce_conditional_value(nonce, ""));
    tmux.apply(std::slice::from_ref(&cmd))?;
    Ok(())
}

/// Replace the lock value nonce-conditionally: write `new_value` iff the stored nonce still equals
/// `nonce`, else hold. `new_value` MUST keep the same `nonce` so subsequent clears still key on it;
/// this is the detached supervisor's pid-custody handoff. Returns whether the rewrite landed
/// (the lock was still held), decided by reading the value back.
pub fn rewrite(
    tmux: &Tmux,
    pane_id: &str,
    nonce: &str,
    new_value: &LockValue,
) -> Result<bool, LockError> {
    debug_assert_eq!(
        new_value.nonce, nonce,
        "rewrite must preserve the lock nonce"
    );
    let encoded = new_value.encode();
    let cmd = set_action_guarded(pane_id, &nonce_conditional_value(nonce, &encoded));
    tmux.apply(std::slice::from_ref(&cmd))?;
    let stored = tmux.get_pane_option(pane_id, opt::ACTION)?;
    Ok(stored.as_deref() == Some(encoded.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_guard_matches_the_pinned_expression() {
        // The normative expression pinned in ACTIONS.md. A drift here is a review
        // finding: the reclaim arm silently breaks if the extraction is mangled.
        assert_eq!(
            ACQUIRE_GUARD,
            "#{?#{||:#{==:#{@agent_action},},#{e|<:#{s/[^0-9].*//:#{@agent_action}},NOW}},NEW,#{@agent_action}}"
        );
    }

    #[test]
    fn acquire_guard_embeds_the_pinned_expiry_extract() {
        assert!(
            ACQUIRE_GUARD.contains(EXPIRY_EXTRACT),
            "the acquire guard must reclaim via the pinned `s/[^0-9].*//` extraction"
        );
        // The pinned extraction must never begin its pattern with a colon: tmux would consume it as
        // the modifier/format separator and silently disable the reclaim arm.
        assert!(!EXPIRY_EXTRACT.contains("s/:"));
    }

    #[test]
    fn lock_value_round_trips() {
        let v = LockValue {
            expiry_ms: 1_700_000_030_000,
            nonce: "0123456789abcdef0123456789abcdef".to_string(),
            pid: 4242,
            name: "approve".to_string(),
        };
        assert_eq!(
            v.encode(),
            "1700000030000:0123456789abcdef0123456789abcdef:4242:approve"
        );
        assert_eq!(LockValue::parse(&v.encode()), Some(v));
    }

    #[test]
    fn lock_value_parse_rejects_empty_and_malformed() {
        assert_eq!(LockValue::parse(""), None);
        assert_eq!(LockValue::parse("1700"), None); // too few fields
        assert_eq!(LockValue::parse("1700::4242:approve"), None); // empty nonce
        assert_eq!(LockValue::parse("notnum:n:4242:approve"), None); // bad expiry
        assert_eq!(LockValue::parse("1700:n:notpid:approve"), None); // bad pid
    }

    #[test]
    fn nonce_is_thirty_two_lowercase_hex() {
        let n = fresh_nonce().expect("/dev/urandom readable on unix");
        assert_eq!(n.len(), 32);
        assert!(n
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
        // Two draws are (overwhelmingly) distinct: the uniqueness the read-back rides on.
        assert_ne!(n, fresh_nonce().unwrap());
    }

    #[test]
    fn acquire_value_interpolates_now_and_new() {
        let got = acquire_value(1234, "1700000030000:ab:9:x");
        assert_eq!(
            got,
            "#{?#{||:#{==:#{@agent_action},},#{e|<:#{s/[^0-9].*//:#{@agent_action}},1234}},1700000030000:ab:9:x,#{@agent_action}}"
        );
    }

    #[test]
    fn pid_alive_tracks_a_reaped_child() {
        // Our own pid is alive; pid 0 (the sentinel) is never a real holder.
        assert!(pid_alive(std::process::id()));
        assert!(!pid_alive(0));
        // A spawned-then-reaped child is dead, so `kill(pid, 0)` reports ESRCH (reclaimable).
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn `true`");
        let dead_pid = child.id();
        child.wait().expect("reap `true`");
        assert!(!pid_alive(dead_pid), "a reaped child is dead");
    }

    #[test]
    fn nonce_conditional_clear_and_rewrite_values() {
        let clear = nonce_conditional_value("abcd", "");
        assert_eq!(
            clear,
            "#{?#{m:*:abcd:*,#{@agent_action}},,#{@agent_action}}"
        );
        // A rewrite's encoded value (which itself carries the nonce) is inserted verbatim, not
        // re-substituted.
        let rewrite = nonce_conditional_value("abcd", "1700:abcd:9:x");
        assert_eq!(
            rewrite,
            "#{?#{m:*:abcd:*,#{@agent_action}},1700:abcd:9:x,#{@agent_action}}"
        );
    }
}
