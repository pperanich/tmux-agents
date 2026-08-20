//! The ordered-input "seen" test: has the user looked at a pane since its attention flag was
//! raised, without ever navigating away from it?
//!
//! The focus hooks clear `@agent_attention` on arrival and on departure, which covers every case
//! where the user *navigates*. It leaves the one the reporter hit: you sit on the pane, the agent
//! finishes under your eyes, and no navigation ever happens, so nothing clears. This layer closes it
//! by reading the two facts tmux already keeps — which pane each client is displaying, and when that
//! client last received real terminal input — and clearing iff the input came **after** the raise.
//!
//! **Ordered, never windowed.** "The user typed within the last N seconds" would break the headline
//! case: type a prompt, walk away, the agent finishes 30 s later, and the marker is suppressed with
//! no way to get it back. Ordering against the raise instant makes an absent human generate no
//! input, so the flag survives arbitrarily long. See the rejected-alternatives list in
//! `docs/internal/TASKS-detection-repair.md`.

/// One attached client's view, as `list-clients` reports it.
///
/// `pane_id` is that client's current window's active pane — the pane it is DISPLAYING, which is
/// not the same question as which pane tmux calls active server-wide.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientView {
    /// `#{pane_id}` resolved in the client's context: the pane on that client's screen.
    pub pane_id: String,
    /// `#{client_activity}`: epoch **seconds** of the client's last real terminal input. Moves only
    /// on input a human (or their terminal) actually sent — including the prefix key and mouse
    /// events — never on pane output, never on `send-keys`, and never on tma's own polling, whose
    /// command clients do not appear in `list-clients` at all.
    pub activity_secs: u64,
    /// `#{client_control_mode}`: a control-mode client (iTerm2's `-CC`), whose `client_activity`
    /// freezes at attach time and is therefore not evidence of anything. Ignored by [`seen_by_input`].
    pub control_mode: bool,
}

/// Has the user seen this pane's raised attention? True iff some non-control-mode client is
/// displaying `pane_id` **and** that client's last input is strictly later than `raised_at_ms`
/// (`@agent_since`, which is write-once per state run and so *is* the raise instant while the flag
/// stands).
///
/// The two clocks have different resolutions: `activity_secs` is seconds, `raised_at_ms` is
/// milliseconds. Flooring the second to `secs * 1000` and demanding a strict `>` makes the rounding
/// error one-directional — a keystroke in the same second as the raise reads as "not yet seen", so
/// the layer can only fail to clear, never clear a pane nobody looked at.
pub fn seen_by_input(clients: &[ClientView], pane_id: &str, raised_at_ms: u64) -> bool {
    clients.iter().any(|c| {
        !c.control_mode
            && c.pane_id == pane_id
            && c.activity_secs.saturating_mul(1000) > raised_at_ms
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(pane: &str, secs: u64) -> ClientView {
        ClientView {
            pane_id: pane.to_string(),
            activity_secs: secs,
            control_mode: false,
        }
    }

    #[test]
    fn no_clients_never_clears() {
        assert!(!seen_by_input(&[], "%1", 1_000_000));
    }

    #[test]
    fn a_client_on_another_pane_never_clears() {
        let clients = [client("%2", 2_000)];
        assert!(!seen_by_input(&clients, "%1", 1_000_000));
    }

    /// Walk-away, the case the whole design is shaped around: you typed, left, and the agent
    /// finished behind you. The client is still parked on the pane, but its last input predates the
    /// raise, so the marker must survive — for hours, not for a window.
    #[test]
    fn input_older_than_the_raise_never_clears() {
        let clients = [client("%1", 1_000)];
        assert!(!seen_by_input(&clients, "%1", 1_030_000));
    }

    #[test]
    fn input_after_the_raise_clears() {
        let clients = [client("%1", 1_060)];
        assert!(seen_by_input(&clients, "%1", 1_030_000));
    }

    /// Same second as the raise: floored, that is not strictly later, so it does not clear. The
    /// error stays one-directional and the next keystroke clears it a second later.
    #[test]
    fn input_in_the_same_second_as_the_raise_does_not_clear() {
        let clients = [client("%1", 1_030)];
        assert!(!seen_by_input(&clients, "%1", 1_030_000));
        assert!(!seen_by_input(&clients, "%1", 1_030_400));
        assert!(seen_by_input(&clients, "%1", 1_029_999));
    }

    /// Two clients, only one of them looking at this pane: the busy one on the other pane must not
    /// speak for it.
    #[test]
    fn only_the_client_displaying_the_pane_counts() {
        let clients = [client("%2", 9_000), client("%1", 1_000)];
        assert!(!seen_by_input(&clients, "%1", 1_030_000));
        assert!(seen_by_input(&clients, "%2", 1_030_000));
    }

    /// A control-mode client's `client_activity` freezes at attach time, so it reports a timestamp
    /// that has nothing to do with whether the human looked. Ignored entirely — which makes this
    /// layer a no-op under iTerm2 `-CC`, by design.
    #[test]
    fn a_control_mode_client_is_ignored() {
        let clients = [ClientView {
            pane_id: "%1".to_string(),
            activity_secs: 9_000,
            control_mode: true,
        }];
        assert!(!seen_by_input(&clients, "%1", 1_030_000));
    }

    /// A second, non-control-mode client on the same pane still decides it: the filter drops the
    /// control-mode client, it does not disqualify the pane.
    #[test]
    fn a_real_client_beside_a_control_mode_one_still_clears() {
        let clients = [
            ClientView {
                pane_id: "%1".to_string(),
                activity_secs: 1_000,
                control_mode: true,
            },
            client("%1", 1_060),
        ];
        assert!(seen_by_input(&clients, "%1", 1_030_000));
    }

    /// A nonsense timestamp from a corrupt read must not panic the cycle.
    #[test]
    fn an_absurd_activity_stamp_saturates_instead_of_overflowing() {
        let clients = [client("%1", u64::MAX)];
        assert!(seen_by_input(&clients, "%1", 1_030_000));
    }
}
