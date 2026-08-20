# The detection model

This page explains how `tma` decides what an agent pane is doing, and why it
trusts what it trusts. The exact option names and JSON keys are in
[pane options and JSON contracts](../reference/pane-options-and-json.md); the
per-agent evidence tables are in [agent coverage](../reference/agent-coverage.md).
The full arbitration record is kept in the repository's `docs/internal/` notes
rather than on this site.

## Four states, plus detail, plus attention

The published state (`@agent_state`) is one of exactly four tokens: `working`,
`blocked`, `idle`, `unknown`. That vocabulary is closed and frozen. The reason
it stays small is that the only question every consumer actually asks is *whose
move is it*: the agent's (`working`), the human's (`blocked`), nobody's
(`idle`), or unreadable (`unknown`). `tma jump --blocked` has to mean the same
thing for every agent, so the mappings from an agent's own events into these
four tokens are normative, not something a manifest gets to redefine.

Prior tools reached for larger enums (six or seven states) and ended up
conflating orthogonal things. "Rate limited" and "error" are *reasons*, not
states; "done" is really "idle, and you haven't looked yet". `tma` splits those
onto two other axes so the state token stays stable:

- `@agent_detail` is an open, additive token that qualifies the state
  (`permission`, `rate_limit`, `compacting`, and so on). It can be empty. A
  rate-limited agent is `working/rate_limit`, because the agent auto-resumes and
  the ball is not with the human; an agent that *halts* asking for confirmation
  is `blocked/permission` on its own prompt evidence.
- `@agent_attention` is a presentation flag meaning "this changed and you have
  not seen it yet". It is set on a noteworthy transition and cleared two ways.
  By navigation: on the pane you move to, and on the pane you move away from. And
  by input: your next keystroke at a pane a client of yours is displaying, if that
  keystroke lands after the mark went up. In one line, **the done mark survives
  until your next input while that pane is on screen, or until you navigate off
  it.** Nothing else takes it down. Walking away clears nothing, so leaving an
  agent running and going for coffee still leaves the mark waiting for you however
  long that takes; navigation that moves nothing clears nothing either, since
  selecting the pane or the window you are already in is not a departure. A
  finished agent is `idle` with attention still set, which
  the surfaces render as the distinct **done** glyph. Keeping **done** on this
  separate flag rather than making it a fifth state token is deliberate: the
  closed `state` vocabulary stays four tokens, and a script reading `state` never
  has its value change shape under it. Attention is also *not* the notification
  record: navigating clears attention, but a blocked episode you glanced at
  and walked away from is still blocked, so the notifier keeps its own separate
  marker.

  The input half reads two facts tmux already keeps: which pane each client is
  displaying, and when that client last received real terminal input
  (`#{client_activity}`, which moves for a keystroke, the prefix key or the mouse,
  and never for pane output or for `tma`'s own polling). It is an ordering against
  the raise, never a window: "you typed in the last N seconds" would eat the mark
  for the very case the mark exists to serve. Two limits are honest ones. A
  control-mode client (iTerm2's `-CC`) has its activity clock frozen at attach, so
  under `-CC` this half does nothing and navigation is the only clear. And a person
  who reads the output without touching the keyboard looks exactly like a person
  who is not there, so their mark stands until they type or move.

## Three evidence sources, one ranking

`tma` learns a pane's state from three kinds of evidence, in descending
fidelity:

1. **Agent hooks.** A cooperating agent runs a command at each lifecycle point,
   so it *tells* `tma` it just blocked, at the instant it blocks, with zero
   inference. Highest fidelity.
2. **Screen chrome.** Capturing the pane and matching its on-screen text against
   the agent's manifest rules. This is how a hookless agent, or a missed hook,
   still gets detected.
3. **Process facts and the pane title.** The process walk (is the agent still
   alive?) and the OSC title the agent publishes. Output activity is not on this
   list: a pane producing bytes tells the daemon *when to look*, not what state
   to report.

They are combined by a deterministic fold, not a probabilistic fusion. The
sources have a natural strict ranking, and the verdict has to be *explainable*
(`tma debug explain` names the rule or event that decided), so weighting would
be both unnecessary and opaque. The order the fold applies is:

1. a fresh hook event from a registered pane;
2. visible blocker chrome on the live viewport;
3. visible working chrome, which means `working`;
4. visible idle chrome, which means `idle`;
5. otherwise hold the previous state, or `unknown`.

Two things stop the fold before it reads the screen at all. If the pane's
foreground process is not the agent, the screen belongs to something else and
the verdict is capped at `unknown`. What that cap governs is the *screen*, not
what the agent said about itself: a pane already carrying a hook claim keeps it
as long as the agent's own process is still in the pane's tree. An agent that
hands the tty to `$EDITOR` or pipes a diff into a pager is alive and mid-task,
and dropping its `blocked` the moment `vim` comes up would lose exactly the
state you needed. A pane with no hook claim behind it has only the process walk
to go on, and that walk is stale while someone else holds the foreground, so it
still caps at `unknown` — as does a pane whose agent pid is gone, which is the
claim expiring on process evidence rather than on the foreground. If the
viewport is not the live screen, the
last state is frozen rather than matched against whatever is on display: a rule
written for the current prompt would happily match a prompt you scrolled back
to. That freeze keys on the scroll *offset*, not on copy-mode itself. tmux
reports offset 0 the moment you enter copy-mode, and at offset 0 you are still
looking at the live screen, so entering copy-mode to copy an error message does
not quietly suspend detection on the pane; scrolling up by a line does.

### Why a hook can lose to the screen, and when it cannot

Ranking hooks first raises an obvious hazard: a stale hook claim outliving
reality. The fold handles this with *coverage-aware* decay rather than a blanket
timeout. A hook claim is expired by process evidence (the pid is gone, so the
agent died without firing its end hook) at any time. It is expired by screen
evidence only for states the agent's manifest declares its screen rules can
actually see. A blocked agent can sit silent for ten minutes precisely because a
permission prompt produces no output, so the reconciliation sweep must never
read that silence as idle and flip a hook-reported `blocked`.

Silence, then, never expires anything. What can expire a claim is the screen
saying something *else*, and even that has to clear three gates at once: the
claim is older than its decay window, the manifest declares the claimed state
screen-visible, and this capture carries positive contrary chrome. `blocked`
gets its own, much longer window (`blocked_decay_secs`, five minutes against
`hook_decay_secs`' sixty seconds) because answering a prompt takes as long
as it takes. It is a window rather than "never" for one failure mode: a
follow-up hook that never fired. Without a bound, one dropped event pins a pane
`blocked` for the rest of the session, and no amount of screen evidence, an idle
composer sitting there with the prompt long gone, could correct it. With the
bound, an agent whose manifest can actually read `blocked` off the screen (see
[agent coverage](../reference/agent-coverage.md)) recovers on its own; one whose
manifest cannot, such as `pi`, keeps holding, because for that agent the absence
of blocker chrome carries no information.

The one case where blocker chrome overrides a live hook claim is decided by
evidence timestamps, not by "immediately" or "after a wait". Visible blocker
chrome overrides a `working` or `idle` hook claim only when the stamped evidence
timestamp *predates* the capture. That single rule resolves the answered-prompt
race in both directions. Capture at T0 sees a prompt; the user answers; the hook
stamps `working` at T1. The capture's blocked write carries time T0, which is
older than T1, so it is suppressed: the hook is newer evidence and wins. Reverse
the order and the capture is newer, so the block wins with no decay wait
(millisecond timestamps keep that ordering unambiguous; see the
[pane options reference](../reference/pane-options-and-json.md)).

## Identifying the pane

Before any of this runs, `tma` has to decide a pane is an agent pane at all. A
pane earns that identity two ways: by observation (the process walk finds a known
agent binary) or by self-registration (a hook stamped it). Observation is what
lets hookless agents show up without cooperation. Some agents run under a generic
process name (several launch as `node`), where the binary name alone would either
miss them or match every unrelated app; for those, a manifest adds
`title_patterns` that narrow a generic process match, so the pane is that agent
only when the process and the pane title agree. A hook registration is
authoritative and skips the title gate; title flicker is absorbed by holding the
last match while the pane's agent pid is unchanged.

Narrowing shrinks the false-positive window but cannot close it: a dev server
whose title happens to match still looks like an agent. That pane, and only that
pane, opts out with `tmux set-option -p @agent_ignore 1`, after which it is
never identified, captured, or stamped, and any stamp it still carries is
cleared — no need to disable the whole agent type. `tma doctor` lists the panes
carrying it (see [pane options](../reference/pane-options-and-json.md)).

Two kinds of pane are ruled out before the walk even runs, because for both the
walk would come back empty while the screen invites a false match. A remote shell
(`ssh`, `mosh`, `docker`, and friends) runs its real work on a host tma cannot
see. A nested multiplexer client (`tmux`, `zellij`, `screen`, `dvtm`, `abduco`)
is the same shape one level down: whatever runs inside belongs to the inner
server, not to this pane's process tree, and the outer pane's screen is a
composite of the inner ones that a screen rule would happily match by
coincidence. Neither gets a stamp or a row, and a stamp left on such a pane is
removed rather than trusted. `tma debug explain` names both (`out_of_scope` with
its kind); `tma doctor` lists the nested case, saying where the state actually
lives.

A live hook registration outranks both carve-outs. The carve-outs exist because
the walk comes back empty and the screen is somebody else's; a registration is
positive evidence of the thing they infer the absence of — an agent fired a hook
*in this pane*, which it could only do from inside. So a registered pane keeps
its stamps and its row even when the foreground is `docker` or a nested `tmux`:
tma stops capturing it (nothing readable crosses the boundary) and lets the hook
path be its only evidence source, with the usual dead-registration reaper as the
liveness bound. That is what makes an agent [in a
container](../how-to/agents-in-containers.md) work. Without a registration
nothing changes: an outer nested-tmux pane is as invisible as it always was.

## Three tiers, none required

The same detection runs at three tiers. Each is a strict upgrade in latency or
coverage, and consumers see no difference between them because they all read the
same stamped options.

- **Polling floor.** Any one-shot invocation refreshes stale panes when it runs.
  This is the only tier a hookless agent gets with no daemon, and it has no
  driver of its own: something must invoke `tma` for stamps to stay fresh.
  `#(tma status)` in `status-right` is that required ambient driver; without it,
  ambient surfaces render nothing.
- **Hook tier.** `tma event` direct-stamps the moment a hook fires, with no
  daemon involved. State is event-latency, and a resident `tma watch`
  refreshes within about a fifth of a second of a focus change: the
  `after-select-pane` / `session-window-changed` hooks that already clear attention
  also walk panes for a watcher's advertised pid (`@tma_watch_pid`, set on the
  watcher's own pane so it dies with that pane) and send `SIGUSR1`, which the
  watcher treats as "refresh now". The picker popup is deliberately outside that
  scheme: `display-popup -E` runs in a hidden pane `list-panes -a` never
  enumerates, so no hook can find it, and its own one-second refresh is what
  keeps it current. This is the sweet spot for a single-user setup: hook-fresh
  state, no background process.
- **Daemon tier.** A background process holds control-mode clients, captures
  hookless panes on an activity-quiet edge, runs a slow reconciliation sweep,
  and dispatches deduplicated notifications. It adds cross-event intelligence,
  not basic liveness.

Deduplication is per state run, not per pane and not per episode. Whichever
process fires a notification stamps the time on the pane as
`@agent_notified_at`, and a notifier fires only when that marker predates the
pane's `@agent_since`, which is written once per state. Five producers noticing
the same blocked run therefore ring once between them, while an agent that
blocks, gets answered, and later finishes rings twice (blocked, then done, if
you opted into done). The marker is a pane option rather than daemon memory on
purpose: a daemon restart mid-session must not re-announce every blocked pane
you already dealt with. Without a daemon nothing is resident to dispatch from,
so the hook path can fire for itself instead, opt-in via `notify.from_event`
(see [notifications](../how-to/notifications.md)).

`tma doctor` reports which tier each pane is actually running at and why it is
not higher.

## Reading a pane only when it can have changed

A capture is a `capture-pane` subprocess, and the poll cycle spawns them one
after another, so a session with a dozen agent panes pays for every one on every
cycle even when nothing has happened. The cost was measured against a release
build on a throwaway server of 40 panes, 10 of them agents (tmux 3.6, macOS,
arm64): a cold cycle that captures all ten takes about 104 ms, while the same
cycle with every stamp fresh, capturing nothing, takes about 24 ms. That is
roughly 8 ms of cycle time per agent pane, nearly all of it process spawn rather
than capture payload, and it grows linearly with the number of agents. The cycle
avoids most of that by asking tmux a cheaper
question first: `#{window_activity}`, the timestamp of the last output in the
pane's window. When that timestamp falls strictly before the pane's own
`@agent_stamped_at`, the screen behind the stored verdict is byte-for-byte the
screen a capture would return, so the cycle reuses the stamp and reads nothing.
The check is window-scoped, which is conservative in the useful direction: a
quiet window proves a quiet pane, never the reverse. tmux reports it in whole
seconds, so a write in the same second as the stamp counts as activity.

An unchanged screen is not the same as an unchanged verdict, because two of the
fold's rules are driven by the clock rather than the screen. The dwell that
delays a working→idle publish resolves off idle chrome that is already on the
unchanged screen, so a `working` pane is always re-read. A hook claim past its
decay window can be expired by contrary chrome that has likewise been sitting
there since before the stamp, so a claim that old is re-read too. Inside its
window the claim holds whatever the screen says, and since a skip writes nothing,
the next cycle re-asks the same question against a later clock and captures the
moment either window closes. `--debug-timing` reports the skips as
`capture-skipped` next to the captures.

## Why concurrent producers are safe

Several producers stamp the same pane options at once: a status poll in one
client, another client's poll, a hook firing, the daemon. tmux options have no
transactions, no compare-and-set, and no writer identity, so an uncoordinated
read-then-write loses races exactly on the transitions that matter, because
hooks fire *inside* the read-to-write window.

The fix is to never decide client-side. Every guarded write is a server-side
conditional (`set-option -pF`), which tmux expands in the target pane's context
atomically at write time. A capture producer's state write carries a guard that
says, in effect, "only commit if a hook has not already claimed this pane with
newer evidence". The whole chained write, state, provenance, timestamps, detail,
and the write-once transition marker, carries the *same* suppression condition,
so the tuple commits together or holds together. A losing producer changes
nothing, including the notification marker, so it cannot fire a stray alert
either. Everything that is not guarded this way is last-writer-wins over
deterministic values (the same fold, the same persisted inputs), which
converges.

## Honest margins

Two properties are margins, not proofs, and the design says so plainly rather
than dressing them up.

A margin is tolerable here only because the two directions of error cost
different amounts. A blocked agent shown as working or idle is the expensive
failure: you never go back, and the agent sits on its prompt until you happen to
look. A working agent shown as idle for a cycle costs you one glance. Where the
evidence is genuinely ambiguous the fold leans toward blocked. It stops short of
guessing, though, because a false blocked flag is expensive in its own currency:
flags that turn out to be nothing teach you to ignore the flag, and then the real
one goes unanswered too. So `blocked` is asserted only from direct evidence, a
blocked-class hook event or blocker chrome on the live viewport, and never
inferred from silence, from the pane title, or from a lull in output.

The daemon triggers a hookless capture on an activity-*quiet* edge, the moment a
pane stops producing output, because a permission prompt is exactly when output
stops. But the activity gauge sees `%output` events, not the kernel's buffers,
so "quiet" is not proof that nothing is happening; it is a strong signal with a
settle window layered on top. The quiet threshold plus settle is a generous
empirical margin, chosen to be safely past real output bursts, not a structural
guarantee. Calling it a margin is the honest description. What the quiet edge
buys is a look rather than a verdict: it decides when to capture, and the
`blocked` call still has to come off chrome that is actually on the screen.

Pure event-driving fails open: a hook can be missed (the agent was killed with
`-9`, the hook was misconfigured, the daemon restarted mid-session). So state is
never *only* event-driven. The recovery paths are layered: process evidence
expires a claim whose pid is gone; a pane close clears state immediately; and a
low-frequency reconciliation sweep, the full poll cycle every 30 to 60 seconds,
rediscovers agents that never announced themselves and corrects any drift. The
governing invariant is that events *drive* state and the sweep *repairs* it, so
the sweep's latency bounds only how long an anomaly can persist, never how fast
a normal transition is seen.

The numbered decision records behind this model live in the repository:
[`docs/internal/ARCHITECTURE.md`](https://github.com/pperanich/tmux-agents/blob/main/docs/internal/ARCHITECTURE.md)
for the arbitration rules and
[`docs/internal/DAEMON.md`](https://github.com/pperanich/tmux-agents/blob/main/docs/internal/DAEMON.md)
for the event sources and the daemon tier.
