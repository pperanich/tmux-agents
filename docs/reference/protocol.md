# The remote wire protocol

The frames a remote device and a serving `tma` exchange. This page is a contract for the wire, not a
how-to: **there is no command that speaks it yet**. `crates/tma-proto` ships the types, their
versioning discipline and a golden corpus so that the serve loop, when it lands, has one definition
to build against instead of two.

Everything on this page is already pinned by tests. The corpus lives in `crates/tma-proto/vectors/`,
one JSON file per frame, and `crates/tma-proto/README.md` is the guide to changing it.

## Framing

One NDJSON line per message, in both directions.

```json
{"schema":1,"id":"7","t":"window","pane":"%1","last":200,"budget":{"header_bytes":256,"read_bytes":1048576,"frame_bytes":32768}}
```

`schema` is the wire version, stated once per line, on the envelope. `id` is the caller's own
correlation id, echoed on every response to that request; a streamed frame carries the id of the
subscription that opened it. `t` names the frame, and the rest of the line is that frame's body,
inline.

## Requests

| `t` | carries | answered with |
|---|---|---|
| `hello` | the client name, its version, and the device fingerprint | `hello`, or `error` when the envelope's `schema` is one this host does not implement |
| `snapshot` | an optional selector (session, repo, branch, agent, state) | `snapshot` |
| `subscribe` | `events` plus the same selector | `ack`, then a stream of `snapshot` or `edge` frames |
| `card` | a pane id | `card` |
| `window` | a pane id, how many events, a cursor to page back from, and a budget | `window` |
| `event` | a pane id and one cursor | `event`, with the body populated |
| `dispatch` | a slot, a pane, an action, the binder, and the `text` or `answers` payload | `receipt` |
| `receipts` | a slot id, a claim time, or both | `receipts` |

## Responses

| `t` | carries |
|---|---|
| `hello` | the host name, its version, the reconcile interval, and the scopes this device was granted |
| `snapshot` | `agents`, the fleet rows in scope |
| `edge` | one pane's state transition, as the cycle observed it |
| `card` | what a blocked pane is asking: `permission`, `question`, `informational`, or `none` |
| `window` | a page of transcript event headers, newest first, with an `older` cursor |
| `event` | one event with its body |
| `receipt` | what one dispatch resolved to |
| `receipts` | the ledger, filtered |
| `ack` | the request was accepted and has no body of its own |
| `error` | a typed refusal: `unsupported-schema`, `bad-request`, `not-found`, `cursor-invalid`, `scope-denied`, `unsupported`, `internal` |

## What a card carries

A `card` answers "what is this pane asking", typed by variant so the wrong affordance cannot be
expressed. An informational card has no field an approve control could be put in, which is the
plan-dialog bug class made unrepresentable rather than merely unhandled.

**Blocked-ness comes from detection and from nowhere else.** A dangling tool call in a transcript
means a call is in flight, which a slow tool, an open prompt and a crashed process all produce. The
host reads it only to say what a blocked pane is blocked *on*, never that it is blocked, so a pane
the cycle calls `working` has no card whatever its transcript holds.

| `card` | when | carries |
|---|---|---|
| `permission` | `blocked/permission` | the lane, the options, an extraction confidence, the pending call, and the binder |
| `question` | `blocked/question`, with the agent's own question set fetched | the request id and the questions verbatim |
| `informational` | every other blocked detail, a token this build has never heard of included | the detail and a short headline, and nothing to fire |
| `none` | anything the cycle does not call `blocked` | nothing |

### Lanes

`lane` says which transport produced the card and would answer it, so a structured card is visibly
distinguishable from a scraped one and a receipt's reader can tell what "approved" meant.

- **`hook`**: a blocking agent hook parked the request as data. The tool name and the tool input
  arrive as the agent's own object, never as a rendered line, which is the whole reason the lane
  exists: a consent label wraps at a phone width and the wrap is not invertible. Exactly two
  options, `allow-once` and `reject-once`; no always-grant is offered, because R25's second
  deliberate interaction has no surface on this lane yet.
- **`api`**: the reply travels over the agent's own HTTP surface (OpenCode). A fact about the
  transport, not a claim that a dialog was read.
- **`screen`**: everything else. The options are the two actions this host would fire.

### What `extraction: failed` means for the app

`exact` is the only value that licenses drawing the dialog's own controls. `wrapped` and `failed`
both mean **open the pane on the host**, and not merely because a label might be truncated: a
wrapped consent line carries no signal telling a break inside a token from a break on a space, so
rejoining guesses, and a wrong guess yields a different filesystem path inside a consent string.

In this release `failed` is what every non-hook permission card reports, because no dialog extractor
ships for any agent yet. A `hook` card reports `exact` by construction: nothing was extracted. An
option carrying no `option_id` prints no index, so an app must not render one as a keycap: a
position the host invented is not a key the user can type.

## Windows and events

A `window` is a page of transcript event **headers**, newest first, with an `older` cursor to page
back from. Headers only, by construction: every string leaf is capped at the header budget and no
body rides a window, so 200 events cost kilobytes. An `event` request fetches exactly one cursor's
body.

**The device asks and the host clamps.** Every field of `budget` is capped at the host's own
default (`header_bytes` 256, `read_bytes` 1 MiB, `frame_bytes` 32 KiB), and so is `last`. A frame
budget is a promise to the network, so a caller cannot raise it. `budget_truncated` means the byte
budget, not the event count, ended the scan: the page is still exact and `older` pages on.

**Cursors are opaque.** A device only ever echoes back a token the host minted. A cursor stops
addressing its bytes when the file is rewritten, truncated or replaced, and a hand-edited one was
never valid; both earn `cursor-invalid`, whose one correct response is to drop the cursor and ask
for a fresh end-anchored window. `unknown` counts records in the page the reader could not
classify: a store that grows a record type raises it rather than erroring, and somebody still
notices.

Two stores are refused rather than served, each with `unsupported` and a sentence saying why:
cursor-agent's transcript has no tool results, timestamps or version stamp, so a window over it
would render as holes, and OpenCode keeps its conversation in SQLite. An empty window would read as
"nothing happened", which is the failure the refusal exists to avoid.

## What a fleet row carries

Exactly the key set `tma ls --json` emits **minus `title`**, which is why the two are described in
one place: see [Pane options and JSON contracts](pane-options-and-json.md#tma-ls---json). A pane title
is agent-supplied text and rides no frame leaving the machine; `tma_proto::FleetRow` has no such
field, and a test compares its key set against the host's writer so the two cannot drift.

## Versioning

`schema` is `1` and grows additively: a new key keeps it, a removal or a re-typing bumps it. The
closed vocabularies (`state`, `detail`, `outcome`, `reason`, the option kinds, the error codes) grow
the same way, and every one but `state` has an `Other` arm, so a token minted after your build
round-trips intact instead of being collapsed into a neighbour. `state` is closed because the
published state vocabulary is frozen.

Unknown **fields** are ignored on parse and omitted on re-emission. Unknown **variants** are
preserved. The two answers differ on purpose: dropping a field loses detail, dropping a variant
changes meaning.
