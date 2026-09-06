# tma-proto

The frames a remote device and `tma serve` exchange, and nothing else. No I/O, no tmux, no serve
loop: types, the versioning discipline that keeps two builds of different ages talking, and the
golden corpus under `vectors/`.

One crate, linked by the host and by the app alike. There is one implementation of this protocol,
so the corpus is a snapshot of the wire rather than an arbitration between two parsers.

## The wire

One NDJSON line per message, in both directions:

```json
{"schema":1,"id":"7","t":"window","pane":"%1","last":200,"budget":{...}}
```

`schema` rides the envelope and nothing inside repeats it, so one line carries exactly one version
claim. `id` is the device's correlation id, echoed on every response to that request. `t` names the
frame. Requests: `hello`, `snapshot`, `subscribe`, `card`, `window`, `event`, `dispatch`,
`receipts`. Responses: `hello`, `snapshot`, `edge`, `card`, `window`, `event`, `receipt`,
`receipts`, `ack`, `error`.

`docs/reference/protocol.md` lists what each frame carries.

## The five rules

1. **Additive only; `SCHEMA` stays 1.** A new field is optional, defaulted on read, and skipped on
   write when it has nothing to say. A removal or a re-typing is `schema = 2`, and the host serves
   both for one release.
2. **Closed vocabularies grow, they never re-mean.** A new `Detail`, `Outcome`, `Reason`,
   `OptionKind` or `ErrorCode` token is additive, because both sides have an `Other` arm. Changing
   what an existing token means is a schema bump.
3. **Unknown fields are ignored; unknown enum variants are preserved.** Opposite answers on
   purpose: dropping a field loses detail, dropping a variant changes meaning. An older device
   meeting a newer host degrades to "I cannot type this dialog", never to "this must be a
   permission prompt".
4. **The device states its schema in the hello.** A schema this build does not implement earns a
   typed `unsupported-schema` error, not a parse failure and not a silent downgrade.
   `RequestFrame::accept_hello` is that check.
5. **Every type and every variant has a vector.** `tests/vectors.rs` reads this crate's own sources
   for the inventory, so a variant added without a vector fails CI.

Two conventions follow from rule 1 and are worth stating, because they look inconsistent until you
know why. A key the **host row already emits** keeps its explicit `null`, so `FleetRow` and the host
writer stay comparable key for key. A key this crate adds for a **request** is skipped when absent,
so an older writer's frame and a newer one's are byte-identical when the value is absent. Both sides
carry `#[serde(default)]`, which is the reader's half of the rule either way.

## Where the tokens come from

Nothing in `State`, `Detail`, `Outcome` or `Reason` was invented here. They are the host's own
published vocabularies, and `crates/tma/tests/proto_drift.rs` asserts that from the host side, where
both halves are in scope: an exhaustive test inside this crate could never notice a token the *host*
grew. The same test compares `FleetRow`'s key set against `tma_ui::surfaces::RowSurface::Protocol`,
which is the definition of what a fleet row carries off the machine.

`FleetRow` has no `title` field and never will. A pane title is agent-supplied text, and keeping it
off this surface is a property of the struct rather than of a redaction pass someone can forget to
run.

## Adding a field

1. Add it to the struct with `#[serde(default)]`, plus `skip_serializing_if` if it is a request
   field (see the two conventions above). Put it at the end unless the host writer puts it
   elsewhere: field order is key order, and key order is pinned.
2. Populate it in `tests/corpus/mod.rs`, in the vector that already covers that type.
3. `TMA_PROTO_BLESS=1 cargo test -p tma-proto --test vectors` rewrites the files.
4. Read the diff. That diff is the wire change, and it is the whole point of the corpus.
5. `mise run test`.

## Adding a variant

The same, plus one step: list the new `Enum::Variant` in that vector's `covers`. A variant with no
vector fails `every_type_and_variant_has_a_vector` with the name it is missing. Most files hold one
frame; a few hold a JSON array of them, because a vocabulary needs every token covered and one frame
carries one token at a time.

Vectors are **synthesized, never captured**. Every string leaf is a closed-vocabulary token or
`x`-fill, and `the_acting_surfaces_carry_no_free_text` enforces that on every snapshot, edge, card,
receipt and notification vector. The one exemption is a permission option's `name`, which carries
the dialog's own line verbatim because a generic label is the consent bug that field exists to
prevent; it gets 4 KiB where every other leaf gets 512.

`vectors/notify-MAXIMAL.json` is pinned rather than derived: it is the longest value this crate will
carry per field, committed so the 2000-byte notification bound is measured against a file instead of
against whatever the corpus holds this week. Regenerate it deliberately and say so in the commit
message.
