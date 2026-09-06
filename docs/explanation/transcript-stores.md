# Agent transcript stores

Every coding agent tma watches writes its conversation to disk, and no two of
them agree on how. [`tma transcript`](../reference/cli.md#tma-transcript) reads
five of those stores into one event vocabulary. This page is the honest account
of what that buys and what it does not, because a reader that quietly renders
half a conversation is worse than one that says it cannot.

## What each store is

| agent | store | tail | served |
|---|---|---|---|
| claude | `~/.claude/projects/<cwd-slug>/<session>.jsonl`, plus `<session>/subagents/agent-*.jsonl` | byte offset | yes |
| codex | `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-<iso>-<session>.jsonl` | byte offset | yes |
| gemini | `~/.gemini/tmp/<projectHash>/chats/session-<iso>-<short>.jsonl` | byte offset, with `$set` dropped | yes |
| pi | `~/.pi/agent/sessions/--<cwd-slug>--/<iso>_<session>.jsonl` | byte offset | yes |
| OpenCode | `~/.local/share/opencode/opencode.db` (SQLite) | `event.seq` | yes |
| cursor-agent | `~/.cursor/projects/<cwd-slug>/agent-transcripts/<chat>/<chat>.jsonl` | byte offset | **no** |

Four of the served stores are append-only JSONL and the fifth is a database, and
all five are written *during* the turn rather than at the end of it, so what you
read is what the agent has done so far rather than what it did last time it
finished.

## The refusal

**cursor-agent is refused because its transcript is not one.** It records the
user's prompt, the assistant's prose, a bare `tool_use`, and `turn_ended`. There
is no tool result, no timestamp, no version stamp, and no header record; a
driven run that used a tool wrote the call and then the model's reply, and the
tool's output never landed in the file at all. Rendering that beside a claude
session would produce a screen full of holes that looks like tma is broken.
Returning an empty window would be worse still, because "nothing happened" is a
claim, and it would be false. So the request is refused with `store-incomplete`,
which names the store and the reason.

## OpenCode, the store that is a database

Everything OpenCode writes lives in one SQLite file holding every session it has
ever run, so none of the file reader above applies to it: there is no path to
stat, no byte offset to page from, and no way to read it without a SQLite
client. tma links one (the transcript crate's `opencode` feature, on in the
binary you install) rather than driving the `sqlite3` command, which means an
OpenCode transcript needs nothing on your `PATH` and no second process.

**The reader holds one connection, and that is the whole design.** A
read-only reader that opens a fresh connection for each poll makes the writing
agent's own commits fail: measured over 400 committed appends, 42 of them came
back `database is locked` against a reconnecting reader and none against a
reader holding one connection, which also read about 40 times faster. Opening is
the moment that costs, not reading. Attaching to a WAL database takes a lock the
writer wants, for long enough to lose about one commit in every twenty-five
opens, and opencode's own connections carry no busy timeout to ride that out. A
reader that opens once pays that risk once; one that opens a thousand times pays
it a thousand times. So tma opens each database once and keeps it. It opens read-only twice over (the
`SQLITE_OPEN_READ_ONLY` flag and `mode=ro` in the URI), never writes, and never
checkpoints. It also never asks SQLite to treat the file as immutable, which
would be faster and would be a lie: there is a writer, and telling SQLite
otherwise is how a reader gets silently stale data instead of correct data.

**History and the live tail come from different tables.** OpenCode moved to an
event-sourced store partway through its life, so older sessions have only
`message` and `part` rows while newer ones also have an `event` log keyed
`(aggregate_id, seq)` with a high-water mark per aggregate. That log is the best
tail any of these agents offers, a subscriber stores one integer, but it does
not cover the sessions written before it existed. tma serves every window from
`message` and `part` ordered by `time_created`, and uses the event log only as a
notification that something changed, then re-reads the row it names. The payload
in the log is never the thing rendered, which is what keeps a schema change
there from becoming a wrong transcript here.

**A tool call and its result are the same row.** Where a JSONL store appends a
call record and later a result record, OpenCode mutates one `part` row in place:
`state.status` walks from `pending` to `running` to `completed` or `error`. So
the call event is minted the moment the row appears and the result event only
once it settles, and a row seen three times is one call whose status advanced,
not three events. Two details of that are worth knowing if you are reading the
output: a call waiting on your approval reads as `running`, never `pending`, and
a call you denied settles as `error` with your own feedback quoted in the body.

One more difference from the file stores: OpenCode's cursors are addressed by
`(message timestamp, index)` rather than by byte offset, since a database has no
meaningful "size when the cursor was minted". They are still opaque, still page
backwards without gaps, and are still refused as `cursor-invalid` when they
belong to a different database.

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
