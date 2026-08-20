//! Acceptance: the ordered-input clear, on an isolated scratch tmux server with a REAL attached
//! PTY client (`client_activity` moves only for one, and only on genuine terminal input).
//!
//! The property under test is an ORDERING, not a window: a done marker raised *after* the user's
//! last keystroke survives however long they stay away, and comes down on the next keystroke while
//! that pane is on their screen. Every case here therefore pins both sides of the order on the SAME
//! pane and the SAME client, so a passing run cannot mean "the clear is dead" — the walk-away case
//! proves liveness by then reordering the raise and watching the very next cycle clear it.
//!
//! The PTY attach needs `python3`; absent it these skip rather than fail (per the jump suite).

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tma_core::FoldConfig;
use tma_runtime::capture::CaptureState;
use tma_runtime::cycle::{self, SeenClear};
use tma_runtime::seen;
use tma_tmux::tmux::Tmux;

use common::{AttachOutcome, Scratch};
use tma_test_support as common;

const ATTENTION: &str = "@agent_attention";

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Freshness wide enough that every cycle here stays on the consumer path: the seeded stamp is read
/// as-is, so what these cases observe is the seen pass and nothing else.
fn cfg() -> FoldConfig {
    FoldConfig {
        freshness_secs: 600,
        ..FoldConfig::default()
    }
}

fn client(s: &Scratch) -> Tmux {
    Tmux::new(Some(s.socket.clone()))
}

/// A detached 80x24 `home` session running a long-lived `sleep`, plus a second window (created with
/// `-d`, so the client stays on window 0). Returns `(watched, witness)` pane ids.
fn two_panes(s: &Scratch) -> (String, String) {
    assert!(s
        .tmux(&[
            "new-session",
            "-d",
            "-s",
            "home",
            "-x",
            "80",
            "-y",
            "24",
            "exec sleep 100000",
        ])
        .status
        .success());
    assert!(s
        .tmux(&["new-window", "-d", "-t", "home", "exec sleep 100000"])
        .status
        .success());
    (s.get("home:0", "#{pane_id}"), s.get("home:1", "#{pane_id}"))
}

/// Seed a finished-but-unreviewed pane: idle + `@agent_attention`, raised at `since_ms`, with a
/// stamp fresh enough to be consumed rather than re-folded.
fn mark_done(s: &Scratch, pane: &str, since_ms: u64) {
    s.set_opt(pane, "@agent_name", "claude");
    s.set_opt(pane, "@agent_state", "idle");
    s.set_opt(pane, "@agent_since", &since_ms.to_string());
    s.set_opt(pane, "@agent_stamped_at", &now_ms().to_string());
    s.set_opt(pane, ATTENTION, "1");
}

/// The pane the (single) attached client is displaying, per `list-clients`.
fn displayed_pane(s: &Scratch) -> String {
    String::from_utf8_lossy(&s.tmux(&["list-clients", "-F", "#{pane_id}"]).stdout)
        .trim()
        .to_string()
}

/// The attached client's `#{client_activity}` — epoch SECONDS — scaled to ms.
fn client_activity_ms(s: &Scratch) -> u64 {
    String::from_utf8_lossy(&s.tmux(&["list-clients", "-F", "#{client_activity}"]).stdout)
        .trim()
        .parse::<u64>()
        .map(|secs| secs * 1000)
        .unwrap_or(0)
}

/// Attach a PTY client to `home`, or report why not.
fn attach(s: &mut Scratch) -> bool {
    match s.attach_client("home") {
        AttachOutcome::Attached => true,
        AttachOutcome::NoPython => {
            eprintln!("skipping: python3 unavailable for the PTY attach");
            false
        }
        AttachOutcome::Failed => {
            panic!("PTY client failed to attach after python3 ran (regression, not env)")
        }
    }
}

/// Type at the client's real terminal until its activity clock reads strictly past `since_ms`.
/// Repeated because `client_activity` has one-second resolution: a keystroke inside the raise's own
/// second is deliberately not "later than" it, so the loop keeps typing into the next second.
fn type_past(s: &Scratch, since_ms: u64) {
    let deadline = Instant::now() + common::POLL_CEILING;
    while Instant::now() < deadline {
        // `q` reaches a pane running `sleep`, which discards it.
        s.send_client_keys("q");
        std::thread::sleep(Duration::from_millis(200));
        if client_activity_ms(s) > since_ms {
            return;
        }
    }
    panic!("the PTY client's input never registered past the raise");
}

/// A raise the user has since typed past, on the pane they are looking at, comes down — and only
/// there. The witness pane carries an identically-aged marker in a window no client is displaying,
/// so a clear that ignored which pane is on screen fails here.
#[test]
fn input_after_the_raise_clears_the_pane_you_are_watching() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not available");
        return;
    }
    let mut s = Scratch::new("seen_clear");
    let (watched, witness) = two_panes(&s);
    if !attach(&mut s) {
        return;
    }
    assert_eq!(
        displayed_pane(&s),
        watched,
        "the attached client must be displaying the watched pane"
    );

    // Raised a minute ago; the attach itself is more recent than that, and the typing below more
    // recent still.
    let raised = now_ms() - 60_000;
    mark_done(&s, &watched, raised);
    mark_done(&s, &witness, raised);
    type_past(&s, raised);

    let report = cycle::run_cycle(&client(&s), &[], &cfg()).expect("cycle");
    assert_eq!(
        s.pane_option(&watched, ATTENTION),
        "",
        "typing at the pane you are watching clears its done marker"
    );
    assert_eq!(
        s.pane_option(&witness, ATTENTION),
        "1",
        "a pane no client is displaying keeps its marker"
    );

    // The rows this cycle hands its surface must already reflect the clear it just made, or
    // `tma status` counts a done pane it has itself just retired.
    let row = |pane: &str| {
        report
            .rows
            .iter()
            .find(|r| r.pane_id == pane)
            .unwrap_or_else(|| panic!("no row for {pane}"))
            .attention
    };
    assert!(
        !row(&watched),
        "the cleared row must not still read attention"
    );
    assert!(row(&witness), "the untouched row keeps it");
}

/// Walk-away, the case the ordering exists to protect: the user typed, then left; the agent
/// finished behind them. Their client is still parked on the pane, but its last input predates the
/// raise, so the marker must stand. The second half is the liveness proof — move the raise back
/// behind that same keystroke and the very next cycle clears it.
#[test]
fn a_marker_raised_after_your_last_input_survives() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not available");
        return;
    }
    let mut s = Scratch::new("seen_walkaway");
    let (watched, _witness) = two_panes(&s);
    if !attach(&mut s) {
        return;
    }

    // Type first, then walk away: the raise is everything after the last keystroke.
    type_past(&s, 0);
    let last_input = client_activity_ms(&s);
    common::poll_until("the client's activity second to elapse", || {
        now_ms() > last_input + 1_000
    });
    let raised = now_ms();
    mark_done(&s, &watched, raised);

    cycle::run_cycle(&client(&s), &[], &cfg()).expect("cycle");
    assert_eq!(
        s.pane_option(&watched, ATTENTION),
        "1",
        "a marker raised after the user's last input must survive: they have not seen it"
    );

    // Same pane, same client, same still-idle keyboard — only the order changes.
    s.set_opt(&watched, "@agent_since", &(last_input - 1_000).to_string());
    s.set_opt(&watched, "@agent_stamped_at", &now_ms().to_string());
    cycle::run_cycle(&client(&s), &[], &cfg()).expect("cycle");
    assert_eq!(
        s.pane_option(&watched, ATTENTION),
        "",
        "with the raise moved behind the same keystroke, the clear fires — so the case above was \
         the ordering holding, not the pass being dead"
    );
}

/// Nobody attached, nobody looking: the marker stands whatever its age. This is the daemon-only /
/// detached-server floor, and it costs no `list-clients` guesswork to get wrong.
#[test]
fn a_server_with_no_clients_never_clears() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not available");
        return;
    }
    let s = Scratch::new("seen_noclient");
    let (pane, _witness) = two_panes(&s);
    mark_done(&s, &pane, now_ms() - 60_000);

    cycle::run_cycle(&client(&s), &[], &cfg()).expect("cycle");
    assert_eq!(
        s.pane_option(&pane, ATTENTION),
        "1",
        "with no client attached there is no evidence anyone saw anything"
    );
}

/// The daemon's sweep must NOT clear inline: its notification dispatch runs afterwards and reads
/// the same persisted flag, so a clear inside the sweep would retire a completion nobody had been
/// told about. The sweep hands the candidates over instead, and the caller clears them when it is
/// ready.
#[test]
fn the_sweep_defers_its_clear_to_the_caller() {
    if !common::tmux_available() {
        eprintln!("skipping: tmux not available");
        return;
    }
    let mut s = Scratch::new("seen_defer");
    let (watched, _witness) = two_panes(&s);
    if !attach(&mut s) {
        return;
    }
    let raised = now_ms() - 60_000;
    mark_done(&s, &watched, raised);
    type_past(&s, raised);

    let tmux = client(&s);
    let mut capture = CaptureState::new(cfg(), 3);
    capture.run_sweep(&tmux, &[]).expect("sweep");
    assert_eq!(
        s.pane_option(&watched, ATTENTION),
        "1",
        "the sweep must leave the flag standing for the notification dispatch that follows it"
    );

    let deferred = capture.take_deferred_seen();
    assert_eq!(
        deferred.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(),
        vec![watched.as_str()],
        "the sweep hands the raised pane to its caller"
    );
    assert_eq!(
        seen::clear_seen(&tmux, &deferred),
        vec![watched.clone()],
        "and the caller's own pass is what clears it"
    );
    assert_eq!(s.pane_option(&watched, ATTENTION), "");
    assert!(
        capture.take_deferred_seen().is_empty(),
        "a taken candidate is never replayed against a later sweep's flags"
    );

    // The two policies, contrasted on the same re-raised marker: deferred reports and leaves the
    // write to its caller, inline does it itself.
    mark_done(&s, &watched, raised);
    let report = cycle::run_cycle_with(&tmux, &[], &cfg(), SeenClear::Deferred).expect("deferred");
    assert_eq!(
        report
            .deferred_seen
            .iter()
            .map(|(p, _)| p.as_str())
            .collect::<Vec<_>>(),
        vec![watched.as_str()]
    );
    assert_eq!(
        s.pane_option(&watched, ATTENTION),
        "1",
        "a deferred cycle writes nothing"
    );

    let report = cycle::run_cycle(&tmux, &[], &cfg()).expect("inline");
    assert_eq!(s.pane_option(&watched, ATTENTION), "");
    assert!(
        report.deferred_seen.is_empty(),
        "an inline cycle defers nothing to anybody"
    );
}
