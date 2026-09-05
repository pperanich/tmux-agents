# Agent transcript stores

Every coding agent tma watches writes its conversation to disk, and no two of
them agree on how. [`tma transcript`](../reference/cli.md#tma-transcript) reads
four of those stores into one event vocabulary. This page is the honest account
of what that buys and what it does not, because a reader that quietly renders
half a conversation is worse than one that says it cannot.

## What each store is

| agent | store | tail | served |
|---|---|---|---|
| claude | `~/.claude/projects/<cwd-slug>/<session>.jsonl`, plus `<session>/subagents/agent-*.jsonl` | byte offset | yes |
| codex | `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-<iso>-<session>.jsonl` | byte offset | yes |
| gemini | `~/.gemini/tmp/<projectHash>/chats/session-<iso>-<short>.jsonl` | byte offset, with `$set` dropped | yes |
| pi | `~/.pi/agent/sessions/--<cwd-slug>--/<iso>_<session>.jsonl` | byte offset | yes |
| cursor-agent | `~/.cursor/projects/<cwd-slug>/agent-transcripts/<chat>/<chat>.jsonl` | byte offset | **no** |
| OpenCode | `~/.local/share/opencode/opencode.db` (SQLite) | `event.seq` | **no** |

All four served stores are append-only JSONL and all four are written *during*
the turn, not at the end of it, so what you read is what the agent has done so
far rather than what it did last time it finished.

## The two refusals

**cursor-agent is refused because its transcript is not one.** It records the
user's prompt, the assistant's prose, a bare `tool_use`, and `turn_ended`. There
is no tool result, no timestamp, no version stamp, and no header record; a
driven run that used a tool wrote the call and then the model's reply, and the
tool's output never landed in the file at all. Rendering that beside a claude
session would produce a screen full of holes that looks like tma is broken.
Returning an empty window would be worse still, because "nothing happened" is a
claim, and it would be false. So the request is refused with `store-incomplete`,
which names the store and the reason.

**OpenCode is refused because its store is a database.** It moved off
one-file-per-message some time before this reader was written, and everything
now lives in SQLite. Its `event` table is in fact the best tail any of these
agents offers (`(aggregate_id, seq)` with a high-water mark per aggregate, so a
subscriber stores one integer), but reading it means a SQLite dependency and
WAL-reader concerns, and none of the file reader applies to it. It is its own
workstream, and until then the refusal is `unsupported-store` rather than a
half-working adapter.

## Two quirks that would silently corrupt a rendering

These are worth naming because both fail *quietly*: neither produces an error,
and neither raises the unknown counter.

**codex writes every turn twice.** `event_msg` is the UI stream and
`response_item` is the model-API transcript, and both are in the same file. A
reader that maps both renders each message twice. The split tma takes is prose
from `event_msg`, tool calls from `response_item`, and each channel's mirror of
the other as bookkeeping.

**pi hoists its tool results to a role of their own.** A `toolResult` record's
content is a plain `text` block, so a reader that dispatches on block type
before role maps the tool's output to assistant prose: the file's contents,
rendered as if the model had said them, at 100% mapped and zero unknowns. That
is why every adapter reads the record's role first. It is also why a store's
role vocabulary is part of the pinned fixture corpus rather than a detail of the
code.

**gemini restates its whole history.** Interleaved with the per-message records
are `$set` records that replace the entire `messages` array; nineteen real
messages on the spike machine came with twenty-five restatements of them. Those
are dropped, not rendered.

## What no store can tell you

**Nothing streams tokens.** Every store on disk writes settled records. The
finest grain available is a whole message, so a reader can show "the agent is
writing" (from tma's own detection) and then the whole paragraph when it lands,
but never a word at a time.

**An unresolved tool call does not mean blocked.** The transcript answers *what*
the agent wants to do, never *why it stopped*. A tool call with no result yet
means in flight, and a slow command, a permission prompt and a crashed process
are indistinguishable from the file alone. That distinction lives in tma's
detection and its `@agent_state`, and joining the two is the caller's job. A
design that infers "blocked" from a dangling call will fire on every slow test
run.

The join is worth making, though, because the transcript is the only place the
*specifics* live: for claude and codex, a pending call's real tool name and real
arguments are on disk before any approval, which is the difference between
asking "allow this?" and asking "allow `Bash: rm -rf build/`?".

**Token usage is uneven.** codex, gemini and pi write it per turn; pi is the
only one that writes cost. Claude writes none of it in the transcript at all
(its numbers arrive through the statusline shim, which is why tma reads them
there instead).

**Subagents are not merged.** A claude `Task` call becomes a `subagent_ref`
pointing at a child file, and `--subagent <id>` serves that child as its own
session. There is deliberately no interleaving: the parent and child streams
have no ordering guarantee between them, and inventing one would put events in
an order neither agent wrote.

## Drift, and why the reader never errors on it

Store formats move fast. Thirty-seven claude sessions on one machine spanned
eight CLI versions in two months, and inside that window subagent transcripts
moved out of the parent file entirely and two new record types appeared.

So the reader is built to degrade, not to fail. An unseen record type becomes an
`unknown` event and raises a counter; an unseen *version* is simply read. A
reader that errored on drift would break the day an agent shipped a release,
which is the one thing it must never do. The counter is what turns that
tolerance back into a signal: tma's committed fixture corpus asserts the count
is zero for every version it pins, and a deliberately-drifted fixture asserts it
is not, so a refresh from a newer session surfaces the change as a test diff
instead of as a hole a user notices first.
