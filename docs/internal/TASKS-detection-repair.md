# Task plan: detection repair (ActivityDelta + "seen")

A worked queue for an agent execution loop. An agent reads this file, takes the first task whose
status is `OPEN` in the lowest-numbered unfinished batch, does it, marks it `DONE`, and stops.
A review gate runs after each batch before the next one opens.

Status vocabulary: `OPEN` (available) · `WIP <agent>` (claimed) · `DONE` · `BLOCKED <reason>`.

---

## 0. Rules every agent follows

1. **Claim before working.** Flip the task's status to `WIP <your id>` and commit that edit alone
   before touching code, so two agents cannot take the same task.
2. **One batch at a time.** Do not start a task in batch N+1 while any batch-N task is not `DONE`
   and its review gate has not passed. Tasks *within* a batch are grouped so one agent can take
   several in a single pass; prefer taking a whole group.
3. **Never weaken a test to make a change pass.** Inverting a test deliberately is fine and is
   called out per task. Deleting an assertion because it now fails is not. If a task seems to
   require it, stop and set `BLOCKED`, with the assertion quoted.
4. **Gate before marking DONE**, from the repo root:
   ```
   cargo fmt --all --check
   cargo clippy --workspace --all-targets --all-features   # must be silent
   cargo test --workspace --all-features --no-fail-fast
   ```
   The suite is 1174 passing as of `ef12d02`. A count that drops without a task explicitly saying
   so is a regression, not a pass.
5. **Commit per group**, Conventional Commits, no AI attribution. Reference the task ids in the body.
6. **Evidence over assumption.** Every manifest rule needs a real capture behind it. Verify any tmux
   claim by running tmux on an isolated socket (`tmux -L <name> -f /dev/null …`, `kill-server` after).
   Never touch the user's default server.
7. **Scratch files** go in the session scratchpad, never the repo.

---

## 1. Verified facts (do not re-derive)

Established with live probes; cited so no agent burns time rediscovering them.

- `fold.rs:291-293`, verbatim: *"Mere silence never decays anything: this gate is only reached with
  positive contrary chrome."* The decay gate is unreachable without contrary chrome, so
  ActivityDelta's corroboration role is redundant wherever it could act.
- `fold.rs:275` is `let candidate = claims.working.or(claims.idle);` — an activity `working` record
  **shadows real idle chrome inside the hook path**, so a user typing at a finished prompt
  corroborates a stuck `working` claim and postpones its recovery decay.
- Rule evidence and the activity push carry **identical** timestamps (`cycle.rs:284` `captured_at: now`,
  `engine.rs:204` `at: snap.captured_at`, `cycle.rs:296` `at: now`). `latest()` (`fold.rs:431`) is
  `max_by_key(|e| e.at)` and Rust returns the **last** of equal keys, and the activity record is
  pushed after rule evidence — so activity systematically shadows the manifest's own working rule
  and `@agent_source` misreports which evidence fired.
- There are **three** producers of the activity push, not two: `cycle.rs:289`, `capture.rs:405`,
  and `debug.rs:148`.
- All six bundled manifests have a positive `working` rule whose chrome is present for the whole
  turn. Only `claude.toml` has `idle` rules.
- The five manifests without an idle rule (`codex`, `cursor`, `gemini`, `opencode`, `pi`) pin a pane
  at `working` forever after a real turn: no idle claim exists, so every later cycle lands on
  `hold previous` (`fold.rs:222-227`). **This needs no user input** and ActivityDelta is not its cause.
- tmux keeps two orthogonal clocks. Verified three times independently:
  `window_activity` moves **only** on pane output; `client_activity` / `session_activity` move
  **only** on real client tty input (including the prefix key and mouse), never on pane output,
  and never on `send-keys` or on tma's own polling (command clients do not appear in `list-clients`).
- `client_activity` is **epoch seconds**. `@agent_since` is ms. Floor the comparison and use strict
  `>`, which makes the error one-directional (can only fail to clear).
- **A control-mode client's `client_activity` freezes at attach time.** Any ordered-input test is a
  no-op for iTerm2 `-CC` users. Filter clients on `#{client_control_mode}`.
- At `after-select-pane` hook time, `#{P:#{?pane_last,#{pane_id},}}` already names the **departed**
  pane. At `after-select-window`, `#{W:#{?window_last_flag,#{P:#{?pane_active,#{pane_id},}},}}`
  names the departed window's active pane. Verified on tmux 3.6a, key-driven and out-of-band.
  Target aliases (`-t '{last}'`) are **not** reliable at hook time — use formats.
- `#{hook_pane}` and `#{hook_client}` expand **empty** on `after-select-pane` / `after-select-window`.
  `#{pane_id}` resolves correctly there. (This was the `ef12d02` bug.)
- `#{pane_unseen_changes}` means "output arrived while the pane was in copy-mode". It is **not** a
  general unseen primitive.
- Appending `\; list-clients -F …` to the existing `list-panes` call is free (~3.2 ms, one process);
  a second `tmux` invocation costs a full extra round trip.
- Idle fixtures already exist at both widths for all five manifests
  (`crates/tma-core/fixtures/{codex,cursor,gemini,opencode,pi}_idle_w{60,100}.txt`).

---

## Batch A — remove ActivityDelta as state evidence

Goal: a changed viewport hash stops being a state claim. It stays a capture-scheduling input
(`can_reuse_stamp` still reads `p.hash`). Fixes the fabricated `done`, the shadowed working rule,
and the keyboard-disarmed decay.

**A1 — delete the three producers.** `DONE`
- `crates/tma-runtime/src/cycle.rs:289-301`, `crates/tma-runtime/src/capture.rs:405-418`,
  `crates/tma-runtime/src/debug.rs:148-160`.
- `tail_hash` must still flow to the stamp; only the evidence push goes.
- Accept: no `Source::ActivityDelta` construction remains in the workspace.

**A2 — retire the `Source` variant, keep the `Provenance`.** `DONE`
- Remove `Source::ActivityDelta` (`evidence.rs:20`) and its `provenance()` arm (`:32`), and the
  name arms at `fold.rs:454` and `debug.rs:522`.
- **Keep `Provenance::Activity`, its token, and its `FromStr` arm** (`evidence.rs:44`, `:53`, `:72`).
  A live tmux server holds `@agent_source=activity` strings and `stamp.rs:242` turns a parse failure
  into a `GrammarError`. Comment it as legacy-only, no longer produced.
- Keep `provenance_token_roundtrip` (`evidence.rs:165`) intact — it guards that compat.
- Accept: an existing server carrying `@agent_source=activity` still decodes.

**A3 — re-source the affected tests (mechanical, no coverage lost).** `DONE`
- `fold.rs:725` `blocker_chrome_beats_activity_working` → rename to `..._beats_working_chrome`.
- `fold.rs:906` `corroborating_evidence_refreshes_hook_and_resets_decay` → `Source::ScreenRule`
  (strictly better: that is the path production actually takes).
- `fold.rs:980` `idle_to_working_is_immediate`, `fold.rs:1053` `pid_change_is_episode_boundary` →
  `Source::ScreenRule`; both properties are source-independent.
- `fold.rs:1129` proptest `arb_non_hook_source` → drop the `Just(Source::ActivityDelta)` arm.
- `evidence.rs:157` `source_folds_to_provenance` → drop only the ActivityDelta assertion.

**A4 — replace, do not delete, the ordering test.** `DONE`
- `fold.rs:735` `activity_working_beats_idle_chrome` becomes meaningless once activity is gone, but
  it is the **only** test asserting working-slot-before-idle-slot ordering (`fold.rs:203-217`) — and
  that ordering becomes *more* load-bearing in batch B, where five manifests gain idle chrome that
  coexists with working chrome.
- Replace with `working_chrome_beats_coexisting_idle_chrome`: two `ScreenRule`/`Title` records at
  equal `at`, asserting `Working`.
- ⚠️ Deleting without this replacement is a silent coverage loss. Do not.

**A5 — amend the normative docs.** `DONE`
- `docs/internal/REQUIREMENTS.md`: F7 (content-hash becomes a capture-scheduling input, not a
  detection source), F8 (drop "then activity delta ⇒ working" — **this is a spec amendment**),
  and the decay wording at `:75` ("corroborating activity" → "corroborating evidence").
- `docs/internal/DAEMON.md:422-427` (drop item 3, renumber) and check `:838`.
- `docs/explanation/detection-model.md:52-53`, `:62`.
- `docs/reference/pane-options-and-json.md:45`, `:311`: annotate `activity` as a legacy
  `@agent_source` value.
- CHANGELOG entry under `## [Unreleased]`, prose, matching the existing voice.

**A6 — R-A cleanup (docs/comment accuracy).** `DONE`
- R-A returned PASS WITH FINDINGS: code correct and complete, 11 findings all documentation or
  comment accuracy. Cleared in one pass.
- Notable: R-A found that NO hash-to-hash comparison remains anywhere in the workspace (verified —
  the activity push was the only one). `@agent_hash`'s VALUE is now write-only; its PRESENCE is a
  "this pane has been captured" flag. Several comments claimed a pairing that no longer happens,
  including one this batch introduced at `snapshot.rs:32`. All corrected.
- Also: `ARCHITECTURE.md:146` still carried the old F8 ladder (a second, disagreeing copy inside the
  AD2 decision record `fold.rs` cites as its spec); the AD2 Question now carries an amendment note
  rather than being rewritten; the hold-writes-hash rationale is marked HISTORICAL; two vestigial
  `evidence.clone()` calls became borrows.

> **Review gate R-A.** `PASSED` (with findings, all cleared in A6). Focus was: did anything lose
> coverage? Is `Provenance::Activity` still decodable? Does `can_reuse_stamp` still work with
> `p.hash`? Any remaining reference to activity-as-evidence in code or docs?
>
> R-A confirmed the A4 replacement genuinely guards slot ordering (swap the ladder arms and it
> fails; `prev = None` so the dwell guard cannot mask it; equal timestamps so `latest()` cannot
> decide it). It also noted a PRE-EXISTING gap for batch B to close: the SECOND ordering site,
> `fold.rs:275` `claims.working.or(claims.idle)` inside `fold_against_hook`, has no test — and
> batch B's coexisting idle chrome will flow through it whenever a hook claim is live. **Add that
> test in B4.**

---

## Batch B — positive idle rules for the five manifests

Goal: close the pinned-`working` trap. **Must land after batch A** — shipping idle rules first would
give these five the missing half of the working→idle transition and spread the fabricated-`done`
symptom from claude to all six.

Per manifest: author one `[[rules]] state = "idle"` at a priority **below** that manifest's working
rule, anchored on invariant composer chrome. Precedent: claude's `⏵⏵` idle rule already coexists with
the working spinner and is disambiguated by slot order (`claude.toml:100-102`).

**Do NOT add `idle` to `[capture].visible`.** That is a separate, riskier change — it would newly let
working chrome decay an idle hook claim. Update the design comments at `gemini_manifest.rs:180` and
`opencode_manifest.rs:229` to say idle now has a rule but stays outside `visible`.

**B1 — codex + gemini.** `OPEN`
- Existing idle fixtures at both widths; confirm they carry the intended anchor with ANSI stripped
  (escapes can sit *between* words — that is how the opencode `esc interrupt` anchor was missed).
- Suggested anchors to verify, not to trust: gemini `Type your message or @path/to/file`,
  codex the `›` composer arrow.

**B2 — opencode + pi.** `OPEN`
- Same shape. opencode's idle row is the pane cwd (per-pane, not matchable) — find invariant chrome
  elsewhere on the idle screen or set `BLOCKED` with what you found.

**B3 — cursor.** `OPEN`
- ⚠️ Its only idle fixture is a **fresh session** (`→ Plan, search, build anything`). A post-turn idle
  screen almost certainly reads `→ Add a follow-up …` without the stop hint. **Capture a real
  post-turn idle screen at both widths before authoring**, redact via `tma debug redact`, and anchor
  on the `→` composer arrow rather than hint text.

**B4 — invert only the idle half of the negative tests.** `OPEN`
- `codex_manifest.rs:261`, `cursor_manifest.rs:275`, `gemini_manifest.rs:246`, `pi_manifest.rs:186`,
  `opencode_manifest.rs:191`.
- These currently assert the idle screen raises **no** state evidence. Only that half inverts, to
  "raises exactly an `idle` claim".
- ⚠️ **The never-false-block assertion must survive verbatim.** Under D2 it is the most
  safety-critical assertion in the suite. A wholesale rewrite of these tests is the likeliest way to
  lose it. Add, per agent, a coexistence test that the **working** fixture also matches the new idle
  rule yet still folds to `working`.

**B5 — docs + changelog for batch B.** `OPEN`
- `docs/reference/agent-coverage.md` per-agent notes; CHANGELOG under `## [Unreleased]`.

> **Review gate R-B.** Focus: is every new rule backed by a real capture at two widths? Did any
> never-false-block assertion get dropped or softened? Does any new idle rule match its own agent's
> *working* fixture in a way that outranks working?

---

## Batch C — seen-on-leave (the core "seen" fix)

Goal: clear the flag on the pane you **depart**, not only the one you arrive at. Fixes the larger
residue (finish while you watch → move to another window → flag survives for hours). Walk-away is
preserved structurally: walking away means not navigating, so no hook fires.

**C1 — make the hook command kind-aware.** `OPEN`
- `crates/tma/src/install.rs:494-514` `clear_attention_command`, and pass the hook name through from
  `install_tmux_hooks` (`:531-554`).
- ⚠️ **Encode the kind as an environment variable, not an argv flag.** The command is deliberately
  late-bound, so a *new* hook string can invoke an *old* binary; an unknown flag would make clap
  error on every pane switch, and the `-x` branch has no `2>/dev/null || true`. An unknown env var
  is ignored silently. Add `2>/dev/null || true` to the `-x` branch while there.
- The existing drift arm rewrites old installs in place, so no migration code is needed.

**C2 — resolve and clear the departed pane.** `OPEN`
- `crates/tma/src/dispatch.rs:11-20`. Read the kind, resolve via the verified formats (§1), unset
  `opt::ATTENTION` there too. Keep the "a focus hook must never error" posture: any failure is a
  silent no-op.

**C3 — a narrow reader, not a wider `list-panes`.** `OPEN`
- New method beside `list_clients` (`crates/tma-tmux/src/tmux/read.rs:144-155`).
- ⚠️ **Do not touch `list_panes_format()` / `FIXED_FIELDS` / `parse_pane_line`.** Adding fields there
  shifts every positional offset for no benefit.

**C4 — tests, including one deliberate inversion.** `OPEN`
- ⚠️ `crates/tma/tests/attention_integration.rs:~225` currently asserts
  *"selecting a different pane must not clear this one"*. That is an over-clearing guard added with
  the `ef12d02` fix, and batch C **inverts it deliberately**. Replace with three tests, all of which
  must exist or the guard has been weakened rather than moved:
  1. departing a pane clears it;
  2. selecting a pane in an **unrelated window** leaves the flag standing (the guard's original job);
  3. a flag raised **after** the departure survives (protects walk-away; currently protected by nothing).
- Re-verify the `pane_last` / `window_last_flag` hook-time ordering on the oldest supported tmux
  before relying on it, and write the test so it fails loudly rather than clearing nothing.

**C5 — docs + changelog.** `OPEN`
- `docs/reference/cli.md` (`clear-attention`), `docs/how-to/install-agent-hooks.md`,
  `docs/explanation/detection-model.md`, `docs/internal/ARCHITECTURE.md`, `docs/internal/DAEMON.md`.
- Note that users must re-run `tma install-hooks` to pick it up.

> **Review gate R-C.** Focus: can a departure clear a pane the user never saw? Does an old binary
> survive the new hook string? Are all three replacement tests present?

---

## Batch D — ordered input clear (secondary layer)

Goal: the residue you actually reported — sitting on the pane, never navigating. Clear iff a client
displays the pane **and** its last input is strictly later than the raise.

**D1 — client view reader.** `OPEN`
- `list-clients -F '#{pane_id}<SEP>#{client_activity}<SEP>#{client_control_mode}'`, appended to the
  existing `list-panes` call as `\; list-clients …` (one process). Filter out control-mode clients.

**D2 — the predicate, pure and unit-tested.** `OPEN`
- Beside `is_done` in `crates/tma-core/src/row.rs`, or a small module. Signature roughly
  `seen(displayed: &[(pane_id, activity_secs)], pane, raised_at_ms) -> bool`.
- Strict `>`, never `>=`. Floor `activity_secs * 1000`. The raise instant is `@agent_since`
  (write-once per state run, so it *is* the raise time whenever the flag is set).
- Unit cases: no clients; client on another pane; client on this pane with older activity
  (**walk-away — must not clear**); newer activity (must clear); two clients where the wrong one is
  active; a control-mode client (must be ignored).

**D3 — wire into the cycle.** `OPEN`
- `crates/tma-runtime/src/cycle.rs`, end of `run_cycle`. Gate on `!stampede_skip` **and** some row
  carrying attention, so the zero-config floor pays nothing in steady state.
- **Mutate `report.rows` to match**, or `tma status` lags a cycle behind its own clear.

**D4 — sequence the clear after notification dispatch.** `OPEN`
- `crates/tma-daemon/src/daemon/serve.rs:377-379`. `notify.rs:50-56` gates on the persisted flag, so
  a clear landing between raise and dispatch eats the desktop notification. The race pre-exists;
  do not widen it. If a notify test turns flaky, fix the ordering — **do not add a sleep**.

**D5 — docs + changelog.** `OPEN`
- Document the invariant in one line: *the done mark survives until your next input while that pane
  is on screen, or until you navigate off it.*
- Note the two honest limits: no-op for control-mode (`-CC`) clients, and the reader who never types.
- Note that `subscribe --events` gains `done → idle` edges, meaning "the user saw it".

> **Review gate R-D.** Focus: can the predicate clear on a pane the human never touched? Is the
> walk-away case still safe? Did any cycle-cost bound get raised silently?

---

## Batch E — deferred, only on explicit go-ahead

**E1 — idle→idle re-signal.** `OPEN (deferred)`
- A second real completion with no observed `Working` between raises nothing, and there is no
  recovery. Fix belongs in the **event intake**, not the fold: `event/mapping.rs:194-198` receives a
  hook that *means* "a turn ended" and throws that away by re-deriving `set_attention` from
  `prev == Some(Working)`. Add a `turn_end` discriminator; `set_attention` for `Idle` becomes
  `turn_end || prev == Some(Working)`.
- Needs exactly **one** new field, `@agent_turn_at` (epoch ms): `@agent_since` is write-once per
  state run, so notify dedup (`event.rs:419-421`) and `wait --since` would both miss the second
  completion. Both comparisons become `max(since, turn_at)`. **Do not move `@agent_since`** — it means
  "in this state since" and the uptime display reads it.
- Adding a key is compat-safe (options are keyed by name; positional layout is per-invocation, not a
  wire format). Add it to `REMOVABLE` (`render.rs:541`) or uninstall leaves it behind.

**E2 — verify the control-mode alert-suppression footgun.** `OPEN (deferred)`
- Claim: an attached control-mode client counts as a viewer, so tma's daemon may be silently
  clearing the user's own tmux activity/silence alert flags for the current window of each monitored
  session. **Not reproduced** in our probe (the test control client never attached). Verify properly,
  and if real, decide whether to document or avoid.

**E3 — dead code.** `OPEN (deferred)`
- `Source::ProcessFact` is declared (`evidence.rs:20` neighbourhood) and produced nowhere.
- `#{alternate_on}` is read into `PaneSnapshot` and consumed only by a debug line, while every agent
  pane is alt-screen — which is why the `scroll_position` freeze has no coverage over in-app
  scrolling. Decide: use it to gate strategy, or document it as diagnostic.

---

## Explicitly rejected (do not resurrect)

- **Suppress-at-set** (never raise on a pane the viewer is on, herdr's design). `wait --until done`
  reads `is_done` directly (`wait.rs:67` → `row.rs:98`), so a pane completing while focused would
  never satisfy `--until done`, and `--since` cannot rescue it because `row.since` is write-once.
  A script would work or hang depending on which pane a human sat in.
- **Any windowed presence test** ("cleared if the user typed within the last N seconds"). Large N
  destroys the walk-away signal; small N does not fix quiet reading. Ordered beats windowed.
- **`pane-focus-out` as the leave hook.** It also fires when the *client* loses focus, so alt-tabbing
  to a browser would clear the flag.
- **`monitor-silence` / `alert-silence`.** Weaker than reading `window_activity` directly, and it
  requires mutating a user-facing global (`silence-action`, whose default `other` suppresses the
  alert for the current window anyway).
- **`#{pane_unseen_changes}` as a seen primitive.** It means copy-mode changes only.
- **Region- or chrome-scoped hashing.** Composer and working chrome share the footer, so no region
  split separates them; and a rule over that region is strictly more precise than a hash of it.
- **A third `Claim` variant** for "something happened". Over-modelling: `SnapshotFacts` is the
  documented vehicle for non-claim facts, and the semantics are not wanted once activity is deleted.
