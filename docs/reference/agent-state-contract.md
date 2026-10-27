# The `@agent_state` contract

`tma` keeps a pane's agent state in tmux user options on the pane itself. There is
no privileged writer in the way, so anything that can run `tmux set-option`
can overwrite those options. This page defines their public meaning and the
ownership requirement for cooperating producers. Direct option access is not
permission to bypass an active program owner.

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

The bundled manifests emit these detail tokens:

| token | with state | meaning |
|---|---|---|
| `permission` | `blocked` | a tool-use permission prompt; approving grants the one action in front of the user |
| `plan` | `blocked` | a plan-approval dialog, whose affirmative option grants every following action |
| `trust` | `blocked` | a workspace-trust gate, whose affirmative option grants the whole folder |
| `question` | `blocked` | a question waiting for a human answer, not a permission grant |
| `rate_limit` | `working` or `blocked` | a usage-limit wait; `working` when the agent resumes by itself, `blocked` when the wait halted and needs a person |

Native OSC 7501 can also supply `auth` on blocked and `error` on a fresh failure
result projected to idle. `tma-core` declares `background` and `compacting` as
constants without a bundled mapping. Failure is not an approval target.

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

`@agent_stamped_at` is the state stamp's freshness marker. Baseline state
publication/hold writes it after the public state fields; it is not the last
write in every transaction. Private commit/receipt fields can follow it and
independent metadata or acknowledgement writes need not advance it. A reader
that finds it slightly older than `@agent_since` or `@agent_evidence_at` treats
the public tuple as in progress. The 2-second clock-step exception above still
applies. For native authority, lease and owner validation also matter: an old
report can remain valid during silence, and a fresh timestamp alone cannot
restore a lost stream.

## Identity and provenance

`@agent_name` is the agent's name, free text. tma writes the manifest stem
(`claude`, `codex`, `pi`). This is the only option in the contract with an open
vocabulary and no parse, so it is where a second writer says which agent it is
reporting on.

`@agent_source` is the provenance of the current state, and it is **closed**:

| token | meaning |
|---|---|
| `hook` | direct application evidence, from a hook event or a validated native program report |
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
baseline guards arbitrate on. It does not supersede program ownership.

`@agent_session` is the agent's own session id, as the agent reports it. tma
stamps it from the hook payload at registration. A writer that knows its agent's
session id should set it: it is what attributes a later event to the right pane,
what the subagent guard compares incoming events against, and the key the Codex
rollout tail discovers its file by. Absent is legal, and costs those three
things.

## Writing without clobbering

tmux has no atomic option-tuple transaction. Current tma producers use typed
ownership guards in addition to evidence timestamps. A frozen `if-shell -F`
admission body contains only literal, explicitly targeted option mutations.
Private owner, commit and receipt comparisons plus full readback determine
whether publication won. State-dependent identity, freshness, attention and
removal operations obey the same ownership policy.

Failed admission makes no edits. A write that executed may then lose readback
to a concurrent receipt replacement; that Lost result is not proof of no
mutation and does not authorize replay. A batched advisory poll hint can remain
after this outcome, but grants no state or notification permission. Separate
exact acknowledgement and notification claims provide at-most-once admission,
not exactly-once external delivery.

Baseline timestamps still prevent old capture evidence overwriting a newer
hook claim, with the documented newer-blocker and coverage-aware decay rules.
They cannot override live program authority. Public `source=hook` encodes both
direct origins, so checking that token or `evidence_at` alone is insufficient.
Upgrade all tma producers together. Mixed older writers and third-party direct
writers that lack these guards are not guaranteed safe. Integrations should
submit agent events through `tma event` instead of copying a timestamp-only
`set-option` recipe. The private protocol is an implementation contract, not a
stable third-party write API.

The adapter probes conditional-write behavior. An effective nonempty
`after-set-option` hook can yield within a body and disables program authority
for that target. tma revokes the matching pane commit, keeps protected baseline
fallback, and can retain healthy peers through another supported lease target.
It never removes the user's hook or downgrades native authority to unguarded
writes. Unsupported servers remain on baseline behavior.

Session/transcript, model/API endpoint, subagent and pending-request metadata
have independent process/session ownership. A valid metadata update can apply
when its accompanying hook state loses. A sessionless event may establish a
private validated binding without inventing public session/tool/request data.
First metadata admission has one bounded receipt-only recovery attempt when
the original identity, metadata and episode are still unchanged; an already
applied update that lost readback is not replayed.

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
validated state, and tma writes a recomputed value only when it differs and the
complete scope snapshot still matches. The snapshot is capped at 64 KiB. An
oversized scope keeps its previous summary hint and emits a bounded warning;
its summary is not guaranteed to converge while it remains oversized. Pane
publication and resolved rows retain their independent guards.

Within an admitted scope, a cooperating second writer needs no rollup code of
its own. An out-of-set state token fails to parse and contributes nothing to
the total.

## Removal when the agent exits

A pane outlives its agent, so a stamp nothing refreshes has to go. tma unsets the
per-pane set under ownership guards and recomputes both rollups:

`@agent_state`, `@agent_detail`, `@agent_source`, `@agent_evidence_at`,
`@agent_since`, `@agent_stamped_at`, `@agent_attention`, `@agent_notified_at`,
`@agent_turn_at`, `@agent_hash`, `@agent_pid`, `@agent_name`, `@agent_session`, `@agent_transcript`,
`@agent_subagents`, `@agent_context_pct`, `@agent_context_at`, `@agent_tokens`,
`@agent_tokens_at`, `@agent_context_notified_at`, `@agent_stall_notified_at`, `@agent_quota_pct`,
`@agent_quota_window`, `@agent_quota_resets_at`, `@agent_quota_at`,
`@agent_cost_usd`, `@agent_model`, `@agent_permission_request`, `@agent_question_request`,
`@agent_pending_tool`, `@agent_pending_call`, `@agent_pending_summary`,
`@agent_api_endpoint`, and two internal anchors, `@tma_title_match_pid` and
`@tma_reg_dead_since`.

Private protocol fields use their own exact owner/history cleanup. SessionEnd
while the agent is still alive clears its session metadata and pending requests,
preserving state rather than treating an in-process session boundary as exit.

Independent action/user/watcher options survive ordinary agent removal:
`@agent_action` (a detached action can outlive the agent that triggered it),
`@agent_mute_until` (a mute belongs to the user, not to the episode), and
`@tma_watch_pid` (its owner is a `tma watch`, not the agent). Action repeat
bookkeeping (`@agent_act_repeat`) is also independent. `@agent_ignore` is
never cleared by tma at all: the user wrote it, and only the user takes it back.

A cooperating writer removing its own pane, after validating ownership, unsets at least `@agent_state`,
`@agent_detail`, `@agent_source`, `@agent_evidence_at`, `@agent_since` and
`@agent_stamped_at`. Leaving `@agent_state` standing with nothing refreshing it
leaves a historical snapshot behind. Age is a baseline freshness hint; native
state also requires the separate live owner and collector lease.

## The rules, condensed

A second writer **MUST**:

- write only `idle`, `working`, `blocked` or `unknown` into `@agent_state`, and
  `unknown` rather than a fifth token when nothing fits;
- keep state freshness in epoch milliseconds and preserve the public-field
  ordering and private commit/readback rules;
- write `@agent_source` from its closed set, `hook` for the agent's own event and
  `capture` for a screen read, and put its own identity in `@agent_name` instead;
- obey program ownership on state and companion writes, holds and removals;
- remove its own state when its agent exits without clearing a newer owner.

**SHOULD**:

- set `@agent_evidence_at` to the instant of the evidence rather than of the
  write, since that is what arbitration compares;
- set `@agent_name`, and `@agent_session` when it knows the agent's session id;
- use tma's event intake for supported agent events; a direct writer must
  implement ownership as well as evidence ordering;
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
