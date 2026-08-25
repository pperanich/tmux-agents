//! The action broker against a live scratch `tmux -L` server (killed on drop): a full `keys` action
//! fired end to end, and the single-flight guarantee (a second act on a held lock exits 5).
//!
//! The broker is driven as a library (`tma_runtime::broker::fire`), not through the CLI verb. A pane
//! is stamped as a fresh `blocked/permission` claude agent, so the freshness re-verify is skipped
//! (no real claude process is needed) and the guarded keys path is exercised directly.

use std::process::Command;

use tma_test_support::{self as common, Scratch};

use tma_core::{ActionManifest, FoldConfig};
use tma_runtime::broker::{self, Outcome};
use tma_runtime::config::ApiSection;
use tma_tmux::lock::{self, Acquire, LockValue};
use tma_tmux::tmux::Tmux;

/// A `Tmux` adapter pointed at the scratch server's `-L` socket.
fn adapter(s: &Scratch) -> Tmux {
    Tmux::new(Some(s.socket.clone()))
}

/// Stamp `pane` as a fresh `blocked/permission` claude agent so the gate passes and the keys action
/// skips re-verify (fresh `@agent_stamped_at`).
fn stamp_blocked_claude(s: &Scratch, pane: &str) {
    let now = tma_runtime::now_ms().to_string();
    s.set_opt(pane, "@agent_name", "claude");
    s.set_opt(pane, "@agent_state", "blocked");
    s.set_opt(pane, "@agent_detail", "permission");
    s.set_opt(pane, "@agent_stamped_at", &now);
    s.set_opt(pane, "@agent_source", "capture");
    s.set_opt(pane, "@agent_pid", "4242");
}

/// A pid that is provably not running: a child spawned, waited for, and reaped. Asserted dead, so
/// the lock's reclaim liveness pre-check cannot silently turn a planted stale lock into a refusal.
fn reaped_pid() -> u32 {
    let mut child = Command::new("true").spawn().expect("spawn /usr/bin/true");
    let pid = child.id();
    child.wait().expect("reap it");
    assert!(
        !lock::pid_alive(pid),
        "pid {pid} should be dead after reaping"
    );
    pid
}

/// The bundled `approve` action (claude ⇒ ["1"]).
fn approve() -> ActionManifest {
    let src = include_str!("../../tma-core/actions/approve.toml");
    ActionManifest::parse(src, "approve", "approve.toml").unwrap()
}

#[test]
fn keys_action_delivers_and_clears_the_lock() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("broker_keys");
    let pane = s.new_shell_pane();
    stamp_blocked_claude(&s, &pane);

    // Plant a reclaimable lock: expired, and held by a pid that is provably dead. The broker must
    // take it (the reclaim pre-check only refuses an expired lock whose holder is still alive) and
    // then release its OWN nonce. Without something planted here the release assertion below is
    // unfalsifiable — an empty `@agent_action` reads the same whether the lock was taken and
    // released or never acquired at all.
    let stale = LockValue {
        expiry_ms: 1,
        nonce: "0".repeat(32),
        pid: reaped_pid(),
        name: "approve".to_string(),
    };
    s.set_opt(&pane, "@agent_action", &stale.encode());

    let tmux = adapter(&s);
    let manifests = tma_runtime::manifests::load(None, &[])
        .expect("load manifests")
        .manifests;
    let cfg = FoldConfig::default();

    let r = broker::fire(
        &tmux,
        &manifests,
        &cfg,
        &ApiSection::default(),
        broker::DetachCtx::default(),
        &approve(),
        &pane,
        broker::FireArgs::default(),
    );
    assert_eq!(r.outcome, Outcome::Sent, "approve fires on a blocked pane");
    assert_eq!(r.exit_code(), 0);

    // The `1` keystroke reached the pane's shell prompt. Anchored on the prompt: a bare `1` is
    // ambient text (it sits in most themed prompts), so the unanchored needle stayed green with
    // `Tmux::send_keys` stubbed to a no-op.
    let echoed = format!("{}1", common::SHELL_PROMPT);
    assert!(
        common::wait_capture_contains(&s.socket, &pane, &echoed, common::POLL_CEILING),
        "the approve keystroke `1` should be echoed at the pane's prompt as {echoed:?}"
    );
    // The single-flight lock is released nonce-conditionally on the send path (empty == absent).
    // The planted stale lock is what makes this a real assertion: an empty option now means the
    // broker overwrote it (acquired) and then cleared its own nonce (released). A broker that
    // never acquired would have left the planted value standing — its nonce-conditional clear
    // matches nothing.
    let held = s.pane_option(&pane, "@agent_action");
    assert!(held.is_empty(), "lock should be cleared, got {held:?}");
}

#[test]
fn second_act_on_a_held_lock_exits_five() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("broker_singleflight");
    let pane = s.new_shell_pane();
    stamp_blocked_claude(&s, &pane);

    let tmux = adapter(&s);
    let manifests = tma_runtime::manifests::load(None, &[])
        .expect("load manifests")
        .manifests;
    let cfg = FoldConfig::default();

    // A first actor holds the lock with a far-future expiry (unexpired), so the broker's acquire
    // read-back loses the CAS and refuses.
    let now = tma_runtime::now_ms();
    let expiry = now + 1_000_000_000;
    let first = match lock::acquire(&tmux, &pane, now, expiry, std::process::id(), "approve")
        .expect("first acquire")
    {
        Acquire::Acquired(v) => v,
        Acquire::Contended => panic!("first acquire should win on a fresh pane"),
    };

    let r = broker::fire(
        &tmux,
        &manifests,
        &cfg,
        &ApiSection::default(),
        broker::DetachCtx::default(),
        &approve(),
        &pane,
        broker::FireArgs::default(),
    );
    assert_eq!(r.reason(), Some("locked"), "a held lock refuses");
    assert_eq!(r.exit_code(), 5);

    // The first holder's lock is intact (the loser never cleared it): its nonce still stored.
    let stored = s.pane_option(&pane, "@agent_action");
    let parsed = LockValue::parse(&stored).expect("held lock still parses");
    assert_eq!(parsed.nonce, first.nonce, "the held lock is untouched");
}

/// Stamp `pane` as a fresh `blocked/permission` OpenCode agent with a pending request id and an API
/// endpoint, so the bundled `approve` fires over the API lane rather than keystrokes.
fn stamp_blocked_opencode(s: &Scratch, pane: &str, request_id: &str, endpoint: &str) {
    let now = tma_runtime::now_ms().to_string();
    s.set_opt(pane, "@agent_name", "opencode");
    s.set_opt(pane, "@agent_state", "blocked");
    s.set_opt(pane, "@agent_detail", "permission");
    s.set_opt(pane, "@agent_stamped_at", &now);
    s.set_opt(pane, "@agent_source", "capture");
    s.set_opt(pane, "@agent_pid", "4242");
    s.set_opt(pane, "@agent_permission_request", request_id);
    s.set_opt(pane, "@agent_api_endpoint", endpoint);
}

/// A one-shot HTTP/1.1 server on `127.0.0.1:0` that replies `status_line` to the first request and
/// records the request line; returns `(http_base_url, join_handle)`.
fn mock_http(status_line: &'static str) -> (String, std::thread::JoinHandle<String>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("mock server accept");
        let mut buf = [0u8; 1024];
        let n = stream.read(&mut buf).unwrap_or(0);
        let _ = stream.write_all(
            format!("{status_line}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes(),
        );
        String::from_utf8_lossy(&buf[..n]).to_string()
    });
    (base, handle)
}

#[test]
fn api_action_replies_over_http_and_clears_the_lock() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let (endpoint, server) = mock_http("HTTP/1.1 200 OK");
    let s = Scratch::new("broker_api");
    let pane = s.new_shell_pane();
    stamp_blocked_opencode(&s, &pane, "per_test123", &endpoint);

    let tmux = adapter(&s);
    let manifests = tma_runtime::manifests::load(None, &[])
        .expect("load manifests")
        .manifests;
    let cfg = FoldConfig::default();

    let r = broker::fire(
        &tmux,
        &manifests,
        &cfg,
        &ApiSection::default(),
        broker::DetachCtx::default(),
        &approve(),
        &pane,
        broker::FireArgs::default(),
    );
    assert_eq!(
        r.outcome,
        Outcome::Replied,
        "opencode approve answers over the API"
    );
    assert_eq!(r.exit_code(), 0);

    // The broker POSTed to the pinned endpoint with the request id in the path.
    let request = server.join().expect("mock server thread");
    assert!(
        request.starts_with("POST /permission/per_test123/reply "),
        "unexpected request line: {request:?}"
    );
    // The single-flight lock is released on the API path too.
    let held = s.pane_option(&pane, "@agent_action");
    assert!(held.is_empty(), "lock should be cleared, got {held:?}");
}

#[test]
fn api_action_404_is_vanished() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let (endpoint, server) = mock_http("HTTP/1.1 404 Not Found");
    let s = Scratch::new("broker_api_404");
    let pane = s.new_shell_pane();
    stamp_blocked_opencode(&s, &pane, "per_gone", &endpoint);

    let tmux = adapter(&s);
    let manifests = tma_runtime::manifests::load(None, &[])
        .expect("load manifests")
        .manifests;
    let cfg = FoldConfig::default();

    let r = broker::fire(
        &tmux,
        &manifests,
        &cfg,
        &ApiSection::default(),
        broker::DetachCtx::default(),
        &approve(),
        &pane,
        broker::FireArgs::default(),
    );
    let _ = server.join();
    assert_eq!(
        r.outcome.token(),
        "vanished",
        "a 404 is the target vanished"
    );
    assert_eq!(r.exit_code(), 3);
    assert_eq!(
        r.reason(),
        Some("request-gone"),
        "the request went away; the pane is still here"
    );
    assert_eq!(
        s.pane_option(&pane, "@agent_permission_request"),
        "per_gone",
        "a 404 leaves the stamp alone: it may already be a NEWER request"
    );
}

/// A 2xx reply spends the request, so the broker clears `@agent_permission_request` under the same
/// held lock. Until it did, the stamp outlived its request until the plugin's next
/// `permission.replied` event, and anything reading the stamp as "a request is pending" read a
/// spent id.
#[test]
fn a_replied_permission_clears_the_request_stamp() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let (endpoint, server) = mock_http("HTTP/1.1 200 OK");
    let s = Scratch::new("broker_api_clear");
    let pane = s.new_shell_pane();
    stamp_blocked_opencode(&s, &pane, "per_spent", &endpoint);

    let tmux = adapter(&s);
    let manifests = tma_runtime::manifests::load(None, &[])
        .expect("load manifests")
        .manifests;
    let cfg = FoldConfig::default();
    let fire = |action: &ActionManifest| {
        broker::fire(
            &tmux,
            &manifests,
            &cfg,
            &ApiSection::default(),
            broker::DetachCtx::default(),
            action,
            &pane,
            broker::FireArgs::default(),
        )
    };

    let r = fire(&approve());
    let _ = server.join();
    assert_eq!(r.outcome, Outcome::Replied);
    assert_eq!(
        s.pane_option(&pane, "@agent_permission_request"),
        "",
        "the spent request id must not outlive its reply"
    );
    assert_eq!(
        s.pane_option(&pane, "@agent_api_endpoint"),
        endpoint,
        "only the request id is cleared; the endpoint is not the broker's to unstamp"
    );

    // The binder's reading of the same fact: with the stamp gone, a second dispatch is refused
    // before the lock rather than fired at an id the server has already spent. No server is
    // listening now, so a fire that got past the gate would surface as an `error`, not a refusal.
    let again = fire(&approve());
    assert_eq!(again.reason(), Some("requires-unmet"));
    assert_eq!(again.exit_code(), 4);
}

#[test]
fn expired_lock_is_reclaimed_only_when_its_holder_is_dead() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("broker_reclaim");
    let pane = s.new_shell_pane();
    let tmux = adapter(&s);
    let now = tma_runtime::now_ms();
    let expired = now.saturating_sub(60_000); // one minute in the past

    // A guaranteed-dead pid: spawn `true`, reap it, reuse its pid.
    let mut dead = Command::new("true").spawn().expect("spawn `true`");
    let dead_pid = dead.id();
    dead.wait().expect("reap `true`");

    // Expired lock held by a DEAD pid ⇒ the liveness pre-check lets the acquire reclaim it.
    let dead_lock = LockValue {
        expiry_ms: expired,
        nonce: "0123456789abcdef0123456789abcdef".to_string(),
        pid: dead_pid,
        name: "approve".to_string(),
    };
    s.set_opt(&pane, "@agent_action", &dead_lock.encode());
    match lock::acquire(
        &tmux,
        &pane,
        now,
        now + 30_000,
        std::process::id(),
        "approve",
    )
    .expect("acquire over a dead-pid expired lock")
    {
        Acquire::Acquired(v) => assert_ne!(v.nonce, dead_lock.nonce, "a fresh nonce was written"),
        Acquire::Contended => panic!("an expired lock with a dead holder must be reclaimable"),
    }

    // Expired lock held by a LIVE pid (our own) ⇒ the liveness pre-check refuses despite the past
    // expiry (a suspended supervisor whose child still runs must not be reclaimed).
    let live_lock = LockValue {
        expiry_ms: expired,
        nonce: "ffffffffffffffffffffffffffffffff".to_string(),
        pid: std::process::id(),
        name: "approve".to_string(),
    };
    s.set_opt(&pane, "@agent_action", &live_lock.encode());
    let outcome = lock::acquire(
        &tmux,
        &pane,
        now,
        now + 30_000,
        std::process::id(),
        "approve",
    )
    .expect("acquire over a live-pid expired lock");
    assert_eq!(
        outcome,
        Acquire::Contended,
        "an expired lock whose holder is still alive refuses reclaim"
    );
    // The live holder's lock is untouched.
    let stored = s.pane_option(&pane, "@agent_action");
    assert_eq!(
        LockValue::parse(&stored).map(|v| v.nonce),
        Some(live_lock.nonce),
        "the live holder's lock is left intact"
    );
}
