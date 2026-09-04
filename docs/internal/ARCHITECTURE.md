# tma architecture plan

Status: draft v1
Date: 2026-07-20
Inputs: REQUIREMENTS.md (cited by ID), DAEMON.md, prior-art review of
tmux-agent (`ta`) and tmux-agent-sidebar (both MIT — ideas and code borrowable with
attribution; herdr remains clean-room), and the product-requirements draft this
document was written against. That draft is retired: its problem framing, prior-art
comparison, and the tmux-as-the-state-store argument now live in
`docs/explanation/why-tma.md`, and its option table and phasing were superseded here.
Mentions of "the PRD" below cite the retired draft.

Method: this document records architectural decisions (AD1–AD9) as explored choices,
not inherited ones. Each lists the options considered, the pick, and the condition
that would reopen it. Prior art is treated as evidence about what works, not as a
default answer — both reference projects made structural choices we reject below.

## System shape

```
            evidence sources                     core                    store              surfaces
┌──────────────────────────────────┐   ┌──────────────────────┐   ┌───────────────┐   ┌─────────────────┐
│ agent hooks ──► tma event        │   │  identity engine     │   │ tmux user     │   │ tma (picker)    │
│ tmux control mode (daemon)       ├──►│  evidence log        ├──►│ options       ├──►│ tma status      │
│ capture-pane (on demand)         │   │  state engine        │   │ (@agent_*)    │   │ tma watch       │
│ process tree (ps / sysinfo)      │   │  (pure, manifest-    │   │               │   │ tma jump / ls   │
│ pane title / activity / flags    │   │   driven)            │   │ daemon memory │   │ user tmux conf  │
└──────────────────────────────────┘   └──────────────────────┘   │ (history only)│   └─────────────────┘
                                                                  └───────────────┘
```

Invariants the shape enforces:

- The core is pure (D7): snapshot + evidence in, verdict out. All I/O lives in edges.
- tmux user options are the only shared store (AD4). Surfaces are dumb readers.
- Every evidence source is optional; the system degrades by losing latency or
  coverage, never by breaking (goal 5, DAEMON.md tiers).

## AD1 — State model: small closed core + open detail dimension

**Question.** What does `@agent_state` enumerate? Prior art disagrees: `ta` has seven
states (Working/Waiting/Done/Idle/RateLimited/Error/Unknown), tmux-agent-sidebar six
(Running/Background/Waiting/Idle/Error/Unknown), our PRD four.

**Analysis.** Both prior enums conflate orthogonal axes. `RateLimited` and `Error` are
*reasons*, not states; `Done` is a transition marker (it means "idle and you haven't
looked yet" — ta even auto-clears it on focus); `Background` is agent-specific detail
("turn over, background shell alive"). Widening the enum couples the public grammar
(F14: frozen once released) to per-agent UI details, and every consumer conditional
(`window-status-format` matchers, scripts) must handle more cases.

The stable question consumers actually ask is *whose move is it*: the human's
(`blocked`), the agent's (`working`), nobody's (`idle`), or unreadable (`unknown`).
Everything else is qualification or presentation.

**Decision.** Three orthogonal published dimensions:

| option | vocabulary | semantics |
|---|---|---|
| `@agent_state` | `idle` `working` `blocked` `unknown` — closed, frozen (F14) | whose move is it |
| `@agent_detail` | `permission` `question` `error` `rate_limit` `background` `compacting` … — open, additive | why / qualification; empty allowed |
| `@agent_attention` | set on a noteworthy transition, cleared on pane focus or on your next real terminal input at the pane | "unseen since it happened" |

Mappings are **normative, not per-manifest** (adversarial review: manifest-decided
mappings make `@agent_state` semantics agent-dependent, gutting the closed-vocabulary
promise — `tma jump --blocked` must mean the same thing for every agent). Fixed rules:
sidebar's `Waiting` ⇒ `blocked/permission` or `blocked/question`; `Error` ⇒
`blocked/error` (a halted agent needs a human); ta's `Done` ⇒ `idle` + attention flag;
`Background` ⇒ `idle/background`; rate limiting ⇒ `working/rate_limit` always — a
rate-limited agent auto-resumes and the ball is with the provider, not the human; an
agent that *halts* on rate limit surfaces its confirmation prompt, which is detected
as `blocked/permission` on its own evidence. Manifests map their agent's events and
screens *into* this table; they do not define it.

`@agent_detail` vocabulary is **explicitly unstable until 1.0** (F14 covers
`@agent_state` from first release; detail tokens may still change). Consumers are
warned against glob-matching detail in tmux conditionals until then.

The attention flag is presentation state only — set on noteworthy transition, cleared
by navigation (the pane arrived at, and the pane departed) or by input ordered after
the raise (below). It is **not** the notification-episode marker; that is the separate
`@agent_notified_at` stamp (AD4, F22), because focus-then-leave-unanswered clears
attention while the blocked episode continues.

"Noteworthy transition" is an edge over states everywhere but one place. A turn end is
not an edge: an agent that finishes twice with nothing visibly happening in between
draws `idle`→`idle`, and the fold — which sees only states — cannot separate that from a
pane sitting still, so any rule it could express would re-raise on every poll of a quiet
idle pane and make the mark unclearable. The hook knows, and the hook is where it is
decided: a `[[hooks.map]]` entry marked `turn_end` raises the mark whatever the previous
state was, and stamps `@agent_turn_at` (the episode instant the notify dedup and
`wait --since` read, since `@agent_since` is write-once per state run and cannot move).
It records only when the mark was DOWN, which is what keeps one turn end reported on two
channels at one raise. Screen rules never carry `turn_end`: idle chrome co-renders
mid-turn for most agents, so it is evidence of idleness, never of a turn having ended.

Clearing mechanics, corrected by round-2 empirical review:

- Hooks used are `after-select-pane` / `session-window-changed`, **not**
  `pane-focus-in`: focus hooks are gated on `focus-events`, which
  defaults *off*, so a `pane-focus-in` auto-clear silently never fires on a default
  config. An optional `[focus]` / `events = true` config enables the focus-hook variant
  for completeness, with its side effect documented (`focus-events on` changes
  escape-sequence behavior for every application in the session and requires client
  reattach).
- Hook commands bind their pane via `#{pane_id}` format expansion, never
  `$TMUX_PANE` — `run-shell` inherits the server's *startup* environment, so
  `$TMUX_PANE` there is stale or foreign (verified). Not `#{hook_pane}` either,
  which is what every release up to 0.3.6 used: tmux populates it only on the
  notify_pane-style hooks and expands it EMPTY on the hooks tma installs, so the
  always-on pair cleared nothing at all (fixed in `ef12d02`).
- **Seen-on-leave.** The clear runs on the pane departed as well as the pane
  arrived at, because every arrival-only path leaves the commonest residue
  standing: finish while you watch, move to another window, and the flag survives
  on the pane you were just looking at. The departed pane is resolved from the
  hook's own kind — `#{P:#{?pane_last,#{pane_id},}}` at `after-select-pane`,
  `#{W:#{?window_last_flag,#{P:#{?pane_active,#{pane_id},}},}}` at
  `session-window-changed`, both scoped with `display-message -t <arrival pane>`
  since an untargeted query answers for whichever session tmux calls best, which
  from our side is arbitrary. Formats, not target aliases: `-t '{last}'` is not
  reliable at hook time. Walk-away survives *structurally* rather than by any
  threshold — walking away means not navigating, so no hook fires. Resolving both
  formats on either hook would break that, by clearing the previous window's
  active pane on every ordinary pane switch.
- **Why the window half is a notification hook.** `after-select-window` was the
  obvious name and it is wrong: tmux runs it even for a `select-window` onto the
  window you are already in, where `window_last_flag` still names a window left
  long ago — so the departure clear landed on a pane the user had not seen since
  (`tma jump` to a pane in your own window, `prefix <N>` onto the current window,
  `choose-tree` onto it). Nothing in the hook-time format vocabulary says "the
  current window really changed"; tmux updates `lastw` only on a genuine switch
  and returns early otherwise, leaving `window_last_flag` at `0` on the arrival
  window either way. `session-window-changed` is emitted only for a real change,
  and additionally covers changes `select-window` never sees (leaving a window by
  creating a new one). Verified on 3.6a, detached and with a pty client driving
  `prefix <N>`. The name is retired, not merely dropped: `install-hooks` removes
  tma's `after-select-window` entry, and the binary no longer maps that name to a
  departure, so a hook string left on a server can only clear the arrival pane.
  The residual over-clear MOVED rather than vanished, and that is worth stating:
  `session-window-changed` fires for any real window change in ANY session,
  attached or not, so a non-`-d` `new-window` or an `attach -t sess:win` against a
  background session clears that session's departed pane with nobody having
  navigated. Narrower than what it replaced — `-d` is the scripting default and
  fires nothing, a background `select-window` over-cleared before this change too,
  and the pane cleared is one the user genuinely last had current in that session.
  A guaranteed every-jump over-clear traded for a rare one: take it.
  `Tmux::focus` also skips `select-window` when the destination window is already
  its session's current one (`#{window_active}` is per session), so a jump that
  moves nothing fires nobody's hook.
- **The session departure that stays open, deliberately.** Departing a pane clears
  and departing a window clears; departing a whole SESSION does not, and that is a
  decision rather than an omission. `switch-client` fires `client-session-changed`,
  and additionally `session-window-changed` when it also changes the TARGET
  session's current window (`switch-client -t s2:1`); the bare `-t <session>` form
  fires only the first. `client-session-changed` fires exactly once per switch in
  the ARRIVAL session's context, and the pane the departed session was showing IS
  resolvable there in one format:
  `#{S:#{?#{==:#{session_name},#{client_last_session}},#{W:#{?window_active,#{P:#{?pane_active,#{pane_id},}},}},}}`.
  That is not the problem. The problem is that tmux notifies **outside** the test
  for a real change: `server_client_set_session` updates `last_session` only when
  `c->session != s`, then calls `notify_client` unconditionally — identical in the
  3.2 source, so this holds across the whole supported range. A
  `switch-client -t <the session you are already on>` therefore fires the hook with
  a `client_last_session` naming a session left however long ago, which is the
  retired `after-select-window` defect exactly, one scope up, and no hook-time
  format says "the session really changed". Measured with the departure clear
  wired up for real (the
  live binary, the live hook string): a no-op `switch-client -t s1` cleared the
  done mark on the current pane of `s2`. **tma is itself the loudest producer of
  that no-op** — `Tmux::focus` runs `switch-client` unconditionally, so every jump
  that stays inside the current session fires it, and the same probe showed a
  cross-window jump inside `s1` clearing `s2`'s mark. The trade is one-sided: the
  residue left open is a mark standing on a session you walked away from, which the
  input clear takes down the moment you return and type and which `prefix-j` acts
  on correctly meanwhile; the residue a fix would introduce is a silently destroyed
  record of a completion nobody ever saw. So the name maps to no departure
  (`DepartureKind::from_hook_name`), and a hook string wired onto it by hand
  degrades to the arrival clear —
  `a_hand_wired_session_hook_can_only_clear_the_pane_you_arrived_at` pins that with
  a real PTY client, and fails if the name is ever mapped. Two further reasons not
  to revisit it lightly: a second client on the departed session is still LOOKING at
  that pane when the first client leaves, and the semantic case is weak in the first
  place — the mark means "finished, unreviewed", and walking out of a session is the
  clearest case of not having reviewed it. What a future attempt would need is a
  per-client memo of the last-seen session (`#{session_id}`, rename-stable), which
  tmux gives no per-client option scope for, whose first fire after install is
  always unknown, and whose key (`client_name` is a tty path) is reused by the next
  client on that terminal.
- **There IS a second hook, and it is `pane-focus-out` — refused on measured
  grounds, not for want of a mechanism.** The record used to say
  `client-session-changed` was the only notification a session change fires. It is
  not, and a maintainer running the obvious probe falsifies that in seconds, so the
  claim is gone. On tmux 3.6a with `focus-events` at its default OFF,
  `pane-focus-out` fires on exactly a genuine session switch — both directions, key
  driven and out of band — and hands over the departed pane directly in
  `#{pane_id}`: no nested format, no `client_last_session`, and nothing at all on a
  no-op `select-pane`, a no-op `select-window`, or a no-op `switch-client`. It is
  cleaner at the point of use than the format above. Four measurements decide
  against it anyway, all on an isolated 3.6a socket with a real PTY client:
  1. **It is suppressed by any other viewer, including tma's own daemon.**
     `window_pane_update_focus` (3.6a `window.c:481`) notifies only when NO attached,
     focused client still has that window current. A control-mode client counts as
     one (E2), and the daemon parks one on every monitored session — so the session
     departure clear would do nothing at all for daemon users while the pane and
     window clears kept working. A departure rule that exists only when the daemon
     is off cannot be written down, and it would make the daemon subtractive.
     Guarded by `a_control_mode_client_suppresses_the_session_departure_focus_out`.
     (The same suppression is a genuine safety property in the two-real-client case:
     the first client leaving does not clear a pane the second is still displaying,
     which is more than the `client-session-changed` construction could say.)
  2. **The same edge fires on a clean `detach-client`**, clearing the pane the
     departing client was showing — the end-of-day flow the done mark exists for. A
     client that is KILLED instead (a dropped ssh connection) fires nothing —
     `server_client_lost` drops the client without going through
     `server_client_set_session`, measured by SIGKILLing a PTY client — so the behaviour
     would differ between closing your terminal and losing your link. Nothing at
     hook time separates a detach from a switch: both are one
     `server_client_set_session`. Guarded by
     `a_hand_wired_focus_out_hook_clears_a_pane_you_only_detached_from`.
  3. **Every overlay fires it too, ungated by `focus-events`**
     (`server_client_set_overlay` → `window_update_focus`): `display-menu`,
     `display-popup`, `display-panes`. tma's own `prefix-a` picker is a
     `display-popup`, and `jump --menu` / `act --menu` are `display-menu`, so
     opening the surface that lists your done marks would clear the one on the pane
     you are sitting on. This is the weakest of the four — the keystroke that opened
     the overlay moves `client_activity`, so batch D takes the same mark down within
     a cycle anyway — but it is a second, faster, unconditional path to the same
     loss, and it reaches a script-opened popup that no input preceded.
  4. **It does not exist below tmux 3.3.** In 3.2 (`server-client.c:1368`) the focus
     check runs from the server loop behind `if (focus)`, so with the default option
     the notification is never emitted; 3.3 moved focus to event call sites
     ("Change focus to be driven by events rather than scanning panes", CHANGES).
     tma supports 3.2, so the clear would be present or absent by tmux version.

  With `focus-events on` it additionally fires on every pane and window switch,
  double-clearing what `after-select-pane` and `session-window-changed` already
  handle, and on a terminal focus loss — that last one is not new, since R-D
  finding 3 established the focus-report bytes already move `client_activity` and
  clear through batch D. Nested tmux propagates it only when the INNER server has
  `focus-events on` (measured both ways).

  Refusing the NAME in `DepartureKind::from_hook_name` would buy nothing here,
  unlike every other refusal in that function: the hook hands over the departed
  pane as `#{pane_id}`, so the plain arrival clear would clear it with no kind
  involved. The refusal lives at the install set instead, guarded by
  `pane_focus_out_is_not_a_hook_tma_installs`.
- The hook kind travels in the `TMA_HOOK_KIND` **environment variable**, never as
  an argv flag, which the late binding below forces: a hook string written by a
  new install routinely invokes an older binary, where an unrecognized flag would
  make clap error on every single pane switch. An unrecognized environment
  variable is ignored in silence. Both branches of the command end
  `2>/dev/null || true` for the same reason.
- Installation appends with **unindexed** `set-hook -ga` (tmux picks the next free
  index — verified safe), then records the actual index by re-reading `show-hooks`.
  Explicitly-indexed writes silently overwrite whatever occupies that index, and
  tmux has no reservation concept, so the only indexed write is the drift rewrite:
  an entry that is ours but no longer matches the command this build renders is
  replaced at *its own* recorded index, keeping the record valid. Ownership
  (`clear-attention` substring, what uninstall may remove) and currency (equal to
  the freshly rendered command, modulo tmux's re-quoting) are separate questions.
- The hook command late-binds the binary — install-time absolute path when it is
  still executable, else `tma` off `$PATH`, the `tma-hook` wrapper's own order — so
  a moved or rebuilt binary degrades to the PATH copy instead of dying. The
  statusline context shim resolves the same way.
- Known hazard, detected not prevented: a user's own unindexed `set-hook -g` (the
  normal tmux.conf form) *replaces the whole hook array*, deleting tma's entry on
  every `source-file`; `tma install-hooks --check` detects the disappearance and
  offers reinstall (F30). Hooks are runtime server state, so a restart wipes them
  too: recorded-but-absent-server-wide is reported as its own state, distinct from
  never installed.

- The navigation hooks cannot see the user who never navigates, so the poll cycle
  carries a second, ordered clear: `list-clients` says which pane each client is
  displaying and when that client was last typed into (`#{client_activity}`, epoch
  seconds), and the flag comes down iff that input is strictly later than the raise
  instant in `@agent_since`. Ordered rather than windowed on purpose — "typed within
  the last N seconds" would suppress the mark for someone who typed a prompt and left,
  which is the signal's headline use. Cost is one `list-clients` per cycle, and only
  while some pane actually carries the flag. `client_activity` counts every byte a real
  terminal sends, including the focus reports a terminal emits while `focus-events` is
  on, so alt-tabbing away from a marked pane counts as having seen it — consistent with
  the navigation half, where leaving is also seen. Two limits are accepted: a
  control-mode (`-CC`) client's activity clock freezes at attach, so the layer no-ops
  there, and a reader who never types is indistinguishable from an absent one.
- **Control-mode clients are filtered because of tma itself**, not as an iTerm2
  courtesy. The daemon parks one `tmux -C attach-session -t <session>` per monitored
  session, and each of those clients is pinned to that session's current-window active
  pane with an attach-time `client_activity`. A daemon that restarts after a marker goes
  up therefore carries a timestamp postdating every standing `@agent_since`: unfiltered,
  its own presence would clear every flagged pane in every session it watches with no
  human in the room. Verified live (four parked clients, one per session, `cm=1`). The
  reader treats anything but a literal `0` as control mode, so an unreadable field can
  only fail to clear.

Known accepted edge: the ordered clear needs one keystroke, so a user sitting on a
pane in total silence keeps the mark until they touch the keyboard or navigate;
harmless because they are looking at the pane.

**Revisit if** a consumer need appears that detail + attention cannot express without
parsing, or agents converge on a state the triad genuinely lacks.

## AD2 — Evidence model: typed evidence, deterministic fold

**Question.** How do heterogeneous signals (hook events, screen rules, titles, activity
deltas, process facts) combine? (Amended 2026-08-20: activity deltas were removed as a
signal — a viewport-hash change cannot tell agent output from a user-caused repaint. The
question is preserved as asked; the Decision below reflects the amendment.) `ta` hardcodes a priority ladder in one function;
sidebar trusts hooks exclusively; herdr arbitrates screen vs PTY activity.

**Analysis.** A hardcoded ladder is opaque and unextensible (adding an evidence source
means editing arbitration code); hook-only trust fails for hookless agents and for
missed events. But full probabilistic fusion (weights, Bayesian updates) is
overengineering: sources have a natural strict ranking, and D1/D2 demand explainable
verdicts (`tma debug explain` must say *which rule/event decided*).

**Decision.** A typed evidence record and a deterministic fold:

```rust
Evidence {
  source:   HookEvent | ScreenRule | Title,
  claim:    StateClaim { state, detail },        // or lifecycle claims (agent start/end)
  at:       Timestamp,                            // injected, never read from a clock (D7)
  meta:     rule id / hook name / matcher — for explain output
}
```

`verdict(prev_state, evidence_set, config) -> Verdict` is a pure function implementing
the F8 order (hook > blocker chrome > working chrome > idle chrome > hold/unknown) with
per-source freshness windows. As built (`tma-core/src/fold.rs`), the fold also takes a
`SnapshotFacts` parameter alongside the evidence set: process/screen facts (pid,
`foreground_is_agent`, `scrolled`, `history_view`) that gate rules F5/F9/F10/F4 but are
not themselves Evidence records. Decay is **coverage-aware** (adversarial review finding):
a stale hook claim is expired by *process* evidence (pid gone — heals
died-without-SessionEnd) or by screen evidence **only for states the manifest declares
capture-visible** (D14). Screen evidence can never expire a hook claim for a
hook-covered state — a blocked agent sits silent for ten minutes precisely when its
prompt has no matchable chrome, and the fold must not let the reconciliation sweep
flip it to idle. Every verdict carries the winning evidence for explain output (F24).

Critically, evidence provenance is *persisted* (AD4: `@agent_source`,
`@agent_evidence_at`), so the evidence set exists across processes: a one-shot's
inputs are its own fresh capture claims *plus* the stamped prior claim with
its source and timestamp. Without persisted provenance, a stateless producer cannot
rank a hook stamp above its own stale capture verdict and will clobber `blocked` with
`working` — the failure mode that motivated this design revision. This is also why
the fold survives the "just use a hardcoded ladder" simplification: the ladder's
inputs must be reconstructed from the store, and the fold is the one place that logic
lives.

**Do not feed presence back into the fold.** The 2026-08-20 amendment deleted a
viewport-hash delta that was a *guess* at "is the human here", used as state evidence.
The ordered-input clear reintroduces the true version of that question — tmux's own
`client_activity`, which is not a guess — but strictly in PRESENTATION: it retracts
`@agent_attention` and touches no state. The tempting refactor is to give the fold that
better signal ("the user is typing here, so the pane is not really idle"). It is the
same mistake with a better sensor: presence is not evidence about what the agent is
doing, an agent finishes whether or not you are watching, and a fold that reads presence
becomes unreproducible from the persisted record (AD4) because the deciding input was
never stamped.

**Revisit if** two sources genuinely need weighted combination rather than ranking —
no known case yet.

## AD3 — Identity: observation ∪ self-registration

**Question.** What makes a pane an "agent pane"? Sidebar: only hook self-registration
(`@pane_agent` stamped by SessionStart) — hookless agents are invisible. `ta`: only
observation (process walk, content, title) — no enrichment when the agent could have
told it more.

**Decision.** Union, with provenance. An identity record per pane:

- **observed** — process-tree walk finds a known agent binary (F2). Provides
  existence, agent name, pid. Works for every agent, zero setup.
- **registered** — a SessionStart-class hook event claimed the pane (F26, guarded by
  F27). Provides existence plus session id, cwd, task metadata, and marks the pane
  hook-capable (which tells the state engine what capture must still cover, D14).
- **out-of-scope** — a remote-shell foreground (`ssh` / `mosh` / `docker` / `podman`
  / `kubectl`) is marked out-of-scope before the process walk runs, so a stray local
  child cannot flip a remote pane into a false agent (F6,
  `tma-runtime/src/identity.rs` `REMOTE_SHELLS`).

Conflicts resolve by freshness and the F27 ownership guard; a registration with no
observable process after the reconciliation window is cleared (agent killed -9). This
is the project's defining differentiator versus sidebar and costs one `ps` per cycle
in the polling fallback only — hook-driven steady state does no process scanning.

Process walk implementation: single `ps -eo pid,ppid,pgid,comm` parse (portable across
procps/BSD, N11) rather than the `sysinfo` crate `ta` uses — one subprocess per cycle
beats linking a platform-abstraction crate for one query shape. Revisit if `ps`
parsing grows platform warts in practice.

## AD4 — Store: tmux options only; machine grammar; render at the edge

**Question.** Where does shared state live? Sidebar splits it three ways: tmux options
+ in-process runtime state + `/tmp` activity-log files — and needed pruning code,
liveness sweeps, and a documented invariant list to keep them coherent. `ta` stores an
emoji string in the option and reverse-maps it to an enum.

**Decision.**

- tmux user options are the *only* inter-process store. Daemon memory holds only
  transition history (bounded, N4) — data that is daemon-value-add by definition
  (DAEMON.md) and whose loss on restart is acceptable. No files: nothing under `/tmp`
  or XDG except the daemon lock and socket. Exemption: *install-time metadata* (the
  recorded tmux-hook indexes from F30, keyed per server as
  `~/.config/tma/hooks-state-<server>.toml`) — the no-files rule governs detection
  state, not installation records. This kills the
  multi-store staleness class outright and automatically satisfies N8 (nothing
  captured ever touches disk).
- Option values are machine tokens (`blocked`, `permission`, epoch **milliseconds**),
  never glyphs. Glyph/color rendering happens only in surfaces, configurable (F23). ta's
  emoji-as-protocol is the explicit anti-pattern. **Timestamp resolution (normative):**
  every `@agent_*_at` value is epoch *milliseconds*, not seconds. Millisecond resolution is
  what makes two blocked episodes opening in the same wall-clock second distinguishable
  (write-once `@agent_since` differs, so the F22 dedup and the blocker/hook carve-out
  resolve the sub-second races that second resolution folds together — DAEMON.md "Known
  timing limitations"). The tmux `-F` guards do this arithmetic on 13-digit values (`e|<=`,
  `e|<`, `e|/`; verified on 3.6a, no new floor over the existing `set -F` requirement, N10).
  Backward compat: a store still holding pre-migration 10-digit epoch-seconds stamps is
  normalized on read — a nonzero `*_at` below 10^12 is scaled `×1000` (`tma-core::stamp`),
  so the pure fold and every freshness comparison always see one unit across an in-place
  upgrade. `FoldConfig`'s dwell/decay/freshness windows and the AD5 stampede guard stay
  authored and reasoned in *seconds* (the guard buckets `now`/`@tma_last_poll` to seconds
  before comparing); only the stored timestamps and their comparisons are ms.
- Consolidated option schema (supersedes the PRD table). Adversarial review exposed
  the original "any producer writes anything" model as the design's central defect:
  tmux options have no transactions, no CAS, no writer identity, and every value that
  depends on a previous value (since-preservation, dedup, baselines) breaks under
  uncoordinated read-modify-write. The schema now carries provenance, and writes are
  governed by ownership rules:

  The consolidated per-pane/window/server option schema (the `| option | scope |
  semantics |` table) is the user-facing contract in
  [reference/pane-options-and-json.md](../reference/pane-options-and-json.md),
  which is the single source of truth the `tma-core::stamp` drift guard pins for
  the user-readable options. The internal bookkeeping options below are kept out
  of that reference (implementation detail, not a consumer contract) and
  documented here instead:

  | option | scope | semantics |
  |---|---|---|
  | `@agent_hash` | pane | hash of the last captured viewport tail. Its PRESENCE marks the pane as having been captured at least once, which is all `can_reuse_stamp` reads; the value itself is no longer compared against anything (see AD2) |
  | `@tma_setpf_ok` | server | capability-probe cache: `1` when the server supports `set -pF` conditional writes, `0` when it does not (advisory degrade). A server's version is constant for its life, so the probe runs once |
  | `@tma_origin_<client>` | server (keyed by client) | jump-origin trail: a newline-joined, bounded stack (cap 8) of `session:window.pane` locators forward jumps left. Keyed by the sanitized invoking client name plus a short hash of the raw name, so each tmux client's trail stays independent even for punctuation-only-differing names. `--back` pops one entry, `--home` returns to the bottom and clears; written by `tma jump` and the picker |
  | `@tma_title_match_pid` | pane | flicker-stickiness anchor: the agent pid a title-narrowed manifest last matched by `#{pane_title}`. The identity resolver holds the title match while the pane's agent pid is unchanged, and a new pid re-requires a match. Written only for a `title_patterns` manifest, never for a process-only one |
  | `@tma_reg_dead_since` | pane | dead-registration reaper marker: epoch **ms** of the first cycle a hook-registered pane (`@agent_pid == 0`) was seen SHELL-ONLY (its named process gone, no non-shell process under the pane). Once shell-only persists past the reaper threshold the poll cycle clears the registration; any non-shell process reappearing clears the marker |
  | `@agent_model` | pane | best-effort model-name label the file-tail context intake reads from the rollout tail window (ACT9). Never load-bearing for a gauge; it only feeds `tma doctor`'s recognized-model line (a model no `[telemetry.windows]` entry names). Plain-set, cleared on deregister, absent when no model record sat in the tail window |

- **Write-ownership rules** (the coherence mechanism, in place of transactions).
  Round-2 adversarial review showed advisory rules (producer reads `@agent_source`,
  then decides) are still TOCTOU: the read-to-write span is a whole cycle, and hook
  events land *inside* it, correlated with exactly the transitions that matter.
  Enforcement is therefore **server-side conditional writes**: `set-option -pF`
  expands formats in the target pane's context atomically at write time, giving
  CAS-shaped guards with zero new infrastructure. Verified on tmux 3.6a
  (2026-07-20):

  ```
  # capture producer's state write — cannot clobber a hook-sourced stamp:
  set -pF -t %13 @agent_state '#{?#{==:#{@agent_source},hook},#{@agent_state},working}'
  # write-once since — keeps existing value when state already equals the new state,
  # chained BEFORE the state write so once-ness is evaluated in the server:
  set -pF -t %13 @agent_since '#{?#{==:#{@agent_state},working},#{@agent_since},<now>}' \; ...
  ```

  **Normative chain rule (round-3 fix — the flagship hole):** every field in a
  producer's chained write MUST carry the *same suppression condition* as the state
  write. A suppressed state write with unsuppressed companion writes self-destructs
  in one cycle: the since-guard (comparing against the *rejected* new state) bumps
  `@agent_since` mid-episode (re-firing F22), and an unguarded `@agent_source`
  overwrite flips `hook` → `capture`, letting the next cycle's state guard pass and
  clobber after all. Concretely: the producer computes one guard expression
  (`SUPPRESSED = source is hook ∧ my evidence may not override`) and embeds it in
  every `-pF` write of the chain — state, source, evidence_at, detail, since — so
  the whole tuple commits or holds together. The examples above are illustrative;
  this paragraph is the spec.

  Rules, all enforced via such guards, never via producer-side reads:
  1. Hook-sourced state is overwritten by a capture producer only when the
     guard passes: source is not `hook`, OR the evidence is process-level (pid gone),
     OR the hook claim has aged past its freshness window *and* the state is
     capture-visible (AD2 coverage-aware decay). Round-2 carve-out, made precise in
     round 3: **visible blocker chrome overrides a `working`/`idle` hook claim iff
     the stamped `@agent_evidence_at` predates the capture's timestamp** — an
     evidence-ordering comparison, not "immediately" and not "stale". This closes
     the answered-prompt race: capture at T0 sees the prompt, user answers, hook
     stamps `working` at T1 with evidence_at=T1; the capture producer's blocked
     write carries capture-time T0 < T1, so the guard suppresses it — the hook
     claim is *newer evidence* and wins, per F8's core ordering. The guard is
     expressible in `-pF` via timestamp comparison with the producer's capture time
     embedded as a literal (tmux `e|` arithmetic; on the N10 probe list). Blocker
     chrome never needs a decay wait when its evidence genuinely postdates the hook
     claim; coverage gating applies only to flips D1 tolerates. This rule is stated
     identically here, in F8, and in DAEMON.md — any future edit changes all three.
  2. `@agent_since` and `@agent_notified_at` are write-once per episode via the
     guard-before-state chain above. A pid mismatch (walked pid ≠ stamped
     `@agent_pid`) is an episode boundary: same-pane agent replacement between
     cycles resets `since`/`notified_at`/attention in that cycle (F4).
  3. Everything else is last-writer-wins over *deterministic* values (same fold, same
     persisted inputs), which converges.
  4. Writers order stamped fields so `@agent_stamped_at` is written last in the
     chained `tmux` invocation; readers treating `stamped_at` older than `state` as
     in-progress get a documented read-consistency rule instead of torn-tuple
     surprises (chained `set-option`s are sequential in the server, not atomic to
     readers).
  5. **Writes-on-hold**: a producer whose verdict is freeze/suppress (scrolled pane
     F9, history view F10, dwell suppression F12) refreshes `@agent_stamped_at`
     **and `@agent_hash`** — never `@agent_evidence_at`, never state.
     `@agent_evidence_at` means "most recent evidence *consistent with the stamped
     state*"; evidence for a not-yet-published candidate state updates nothing until
     it publishes. (Without this rule, dwell livelocks: fresh contradicting evidence
     would reset its own clock every cycle.) The hash refreshes because it is an
     *observation baseline*, not state-consistent evidence. HISTORICAL RATIONALE: this
     guarded against the next cycle diffing a static screen against the pre-pause
     streaming hash, manufacturing a phantom activity edge and restarting the dwell
     clock (round-3 finding). No cycle diffs two hashes any more (the activity-delta
     source was removed), so the refresh now only keeps the baseline honest for the
     presence check. Harmless, and kept for that; re-derive before relying on it.

  Runtime floor note: `set-option -F` needs probing at the tmux 3.2 floor (N10);
  verified present on 3.6a. If absent, degrade is documented advisory writes with
  the known race, not silent behavior change.
- Stamping is batched: one `tmux` invocation with `;`-chained commands per pane
  (~70 individual spawns per 10-agent cycle otherwise — a predictable cost, not a
  profiling surprise).

## AD5 — Process model: one-shots + `tma event` + optional daemon

Decided in DAEMON.md; summarized here as the standing model. Three tiers, each a strict
upgrade, no consumer-side changes between tiers (goal 5):

1. **Polling floor** — any one-shot self-polls when stamps are stale (per-pane
   `@agent_stamped_at` freshness). Only tier available for hookless agents without a
   daemon. **The floor has no ambient driver of its own**: something must invoke tma
   for stamps to exist. `#(tma status)` in status-right is that driver and the docs
   MUST present it as required for ambient surfaces (window flags, summaries), not
   optional garnish — a config with only the `window-status-format` snippet renders
   nothing. Multiple attached clients each run status jobs; a cheap stampede guard
   (skip the cycle if another producer stamped in the same second — the ms `now` and
   `@tma_last_poll` are bucketed to seconds before comparing, keeping this guard's
   second resolution even though stamps are ms; already specified in DAEMON.md) bounds
   duplicate work.
2. **Hook tier** — `tma event` direct-stamps when no daemon runs, including
   recomputing its window's `@agent_summary` (deterministic rollup, converges);
   resident surfaces advertise pids on their own pane and take SIGUSR1 nudges from
   tmux focus hooks. Event-latency state, instant refresh, still no daemon.
3. **Daemon tier** — event hub (socket + control-mode client + on-demand capture +
   30–60 s reconciliation sweep). Adds fallback-detection scheduling for hookless
   agents, history, notification dispatch/dedup, self-healing.

The picker is a resident surface while open: it runs its own refresh (1 s) regardless
of tier, so it never depends on the daemon for liveness (N3).

## AD6 — tmux interaction: subprocess for one-shots, control mode in the daemon only

As revised N7. One-shots shell out to `tmux` and parse `-F` formats (the tms pattern:
predictable, testable, no connection lifecycle). The daemon owns a pool of long-lived
`tmux -C` clients — one per session, control-mode notifications being session-scoped
(DAEMON.md) — because it is the one component whose job is waiting for pushes.
Control-mode feature floor probed at runtime (N10); degrade path is the reconciliation
sweep at higher frequency. Rejected: control mode in one-shots (connection setup cost
and complexity for no win) and subprocess polling in the daemon (defeats its purpose).

Accepted cost of that pool, measured and not fixable at the tmux level: each `-C`
client is a real attach, so for every monitored session tmux stops arming the
activity, silence and bell flags on the *current* window and `destroy-unattached`
never fires. Full measurements and the mitigations that were tried and failed are in
DAEMON.md, "Known cost: the control client counts as a viewer".

## AD7 — Crate layout: layered workspace, one binary

Amended by the layered-workspace restructure (RD1–RD5). The
original decision was two crates — `tma-core` (pure) plus a `tma` binary holding
everything else. Three boundaries inside the bin carried real invariants that only
convention enforced (the tmux I/O choke point, the tier-3 line AD5 draws, and UI's
snapshot-only contract). The split makes the compiler police them. The shipped layout
is seven crates under `crates/`, `tma-`prefixed:

```
crates/
  tma-core        pure library (D7): snapshot & evidence types, manifest schema +
                  engine, identity resolution, verdict fold, option-grammar serde.
                  No tmux, no I/O, no clock. Bundled manifests live here as data
                  (compiled-in TOML), fixture tests alongside (D8).
  tma-tmux        the only crate that spawns tmux: the read-only subprocess adapter
                  (`list-panes`/`capture-pane` formats) + `ps_all` (the other half of
                  the read path, F2), the control-mode client pool, and the guarded
                  `stamp` write adapter. The one I/O choke point.
  tma-runtime     tier 2: config, manifest loading + the hook-event vocabulary (RD3),
                  identity, the poll cycle, on-demand capture, debug/explain/json, the
                  hook-event bridge (`event`), the TMA1 wire protocol (`ipc`, RD2), the
                  single-fire notify primitive, and `ui.rs` — the display layer's
                  complete tmux surface, every read and write `tma-ui` performs routed
                  through named helpers (see `tma-runtime/src/ui.rs`) so the UI crate
                  keeps no `tma-tmux` edge (RD4). Re-exports `Tmux`/`TmuxError`.
  tma-daemon      tier 3 only: the serve loop (`run_cli`/`DaemonOpts`) + notification
                  dispatch (`NotifyState`). Strictly additive (AD5): nothing below it
                  requires it.
  tma-ui-core     the pure TEA core (TEA.md): the picker/watch folds
                  (`update(Event, now, res) -> Vec<Effect>`), the shared
                  Selection/RefreshGate/PreviewCache components, row-format
                  helpers, the `RowPalette` colour resolver, and the
                  ANSI-to-ratatui converter. Deps `tma-core` (+ ratatui, nucleo),
                  not `tma-runtime`: no crossterm and no tmux, both now
                  compiler-enforced (the batch-5 severance retired the grep gate).
  tma-ui          the display layer (RD4): the shell runner + effect executor for
                  the two live surfaces, their draw fns, cross-session jump, the
                  `ls`/`status` render surfaces, and the terminal guard. Atop
                  runtime and `tma-ui-core`, no `tma-tmux` edge; crossterm/nucleo
                  concentrate here and in the core.
  tma             the binary: main + clap dispatch, install, redact, json_value, doctor.
  tma-test-support  dev-dependency only (never shipped): the scratch-socket tmux
                  harness + daemon flock gate shared by four crates' tests (RD5).
```

`tma-ui` (R8) carried `picker`, `surfaces`, `jump`, `ansi` out of the bin atop runtime,
concentrating the ratatui/crossterm/nucleo stack. The R5 boundary fixes made it a pure
file move (snapshot-only inputs): it depends on `tma-runtime` (+ `tma-core`) with no
`tma-tmux` edge, and the bin re-imports the modules via `use tma_ui::{jump, picker,
surfaces}`.

Dep edges stay acyclic (cargo enforces it):

```
tma-core ← tma-tmux ← tma-runtime ← tma-daemon
                              ↑          ↑
              tma-ui-core ← tma-ui  tma ─┘
                          ↑    ↑     │
                          └────┴─────┘
```

`tma-ui-core` deps `tma-core` (+ ratatui, nucleo) only — no runtime edge, so the
compiler forbids it tmux (the batch-5 severance).

**Tier story (AD5).** `tma-daemon` is tier 3, and only tier 3. The bin's edge to it is
a single point: the `tma daemon` subcommand dispatch in `main.rs`. Every non-daemon code
path in the bin imports runtime (+ tmux) only, so the tier boundary is now a crate fact,
not a convention — tier 3 is genuinely never required (RD2). The wire protocol
(`tma_runtime::ipc`) and the single-fire notify primitive (`tma_runtime::notify::fire`)
live in the tier-2 runtime so daemonless `tma event` reaches them; the daemon imports
them for the server side.

**UI-snapshot rule (RD4).** Display code (`picker`, `surfaces`, `jump`, future sidebar)
reads `CycleReport` + config, never `tma-tmux` directly. Every tmux touchpoint the UI
genuinely needs — capturing a preview, moving focus, clearing attention, the jump
trail, the watch-pid advertisement — goes through a named helper in `tma_runtime::ui`.
The rule holds at two strengths, and the distinction matters. The pure fold crate
(`tma-ui-core`) has no runtime edge at all, so `Tmux` is not nameable there and the
compiler forbids it tmux entirely. The shell crate (`tma-ui`) keeps the R8 carve's
runtime-only Cargo edge; `Tmux`/`TmuxError` reach it through runtime's re-export, not a
`tma-tmux` dependency, so the compiler *can* name them and a stray `tmux.set_option(...)`
would build clean. There the `tma_runtime::ui` helper surface plus a source-guard test
(`crates/tma-ui/tests/ui_boundary.rs`, failing on any direct `tmux.<method>(` call) hold
the line, the same compiler-can't-see-it companion the tier boundary gets from
`crates/tma/tests/tier_boundary.rs`.

**Distribution stays one binary.** The workspace ships a single `tma` executable — the
tms-style distribution story (goal 5). Prefixed crate names keep the door open to
crates.io without committing to it; a separate daemon binary is still rejected.

**Programmatic API.** The pane user options `tma` stamps (`@agent_state`, `@agent_detail`,
`@agent_summary`, and friends, readable by any `tmux show-options`/`#{...}` consumer) plus
`tma ls --json` (versioned `"schema": 1`) already are the programmatic surface that
competing tools advertise as a socket API — no daemon, no bespoke IPC, just tmux's own
option store and a stable JSON schema. This is a stated feature, not an accident of the
layout. `tma wait` (F31/H10) is the *blocking* half of that surface: it polls the same
detection cycle and exits with a contract code when a targeted agent reaches a state, so
a script can wait on an agent rather than only read one.

Rejected: a third `tma-agents` crate for manifests (data, not code — a directory in
`tma-core` suffices; X1 keeps "add an agent = one manifest file" true either way).
Never name the display crate `tma-render`: `core::render` is guard codegen and the
collision would be permanent (RD4).

## AD8 — Manifest: one file per agent, covering identity, screens, and hooks

**Question.** Where do per-agent rules live? `ta`: hardcoded Rust consts (recompile to
tweak). Sidebar: hook adapters in Rust, no screen rules at all. herdr: screen-rule TOML
only, identity separate.

**Decision.** One TOML manifest per agent is the complete description of that agent —
the single registration table D13 demands, drift-tested against parser and docs:

```toml
min_engine_version = "0.1"            # D11
[identity]                            # AD3 observation
process_names = ["claude"]
title_patterns = ["^Cursor Agent$"]   # H16/T17 optional secondary signal: regexes over
                                      # #{pane_title} that NARROW a generic process_names
                                      # match. When present, a pane is this agent only when a
                                      # process_name matches AND the title matches a pattern
                                      # (or the pid-anchored flicker-stickiness hold is live —
                                      # @tma_title_match_pid, AD4). Absent (every pre-H16
                                      # manifest), identity is process match alone, byte-for-
                                      # byte unchanged. A hook registration bypasses the title
                                      # gate (registration is authoritative identity).
[hooks]                               # DAEMON.md mapping; presence marks hook-capable
covers = ["working", "blocked", "idle", "lifecycle"]     # D14: what hooks report
[[hooks.map]] event = "Notification" matcher = "permission_prompt|elicitation_dialog"
              claim = { state = "blocked", detail = "permission" }
[[hooks.map]] event = "SessionStart"
              claim = { lifecycle = "start" }   # lifecycle variant: registration /
                                               # deregistration, not a state claim
[[hooks.map]] event = "SessionEnd"
              claim = { lifecycle = "end" }
[capture]
visible = ["working", "idle", "blocked"]  # states screen rules reliably detect —
                                          # the gate AD2's coverage-aware decay reads
                                          # (round-2 fix: decay referenced a
                                          # declaration the schema didn't have);
                                          # evidence-backed per D10, not derived from
                                          # rule presence
[[rules]]                             # screen rules, herdr-idea reimplementation
state = "blocked"  priority = 100  region = "tail_lines(5)"   # v1 accepts tail_lines(N),
                                          # bottom_non_empty_lines(N), visible, and title
                                          # (unknown regions are hard-rejected); further
                                          # regions grow from evidence later
match = { any = [ ... ] }             # contains/regex/line_regex/last_matching_line + any/all/not
[details]                             # token spelling/aliases only — state routing is
                                      # normative in AD1 and NOT manifest-overridable
rate_limit = { aliases = ["ratelimited"] }   # example: alternate spellings an agent's
                                             # hooks/chrome emit, mapped to the
                                             # canonical detail token
```

Bundled manifests compile in; user overrides shadow at `~/.config/tma/agents/`
(F25; hot-reloaded with config on daemon SIGHUP / `tma reload` and on the picker's
refresh tick, H3 — not by mtime watching). Everything evidence-authored (D10/D12)
with redacted fixtures (D8/D9).

Honest scope of X1 (adversarial review): "one manifest, zero core code" holds for
*detection* — identity, screen rules, detail mappings, hook-event-to-claim mapping.
It does **not** hold for hook *installation*: agent config formats are not uniform
(Claude Code: JSON hooks block with events and matchers; Codex: a single `notify`
program in `config.toml`; Cursor: its own `~/.cursor/hooks.json` shape with a flat
`{command}` entry, H16), so `tma install-hooks` needs a small per-agent installer
adapter in core. The manifest declares *what* events map to
*which* claims; the installer adapter knows *where and how* to wire the wrapper into
that agent's config. Third parties can still wire unknown agents without core code by
configuring the agent themselves to call the wrapper (DAEMON.md open q4) — they just
don't get `install-hooks` automation for free.

## AD9 — UI stack: ratatui + nucleo, tms interaction model

Picker and watch use ratatui + crossterm + nucleo (N12) — the tms stack, matching its
popup ergonomics and keybinding feel, which the PRD names as the interaction model to
copy. Rejected: skim (ta's choice — heavier, less maintained, brings its own event
loop) and fzf shelling (external dependency; F15's `tma ls --json` already serves
users who prefer composing their own fzf pipelines). Picker ergonomics borrowed from
ta: ctrl-s session-scope toggle preserving the query. Its digit quick-select was
dropped later — every printable key belongs to the query, or an agent named `auth`
cannot be typed.

## Build order

Maps to PRD phases; each step lands testable.

1. **`tma-core` skeleton** — types (snapshot, evidence, verdict, manifest schema),
   verdict fold with fixture tests. No tmux yet.
2. **Claude Code manifest** — evidence pass with `tma debug capture`/`explain` built
   against the core (these two commands come first *because* they gather the evidence
   the manifests need, D10). Screen rules + hook map + fixtures.
3. **One-shot surfaces** — tmux adapter, discovery, `tma ls [--json]`, `tma status`,
   `tma jump`, stamping + freshness. Usable behind keybindings at this point.
4. **Picker** — ratatui popup, preview, cross-session jump.
5. **Hook tier** — `tma event`, wrapper script, `install-hooks claude` + `--check`,
   subagent guard, attention flag + auto-clear hook.
6. **Remaining phase-1 agents** — Codex, Gemini, Cursor (evidence passes; OpenCode
   candidate), each: manifest + fixtures + hook audit (DAEMON.md coverage table).
7. **Daemon** — socket, control mode, on-demand capture, reconciliation, history,
   notifications. `tma watch` + SIGUSR1 nudges.

## Open questions carried forward

- REQUIREMENTS §6: name, license (lean Apache-2.0), daemon autostart default,
  hysteresis defaults, status cold-path policy.
- Manifest rule-region vocabulary: which of herdr's region *ideas*
  (`bottom_non_empty_lines`, `after_last_horizontal_rule`, `prompt_box_body`) earn
  reimplementation in v1 versus starting with tail-window scoping (ta's simpler
  12/20/50-line model) and growing regions from evidence.
