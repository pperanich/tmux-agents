//! The shell runner: one generic loop (`run_surface`) plus the shared effect executor, replacing the
//! two hand-rolled draw/input loops. It owns the impure edges the core cannot touch:
//! the crossterm poll (mapped to the core's `Key`/`Resize`, else `Tick`), the SIGUSR1 nudge drain,
//! the terminal draw, and the tmux I/O each `Effect` names. The core folds; the runner executes.

use std::collections::VecDeque;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::{
    self, Event as CEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::backend::CrosstermBackend;
use ratatui::{Frame, Terminal};
use tma_core::{AgentRow, AgentState, Selector};
use tma_runtime::config;
use tma_runtime::manifests::LoadedManifest;
use tma_runtime::{nudge, ui, Server, Tmux};
use tma_ui_core::palette::{RowPalette, RowStyles};
use tma_ui_core::{Effect, Event, Key, Mouse, MouseKind};

use crate::dash;
use crate::jump;
use crate::term::TerminalGuard;

/// A refresh-and-select surface the runner drives: its own model, its scratch resource (`Res`), the
/// fold, and the draw. `PickerModel`/`WatchModel` implement it; the runner stays surface-agnostic.
pub(crate) trait Surface {
    /// Per-surface scratch state that cannot live in a `Debug` model (the picker's `Matcher`; `()`
    /// for `watch`).
    type Res;
    fn update(&mut self, ev: Event, now: u64, res: &mut Self::Res) -> Vec<Effect>;
    fn view(&self, f: &mut Frame, now: u64, palette: &RowPalette);
}

/// A surface's row filter, applied to each refresh's rows on the way into the fold: the CLI
/// selector (`watch`) and an optional pane to drop (the picker's own invoking pane). It sits here
/// rather than in the fold so the cycle inside `Refresh` stays unfiltered (every pane is still
/// stamped) and the fold keeps its one row-set input.
pub(crate) struct RowFilter {
    selector: Selector,
    /// The invoking client's active pane, which the picker hides: jumping to the pane you are
    /// already in does nothing. `None` leaves every row in place.
    exclude_pane: Option<String>,
}

impl RowFilter {
    /// A selector-only filter (`watch`, `ls`-style scoping).
    pub(crate) fn from_selector(selector: Selector) -> Self {
        Self {
            selector,
            exclude_pane: None,
        }
    }

    /// The picker's filter: no selector flags (its fuzzy query is its filter), just the self-pane
    /// exclusion.
    pub(crate) fn excluding(pane: Option<String>) -> Self {
        Self {
            selector: Selector::default(),
            exclude_pane: pane,
        }
    }

    /// Drop the rows this surface does not show. Also applied to the picker's stamp-seeded first
    /// frame, so the exclusion holds from frame one and across every refresh.
    pub(crate) fn apply(&self, rows: &mut Vec<AgentRow>) {
        self.selector.retain(rows);
        if let Some(pane) = &self.exclude_pane {
            rows.retain(|r| &r.pane_id != pane);
        }
    }
}

/// Build the draw palette from the config's `[picker]` styles: resolve each state's string pair, then
/// map it to a ratatui `Color`. Rebuilt each draw so a hot-reloaded glyph/colour takes effect (the
/// runtime config stays string-only; the ratatui vocabulary lives in the UI core).
fn row_palette(styles: &config::PickerStyles) -> RowPalette {
    RowPalette::new(RowStyles {
        blocked: styles.resolved_str(AgentState::Blocked),
        working: styles.resolved_str(AgentState::Working),
        idle: styles.resolved_str(AgentState::Idle),
        unknown: styles.resolved_str(AgentState::Unknown),
        done: styles.resolved_done_str(),
    })
}

/// Everything a surface run is configured with, fixed for its whole lifetime: the tmux handles, the
/// hot-reloadable config/manifest pair with the paths to reload it from, and the row filter.
pub(crate) struct SurfaceEnv<'a> {
    pub(crate) tmux: &'a Tmux,
    /// The server the surface is driving, forwarded to the `act --menu` child so the menu lands on
    /// the same tmux the rows came from (the surface may target a socket its environment does not).
    pub(crate) server: &'a Server,
    pub(crate) config: config::Config,
    pub(crate) manifests: Vec<LoadedManifest>,
    pub(crate) config_path: Option<PathBuf>,
    pub(crate) manifest_dir: Option<PathBuf>,
    pub(crate) acting_client: Option<&'a str>,
    /// The surface's row filter, applied to each refresh's rows on the way into the fold.
    pub(crate) filter: RowFilter,
}

/// Run a surface to completion. The shell has already entered `guard` (raw mode + the alternate
/// screen, and any pid advertisement); the runner owns it so it can drop it before the deferred jump.
pub(crate) fn run_surface<S: Surface>(
    mut surface: S,
    mut res: S::Res,
    guard: TerminalGuard<'_>,
    env: SurfaceEnv<'_>,
) -> io::Result<()> {
    let mut exec = Executor {
        env,
        notices: Vec::new(),
    };
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let size = terminal.size()?;
    let mut queue = seeded_queue(size.width, size.height);

    // A batch carrying `Quit` is deferred here and run after the guard drops.
    let mut deferred: Vec<Effect> = Vec::new();
    let loop_result = (|| -> io::Result<()> {
        loop {
            if let Some(batch) = drain(
                &mut surface,
                &mut res,
                &mut queue,
                &mut exec,
                tma_runtime::now_ms,
            ) {
                deferred = batch;
                return Ok(());
            }
            let palette = row_palette(&exec.env.config.picker);
            terminal.draw(|f| surface.view(f, tma_runtime::now_ms(), &palette))?;
            if let Some(ev) = poll_event()? {
                queue.push_back(ev);
            }
            if nudge::take_nudge() {
                queue.push_back(Event::Nudge);
            }
        }
    })();

    // Restore the terminal (drop the guard) before any deferred effect, so a jump lands on a clean
    // terminal (the picker's Enter path).
    drop(terminal);
    drop(guard);
    // The loop's own error is held so the notices still reach stderr; `deferred` is empty on that
    // path (only the `Quit` return fills it), so nothing extra runs.
    let ran = loop_result;
    for eff in deferred {
        let follow = exec.execute(eff);
        // Quit only batches with effects that return no follow-up (Focus/ClearAttention); a follow-up
        // here would be silently dropped, so assert the invariant in debug builds.
        debug_assert!(
            follow.is_none(),
            "a deferred effect returned a dropped follow-up"
        );
    }
    // The one place effect failures reach the user: the surface has closed, the terminal is the
    // shell's again, so a silent no-op (a jump that never moved) now says why.
    for note in &exec.notices {
        eprintln!("{note}");
    }
    ran
}

/// The initial event queue: one synthetic `Resize`. crossterm emits `Resize` only
/// on change, so without this seed the first fold never sees the real dimensions and frame one picks
/// the wrong layout.
fn seeded_queue(width: u16, height: u16) -> VecDeque<Event> {
    let mut queue = VecDeque::new();
    queue.push_back(Event::Resize { width, height });
    queue
}

/// Drain the event queue through the surface, running each batch's effects inline via `exec` and
/// enqueuing any follow-ups, until the queue empties or a batch carries `Effect::Quit`. Returns the
/// `Quit`-carrying batch (to defer past the guard drop), or `None` when it drains.
/// `now` is called fresh per event so the fold reads the clock exactly when it folds.
fn drain<S: Surface, X: ExecuteEffect>(
    surface: &mut S,
    res: &mut S::Res,
    queue: &mut VecDeque<Event>,
    exec: &mut X,
    now: fn() -> u64,
) -> Option<Vec<Effect>> {
    while let Some(ev) = queue.pop_front() {
        let effects = surface.update(ev, now(), res);
        // Presence of `Quit` is the only defer signal; the runner does not know surfaces.
        if effects.iter().any(|e| matches!(e, Effect::Quit)) {
            return Some(effects);
        }
        for eff in effects {
            if let Some(follow) = exec.execute(eff) {
                queue.push_back(follow);
            }
        }
    }
    None
}

/// Poll input for one `POLL_INTERVAL`: a mapped key, mouse report, or resize, `Tick` on timeout or
/// an unmapped event, `None` for an event that maps to nothing (a key release, a drag, ctrl + an
/// unbound char).
///
/// All-motion mouse tracking emits one report per cell the pointer crosses, so a single flick can
/// queue dozens. They are drained here and only the last one is folded: hover is a position, not a
/// history, and redrawing once per crossed cell would make a fast sweep feel like lag.
fn poll_event() -> io::Result<Option<Event>> {
    if !event::poll(dash::POLL_INTERVAL)? {
        return Ok(Some(Event::Tick));
    }
    let mapped = match event::read()? {
        CEvent::Key(k) => map_key(k),
        CEvent::Mouse(m) => map_mouse(m),
        CEvent::Resize(width, height) => Some(Event::Resize { width, height }),
        _ => Some(Event::Tick),
    };
    let Some(Event::Mouse(first)) = mapped else {
        return Ok(mapped);
    };
    if first.kind != MouseKind::Moved {
        return Ok(mapped);
    }
    let mut last = first;
    while event::poll(Duration::ZERO)? {
        match event::read()? {
            CEvent::Mouse(m) => match map_mouse(m) {
                Some(Event::Mouse(next)) if next.kind == MouseKind::Moved => last = next,
                // A press or a wheel notch behind the motion run is the real gesture: it wins the
                // poll and the swallowed motion costs nothing, since the pointer's next move
                // re-establishes the hover anyway.
                Some(other) => return Ok(Some(other)),
                None => {}
            },
            CEvent::Key(k) => {
                if let Some(ev) = map_key(k) {
                    return Ok(Some(ev));
                }
            }
            CEvent::Resize(width, height) => return Ok(Some(Event::Resize { width, height })),
            _ => {}
        }
    }
    Ok(Some(Event::Mouse(last)))
}

/// Map a crossterm mouse report onto the core's alphabet: left press, plain motion (hover), and the
/// wheel. Drags, other buttons, and releases are dropped — the surfaces are a list, and a gesture
/// they do not model is better ignored than approximated.
fn map_mouse(m: MouseEvent) -> Option<Event> {
    let kind = match m.kind {
        MouseEventKind::Down(MouseButton::Left) => MouseKind::Down,
        MouseEventKind::Moved => MouseKind::Moved,
        MouseEventKind::ScrollUp => MouseKind::ScrollUp,
        MouseEventKind::ScrollDown => MouseKind::ScrollDown,
        _ => return None,
    };
    Some(Event::Mouse(Mouse {
        kind,
        col: m.column,
        row: m.row,
    }))
}

/// Map a crossterm key press onto the core's `Key` alphabet, keeping the input backend out of the
/// core. Only `Press` events map; ctrl + an unbound char maps to nothing.
fn map_key(k: KeyEvent) -> Option<Event> {
    if k.kind != KeyEventKind::Press {
        return None;
    }
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let key = match k.code {
        KeyCode::Esc => Key::Esc,
        KeyCode::Enter => Key::Enter,
        KeyCode::Tab => Key::Tab,
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Char('c') if ctrl => Key::CtrlC,
        KeyCode::Char('s') if ctrl => Key::CtrlS,
        KeyCode::Char(c) if !ctrl => Key::Char(c),
        _ => return None,
    };
    Some(Event::Key(key))
}

/// This binary's path for a re-invocation, falling back to the bare name for a `$PATH` lookup when
/// the exe is unreadable (the same resolution `tma act --menu` uses to build its own entries).
fn self_exe() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_else(|| "tma".to_string())
}

/// The effect executor seam: run one `Effect`'s I/O and return its follow-up event, if any. The real
/// [`Executor`] performs tmux I/O; the runner tests supply a recording fake so the drain loop runs
/// without a terminal or a tmux server.
trait ExecuteEffect {
    fn execute(&mut self, eff: Effect) -> Option<Event>;
}

/// The shared effect executor: each `Effect` runs its tmux I/O and may feed back one event.
/// Owns the hot-reloaded config + manifests `Refresh` mutates in place.
struct Executor<'a> {
    env: SurfaceEnv<'a>,
    /// Effect failures, held until [`run_surface`] drops the terminal guard. Writing them as they
    /// happen would put them on the alternate screen, where raw mode staircases the line and
    /// ratatui's diffing repaint leaves it stuck over the rows. Deduped, so a config left malformed
    /// (or a pane that keeps refusing a jump) reports once instead of once per poll tick.
    notices: Vec<String>,
}

impl Executor<'_> {
    fn note(&mut self, msg: String) {
        if !self.notices.contains(&msg) {
            self.notices.push(msg);
        }
    }

    /// Run one effect, recording any failure as a notice.
    fn note_failure<T, E: std::fmt::Display>(&mut self, what: &str, r: Result<T, E>) {
        if let Err(err) = r {
            self.note(format!("tma: {what} failed: {err}"));
        }
    }
}

impl ExecuteEffect for Executor<'_> {
    /// Run one effect and return its follow-up event, if any. A pane effect that fails does not stop
    /// the surface, but it is recorded as a notice rather than discarded; `Quit` is the runner's.
    fn execute(&mut self, eff: Effect) -> Option<Event> {
        match eff {
            Effect::Refresh => {
                // Hot-reload all-or-nothing before the cycle, so an edited config takes effect and a
                // broken one is named once instead of keeping the last good pair in silence.
                if let Err(msg) = config::reload_pair(
                    &mut self.env.config,
                    &mut self.env.manifests,
                    self.env.config_path.as_deref(),
                    self.env.manifest_dir.as_deref(),
                ) {
                    self.note(msg);
                }
                Some(
                    match dash::refresh(self.env.tmux, &self.env.config, &self.env.manifests) {
                        Some(mut rows) => {
                            self.env.filter.apply(&mut rows);
                            Event::RowsRefreshed(rows)
                        }
                        None => Event::RefreshFailed,
                    },
                )
            }
            Effect::CapturePreview { pane } => {
                let ansi = ui::capture_preview(self.env.tmux, &pane);
                Some(Event::PreviewCaptured { pane, ansi })
            }
            Effect::Focus(row) => {
                let r = jump::focus_agent(self.env.tmux, &row, self.env.acting_client);
                self.note_failure("jump", r);
                None
            }
            Effect::ClearAttention { pane } => {
                let r = ui::clear_attention(self.env.tmux, &pane);
                self.note_failure(&format!("clearing attention on {pane}"), r);
                None
            }
            // Hand the menu to tmux (`run-shell -b`) rather than spawning it here: the child is then
            // the server's, so it survives this surface and the overlay it opens, and tmux reaps it.
            // The surface keeps its terminal throughout — `display-menu` is a client-side overlay
            // tmux composites over the pane and repaints from its own grid when it closes — so there
            // is nothing to restore and no redraw to force. Non-fatal like the other pane effects.
            Effect::ActMenu { pane } => {
                let command = crate::menu::act_menu_command(&self_exe(), self.env.server, &pane);
                let r = ui::run_shell_background(self.env.tmux, &command);
                self.note_failure("opening the action menu", r);
                None
            }
            Effect::Quit => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_render::row;

    fn pane_ids(rows: &[AgentRow]) -> Vec<&str> {
        rows.iter().map(|r| r.pane_id.as_str()).collect()
    }

    #[test]
    fn row_filter_drops_the_excluded_pane_and_keeps_the_rest() {
        let mut rows = vec![
            row("%1", "work", 0, 0, AgentState::Blocked, 0),
            row("%2", "work", 0, 1, AgentState::Idle, 0),
            row("%3", "home", 1, 0, AgentState::Working, 0),
        ];

        RowFilter::excluding(Some("%2".to_string())).apply(&mut rows);
        assert_eq!(
            pane_ids(&rows),
            ["%1", "%3"],
            "the picker's own pane is dropped, the others stay"
        );

        // No pane to exclude (unresolvable client, or `watch`'s selector-only filter): nothing drops.
        RowFilter::excluding(None).apply(&mut rows);
        RowFilter::from_selector(Selector::default()).apply(&mut rows);
        assert_eq!(pane_ids(&rows), ["%1", "%3"], "no exclusion drops nothing");

        // The only agent is the invoking pane: an honest empty list, not a fallback to everything.
        let mut lone = vec![row("%1", "work", 0, 0, AgentState::Blocked, 0)];
        RowFilter::excluding(Some("%1".to_string())).apply(&mut lone);
        assert!(lone.is_empty(), "self-only lists nothing");
    }

    /// A surface whose `update` returns pre-scripted batches and records the events it folds, so the
    /// drain loop's mechanics are testable without a real model. It never draws.
    struct ScriptSurface {
        seen: Vec<Event>,
        batches: VecDeque<Vec<Effect>>,
    }

    impl Surface for ScriptSurface {
        type Res = ();
        fn update(&mut self, ev: Event, _now: u64, _res: &mut ()) -> Vec<Effect> {
            self.seen.push(ev);
            self.batches.pop_front().unwrap_or_default()
        }
        fn view(&self, _f: &mut Frame, _now: u64, _palette: &RowPalette) {
            unreachable!("the drain tests never draw");
        }
    }

    /// An executor fake: records the effects it runs and hands back scripted follow-ups, standing in
    /// for the tmux-driven [`Executor`].
    struct RecordExec {
        ran: Vec<Effect>,
        follows: VecDeque<Option<Event>>,
    }

    impl ExecuteEffect for RecordExec {
        fn execute(&mut self, eff: Effect) -> Option<Event> {
            self.ran.push(eff);
            self.follows.pop_front().flatten()
        }
    }

    #[test]
    fn drain_runs_effects_inline_and_enqueues_followups() {
        // A non-Quit batch runs inline; its follow-up event is enqueued and folded before draining.
        let mut surface = ScriptSurface {
            seen: Vec::new(),
            batches: VecDeque::from(vec![vec![Effect::Refresh], vec![]]),
        };
        let mut exec = RecordExec {
            ran: Vec::new(),
            follows: VecDeque::from(vec![Some(Event::RowsRefreshed(Vec::new()))]),
        };
        let mut queue = VecDeque::from(vec![Event::Tick]);

        let deferred = drain(&mut surface, &mut (), &mut queue, &mut exec, || 0u64);
        assert!(deferred.is_none(), "no Quit means no deferral");
        assert!(
            matches!(exec.ran.as_slice(), [Effect::Refresh]),
            "the effect ran inline, got {:?}",
            exec.ran
        );
        assert!(
            matches!(
                surface.seen.as_slice(),
                [Event::Tick, Event::RowsRefreshed(_)]
            ),
            "the follow-up was enqueued and folded, got {:?}",
            surface.seen
        );
        assert!(queue.is_empty(), "the queue drains");
    }

    #[test]
    fn drain_defers_the_quit_batch_without_running_it() {
        // The picker's Enter batch is `[Focus, ClearAttention, Quit]`; here a representative
        // `[ClearAttention, Quit]` stands in. Nothing runs during the drain; the whole batch comes
        // back for the caller to run after the guard drops.
        let mut surface = ScriptSurface {
            seen: Vec::new(),
            batches: VecDeque::from(vec![vec![
                Effect::ClearAttention {
                    pane: "%0".to_string(),
                },
                Effect::Quit,
            ]]),
        };
        let mut exec = RecordExec {
            ran: Vec::new(),
            follows: VecDeque::new(),
        };
        let mut queue = VecDeque::from(vec![Event::Key(Key::Enter)]);

        let batch = drain(&mut surface, &mut (), &mut queue, &mut exec, || 0u64)
            .expect("a Quit batch defers");
        assert!(
            batch.iter().any(|e| matches!(e, Effect::Quit)),
            "the deferred batch carries Quit"
        );
        assert!(
            exec.ran.is_empty(),
            "nothing runs before the guard drop, got {:?}",
            exec.ran
        );
        // The runner then runs the deferred batch past the guard drop, in order.
        for eff in batch {
            let _ = exec.execute(eff);
        }
        assert!(
            matches!(
                exec.ran.as_slice(),
                [Effect::ClearAttention { .. }, Effect::Quit]
            ),
            "all deferred effects run after, in order, got {:?}",
            exec.ran
        );
    }

    #[test]
    fn act_menu_runs_inline_and_the_surface_lives_on() {
        // The dashboards' `a` batch carries no Quit: it runs during the drain (the terminal is never
        // handed over) and the loop continues, unlike the picker's Enter batch.
        let mut surface = ScriptSurface {
            seen: Vec::new(),
            batches: VecDeque::from(vec![vec![Effect::ActMenu {
                pane: "%5".to_string(),
            }]]),
        };
        let mut exec = RecordExec {
            ran: Vec::new(),
            follows: VecDeque::from(vec![None]),
        };
        let mut queue = VecDeque::from(vec![Event::Key(Key::Char('a'))]);

        let deferred = drain(&mut surface, &mut (), &mut queue, &mut exec, || 0u64);
        assert!(deferred.is_none(), "no Quit rides with the menu");
        assert!(
            matches!(exec.ran.as_slice(), [Effect::ActMenu { pane }] if pane == "%5"),
            "the menu ran inline for the selected pane, got {:?}",
            exec.ran
        );
    }

    /// A failing pane effect leaves a notice rather than vanishing: without it the surface closes,
    /// nothing moves, and the exit code is still 0. Repeats fold into the one line, since the
    /// notices flush after a run that may have ticked hundreds of times.
    #[test]
    fn a_failed_pane_effect_is_recorded_once() {
        // A socket path with no server behind it: every tmux call fails, whatever the failure mode.
        let server = Server {
            socket_path: Some(PathBuf::from("/nonexistent/tma-runner-test.sock")),
            ..Server::default()
        };
        let tmux = Tmux::connect(&server);
        let mut exec = Executor {
            env: SurfaceEnv {
                tmux: &tmux,
                server: &server,
                config: config::Config::default(),
                manifests: Vec::new(),
                config_path: None,
                manifest_dir: None,
                acting_client: None,
                filter: RowFilter::excluding(None),
            },
            notices: Vec::new(),
        };

        for _ in 0..3 {
            assert!(exec
                .execute(Effect::ClearAttention {
                    pane: "%1".to_string()
                })
                .is_none());
        }
        assert_eq!(exec.notices.len(), 1, "three failures, one line");
        assert!(
            exec.notices[0].starts_with("tma: clearing attention on %1 failed: "),
            "the notice names the effect and carries the tmux error, got {:?}",
            exec.notices[0]
        );
    }

    #[test]
    fn seeded_queue_makes_the_first_fold_a_resize() {
        // The synthetic Resize is the first event the fold sees.
        let mut surface = ScriptSurface {
            seen: Vec::new(),
            batches: VecDeque::new(),
        };
        let mut exec = RecordExec {
            ran: Vec::new(),
            follows: VecDeque::new(),
        };
        let mut queue = seeded_queue(100, 40);
        assert!(
            matches!(
                queue.front(),
                Some(Event::Resize {
                    width: 100,
                    height: 40
                })
            ),
            "the seed is a Resize with the terminal dimensions"
        );

        drain(&mut surface, &mut (), &mut queue, &mut exec, || 0u64);
        assert!(
            matches!(
                surface.seen.as_slice(),
                [Event::Resize {
                    width: 100,
                    height: 40
                }]
            ),
            "the Resize is the first (and only) event folded, got {:?}",
            surface.seen
        );
    }

    /// The gestures the folds model are mapped; everything else is dropped here rather than
    /// approximated into one of them (a drag is not a click, and a right-press is not a left one).
    #[test]
    fn map_mouse_carries_press_hover_and_wheel_and_drops_the_rest() {
        let ev = |kind| MouseEvent {
            kind,
            column: 7,
            row: 3,
            modifiers: KeyModifiers::NONE,
        };
        assert!(
            matches!(
                map_mouse(ev(MouseEventKind::Down(MouseButton::Left))),
                Some(Event::Mouse(Mouse {
                    kind: MouseKind::Down,
                    col: 7,
                    row: 3
                }))
            ),
            "a left press carries its own cell"
        );
        for (kind, want) in [
            (MouseEventKind::Moved, MouseKind::Moved),
            (MouseEventKind::ScrollUp, MouseKind::ScrollUp),
            (MouseEventKind::ScrollDown, MouseKind::ScrollDown),
        ] {
            assert!(
                matches!(map_mouse(ev(kind)), Some(Event::Mouse(m)) if m.kind == want),
                "{kind:?} maps to {want:?}"
            );
        }
        for kind in [
            MouseEventKind::Down(MouseButton::Right),
            MouseEventKind::Up(MouseButton::Left),
            MouseEventKind::Drag(MouseButton::Left),
            MouseEventKind::ScrollLeft,
        ] {
            assert!(map_mouse(ev(kind)).is_none(), "{kind:?} is not modelled");
        }
    }
}
