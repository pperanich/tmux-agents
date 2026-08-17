# TEA core extraction for tma-ui

Status: reviewed 2026-07-30 (batch 0 gate); normative.
Date: 2026-07-30
Inputs: the two live TUI surfaces (`crates/tma-ui/src/watch.rs`,
`crates/tma-ui/src/picker.rs`), the shared `dash.rs` helpers, and `tma-core`'s
no-clock convention. Method matches ACTIONS.md: each decision names the options
considered, the pick, and the condition that reopens it.

The design record is normative for the `tma-ui-core` extraction: where it and the
implementation disagree, the disagreement is a review finding.

## TEA1: Motivation

Status: reviewed 2026-07-30 (batch 0 gate); normative.

`tma watch` (`watch.rs`, 723 lines) and the fuzzy picker (`picker.rs`, 589
lines) are hand-rolled single-threaded loops that interleave three concerns in
one function body: crossterm input reads, model mutation, and tmux side
effects. Two costs follow.

- **Untestable loop logic.** Selection reanchoring, refresh gating, preview
  caching, and the digit quick-select rules cannot run without a live terminal
  and a tmux server, so none of them has a unit test. The behavior that decides
  whether `j`/`k` follows a pane across a refresh reorder, or whether the
  preview drops when a resize crosses the 76-column threshold, is exercised only
  by hand.
- **Duplicated shape.** The two loops are the same skeleton (seed, draw, poll,
  refresh-on-deadline) written twice, and they have already drifted. The picker
  has no SIGUSR1 nudge path (`picker.rs:231` gates refresh on the timer alone),
  while watch drains `nudge::take_nudge()` every poll tick (`watch.rs:230`). The
  picker nudge gap is a missing feature that falls out of the duplication, not a
  deliberate difference.

Fix: extract an Elm-style pure core into a new crate `tma-ui-core`, keep the
existing synchronous loop (no tokio, no channels), and collapse both surfaces
onto one generic shell runner in `tma-ui`. The picker gains nudge support as a
side effect of sharing the core's refresh gate.

**Revisit if** a third surface appears that is not a refresh-and-select loop
(the out-of-scope list in TEA8 names the current non-loops).

## TEA2: The pure core contract

Status: reviewed 2026-07-30 (batch 0 gate); normative.

The core is one function per surface:

```
update(&mut Model, Event, now: u64, res: &mut Res) -> Vec<Effect>
```

- **`&mut Model`.** The surface state, mutated in place. Both models derive
  `Debug` so an event-script test can assert a model projection after a fed
  event.
- **`Event`.** The single input variant (TEA3). Input, time, signals, and
  effect results all arrive as events; the core has no other entry point.
- **`now: u64`.** Epoch milliseconds, injected by the shell. The core reads no
  clock. The shell's helper is `unix_now` (`picker.rs:373`), a thin alias for
  `tma_runtime::now_ms` (`lib.rs:36`); the unit is milliseconds and
  `RefreshGate` (TEA5) keeps the same unit. This mirrors `tma-core`'s no-clock
  rule, where time is always a parameter and never read inside the pure fold.
- **`res: &mut Res`.** A per-surface associated resource for scratch state that
  cannot live in a `Debug` model. `PickerModel::Res = nucleo::Matcher`;
  `WatchModel::Res = ()`. See TEA5.
- **`-> Vec<Effect>`.** The only way the core requests I/O (TEA4). The core
  performs none itself; it returns a batch the executor runs.

Quit is an effect (`Effect::Quit`), not a `should_quit` flag on the model. This
gives tests one uniform assertion surface (assert the effect vector), and the
runner keeps a local `quit` bool it sets when it executes the effect.

Width is model state, not a per-frame `terminal.size()` read. `WatchModel`
carries `width` and `last_layout`, seeded by the initial Resize event (TEA6),
so the threshold-cross cache drop becomes assertable without a terminal.

**Revisit if** an effect result needs to carry back richer failure detail than
the current `RefreshFailed` marker (TEA3); today the surfaces only distinguish
success from stale-keep.

## TEA3: Key, Event, and Effect

Status: reviewed 2026-07-30 (batch 0 gate); normative.

`Key` is the core's own input alphabet; the shell maps crossterm `KeyEvent`
values onto it, so the core never depends on crossterm (TEA4).

```rust
pub enum Key {
    Up,
    Down,
    Enter,
    Esc,
    Backspace,
    Char(char),
    CtrlC,
    CtrlS,
}
```

`Event` and `Effect`, verbatim:

```rust
pub enum Event {
    Key(Key),
    Tick,                                   // time may have advanced; now is an update param
    Nudge,                                  // SIGUSR1, drained by the shell
    Resize { width: u16, height: u16 },
    RowsRefreshed(Vec<AgentRow>),
    RefreshFailed,                          // dash::refresh returned None; keep stale rows
    PreviewCaptured { pane: String, ansi: String },
}

pub enum Effect {
    Refresh,                                // executor: dash::refresh -> RowsRefreshed | RefreshFailed
    CapturePreview { pane: String },        // executor: ui::capture_preview -> PreviewCaptured
    Focus(Box<AgentRow>),                   // executor: jump::focus_agent (boxed, AgentRow is wide)
    ClearAttention { pane: String },        // executor: tmux.unset_pane_option(pane, ATTENTION)
    Quit,
}
```

`Focus` boxes its `AgentRow` because the row is wide and would otherwise bloat
every `Effect` variant. `RefreshFailed` is a distinct event, not an empty
`RowsRefreshed`: a failed cycle keeps the stale rows, and the core must be able
to tell the two apart to leave the model untouched.

**Revisit if** the executor mapping (TEA7) needs an effect that is not one of
these five; each new effect is a new pure/impure boundary crossing and gets
named here first.

## TEA4: Crate decision and honest enforcement

Status: reviewed 2026-07-30 (batch 0 gate); normative.

The core lives in a new crate, `crates/tma-ui-core`. Deps: `tma-core`,
`tma-runtime`, `ratatui`, `nucleo`, all `workspace = true`. No `crossterm`, no
`tma-tmux`. Cargo.toml follows the workspace-inheritance plus per-dep
rationale-comment convention (model: `crates/tma-runtime/Cargo.toml`).

The enforcement is partial, and the doc states exactly how partial:

- **Compiler-enforced: no crossterm.** The crate does not depend on crossterm,
  so no `update` can read a `KeyEvent`, poll input, or touch the terminal. The
  shell owns that boundary and hands the core a mapped `Key`.
- **Not compiler-enforced: no tmux.** `AgentRow` and `PickerStyles` live in
  `tma-runtime`, which also exports `Tmux` and every effect function. The crate
  boundary therefore cannot forbid a tmux call; it can only forbid crossterm.
  No-tmux purity is held by two non-compiler means:
  - **Signature discipline.** `update` never receives `&Tmux`. Effects are
    requests, executed by the shell; the core cannot perform tmux I/O because it
    holds no handle to a server.
  - **A grep gate**, runnable locally:

    ```
    ! grep -rnE 'Tmux|crossterm|tma_runtime::ui|run_cycle|stamp_rows' crates/tma-ui-core/src
    ```

This is the same shape as `tma-core`'s no-clock rule: the type system does not
forbid reading a clock, so the convention forbids it and a reviewer (or gate)
holds the line. Naming the gap is the point; a reader must not assume the crate
split buys more isolation than it does.

**Revisit if** `tma-runtime` splits its row/style types out from its effect
functions into a leaf crate, which would let the boundary compiler-enforce
no-tmux and retire the grep gate's tmux arm.

*Exercised 2026-07-30 (cleanup batch 5):* `AgentRow`/`sort_rank` moved to
`tma_core::row` and `PickerStyles`' colour mapping into `tma_ui_core::palette`
(`RowPalette`), so the `tma-ui-core → tma-runtime` edge is deleted; no-tmux is
now compiler-enforced and the grep gate is retired.

## TEA5: Two models over shared components

Status: reviewed 2026-07-30 (batch 0 gate); normative.

There are two models, not one god-model behind a `Surface` enum:

```
PickerModel { all, query, scope, sel, visible, gate, preview }
WatchModel  { rows, sel, pref: WidePref, width, last_layout, gate, preview }
```

The overlap between the surfaces is components, which is exactly what today's
code already expresses through shared `dash.rs` helpers. Three components move
into the core and are shared by value, not by inheritance:

- **`Selection`** (`selection.rs`) moves verbatim from `dash.rs:66-98`. It is
  already pure and tested: `move_by` wraps, `clamp` pins to range, `reanchor`
  re-seeks the previously-highlighted pane in the new display order so the
  selection follows a pane across a refresh reorder.
- **`RefreshGate`** (`refresh_gate.rs`) is new: `RefreshGate { last, interval_ms }`
  with `tick(now) -> bool` and `force(now)`. It owns the 1-second deadline that
  both loops open-code today. `Event::Nudge` calls `force`, which is how the
  picker gains nudge support for free (TEA1).
- **`PreviewCache`** (`preview.rs`): `PreviewCache { text: Text<'static>, for_pane: Option<String> }`.
  Recapture gating moves out of both loops. Note the surfaces' refresh behavior
  and encode it, do not assume it: both watch (`watch.rs:244`) and the picker
  (`picker.rs:242`) reset the preview target to `None` on a successful refresh,
  even though the watch comment (`watch.rs:241-243`) describes the picker as
  selection-change-only. The comment and the picker code disagree; the code is
  the contract, so the actual reset-on-refresh behavior is pinned by a test
  before the port.

`nucleo::Matcher` stays out of the Model. It is non-`Debug` and holds scratch
buffers, so putting it in the model would break the `Debug`-projection test
pattern. It is the per-surface associated resource instead:
`PickerModel::Res = Matcher`, `WatchModel::Res = ()`. `compute_visible` and
`quick_select` take `&mut nucleo::Matcher`.

`ansi.rs` (408 lines, pure ratatui) moves wholesale from `tma-ui`. `update`
converts a `PreviewCaptured.ansi` payload via `ansi_to_text` and caches the
result; the conversion is pure and belongs beside the cache.

**Revisit if** a Surface enum ever earns its keep (for example a runner that
must hold a heterogeneous list of live surfaces), which none of the current
call sites needs.

## TEA6: Shell runner invariants

Status: reviewed 2026-07-30 (batch 0 gate); normative.

A new `crates/tma-ui/src/runner.rs` holds one generic `run_surface<S>`
replacing both loops. Per iteration: drain the event queue through `update`,
execute the returned effects (each may push a follow-up event), draw
`view(f, &Model, &Config)`, then `event::poll(POLL_INTERVAL)` mapping crossterm
to `Key`/`Resize` (else `Tick`), and drain `nudge::take_nudge()` into
`Event::Nudge` for both surfaces. The drain is only the receiving leg: SIGUSR1
delivery also needs the shell to install the handler and advertise its pid via
the guard (`@tma_watch_pid`); each surface's shell owns that wiring.

Three invariants are load-bearing and pinned here so no implementer reinvents
them:

1. **Deferred-focus rule.** Effects batched with `Effect::Quit` (`Focus`,
   `ClearAttention`) execute after the `TerminalGuard` drops; all other effects
   execute inline. This reproduces both current behaviors exactly:
   - Picker Enter yields `[Focus, ClearAttention, Quit]`, which runs on a
     restored terminal (`picker.rs:253-257` today: the guard drops, then the
     jump fires).
   - Watch Enter yields `[Focus, ClearAttention]` with no `Quit`, which runs
     inline and the loop continues (`watch.rs:207-215`: jump keeps the watcher
     open, non-modal).

   The presence of `Quit` in the batch is the signal to defer; the runner does
   not special-case surface identity.
2. **Initial Resize seed.** crossterm emits `Resize` only on change, so the
   runner injects one synthetic `Resize { width, height }` as the first event,
   before the first draw. Without it, `WatchModel.width` is unset on frame one
   and the first paint picks the wrong layout. Covered by a shell smoke check.
3. **First frame from stamps.** The shell seeds the model from
   `cycle::stamp_rows` before entering the loop (`watch.rs:129`, picker
   equivalent at `picker.rs:156`). This is instant and stale-tolerant; the next
   refresh runs a full cycle. The seed stays shell-side because `stamp_rows` is
   a tmux read.

**Revisit if** the poll model changes (for example a real event source instead
of the `POLL_INTERVAL` timer), which would move the `Tick`/`Resize` synthesis
but not the three invariants.

## TEA7: Executor mapping and the feedback chain

Status: reviewed 2026-07-30 (batch 0 gate); normative.

A single shared executor function in `tma-ui` maps each `Effect` to its I/O and
returns any follow-up event:

| Effect | I/O | Result event |
|---|---|---|
| `Refresh` | `dash::refresh` (config/manifests stay shell-owned; `reload_pair` keeps mutating them in place) | `RowsRefreshed(rows)` on `Some`, `RefreshFailed` on `None` |
| `CapturePreview { pane }` | `ui::capture_preview` | `PreviewCaptured { pane, ansi }` |
| `Focus(row)` | `jump::focus_agent`, best-effort | none |
| `ClearAttention { pane }` | `tmux.unset_pane_option(pane, ATTENTION)`, best-effort | none |
| `Quit` | set the runner's local `quit` bool | none |

`Focus` and `ClearAttention` are best-effort (errors ignored) exactly as the
loops treat them today (`watch.rs:209-213`, `picker.rs:255-256`).

The feedback chain (effect producing an event producing an effect) is acyclic
and bounded:

```
Refresh -> RowsRefreshed -> (CapturePreview) -> PreviewCaptured -> done
```

A refresh can yield fresh rows, which the core may answer with a single
`CapturePreview`, which yields one `PreviewCaptured`, which terminates. No step
loops back to `Refresh`, so one iteration drains in bounded steps and the queue
cannot grow without an external event (a key, a tick, or a nudge).

**Revisit if** an effect grows a result that the core answers with the same
effect kind, which would break the acyclic property and need an explicit depth
bound.

## TEA8: Out of scope

Status: reviewed 2026-07-30 (batch 0 gate); normative.

The extraction touches the two refresh-and-select loops and nothing else. These
stay where they are:

- **`jump.rs`.** A single-shot CLI verb (focus a pane and exit), not a loop.
- **`menu.rs`.** Resolves via a detached `run-shell` re-invocation, not a loop.
- **`surfaces.rs`.** Pure render helpers already; no loop, no state.
- **`term.rs`.** The `TerminalGuard` and watch's `@tma_watch_pid` advertisement
  stay shell-side, untouched. The runner drops the guard (TEA6 invariant 1) but
  does not move it into the core.

**Revisit if** any of these acquires a refresh loop, at which point it becomes a
candidate surface for the same core.
