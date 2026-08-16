//! The `@agent_action` single-flight lock and the keys write path, exercised against a live scratch
//! `tmux -L` server (killed on drop). This ports the design-record validation from ACTIONS.md:
//! the two syntax facts as primitive probes (the pinned expiry extraction and the `e|<` /
//! empty-equality comparisons) plus the seven-case acquire/reclaim/clear protocol (A–G), then a
//! contended race and the guarded keys delivery.
//!
//! The lock protocol is entirely server-side conditional writes: every acquire/clear/rewrite is one
//! `set-option -pF`, and the winner is read back — there is no client-side read-decide-write, which
//! these tests hold by driving the real `tma_tmux::lock` API, never a hand-rolled sequence.

use tma_test_support::{self as common, Scratch};

use tma_tmux::lock::{self, Acquire, LockValue};
use tma_tmux::tmux::Tmux;

/// A fresh detached pane on the scratch server, its `@agent_action` unset.
fn new_pane(s: &Scratch) -> String {
    assert!(s
        .tmux(&["new-session", "-d", "-x", "80", "-y", "24", "sleep 100000"])
        .status
        .success());
    let pane = s.get("", "#{pane_id}");
    assert!(pane.starts_with('%'), "unexpected pane id {pane:?}");
    pane
}

/// A `Tmux` adapter pointed at the scratch server's `-L` socket (the same server the harness runs).
fn adapter(s: &Scratch) -> Tmux {
    Tmux::new(Some(s.socket.clone()))
}

/// Read a format string against a pane on the scratch server.
fn read_fmt(s: &Scratch, pane: &str, fmt: &str) -> String {
    s.get(pane, fmt)
}

// A far-future clock/expiry so "unexpired" holds the whole test; a small past value forces reclaim.
const NOW: u64 = 1_700_000_000_000;
const FUTURE_EXPIRY: u64 = 1_700_000_030_000;

// ---- primitive probes: the two pinned syntax facts (cases 1–6) ---------------------------------

/// Probe 1 & 2: the pinned expiry extraction `#{s/[^0-9].*//:#{@agent_action}}` yields the leading
/// digits of a well-formed value, and yields empty for a corrupt value (no leading digit), which the
/// acquire guard then treats as expired — the recovery path for a mangled lock.
#[test]
fn primitive_expiry_extraction_strips_from_first_nondigit() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("lock_prim_expiry");
    let pane = new_pane(&s);

    s.set_opt(&pane, "@agent_action", "1700000030000:abcd:4242:approve");
    assert_eq!(
        read_fmt(&s, &pane, lock::EXPIRY_EXTRACT),
        "1700000030000",
        "extraction yields the leading expiry digits"
    );

    s.set_opt(&pane, "@agent_action", "garbage:x:1:y");
    assert_eq!(
        read_fmt(&s, &pane, lock::EXPIRY_EXTRACT),
        "",
        "a corrupt value with no leading digit extracts to empty (treated as expired)"
    );
}

/// Probe 3: extraction of an empty/absent value is empty (so the reclaim arm's `e|<` sees an empty
/// left operand, which compares as less-than any clock).
#[test]
fn primitive_expiry_extraction_of_empty_is_empty() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("lock_prim_empty");
    let pane = new_pane(&s);
    // Never set: absent reads as empty.
    assert_eq!(read_fmt(&s, &pane, lock::EXPIRY_EXTRACT), "");
    assert_eq!(read_fmt(&s, &pane, "#{==:#{@agent_action},}"), "1");
}

/// Probe 4, 5 & 6: the acquire guard's comparison primitives — the empty-equality first arm and the
/// `e|<` expiry comparison (empty, past, and future left operands).
#[test]
fn primitive_empty_equality_and_e_less_than() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("lock_prim_cmp");
    let pane = new_pane(&s);

    // Empty-equality arm: truthy when unset/empty, falsy when set.
    assert_eq!(read_fmt(&s, &pane, "#{==:#{@agent_action},}"), "1");
    s.set_opt(&pane, "@agent_action", "1700000030000:abcd:1:x");
    assert_eq!(read_fmt(&s, &pane, "#{==:#{@agent_action},}"), "0");

    // `e|<`: the empty string as the left operand compares as less-than (covers the cleared state).
    assert_eq!(read_fmt(&s, &pane, "#{e|<:,1700000000000}"), "1");
    // A past expiry is less than now (expired ⇒ reclaimable); a future one is not.
    assert_eq!(read_fmt(&s, &pane, "#{e|<:1000,1700000000000}"), "1");
    assert_eq!(
        read_fmt(&s, &pane, "#{e|<:9999999999999,1700000000000}"),
        "0"
    );
}

// ---- protocol cases A–G --------------------------------------------------------------------------

/// Case A: fresh acquire against an absent lock succeeds; the stored value is exactly what was
/// written, and the read-back nonce matches.
#[test]
fn case_a_fresh_acquire() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("lock_a");
    let pane = new_pane(&s);
    let t = adapter(&s);

    let got = lock::acquire(&t, &pane, NOW, FUTURE_EXPIRY, 4242, "approve").unwrap();
    let Acquire::Acquired(v) = got else {
        panic!("fresh acquire must win, got {got:?}");
    };
    assert_eq!(v.expiry_ms, FUTURE_EXPIRY);
    assert_eq!(v.pid, 4242);
    assert_eq!(v.name, "approve");
    assert_eq!(v.nonce.len(), 32);
    assert_eq!(s.pane_option(&pane, "@agent_action"), v.encode());
}

/// Case B: a held, unexpired lock refuses a second acquirer — the read-back nonce differs — and the
/// stored value is untouched (still the first holder's).
#[test]
fn case_b_held_refusal() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("lock_b");
    let pane = new_pane(&s);
    let t = adapter(&s);

    let Acquire::Acquired(first) =
        lock::acquire(&t, &pane, NOW, FUTURE_EXPIRY, 111, "approve").unwrap()
    else {
        panic!("first acquire must win");
    };
    // A second acquirer at a clock still before the expiry loses.
    let second = lock::acquire(&t, &pane, NOW + 1, FUTURE_EXPIRY, 222, "approve").unwrap();
    assert_eq!(second, Acquire::Contended, "held unexpired lock refuses");
    assert_eq!(
        s.pane_option(&pane, "@agent_action"),
        first.encode(),
        "the stored lock is untouched by the losing acquirer"
    );
}

/// Case C: a wall-clock-expired lock is reclaimed — the new acquirer wins and the stored value
/// becomes the reclaimer's.
#[test]
fn case_c_expired_reclaim() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("lock_c");
    let pane = new_pane(&s);
    let t = adapter(&s);

    // A prior holder whose expiry is already in the past. The pid must be provably dead or the
    // liveness pre-check refuses the reclaim; a low pid can collide with a real process (it did on
    // a CI runner), so use one past i32::MAX, which pid_alive always reports dead.
    s.set_opt(
        &pane,
        "@agent_action",
        "1000:deadbeefdeadbeefdeadbeefdeadbeef:4000000000:approve",
    );
    let got = lock::acquire(&t, &pane, NOW, FUTURE_EXPIRY, 333, "approve").unwrap();
    let Acquire::Acquired(v) = got else {
        panic!("an expired lock must be reclaimable, got {got:?}");
    };
    assert_eq!(s.pane_option(&pane, "@agent_action"), v.encode());
    assert_eq!(v.pid, 333);
}

/// Case D: an empty value (a cleared lock) is re-acquirable via the guard's first arm.
#[test]
fn case_d_empty_reacquire() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("lock_d");
    let pane = new_pane(&s);
    let t = adapter(&s);

    s.set_opt(&pane, "@agent_action", "");
    let got = lock::acquire(&t, &pane, NOW, FUTURE_EXPIRY, 444, "approve").unwrap();
    assert!(
        matches!(got, Acquire::Acquired(_)),
        "empty ⇒ acquirable, got {got:?}"
    );
}

/// Case E: a clear carrying the wrong nonce is a no-op (the stored lock is untouched) — the ABA
/// guard, so a reclaimed holder cannot wipe the new holder's lock.
#[test]
fn case_e_stale_nonce_clear_is_noop() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("lock_e");
    let pane = new_pane(&s);
    let t = adapter(&s);

    let Acquire::Acquired(held) =
        lock::acquire(&t, &pane, NOW, FUTURE_EXPIRY, 555, "approve").unwrap()
    else {
        panic!("acquire must win");
    };
    // A different invocation's nonce must not clear this lock.
    lock::clear(&t, &pane, "ffffffffffffffffffffffffffffffff").unwrap();
    assert_eq!(
        s.pane_option(&pane, "@agent_action"),
        held.encode(),
        "a stale-nonce clear leaves the lock untouched"
    );
}

/// Case F: a clear carrying the holder's own nonce empties the lock.
#[test]
fn case_f_own_nonce_clear_empties() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("lock_f");
    let pane = new_pane(&s);
    let t = adapter(&s);

    let Acquire::Acquired(held) =
        lock::acquire(&t, &pane, NOW, FUTURE_EXPIRY, 666, "approve").unwrap()
    else {
        panic!("acquire must win");
    };
    lock::clear(&t, &pane, &held.nonce).unwrap();
    assert_eq!(
        s.pane_option(&pane, "@agent_action"),
        "",
        "own-nonce clear empties the lock"
    );
}

/// Case G: after a clear, the lock is re-acquirable (empty ⇒ absent for the guard).
#[test]
fn case_g_post_clear_reacquire() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("lock_g");
    let pane = new_pane(&s);
    let t = adapter(&s);

    let Acquire::Acquired(held) =
        lock::acquire(&t, &pane, NOW, FUTURE_EXPIRY, 777, "approve").unwrap()
    else {
        panic!("first acquire must win");
    };
    lock::clear(&t, &pane, &held.nonce).unwrap();
    let again = lock::acquire(&t, &pane, NOW + 2, FUTURE_EXPIRY, 888, "approve").unwrap();
    assert!(
        matches!(again, Acquire::Acquired(_)),
        "post-clear re-acquire wins, got {again:?}"
    );
}

// ---- contended race, rewrite handoff, and the keys write path ------------------------------------

/// Two writers race the same fresh pane: exactly one read-back matches, because the CAS lets only
/// the first `-pF` set change the value and the second's guard holds it.
#[test]
fn contended_acquire_has_one_winner() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("lock_race");
    let pane = new_pane(&s);
    let t = adapter(&s);

    let outcomes = std::thread::scope(|scope| {
        let a = scope.spawn(|| lock::acquire(&t, &pane, NOW, FUTURE_EXPIRY, 1, "approve").unwrap());
        let b = scope.spawn(|| lock::acquire(&t, &pane, NOW, FUTURE_EXPIRY, 2, "approve").unwrap());
        (a.join().unwrap(), b.join().unwrap())
    });

    let winners = [&outcomes.0, &outcomes.1]
        .iter()
        .filter(|o| matches!(o, Acquire::Acquired(_)))
        .count();
    assert_eq!(winners, 1, "exactly one writer wins the race: {outcomes:?}");

    // The stored lock is the winner's; the winner's read-back matched, so its nonce is stored.
    let stored = s.pane_option(&pane, "@agent_action");
    let winner_nonce = match (&outcomes.0, &outcomes.1) {
        (Acquire::Acquired(v), _) | (_, Acquire::Acquired(v)) => v.nonce.clone(),
        _ => unreachable!("one winner asserted above"),
    };
    let held = LockValue::parse(&stored).expect("stored value parses");
    assert_eq!(held.nonce, winner_nonce, "the stored lock is the winner's");
}

/// The nonce-conditional rewrite (the detached supervisor's handoff): a rewrite carrying the held nonce
/// replaces the value in place (same nonce, new pid); a rewrite with a foreign nonce does not.
#[test]
fn rewrite_is_nonce_conditional() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("lock_rewrite");
    let pane = new_pane(&s);
    let t = adapter(&s);

    let Acquire::Acquired(held) =
        lock::acquire(&t, &pane, NOW, FUTURE_EXPIRY, 1000, "approve").unwrap()
    else {
        panic!("acquire must win");
    };
    // The supervisor takes custody: same nonce, its own pid.
    let handed = LockValue {
        pid: 2000,
        ..held.clone()
    };
    assert!(
        lock::rewrite(&t, &pane, &held.nonce, &handed).unwrap(),
        "own-nonce rewrite lands"
    );
    assert_eq!(s.pane_option(&pane, "@agent_action"), handed.encode());
    // The nonce is preserved, so a later own-nonce clear still releases it.
    assert_eq!(
        s.pane_option(&pane, "@agent_action"),
        handed.encode(),
        "the rewrite kept the nonce and only changed the pid"
    );

    // A foreign-nonce rewrite is refused; the value is untouched.
    let foreign = LockValue {
        expiry_ms: FUTURE_EXPIRY,
        nonce: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
        pid: 3000,
        name: "approve".to_string(),
    };
    assert!(
        !lock::rewrite(&t, &pane, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", &foreign).unwrap(),
        "a foreign-nonce rewrite does not land"
    );
    assert_eq!(
        s.pane_option(&pane, "@agent_action"),
        handed.encode(),
        "the held lock survives a foreign-nonce rewrite"
    );
}

/// The keys write path delivers a named-key sequence as one `send-keys` invocation: the characters
/// land on the pane's shell prompt (proving the whole sequence went through in order).
#[test]
fn send_keys_delivers_the_sequence() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("lock_keys");
    // A real shell pane (no `sleep`) so typed keys echo to the screen.
    assert!(s
        .tmux(&["new-session", "-d", "-x", "80", "-y", "24"])
        .status
        .success());
    let pane = s.get("", "#{pane_id}");
    assert!(pane.starts_with('%'));
    let t = adapter(&s);

    // Each element is one send-keys argument; individual chars prove the multi-key sequence.
    let keys: Vec<String> = "tmakeys".chars().map(|c| c.to_string()).collect();
    t.send_keys(&pane, &keys).unwrap();

    assert!(
        common::wait_capture_contains(&s.socket, &pane, "tmakeys", common::POLL_CEILING),
        "the key sequence lands on the pane"
    );
}
