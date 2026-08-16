//! The shared 1 s refresh deadline both surface folds delegate to, in epoch-ms.

/// The two live surfaces' shared 1 s self-refresh cadence, in epoch-ms (mirrors the shell's
/// `dash::REFRESH_INTERVAL`). Both folds seed their [`RefreshGate`] with it.
pub(crate) const REFRESH_INTERVAL_MS: u64 = 1000;

/// Fires `tick` once `interval_ms` has elapsed since the last fire; `force` re-arms it so the next
/// `tick` fires regardless of the deadline (the SIGUSR1 nudge path). Reads no clock: `now` is
/// always a parameter.
#[derive(Debug)]
pub struct RefreshGate {
    last: u64,
    interval_ms: u64,
}

impl RefreshGate {
    /// A gate armed at `now`, first firing `interval_ms` later.
    pub fn new(now: u64, interval_ms: u64) -> Self {
        Self {
            last: now,
            interval_ms,
        }
    }

    /// True once `now` has reached the deadline; a true result re-arms the gate from `now`.
    pub fn tick(&mut self, now: u64) -> bool {
        if now.saturating_sub(self.last) >= self.interval_ms {
            self.last = now;
            true
        } else {
            false
        }
    }

    /// Re-arm so the next `tick` fires immediately, bypassing the deadline (nudge).
    pub fn force(&mut self, now: u64) {
        self.last = now.saturating_sub(self.interval_ms);
    }

    /// The `Tick` fold arm: true once the deadline has passed (refresh due), else false. The fold
    /// lifts the bool into an `Effect::Refresh` batch, so the gate stays free of the effect vocabulary.
    pub fn on_tick(&mut self, now: u64) -> bool {
        self.tick(now)
    }

    /// The `Nudge` fold arm: always due. Re-arm the timer (force, then consume the guaranteed tick)
    /// so a `clear-attention` from another pane lands within one poll interval, and return true.
    pub fn on_nudge(&mut self, now: u64) -> bool {
        self.force(now);
        self.tick(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_holds_until_deadline() {
        let mut gate = RefreshGate::new(1000, 1000);
        assert!(!gate.tick(1999), "before the interval, tick is false");
        assert!(gate.tick(2000), "at the deadline, tick fires and re-arms");
        assert!(!gate.tick(2999), "the fire re-armed from 2000");
        assert!(gate.tick(3000), "past the interval, tick fires again");
    }

    #[test]
    fn nudge_bypasses_refresh_gate() {
        let mut gate = RefreshGate::new(1000, 1000);
        assert!(!gate.tick(1500), "mid-interval, tick is false");
        gate.force(1500);
        assert!(
            gate.tick(1500),
            "force fires the next tick despite the deadline"
        );
    }

    #[test]
    fn on_tick_reports_due_as_bool() {
        let mut gate = RefreshGate::new(1000, 1000);
        assert!(!gate.on_tick(1500), "before the deadline, not due");
        assert!(gate.on_tick(2000), "at the deadline, due and re-armed");
        assert!(!gate.on_tick(2500), "the fire re-armed the gate");
    }

    #[test]
    fn on_nudge_is_always_due_and_rearms() {
        let mut gate = RefreshGate::new(1000, 1000);
        assert!(gate.on_nudge(1200), "a nudge is due even mid-interval");
        assert!(
            !gate.on_tick(1200),
            "the nudge re-armed the gate to now, so an immediate tick is not due"
        );
    }
}
