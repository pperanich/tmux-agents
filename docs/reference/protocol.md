# The remote wire protocol

The frames a remote device and a serving `tma` exchange. `tma serve --stdio` is the host end;
[Serve tma over ssh](../how-to/serve-over-ssh.md) is how a device gets one started, and
[`tma serve`](cli.md#tma-serve) is the command's own reference.

This page is the contract. `crates/tma-proto` ships the types, their versioning discipline and a
golden corpus, linked by the host and by the app alike, so there is one definition of the wire
rather than one on each side of the pipe. Everything here is pinned by tests: the corpus lives in
`crates/tma-proto/vectors/`, one JSON file per frame, and `crates/tma-proto/README.md` is the guide
to changing it.

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
| `error` | a typed refusal: `unsupported-schema`, `bad-request`, `not-found`, `cursor-invalid`, `scope-denied`, `too-many-connections`, `unsupported`, `internal` |

## A session, frame by frame

`tma serve --stdio --device <id>` is one process per connection: NDJSON requests
on stdin, NDJSON responses and events on stdout, logs on stderr. Nothing but
frames reaches stdout. [Serve tma over ssh](../how-to/serve-over-ssh.md) is how
one gets started; this is what it says once it is.

**The handshake.** The first frame of a session is `hello`, and its refusal is
typed rather than silent:

```
→ {"schema":1,"id":"1","t":"hello","app":"tma-ios","app_version":"1.0","device":"SHA256:0Mn3…"}
← {"schema":1,"id":"1","t":"hello","host":"studio","tma_version":"0.5.13","reconcile_interval_ms":2000,"scopes":["read","act:answer","act:steer"]}
```

The scopes come from the host's device store, never from the request. The
`device` field the client sends is its own claim, used for the host's log line
and for nothing else: the connection's identity is the `--device` argument the
spawner passed after authenticating the caller, and serve trusts that and nothing
in the stream.

A frame whose envelope names a schema this build does not implement earns
`unsupported-schema` and the connection survives it, so a device can downgrade
instead of guessing why the pipe went quiet.

**Converge, then subscribe.** A client reads the fleet once and then asks to keep
receiving it:

```
→ {"schema":1,"id":"2","t":"snapshot"}
← {"schema":1,"id":"2","t":"snapshot","agents":[{"pane":"%5","agent":"claude","state":"blocked",…}]}
→ {"schema":1,"id":"3","t":"subscribe","events":true}
← {"schema":1,"id":"3","t":"ack"}
← {"schema":1,"id":"3","t":"edge","at_ms":1730000001234,"pane":"%5","from":"blocked","to":"working",…}
```

Streamed frames carry the id of the subscribe that opened them, so a client can
tell an event from an answer without tracking state.

**That order is the resume discipline, and it is the whole of it.** There is no
`since` cursor on the stream and no replay buffer, because the stream's first
cycle is its baseline and emits no edges: it establishes what is there rather
than describing how it got there. So a connection that drops mid-stream is
recovered by dialling again, taking one `snapshot`, and subscribing, and
whatever the previous stream already delivered is swallowed by the new baseline
rather than re-sent. Synthesizing "appeared" edges for panes that were already
running would be a lie about when they started, which is the same reason
`tma subscribe --events` behaves this way locally.

`"events": false` streams whole `snapshot` frames instead of edges, suppressed
when a cycle repeats the last one. Either way the cadence is the host's
`reconcile_interval_ms`.

**Dispatching, and the three gates it passes.**

```
→ {"schema":1,"id":"4","t":"dispatch","slot":"%5:1730000000000:approve","host":"studio","pane":"%5",
   "action":"approve","binder":{"expect_episode_ms":1730000000000}}
← {"schema":1,"id":"4","t":"receipt","slot":"%5:…:approve","pane":"%5","action":"approve",
   "outcome":"sent","exit_code":0,"cached":false,"device":"SHA256:0Mn3…","at_ms":1730000002000}
```

In order, and none of the three substitutes for another:

1. **The scope** says *this device may approve*. It is checked before the slot is
   claimed and before any tmux call, so a device holding only `read` gets a
   receipt reading `refused` / `scope-denied` with nothing spent and nothing sent.
   An `exec` action and an action outside the scope table are refused here too.
2. **The slot** says *this approval has not already been sent*. It is a
   caller-supplied idempotency key, claimed **before** the fire, in a per-host
   ledger shared by every serve process and every device. A repeat returns the
   cached receipt and dispatches nothing, which is what makes a re-tap after a
   pocket disconnect safe.
3. **The binder** says *the thing you approved is still on screen*. `expect_episode_ms`
   and `expect_permission_request` are re-checked inside the pane's single-flight
   lock, against the same read the gate is re-asserted from. A pane that moved on
   refuses `episode-changed`; one that no longer carries the quoted id refuses
   `request-gone`. A zero `expect_episode_ms` is "no expectation", which is what a
   client sends when it has nothing to bind to.

`force` is not on this surface at any value: a device is never in the room to
have decided to skip the `when` gate.

**Learning an outcome without dispatching for it.** This is the transport's
normal case rather than its edge case, because a phone is suspended mid-request
as a matter of routine:

```
→ {"schema":1,"id":"5","t":"receipts","slot":"%5:1730000000000:approve"}
← {"schema":1,"id":"5","t":"receipts","receipts":[{"slot":"%5:…","outcome":"sent","cached":true,…}]}
```

The ledger is a file, not connection state, so the receipt a lost response was
carrying is there on the next connection, through a different serve process, and
answerable to a different device than the one that dispatched.

**Revocation.** `tma device revoke` removes the record, and every live connection
re-reads the store per request and per publish. The next request is refused
`scope-denied`, the stream stops, and the process exits: absence of the *record*
is not absence of a *scope*, and a revoked device is not a read-only one.

**Reading one pane.** `card`, `window` and `event` all name a pane, and a pane
this host does not have is `not-found` on each of them:

```
→ {"schema":1,"id":"6","t":"card","pane":"%5"}
← {"schema":1,"id":"6","t":"card","card":"permission","pane":"%5","agent":"claude","lane":"hook",…}
→ {"schema":1,"id":"7","t":"window","pane":"%5","last":50}
← {"schema":1,"id":"7","t":"window","pane":"%5","agent":"claude","older":"t1.…","events":[…]}
```

One transcript reader is held for the life of the connection, so its per-file
stat memo and its OpenCode database handle survive between requests. That is not
a cache for speed: a reader that reconnected per page would make the writing
agent's own commits fail, which is a far worse bug than a slow page.

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

### What the host gathers for one

The pane's own read comes first, and it is the same read a `dispatch` gates on, so a card and the
dispatch it invites describe one pane rather than two readings of it. It supplies the state, the
detail, the episode the binder quotes, the permission-request id, and the pending tool and call the
agent's hook stamped. Everything after it is **best-effort, and none of it can refuse the card**:

- **The parked hook record**, when `@agent_permission_request` names one on disk. Its presence is
  the whole difference between the `hook` lane and the `screen` one: a hold that already expired
  left none behind, and the same pane degrades to `screen` / `failed`.
- **The `approve` and `deny` labels** for that agent, from the loaded action manifests. An agent no
  bundled action covers is offered nothing at all, rather than a control this host could not fire.
- **The pending question**, for a pane blocked on one: a `GET /question` against the endpoint
  `@agent_api_endpoint` resolved, bounded at **750 ms**, and asked only when the pane carries a
  `@agent_question_request` id. The request loop answers one frame at a time, so the fetch is short
  by construction; a server that does not answer in time yields an informational card, which is
  something to read, rather than a stalled connection.
- **The transcript tail**: roughly twenty headers off the end of the pane's own file, read only to
  fill `pending_call` when the pane's stamps did not. A pane whose transcript this host cannot
  resolve still gets its card.

Only the pane read itself can fail the request, and a pane that is not there is `not-found`. A
device that gets no frame cannot even fall back to opening the pane on the host, which is why every
other gap subtracts detail instead.

## Windows and events

A `window` is a page of transcript event **headers**, newest first, with an `older` cursor to page
back from. Headers only, by construction: every string leaf is capped at the header budget and no
body rides a window, so 200 events cost kilobytes. An `event` request fetches exactly one cursor's
body.

**The device asks and the host clamps.** Every field of `budget` is capped at the host's own
default (`header_bytes` 256, `read_bytes` 1 MiB, `frame_bytes` 32 KiB), and so is `last`: absent it
is 200, and it is clamped to 1000 however large a number arrives. A frame budget is a promise to the
network, so a caller cannot raise it. `budget_truncated` means the byte budget, not the event count,
ended the scan: the page is still exact and `older` pages on.

**Which file is served** comes from the pane and never from the request. The host resolves it from
`@agent_transcript` (the path the agent's own hook payload named, so it is one `stat`), falling back
to walking the store's layout from `@agent_session` and the pane's working directory. A device names
a pane; it cannot name a path.

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
