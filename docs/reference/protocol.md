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
| `error` | a typed refusal: `unsupported-schema`, `bad-request`, `not-found`, `scope-denied`, `unsupported`, `internal` |

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
