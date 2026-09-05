# The `@agent_state` contract

`tma` keeps a pane's agent state in tmux user options on the pane itself. There is
no socket and no privileged writer in the way, so anything that can run
`tmux set-option` can write those options too. This page is the contract for a
tool other than tma that wants to: what the tokens mean, which options belong to
whom, and how to write them without clobbering a concurrent producer.

A *reader* needs [Pane options and JSON contracts](pane-options-and-json.md),
which documents the whole option set, and [Read agent state from a status bar or
script](../how-to/read-agent-state-from-a-status-bar-or-script.md) for the read
forms. This page is the subset a second *producer* has to get right, plus the
rules that only exist because there are two.

## The known second writer

[`getpipher/agent-status`](https://github.com/getpipher/agent-status), an
extension for the pi agent, writes pane-local `@agent_state` and a window-scoped
`@agent_window_state`, with a working/idle vocabulary that is not published
anywhere. Where its tokens fall inside the closed set below, tma reads them as
its own; where they do not, the pane goes silent in every tma surface. Neither
outcome is anybody's bug: two producers were writing one option with no agreement
about what the values mean, which is what this page ends.

`@agent_window_state` is outside this contract. tma neither reads nor writes it,
and nothing here says what it should contain. tma's window-scoped option is
`@agent_summary`, a rollup with a different grammar and a different owner (below).

## The state token set, closed

`@agent_state` is exactly one lowercase token from this set. It is closed and
frozen: tma will not add a fifth, and a value outside it is not a state tma has
yet to learn, it is a value tma cannot read.

| token | meaning |
|---|---|
| `idle` | prompt shown, nothing running |
| `working` | the agent is processing; the ball is with the agent |
| `blocked` | waiting on a human; the ball is with the person |
| `unknown` | a recognized agent whose evidence is unreadable |

The one question these answer is *whose move is it*. Everything finer belongs on
the detail axis. A producer that cannot map its own state onto one of the four
writes `unknown`, never a fifth token.

What an out-of-set value costs is worth stating exactly, because it is silent.
tma decodes a pane's whole `@agent_*` tuple in one step, and an unrecognized
`@agent_state` fails that decode. Every read path then treats the pane as never
stamped: no row in `tma ls`, no notification, no `tma wait` match, no count in
either rollup, no jump target. Nothing errors and nothing is logged. `tma doctor`
is the one surface that names it.

## `@agent_detail`: open, and unstable until 1.0

`@agent_detail` qualifies *why* a pane is in its state. Unlike the state token,
this vocabulary is open: a value tma has never seen round-trips intact rather
than failing the decode, and the vocabulary itself is unstable until 1.0. Read it
defensively, matching the tokens you know and degrading on the rest.

What tma emits today. The bundled manifests emit four:

| token | with state | meaning |
|---|---|---|
| `permission` | `blocked` | a tool-use permission prompt; approving grants the one action in front of the user |
| `plan` | `blocked` | a plan-approval dialog, whose affirmative option grants every following action |
| `trust` | `blocked` | a workspace-trust gate, whose affirmative option grants the whole folder |
| `rate_limit` | `working` or `blocked` | a usage-limit wait; `working` when the agent resumes by itself, `blocked` when the wait halted and needs a person |

`tma-core` declares four more as constants that no bundled manifest emits yet:
`question`, `error`, `background`, `compacting`.

The `rate_limit` pair is the shape of the axis split: the state says who owes the
next move, the detail says why. A producer that has a reason to report puts it
here and leaves the state token alone.

Keep detail tokens to `[a-z0-9_-]`. A token containing `#`, `{`, `}`, `,`,
whitespace or a control byte is written as empty, because those bytes would
corrupt the conditional-write format below.

## The two clocks

Both are epoch **milliseconds**. A non-zero value below `1000000000000` is read
as legacy epoch seconds and scaled on read, which is the only reason a 10-digit
value is tolerated at all.

`@agent_since` is the instant of the state *transition*, and it is written once
per state run. While the stored state is unchanged it is held, not rewritten. Two
consumers depend on that: a duration display ("blocked for 4m"), and the
notification dedup, which fires only when `@agent_notified_at` predates
`@agent_since`. Bumping `since` on an unchanged state re-rings every notifier on
the machine. There is one exception, and tma implements it: a `since` stranded
more than 2000 ms *ahead* of `@agent_stamped_at` came from a backward wall-clock
step (a suspend, an NTP correction), and the next publish rewrites it rather than
holding a value in the future forever.

`@agent_stamped_at` is the instant of *this write*. Every write tma makes ends
with it, including a refresh that changes nothing else. It is the per-pane
freshness marker: a reader ages the verdict against it, and tma's poll cycle
compares it against `#{window_activity}` to decide whether the pane needs
re-reading at all. Because it is written last, a reader that finds
`@agent_stamped_at` older than `@agent_since` or `@agent_evidence_at` caught a
chained write in flight and should treat the tuple as in progress rather than
acting on it.

## Identity and provenance

`@agent_name` is the agent's name, free text. tma writes the manifest stem
(`claude`, `codex`, `pi`). This is the only option in the contract with an open
vocabulary and no parse, so it is where a second writer says which agent it is
reporting on.

`@agent_source` is the provenance of the current state, and it is **closed**:

| token | meaning |
|---|---|
| `hook` | the agent itself reported this, through an event, with no inference |
| `capture` | it was read off the pane's screen |
| `process` | it came from the process walk |
| `activity` | legacy, accepted on read, produced by nothing since a viewport hash change stopped counting as evidence |

An unrecognized value fails the same tuple decode an unrecognized state does, so
a writer must not put its own name here. `@agent_source` describes where the
*evidence* came from, not who wrote the option; the writer's identity goes in
`@agent_name`. An absent `@agent_source` decodes as `capture`, which is the
weakest provenance and the one tma's guards will overwrite most readily.

`@agent_evidence_at` is the epoch ms of the evidence behind the current state, as
distinct from when it was written. Absent reads as `0`. It is the basis tma's
guards arbitrate on, so a producer that leaves it unset is choosing to lose every
arbitration it enters.

`@agent_session` is the agent's own session id, as the agent reports it. tma
stamps it from the hook payload at registration. A writer that knows its agent's
session id should set it: it is what attributes a later event to the right pane,
what the subagent guard compares incoming events against, and the key the Codex
rollout tail discovers its file by. Absent is legal, and costs those three
things.

## Writing without clobbering

tmux options have no transactions, no compare-and-set, and no writer identity. A
read-then-write from a second process loses exactly the races that matter,
because a state change lands inside the read-to-write window. So tma never
decides client-side. Every write whose correctness depends on a previous value is
a server-side conditional: tmux expands the format in the target pane's context,
at write time, and stores the result.

One `set-option -F` per option, where the value expands either to the new value
or back to the stored one:

```
tmux set-option -p -F -t <pane> <key> '#{?<suppress>,#{<key>},<new value>}'
```

`<suppress>` is a format that expands truthy when the write must be held. tma
chains every option of the tuple under the *same* `<suppress>`, in one `tmux`
invocation with `;` separators, so the tuple commits together or holds together,
and `@agent_stamped_at` goes last.

Two of tma's own guards are the ones a second writer meets:

- A capture-sourced write suppresses on `#{==:#{@agent_source},hook}`. A screen
  read never overwrites a claim the agent itself made.
- Blocker chrome overrides a `working` or `idle` hook claim only when the
  capture postdates the stored evidence:
  `#{&&:#{==:#{@agent_source},hook},#{e|<=:<capture ms>,#{@agent_evidence_at}}}`.

The practical consequence for a second writer that reports its agent's own
events: stamp `@agent_source` as `hook` with a fresh `@agent_evidence_at`, and
tma's capture writes hold off the pane. Blocker chrome newer than your evidence
still wins, which is deliberate. A blocked pane is the expensive thing to get
wrong.

For your own writes, the rule two event-sourced producers use between themselves
is evidence-time arbitration, and it is the one to copy. Suppress when the store
holds a hook claim whose evidence is strictly newer than yours, so the outcome
depends on when the two things happened rather than on which process finished
first:

```sh
now=$(...)   # epoch milliseconds
suppress="#{&&:#{==:#{@agent_source},hook},#{e|<:$now,#{@agent_evidence_at}}}"
tmux set-option -p -F -t "$pane" @agent_state  "#{?$suppress,#{@agent_state},working}" \
   \; set-option -p -F -t "$pane" @agent_source "#{?$suppress,#{@agent_source},hook}" \
   \; set-option -p -F -t "$pane" @agent_evidence_at "#{?$suppress,#{@agent_evidence_at},$now}" \
   \; set-option -p -F -t "$pane" @agent_stamped_at "#{?$suppress,#{@agent_stamped_at},$now}"
```

One caveat on `-F`. A tmux before 3.2 accepts the flag and stores the literal
`#{?...}` string instead of expanding it, which corrupts the tuple rather than
failing loudly. tma probes the behaviour once per server (it writes a format that
can only expand to `ok`, reads it back, and unsets it) and caches the answer in
the server option `@tma_setpf_ok`, degrading to plain unguarded writes when the
answer is no. A second writer should run its own probe under its own key; that
one is tma's.

## The rollups are tma's

Two options carry counts rather than one pane's state:

| option | scope | grammar |
|---|---|---|
| `@agent_summary` | window | `<state>:<count>` pairs, space separated, in the fixed order `blocked working idle unknown`, zero counts omitted (`blocked:1 working:2`) |
| `@agent_session_summary` | session | the same grammar over every agent pane in the session |

Both are unset when the scope holds no agent. They are a distinct key per scope
on purpose: a pane-context format read falls back pane, then window, then
session, so one shared name would make an agentless window render its session's
counts.

**Do not write either one.** They are a pure function of the panes' own
`@agent_state`, and tma recomputes both from every pane in scope and writes only
where the recomputed value differs from what is stored. A second writer that
stamps a valid token is therefore counted for free, with no rollup code of its
own. One that stamps an out-of-set token is dropped from the count silently: the
token fails to parse, and the pane contributes nothing to the total.

## Removal when the agent exits

A pane outlives its agent, so a stamp nothing refreshes has to go. tma unsets the
whole per-pane set in one invocation and recomputes both rollups:

`@agent_state`, `@agent_detail`, `@agent_source`, `@agent_evidence_at`,
`@agent_since`, `@agent_stamped_at`, `@agent_attention`, `@agent_notified_at`,
`@agent_turn_at`, `@agent_hash`, `@agent_pid`, `@agent_name`, `@agent_session`,
`@agent_subagents`, `@agent_context_pct`, `@agent_context_at`, `@agent_tokens`,
`@agent_tokens_at`, `@agent_context_notified_at`, `@agent_quota_pct`,
`@agent_quota_window`, `@agent_quota_resets_at`, `@agent_quota_at`,
`@agent_cost_usd`, `@agent_model`, `@agent_permission_request`,
`@agent_pending_tool`, `@agent_pending_call`, `@agent_pending_summary`,
`@agent_api_endpoint`, and two internal anchors, `@tma_title_match_pid` and
`@tma_reg_dead_since`.

Three survive that removal on purpose, and go only in `tma uninstall`'s sweep:
`@agent_action` (a detached action can outlive the agent that triggered it),
`@agent_mute_until` (a mute belongs to the user, not to the episode), and
`@tma_watch_pid` (its owner is a `tma watch`, not the agent). `@agent_ignore` is
never cleared by tma at all: the user wrote it, and only the user takes it back.

A second writer removing its own pane unsets at least `@agent_state`,
`@agent_detail`, `@agent_source`, `@agent_evidence_at`, `@agent_since` and
`@agent_stamped_at`. Leaving `@agent_state` standing with nothing refreshing it
is the failure this list exists to prevent: no reader can tell a held stamp from
a live one except by its age.

## The rules, condensed

A second writer **MUST**:

- write only `idle`, `working`, `blocked` or `unknown` into `@agent_state`, and
  `unknown` rather than a fifth token when nothing fits;
- write `@agent_stamped_at` on every write, in epoch milliseconds, last in the
  chain;
- write `@agent_source` from its closed set, `hook` for the agent's own event and
  `capture` for a screen read, and put its own identity in `@agent_name` instead;
- unset the state options when its agent exits.

**SHOULD**:

- set `@agent_evidence_at` to the instant of the evidence rather than of the
  write, since that is what arbitration compares;
- set `@agent_name`, and `@agent_session` when it knows the agent's session id;
- write conditionally with `set-option -F`, holding when the stored evidence is
  newer than its own;
- write `@agent_since` only on a state change and hold it otherwise;
- keep `@agent_detail` to `[a-z0-9_-]`.

**MUST NOT**:

- write `@agent_summary` or `@agent_session_summary`;
- write any `@tma_*` option (`@tma_last_poll`, `@tma_setpf_ok`,
  `@tma_watch_pid`, `@tma_origin_*`, `@tma_title_match_pid`,
  `@tma_reg_dead_since`). Those are tma's internals and carry no compatibility
  promise;
- clear or overwrite another writer's stamp unconditionally;
- write `@agent_ignore`, which is the user's opt-out and nobody else's.

## Checking the result

`tma doctor` reports panes whose `@agent_*` options tma did not write or cannot
decode, naming the pane, the value, and the possibility that a second tool is
writing them. Two shapes reach that line: an `@agent_state` outside the closed
set, and an `@agent_state` set with no `@agent_stamped_at` beside it, which no
tma write can produce. Both count toward `tma doctor --exit-code`, and both ride
the `--json` document's `stamp_issues` array.
