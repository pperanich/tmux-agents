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
8. **Take a review agent's evidence; re-verify its remedy.** Gate R-B diagnosed a real defect and
   proposed a fix that would have shipped a false `idle` over `blocked` — the one defect this
   project most guards against. The diagnosis was worth acting on; the remedy was not. Prove any
   suggested fix against the fixtures yourself before applying it. Same rule for this plan: it has
   been wrong three times already (see B6 and batch B's outcome notes).

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
  pane. On a window change, `#{W:#{?window_last_flag,#{P:#{?pane_active,#{pane_id},}},}}`
  names the departed window's active pane. Verified on tmux 3.6a, key-driven and out-of-band.
  Target aliases (`-t '{last}'`) are **not** reliable at hook time — use formats.
- **Resolve that window format on `session-window-changed`, never on `after-select-window`** (C6).
  tmux runs `after-select-window` even for a `select-window` onto the window you are already in, and
  `window_last_flag` is stale there — it names whatever window was left however long ago, and the
  whole expansion resolves in the *client's* session rather than the selected one. There is no
  format that says "the current window really changed": `session_set_current` updates `lastw` only
  on a genuine switch and returns early otherwise, so the arrival window reads `window_last_flag=0`
  either way. `session-window-changed` is emitted only on a real change (4 no-ops, 0 firings,
  attached and detached), and additionally covers `new-window` and a current-window `kill-window`.
  Re-selecting the already-active PANE fires no hook at all, so that arm needs no equivalent.
- `#{window_active}` is per session — each session's current window reads `1` regardless of which
  session tmux would call current — and it resolves from a window target or a pane target alike.
  It is what lets a caller skip a `select-window` that would change nothing.
- `#{hook_pane}` and `#{hook_client}` expand **empty** on `after-select-pane` / `after-select-window`.
  `#{pane_id}` resolves correctly there. (This was the `ef12d02` bug.)
- `#{pane_unseen_changes}` means "output arrived while the pane was in copy-mode". It is **not** a
  general unseen primitive.
- Appending `\; list-clients -F …` to the existing `list-panes` call is free (~3.2 ms, one process);
  a second `tmux` invocation costs a full extra round trip.
- Idle fixtures already exist at both widths for all five manifests
  (`crates/tma-core/fixtures/{codex,cursor,gemini,opencode,pi}_idle_w{60,100}.txt`).
- **Session-change hook facts** (F/F2, tmux 3.6a unless noted). `client-session-changed` fires
  exactly ONCE per switch, in the ARRIVAL session's context; an attach is itself a
  `client-session-changed` (`cmd_attach_session` → `server_client_set_session`), which is why a
  probe that installs its hook before attaching reads two firings and misdiagnoses the second;
  `client_last_session` is STALE on `switch-client -t <the session you are already on>`, because
  `server_client_set_session` updates `last_session` only under `c->session != s` but notifies
  unconditionally (identical in 3.2).
- **`pane-focus-out` also fires on a session change**, and the record used to deny it. Measured
  with `focus-events` OFF (the default), isolated socket, real PTY client: it fires on a genuine
  `switch-client` in BOTH directions naming the departed pane in `#{pane_id}`, and on none of the
  three no-ops (pane, window, session). It ALSO fires on a clean `detach-client` (not on a killed
  client), and on every overlay — `display-menu`, `display-popup`, `display-panes` — because
  `server_client_set_overlay` calls `window_update_focus` ungated. It does NOT fire while any other
  attached client still has that window current, control-mode clients included, which is both a
  safety property and what makes it inert under tma's daemon. With `focus-events on` it adds every
  pane switch, every window switch, and the terminal's own focus-loss report. Nested tmux
  propagates it only when the INNER server has `focus-events on`. Below tmux 3.3 it is not emitted
  at all without `focus-events on` (3.2 `server-client.c:1368` runs the focus scan behind
  `if (focus)`; 3.3 moved focus to event call sites). Full write-up in ARCHITECTURE.md.
- **A `tmux -C` client is a real viewer** (E2, tmux 3.6a, pipes and no tty as `control.rs`
  spawns it): `session_attached` 0→1, `window_active_clients` 0→1,
  `flags=attached,focused,control-mode`. On the **current window of a session with no other
  client** it suppresses `window_activity_flag`, `window_silence_flag` **and**
  `window_bell_flag`, and clears one already set. Background windows are unaffected.
  `destroy-unattached` also stops firing for that session. No `attach-session -f` flag
  (`read-only`, `ignore-size`, `no-output`, all three) and no `activity-action`/`silence-action`
  value re-arms the flag: the gate is upstream of the action. No size effect. Full write-up in
  DAEMON.md, "Known cost: the control client counts as a viewer".

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

**B1 — codex + gemini.** `DONE`
- Existing idle fixtures at both widths; confirm they carry the intended anchor with ANSI stripped
  (escapes can sit *between* words — that is how the opencode `esc interrupt` anchor was missed).
- Suggested anchors to verify, not to trust: gemini `Type your message or @path/to/file`,
  codex the `›` composer arrow.

**B2 — opencode + pi.** `DONE`
- Same shape. opencode's idle row is the pane cwd (per-pane, not matchable) — find invariant chrome
  elsewhere on the idle screen or set `BLOCKED` with what you found.

**B3 — cursor.** `DONE`
- ⚠️ Its only idle fixture is a **fresh session** (`→ Plan, search, build anything`). A post-turn idle
  screen almost certainly reads `→ Add a follow-up …` without the stop hint. **Capture a real
  post-turn idle screen at both widths before authoring**, redact via `tma debug redact`, and anchor
  on the `→` composer arrow rather than hint text.

**B4 — invert only the idle half of the negative tests.** `DONE`
- `codex_manifest.rs:261`, `cursor_manifest.rs:275`, `gemini_manifest.rs:246`, `pi_manifest.rs:186`,
  `opencode_manifest.rs:191`.
- These currently assert the idle screen raises **no** state evidence. Only that half inverts, to
  "raises exactly an `idle` claim".
- ⚠️ **The never-false-block assertion must survive verbatim.** Under D2 it is the most
  safety-critical assertion in the suite. A wholesale rewrite of these tests is the likeliest way to
  lose it. Add, per agent, a coexistence test that the **working** fixture also matches the new idle
  rule yet still folds to `working`.

**B5 — docs + changelog for batch B.** `DONE`
- `docs/reference/agent-coverage.md` per-agent notes; CHANGELOG under `## [Unreleased]`.

**Batch B outcome** (for R-B; anchors as shipped, all verified ANSI-stripped against that agent's
idle AND working/blocked fixtures):
- codex: `line_regex '^›'` in `tail_lines(6)`, with `not { line_regex '^› \d+\. ' }`. The approval
  dialog's `› 1. Yes, proceed (y)` is SIX rows from the end, not seven — §1's plan had it outside
  the window, and it is not. The `not` leaf is what excludes it; the window is what excludes the
  transcript's own `› <user message>` echoes.
- gemini: `contains "Type your message or @path/to/file"` on `visible`. Absent from both blocked
  captures (the dialog replaces the composer).
- opencode: `contains "ctrl+p commands"` on `visible`. The plan's note that "opencode's idle row is
  the pane cwd" was about the wrong row: the composer's status row ends with this invariant hint,
  and it is absent from all three blocked fixtures.
- pi: `all [ line_regex '^─{20,}$', line_regex '\d+(\.\d+)?%/\d+k' ]` on `visible`. The manifest's
  old note ("renders in both states, so no positive idle chrome to anchor") was the actual bug —
  co-rendering chrome is claude's `⏵⏵` shape, not a disqualifier.
- cursor: `all [ line_regex '^\s*▄{10,}\s*$', line_regex '^\s*→ ' ]` on `visible`. NOT blocked: a
  real post-turn screen was driven on cursor-agent 2026.08.11 and captured at both widths
  (`cursor_idle_post_turn_w{100,60}.txt`). It reads `→ Add a follow-up`, as predicted — so the rule
  anchors on the composer FRAME, since the arrow alone also prefixes `→ Run (once) (y)` in the
  approval dialog. Those captures also confirm the title does not revert after a turn (theirs is
  `Just OK`, a conversation summary), which is now a test.
- Priority below the working rule is documentary only: `engine.rs:176-183` uses priority to break
  ties WITHIN one state, and cross-state order is decided solely by the fold's slot ladder. The
  priorities were still set below to match claude and to keep the manifests self-describing.
- R-A's flagged gap is closed: `fold::tests::working_chrome_beats_coexisting_idle_chrome_under_a_hook_claim`
  covers `fold.rs:275`. Mutation-checked (swap the `or` arms and it fails), with a control arm
  proving it is not passing for want of any working path.
- Suite: 1174 → 1192 passing, 0 failed.

> **Review gate R-B.** `PASSED` (with findings, cleared in B6). Focus: is every new rule backed by a
> real capture at two widths? Did any never-false-block assertion get dropped or softened? Does any
> new idle rule match its own agent's *working* fixture in a way that outranks working?

**B6 — clear the R-B findings.** `DONE`
- One functional finding, six documentation/coverage ones.
- ⚠️ **Finding 1 (functional).** gemini's idle anchor is the composer PLACEHOLDER, which gemini draws
  only while the composer is EMPTY. A pane holding a draft loses the anchor and falls back to
  `hold previous` — the very trap batch B exists to close. R-B suggested mirroring cursor's frame
  anchor (`▄{10,}` + `^\s*>\s`); **that suggestion is unsafe and must not be applied.** Both gemini
  blocked fixtures contain both leaves (prior user messages render as framed transcript echoes), so
  the rule would ship a false `idle` on the approval prompt, violating D2. Find an anchor that is
  both draft-robust and provably absent from `gemini_blocked_w{100,60}.txt`; if none exists, KEEP the
  placeholder and replace the manifest comment with an honest statement of the limitation.
- **Finding 5 (coverage).** gemini has no blocked-raises-no-idle test; codex, cursor and opencode
  each got one. Add the equivalent to `crates/tma-core/tests/gemini_manifest.rs`.
- **Findings 2, 3, 4, 6, 7 (documentation accuracy).** `codex_manifest.rs:330-333` (the `tail_lines(6)`
  comment contradicts `codex.toml:196-199`; the `not` leaf is the guard, not the window),
  `opencode.toml:61-68` (stale "idle carries no rule" note), `gemini.toml:149-150` ("no state-unique
  anchor" contradicts the rule below it), `codex.toml:193-194` (measured distances wrong),
  `codex.toml:199-200` (overstates the `not` leaf's precision).

**B6 outcome.** All seven cleared; suite 1192 → 1194, 0 failed.
- Finding 1: a draft-robust anchor DOES exist, so the placeholder is gone. gemini's idle rule is now
  `line_regex '^\s*▀{10,}\s*$'` on `tail_lines(8)`: the composer box's bottom edge, which is
  draft-independent because the box grows UPWARD as its content wraps while its bottom edge stays
  pinned above the two-row `workspace / sandbox / model` footer.
  **The window is the safety, not the glyph.** Measured distance from the last captured row to that
  edge: 5 (idle_w100), 6 (idle_w60), 4 (working_w100), 5 (working_w60) — window 8, so two extra
  footer rows still match. Distance to the nearest `▀` on a blocked screen: 23 (blocked_w100),
  26 (blocked_w60) — a 15-row margin. Structurally the margin cannot close: gemini's approval dialog
  replaces the composer AND the footer and is itself eleven rows tall, so a transcript echo cannot
  reach the bottom eight rows even flush against them.
  R-B's suggested `visible`-region frame rule was confirmed unsafe and NOT applied; it is now a
  control arm inside `blocked_screen_raises_no_idle_claim`, asserting that the same leaf on
  `visible` does match both blocked captures. That is the guard against anyone widening the region.
- Finding 5 plus one more: `blocked_screen_raises_no_idle_claim` and
  `idle_rule_survives_a_non_empty_composer` (splices a two-row draft over the placeholder in the
  real idle captures). Both mutation-checked: reverting the region to `visible` fails only the
  first, reverting the anchor to the placeholder fails only the second.
- Findings 2/6/7: `codex_manifest.rs` and `codex.toml` no longer disagree. The `› <n>. ` option row
  is at exactly 6, INSIDE `tail_lines(6)`; the `not` leaf is the guard, the window is the
  transcript-echo filter. Real echo distances are 10/14 (working) and 20/21 (blocked), floor ten,
  not fourteen. The `not` leaf's false negative (a draft beginning `1. `) is now stated.
- Findings 3/4: the stale "idle carries no rule" notes in `opencode.toml` and `gemini.toml` are
  rewritten, following pi.toml's precedent.
- Doc sites naming the old gemini anchor were corrected too: `DAEMON.md:301`,
  `agent-coverage.md:314`, and the `## [Unreleased]` CHANGELOG entry.

---

## Batch C — seen-on-leave (the core "seen" fix)

Goal: clear the flag on the pane you **depart**, not only the one you arrive at. Fixes the larger
residue (finish while you watch → move to another window → flag survives for hours). Walk-away is
preserved structurally: walking away means not navigating, so no hook fires.

**C1 — make the hook command kind-aware.** `DONE`
- `crates/tma/src/install.rs:494-514` `clear_attention_command`, and pass the hook name through from
  `install_tmux_hooks` (`:531-554`).
- ⚠️ **Encode the kind as an environment variable, not an argv flag.** The command is deliberately
  late-bound, so a *new* hook string can invoke an *old* binary; an unknown flag would make clap
  error on every pane switch, and the `-x` branch has no `2>/dev/null || true`. An unknown env var
  is ignored silently. Add `2>/dev/null || true` to the `-x` branch while there.
- The existing drift arm rewrites old installs in place, so no migration code is needed.

**C2 — resolve and clear the departed pane.** `DONE`
- `crates/tma/src/dispatch.rs:11-20`. Read the kind, resolve via the verified formats (§1), unset
  `opt::ATTENTION` there too. Keep the "a focus hook must never error" posture: any failure is a
  silent no-op.

**C3 — a narrow reader, not a wider `list-panes`.** `DONE`
- New method beside `list_clients` (`crates/tma-tmux/src/tmux/read.rs:144-155`).
- ⚠️ **Do not touch `list_panes_format()` / `FIXED_FIELDS` / `parse_pane_line`.** Adding fields there
  shifts every positional offset for no benefit.

**C4 — tests, including one deliberate inversion.** `DONE`
- ⚠️ `crates/tma/tests/attention_integration.rs:~225` currently asserts
  *"selecting a different pane must not clear this one"*. That is an over-clearing guard added with
  the `ef12d02` fix, and batch C **inverts it deliberately**. Replace with three tests, all of which
  must exist or the guard has been weakened rather than moved:
  1. departing a pane clears it;
  2. selecting a pane in an **unrelated window** leaves the flag standing (the guard's original job);
  3. a flag raised **after** the departure survives (protects walk-away; currently protected by nothing).
- Re-verify the `pane_last` / `window_last_flag` hook-time ordering on the oldest supported tmux
  before relying on it, and write the test so it fails loudly rather than clearing nothing.

**C5 — docs + changelog.** `DONE`
- `docs/reference/cli.md` (`clear-attention`), `docs/how-to/install-agent-hooks.md`,
  `docs/explanation/detection-model.md`, `docs/internal/ARCHITECTURE.md`, `docs/internal/DAEMON.md`.
- Note that users must re-run `tma install-hooks` to pick it up.

**C6 — repair the R-C failure: a no-op `select-window` over-clears.** `DONE`
- R-C returned **FAIL**. `select-window -t <the already-current window>` DOES fire
  `after-select-window` on 3.6a, and at that moment `window_last_flag` still names whatever window
  the user left however long ago — so batch C's departure format clears `@agent_attention` on a pane
  the user has not visited since. That breaks the invariant batch C exists to protect, and it is new
  in batch C. The `after-select-pane` arm is clean (re-selecting the active pane fires no hook).
- Reachable from tma itself: `Tmux::focus` (`crates/tma-tmux/src/tmux/display.rs:82`) runs
  `select-window` unconditionally, so every `tma jump` / picker Enter-jump landing in the window you
  are already in clears the far window's flag. Also reachable as stock `prefix <N>` onto the current
  window, `choose-tree` onto it, or any script running `select-window -t :0`.
- **No format-only fix exists** (R-C checked): on a genuine switch `session_set_current` updates
  `lastw`, on a no-op it returns early, and the arrival window's `window_last_flag` is `0` either
  way. Nothing in the hook-time vocabulary says "the current window actually changed".
- Ship the proven short-circuit in `Tmux::focus` (skip `select-window` when the destination window
  is already its session's current window; discriminator `display-message -p -t <target>
  '#{window_active}'`). Then decide the general case on live probes, not on argument — the
  `@tma_focus_window` memo R-C sketched is UNPROVEN and must not be applied on its say-so. If it
  does not hold up, ship the short-circuit alone and record the residual over-clear path honestly.
- Regression test required, in the existing witness-pane style: a no-op `select-window` of the
  already-current window must NOT clear a flag on the previously-visited window's pane.
- Also fix three inaccurate comments R-C found (findings 2-4): `read.rs:200-202` (the
  one-directional race claim is false for the `SelectWindow` arm, and a programmatic `select-pane`
  can move a background window's active pane), `read.rs:196` (the untargeted-`display-message`
  direction is arbitrary, not determinate — R-C observed the opposite), and `install.rs:1399` (two
  hooks rendering the same string read as CURRENT, not drift; the test is right, its reason is not).

**Batch C outcome** (for R-C):
- Hook-time formats RE-VERIFIED on tmux 3.6a before building on them, out-of-band and key-driven (a
  pty-attached client fed `prefix o` / `prefix n` / `prefix p`): `#{P:#{?pane_last,#{pane_id},}}` at
  `after-select-pane` and `#{W:#{?window_last_flag,#{P:#{?pane_active,#{pane_id},}},}}` at
  `after-select-window` both name the departed pane. §1 stands.
- **One fact §1 does not carry, and the design leans on it:** the formats must be resolved with
  `display-message -t <arrival pane>`. An UNtargeted query answers for whichever session tmux calls
  "best", and that is neither stable nor the hook's session — probed both ways on 3.6a (the
  first-created session won one arrangement, the last-created won another). With no `-t` a
  navigation in one session can clear a flag in another. Guarded by
  `the_departure_lookup_stays_inside_the_arrival_panes_session`, which asserts both directions so
  whichever way tmux would guess, one arm catches it; it is driven by direct invocation with
  `TMUX_PANE` removed, because tmux's own environment otherwise pins the query and hides the bug.
- Also probed, and load-bearing for walk-away: selecting the ALREADY-ACTIVE pane does not fire
  `after-select-pane` at all, so re-selecting where you already are cannot clear a stale `pane_last`.
- C1 as specified: `TMA_HOOK_KIND=<hook name>` as a shell env prefix, no argv change, `2>/dev/null
  || true` now on both branches. Confirmed no migration code is needed —
  `hook_drift_is_path_aware_not_substring` now asserts a kindless hook string reads as drift, which
  is what makes the existing drift arm rewrite it in place.
- C3: `Tmux::departed_pane` + a `DepartureKind` enum, added beside `list_clients`.
  `list_panes_format()` / `FIXED_FIELDS` / `parse_pane_line` untouched.
- C4: the `ef12d02` over-clearing assertion is inverted, and all three replacements exist —
  `departing_a_pane_clears_its_attention_flag`,
  `an_unrelated_windows_pane_switch_leaves_the_flag_standing`,
  `a_flag_raised_after_the_departure_survives` — plus
  `departing_a_window_clears_the_pane_it_was_showing` (the second format) and the cross-session one
  above. Every "the flag survived" assertion is paired with a WITNESS pane that must have been
  cleared by the same hook, so a hook that silently does nothing fails loudly instead of passing.
  Mutation-checked, all four mutants caught: swap the two formats (4 fail), resolve both kinds on
  every hook (only the unrelated-window guard fails, which is its job), make the departure clear a
  no-op (4 fail), drop the `-t` (the cross-session test fails).
- Judgement call for the reviewer: the plan put the departure resolution in tma (C2/C3) rather than
  expanding the format inside the hook string. Kept, for two reasons — the gnarlier
  `after-select-window` format never has to survive nested tmux/sh quoting, and the integration
  tests exercise the real resolution instead of a hand-copied format string. The cost is one extra
  `display-message` per navigation and a race with a second navigation; the race is one-directional
  by construction (`pane_last` only ever names a pane that WAS active, so a late answer can name a
  pane departed slightly later, never one never departed).
- Suite: 1194 → 1201 passing, 0 failed.

**C6 outcome** (for the re-run of R-C):
- **Defect reproduced first**, on an isolated socket, before anything was changed: three `SW` lines
  where the third is a `select-window` of the already-current window reporting a departed pane.
- **The general fix is a different hook, not a memo.** tmux's control-mode notifications are also
  hook names (man page, HOOKS: *"All the notifications listed in the CONTROL MODE section are
  hooks"*), and `session-window-changed` is emitted only when a session's current window really
  changed. The `@tma_focus_window` memo R-C sketched was NOT built: it has a hole the probes found
  first — `new-window` moves a session's current window with no `after-select-window` at all, so the
  memo goes stale and the very next no-op is mis-detected as genuine. The notification hook has no
  such hole because tmux maintains the fact, not us.
- **Probe evidence, tmux 3.6a.** Detached, out-of-band: 4 no-op `select-window`s fired
  `after-select-window` every time, `session-window-changed` zero times; every genuine change fired
  both. Attached and key-driven through a pty client (`prefix 1`, `prefix n`, `prefix c`): same
  result, and `prefix c` (new-window) fires `session-window-changed` with the departed window's
  active pane, which `after-select-window` never saw at all — so leaving a window by creating one
  now clears it. `kill-window` of the current window fires it with an EMPTY departure (`Ok(None)`,
  no clear). Format context at hook time is the CHANGED session's, `#{pane_id}` is the arrival pane.
  Detach/reattach fires nothing; `attach -t <sess>:<win>` fires it with the previous window departed.
- **A worse cross-session face of the same bug, found while probing**: on a no-op `select-window` in
  `s1`, `after-select-window` expands in the CLIENT's session (`s2`) — arrival pane, departed pane
  and all — so today's code could clear a flag in a session the navigation never touched. Gone with
  the hook, since `session-window-changed` never fires for a no-op.
- **Retired, not merely dropped.** `install_tmux_hooks` removes tma's `after-select-window` entry
  (ours-only, by content), and `DepartureKind::from_hook_name` no longer maps that name — so a hook
  string surviving on a server, or hand-wired from the old docs, can only do the arrival clear,
  which is the pre-C behaviour. Two independent layers, either alone sufficient.
- **`Tmux::focus` short-circuit shipped too**, as R-C proved it: `display-message -p -t <window>
  '#{window_active}'` is per-session (verified across two sessions with neither attached), and the
  `select-window` is skipped when it reads `1`. tma therefore stops firing anyone else's
  `after-select-window` on a jump that moves nothing.
- **Tests, each verified to FAIL on the pre-C6 tree** (source reverted, tests kept):
  `a_no_op_window_selection_leaves_the_window_you_left_alone` (fails at its liveness assertion),
  `the_retired_window_hook_can_only_clear_the_pane_you_arrived_at` (fails with the defect verbatim:
  *"a retired hook string still on the server cleared %1"*), `departing_a_window_clears_the_pane_it_was_showing`
  (retargeted), `jump_within_the_current_window_does_not_reselect_it`,
  `install_retires_the_window_hook_it_replaced`, plus a `from_hook_name` unit test. The witness
  pattern is kept and extended: because the correct behaviour for a no-op is that NO hook runs, the
  no-op test proves liveness with a genuine switch afterwards instead of a witness pane.
- **One test rewritten, deliberately, not weakened**: `each_hook_renders_its_own_command` asserted
  every hook renders a distinguishable command, for a stated reason that R-C showed is false (two
  identical strings read as CURRENT, not drift) and that is now unsatisfiable (two kindless hooks
  render identically). It is `each_hooks_command_carries_exactly_its_own_kind`, which pins each
  command completely — kind-carrying hooks name themselves and stay pairwise distinct, kindless ones
  carry no kind at all — so it is strictly stronger than what it replaced.
- Comment fixes: `read.rs` departure race (one-directional on the pane arm ONLY; a programmatic
  `select-pane` can move a background window's active pane), `read.rs` untargeted `display-message`
  (arbitrary, not determinate), `install.rs` distinctness reason (CURRENT, not drift).
- Suite: 1201 → **1206 passing, 0 failed** (`cargo fmt --all --check` clean, clippy silent).

**Known gap, recorded for batch D/E, not a defect of C — SETTLED in batch F, re-settled in F2:**
departing a whole SESSION while an agent is finishing there leaves exactly the residue seen-on-leave
set out to kill, and batch D's ordered-input clear does not reach it either (the user is by then
typing in a different session). C's reading of the mechanism was wrong twice and both corrections
are below: `switch-client` does NOT fire "neither focus hook" — it fires `client-session-changed`,
and `session-window-changed` too when it changes the target session's current window (C7), and
`pane-focus-out` as well (F2). The decision is WONTFIX; see batch F, batch F2, and ARCHITECTURE.md
under "the session departure that stays open".

> **Review gate R-C.** Focus: can a departure clear a pane the user never saw? Does an old binary
> survive the new hook string? Are all three replacement tests present?
> **First pass: FAIL** — the no-op `select-window` over-clear above; repaired in C6, re-run needed.

---

**C7 — clear the R-C re-run findings.** `DONE`
- R-C re-run returned PASS WITH FINDINGS (5, all documentation/nit). Cleared.
- Recorded the thing C6's outcome note framed only as a benefit: the residual over-clear **moved
  rather than vanished**. `session-window-changed` fires for a real window change in ANY session,
  so a non-`-d` `new-window` or `attach -t sess:win` against a background session clears that
  session's departed pane. Narrower than what it replaced (`-d` is the scripting default and fires
  nothing; a background `select-window` over-cleared before this change too), and the pane cleared
  is one the user genuinely last had current there. Net positive, but it is a real trade and batch
  D/E will reason on top of it.
- Corrected `install-agent-hooks.md`, which still told users the contract was "on every
  `select-pane` and `select-window`" — the exact claim C6 falsifies, eight lines below the
  paragraph that says the opposite.
- Corrected ARCHITECTURE's `switch-client` gap: it fires `client-session-changed`, and ALSO
  `session-window-changed` when it changes the target session's current window. Only the bare
  `-t <session>` form fires neither. The consequence (neither ever reports the pane in the session
  you LEFT) stands — but batch D/E would have built on a wrong mechanism.
- Added `all_tmux_hooks_covers_everything_install_can_write`, driven by the `RETIRED_TMUX_HOOKS`
  const. `ALL_TMUX_HOOKS` is the hand-maintained removal set; a hook installed but absent from it
  is orphaned on the user's server forever. Mutation-checked.

> **Review gate R-C.** `PASSED` on re-run (first run FAILED; see C6). The re-run verified the fix
> holds at the tmux **3.2** floor by reading `session_set_current` in both 3.2 and 3.6a sources,
> simulated the full upgrade path, and hit the one rewritten test with seven mutants — three of
> which the ORIGINAL test would have missed, including one that would have silently permitted
> re-adopting the buggy hook.

---

## Batch D — ordered input clear (secondary layer)

Goal: the residue you actually reported — sitting on the pane, never navigating. Clear iff a client
displays the pane **and** its last input is strictly later than the raise.

**D1 — client view reader.** `DONE`
- `list-clients -F '#{pane_id}<SEP>#{client_activity}<SEP>#{client_control_mode}'`, appended to the
  existing `list-panes` call as `\; list-clients …` (one process). Filter out control-mode clients.

**D2 — the predicate, pure and unit-tested.** `DONE`
- Beside `is_done` in `crates/tma-core/src/row.rs`, or a small module. Signature roughly
  `seen(displayed: &[(pane_id, activity_secs)], pane, raised_at_ms) -> bool`.
- Strict `>`, never `>=`. Floor `activity_secs * 1000`. The raise instant is `@agent_since`
  (write-once per state run, so it *is* the raise time whenever the flag is set).
- Unit cases: no clients; client on another pane; client on this pane with older activity
  (**walk-away — must not clear**); newer activity (must clear); two clients where the wrong one is
  active; a control-mode client (must be ignored).

**D3 — wire into the cycle.** `DONE`
- `crates/tma-runtime/src/cycle.rs`, end of `run_cycle`. Gate on `!stampede_skip` **and** some row
  carrying attention, so the zero-config floor pays nothing in steady state.
- **Mutate `report.rows` to match**, or `tma status` lags a cycle behind its own clear.

**D4 — sequence the clear after notification dispatch.** `DONE`
- `crates/tma-daemon/src/daemon/serve.rs:377-379`. `notify.rs:50-56` gates on the persisted flag, so
  a clear landing between raise and dispatch eats the desktop notification. The race pre-exists;
  do not widen it. If a notify test turns flaky, fix the ordering — **do not add a sleep**.

**D5 — docs + changelog.** `DONE`
- Document the invariant in one line: *the done mark survives until your next input while that pane
  is on screen, or until you navigate off it.*
- Note the two honest limits: no-op for control-mode (`-CC`) clients, and the reader who never types.
- Note that `subscribe --events` gains `done → idle` edges, meaning "the user saw it".

**Batch D outcome** (for R-D):
- tmux facts RE-VERIFIED on an isolated 3.6a socket before building on them, with a real pty client:
  `list-clients -F '#{pane_id}<SEP>#{client_activity}<SEP>#{client_control_mode}'` gives that
  client's current window's active pane, epoch SECONDS, and `0`/`1`. `send-keys` did not move
  `client_activity`; real terminal input did. A `-CC` client reported `cm=1` and its activity froze
  at attach across 8 s and a real keystroke, exactly as §1 says.
- **Both formats exist at the tmux 3.2 floor** (read `format.c` on the 3.2 tag: `client_activity` is
  a `FORMAT_TABLE_TIME` entry, `client_control_mode` its own callback returning `"1"`/`"0"`). No
  version gate is needed, and the reader treats anything but a literal `0` as control mode so an
  unreadable field can only fail to clear.
- **Two deviations from the plan's letter, both deliberate.**
  1. D1 said to append `\; list-clients` to the existing `list-panes` call. Not done: the gate D3
     asks for (pay nothing unless some pane carries attention) is only decidable AFTER the cycle has
     rows, so folding the read into the pane call would spend the round trip on EVERY cycle instead
     of the rare one. It is its own `Tmux::client_views`, measured at ~3.2 ms — the same as the
     `list-panes` beside it. N1's budget is amended to say so rather than leaving it silent.
  2. D1 said to filter control-mode clients in the reader. The filter lives in the PREDICATE
     instead, with `control_mode` carried on `ClientView`, because D2 requires a unit case proving a
     control-mode client is ignored — a policy filtered away upstream cannot be tested or
     mutation-checked. The reader parses the field; the predicate decides.
- D2 is `tma_core::seen` (a small module, not `row.rs`: the predicate takes a pane id and two
  timestamps, and is not about rows). Nine unit cases, including the walk-away one, both boundary
  directions of the floored strict `>`, a control-mode client ignored, a real client beside a
  control-mode one still clearing, and a saturating overflow guard.
- D3 is gated on `!stampede_skip` AND `raised_panes()` being non-empty, and mutates `report.rows`.
  One extra guard the plan does not name: a row whose `@agent_since` is 0 (unknown raise instant)
  is skipped, because zero is a time every client's activity postdates.
- D4: the daemon's sweep now runs `run_cycle_with(SeenClear::Deferred)`, which reports the
  candidates instead of clearing them; the serve loop clears one step AFTER `dispatch_notify`. No
  notify test moved, no sleep was added. Worth a reviewer's eye: the race is only closed INSIDE the
  daemon. A one-shot `tma status` from a status line still clears inline and can, in principle,
  retire a flag the daemon has not yet dispatched — that is the pre-existing race (the focus hooks
  have always been able to do the same), narrowed rather than widened.
- Tests: 9 unit + 4 integration (`crates/tma-runtime/tests/seen_integration.rs`, driven with a REAL
  pty client through the shared `attach_client` harness). The walk-away case proves liveness by then
  moving the raise back behind the same keystroke and watching the next cycle clear it, so a dead
  pass cannot pass it; the clearing case carries a witness pane in an undisplayed window that must
  survive. Five mutants, all caught: predicate always true (3 fail), clear a no-op (3 fail),
  `SeenClear::Deferred` clearing inline (the defer test), dropping the `pane_id` match (the witness
  test), dropping the row mutation (the row assertion). A sixth, `>` weakened to `>=`, is caught by
  the same-second unit case.
- Suite: 1207 → **1220 passing, 0 failed** (`cargo fmt --all --check` clean, clippy silent).
- Not covered by an integration test, and honestly so: the `-CC` no-op. The shared attach helper
  drives a plain client; a control-mode client was verified by live probe and is pinned by the unit
  case, not by the suite.

> **Review gate R-D.** Focus: can the predicate clear on a pane the human never touched? Is the
> walk-away case still safe? Did any cycle-cost bound get raised silently?
> **Result: PASS WITH FINDINGS** (2 medium, 1 low-medium, 1 low, 4 nits). Cleared in D6.

---

**D6 — clear the R-D findings.** `DONE`
- F1: the D4 ordering (clear strictly after `dispatch_notify`) is held by source order and a comment
  only — R-D moved the clear above the dispatch and the whole `tma-daemon` suite still passed.
  Add a `notify_integration.rs` case with a real client on the pane and a guaranteed sweep inside
  the raise→dispatch window; it must FAIL under the reorder before it counts.
- F2: `seen.rs`'s control-mode filter is documented as an iTerm2 `-CC` concession. Its real job is
  ignoring **tma's own daemon**, which parks one `tmux -C attach-session` client per session pinned
  to that session's current-window active pane. Name that reason in the doc comment, in
  `ClientView::activity_secs` (whose "tma's own polling does not appear in `list-clients`" is true
  of the COMMAND clients only), and in the ARCHITECTURE bullet; add a unit case whose fixture is
  the daemon's own control client.
- F3: with `focus-events on`, `\e[I`/`\e[O` move `client_activity`, so alt-tabbing away counts as
  input. The behaviour is right (batch C's doctrine: leaving counts as seen); the wording in
  `seen.rs`, `docs/explanation/detection-model.md` and ARCHITECTURE is wrong. Widen it.
- F4: `clear_seen` decides against a `@agent_since` read at the top of the cycle. No `-F` guard
  (`unset_pane_option` has no guarded form and the flag self-corrects next cycle); say so in the doc.
- Nits: unread `usize` from `clear_seen_rows` + double `raised_panes`; `capture.rs`'s deferred-seen
  doc omits the `since != 0` exclusion; `seen_integration.rs`'s walk-away `type_past(&s, 0)` can
  return before the queued key lands; a 128-column line in `pane-options-and-json.md`.
- Also record in ARCHITECTURE, near AD2: batch A deleted a hash-delta GUESS at "is the human here"
  that was feeding STATE; batch D reintroduces the true version of that question (tmux's own input
  clock) confined to PRESENTATION. Feeding `client_activity` back into the fold is the refactor to
  warn against.
- For E2: F2 is direct evidence that tma's control clients are real attached clients on the current
  window of every monitored session — exactly the alert-suppression question E2 defers.

**D6 outcome**:
- **F1 is the one that needed building, and the obvious test does not work.** The daemon dispatches
  on EVERY wake — a bare poll timeout reconciles the pool and dirties the status — so a marker
  raised between two sweeps is notified hundreds of ms before the sweep that clears it, and the
  order never decides anything. Measured, under the reorder: fire at `t+244 ms`, `sweeps` unchanged,
  clear at `t+855 ms`. The collision has to be staged: SIGSTOP the daemon across an out-of-band
  raise, SIGCONT, and its next iteration runs the overdue sweep and the dispatch back to back —
  the only place their order is observable. The pane runs `sleep`, so the client's keystrokes
  produce no pane output and never wake the daemon's control client (that wake was what made the
  first attempt pass under the reorder).
  `a_done_marker_the_user_typed_past_still_fires_before_it_is_cleared`, in `notify_integration.rs`:
  **verified RED with the two blocks swapped** (sink empty, 0 fires, 47 s to the ceiling) and green
  3 runs in a row as shipped. It also pins the clear itself (`@agent_attention` must still come
  down afterwards), so a clear that never runs fails it too.
- **F2 re-verified on an isolated 3.6a socket**, not taken on R-D's word: two sessions, one
  `sleep 60 | tmux -C attach-session -t <s>` each, `list-clients` reports one client per session,
  `cm=1`, `pane_id` = that session's current-window active pane (it followed a `select-window`),
  `client_activity` = attach time. The reason is now in the `seen` module doc, both `ClientView`
  field docs, `read.rs`, the ARCHITECTURE bullet, and a unit case whose fixture is the daemon's own
  client — mutation-checked: dropping `!c.control_mode` fails it (and the `-CC` case) and nothing
  else.
- **F3 probed, and the finding is slightly sharper than stated.** `client_activity` moves for the
  focus-report bytes whether `focus-events` is on or OFF (a client's tty read is a client's tty
  read); the setting governs whether the terminal ever SENDS them, since tmux only emits DECSET 1004
  when it is on. Wording widened accordingly in `seen.rs`, `detection-model.md`,
  `install-agent-hooks.md`, ARCHITECTURE and the CHANGELOG (which said "keystroke" too, and is
  user-facing). No code change: alt-tab counts as leaving, which batch C already decided is seen.
- F4/nits as specified. `clear_seen_rows` now takes the caller's `raised_panes` result (one call,
  no unread count). No `-F` guard, for R-D's reasons, now written down.
- Suite: 1220 → **1222 passing, 0 failed** (`cargo fmt --all --check` clean, clippy silent). The two
  additions are the ordering integration case and the daemon-control-client unit case.
- Worth a reviewer's eye: the new case is the only test in the tree that signals the daemon with
  STOP/CONT. If it ever hangs, that is where to look — `signal_daemon` is shared with the SIGHUP
  helper, and `DaemonGuard` SIGKILLs on drop, which reaps a stopped child too.

---

## Batch E — deferred, only on explicit go-ahead

**E1 — idle→idle re-signal.** `DONE`
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

**E1 outcome.**
- **The bug is REACHABLE, and reproduced before anything was built** — on an isolated 3.6a socket,
  driving the real `tma event` intake: a codex pane, `notify` fired twice with `@agent_attention`
  unset in between, raised the mark neither time. The stronger finding is that "second" is
  understating it: on that wiring NO completion ever raised, because `prev` is `Idle` from the
  registration onward.
- **How a real second turn skips `Working`.** Checked all six hook maps. Five are symmetric — one
  channel carries both the turn-start (`UserPromptSubmit` / `beforeSubmitPrompt` / `BeforeAgent` /
  `user-prompt-submit` / `before_agent_start`) and the turn-end event, so wiring that delivers one
  delivers the other. **codex is not**: `tma install-hooks codex` writes TWO channels, the `notify`
  key in `config.toml` and the `hooks.json` events, and only the latter needs a one-time in-TUI
  trust (`/hooks`) before it fires (`agent-coverage.md:16`, and the installer prints
  `CODEX_TRUST_NOTICE`). An untrusted pane therefore has NO working-claiming event at all while its
  turn ends keep arriving. Daemonless, nothing rescues it; with a daemon, only a turn long enough
  to span a poll does. Two narrower routes, not relied on: any dropped working event is
  unrecoverable by construction (that is the "no recovery" in the task), and for claude alone a
  contradicting idle claim past the decay window can flip a long turn's stored state to `Idle`
  mid-turn (`working` is in its `[capture].visible`), after which the real `Stop` is this same edge.
- **`turn_end` sits on the `[[hooks.map]]` ENTRY, not inside `Claim`.** The rejected-list entry
  ("a third `Claim` variant") stands and was not touched; `StateClaim` was not extended either.
  Two reasons beyond that: `Claim` is shared with `[[rules]]`, where a screen-matched `idle` means
  "the composer is up", never "a turn ended" — most agents' idle chrome co-renders mid-turn — so
  the flag would be meaningless on half its uses; and turn-endness is a property of the EVENT.
  It also kept the change out of `evidence.rs`, which E2/E3 hold.
- **Deriving it from `state = "idle"` was considered and rejected.** It would be zero-schema and is
  correct for all six bundled manifests today, but it silently makes every idle-claiming hook a
  re-raiser: a user mapping an idle-REMINDER notification (claude has exactly such an event, kept
  deliberately unmapped) would get the mark back every time the reminder fired, which is the
  unclearable-mark failure the fold is barred from causing. A drift test
  (`every_bundled_turn_end_is_an_idle_claim_and_every_idle_claim_is_a_turn_end`) pins today's
  equivalence, so the explicit flag cannot quietly diverge from it either.
- **One thing the plan does not name, and it is load-bearing: the raise is gated on the mark being
  DOWN.** `set_attention` for `Idle` is `prev == Working || (turn_end && !standing)`. Codex reports
  ONE turn end on BOTH channels (`Stop` then `notify`, ms apart) and opencode's plugin maps two
  SDK events onto its one `stop` token, so an ungated `turn_end` would raise twice and — since the
  dedup now re-arms on `turn_at` — ring the desktop notification twice per turn. Nothing observable
  separates the pair from two genuine turns except that the user cleared the mark in between, so
  that is what the rule uses. The honest cost: a second completion while the FIRST mark still
  stands records no new `turn_at`, so `wait --until done --since T` cannot see it. The mark is
  already up and unacknowledged, and this is what the pre-E1 tree did for that case anyway, so it
  is a gap held rather than opened.
- **`@agent_turn_at` went on the `StampedState` tuple (`AGENT_OPTIONS`), not `EXTRA_PANE_OPTIONS`.**
  It is episode state in the state lane: written in the same guarded chain as `@agent_notified_at`,
  cleared by the same deregister, and read by the same dedup predicate that already takes
  `notified_at`/`since`. `EXTRA_PANE_OPTIONS` is documented as "NOT part of the tuple" and holds
  parallel lanes (context gauge, model, action lock). Both consumers already hold a decoded
  `StampedState`/`AgentRow`; the alternative made one of them reach into the raw options map for a
  field the other reads off the tuple. The cost is real but is churn, not risk: 28 exhaustive
  struct literals plus the round-trip tests, and the round-trip test updating is the proof the key
  decodes. `to_millis` covers it, so a legacy-seconds value scales like every other epoch.
- **`@agent_since` did not move**, verified live: it stays at the registration instant across three
  turn ends while `@agent_turn_at` advances. The cycle never writes `turn_at`; the row carries the
  stored value through, and drops it when the tuple is removed.
- **Both comparisons are `StampedState::episode_at()` = `max(since, turn_at)`** — the daemon's
  `fire_trigger` and its marker clamp, and `wait`'s `--since` floor via `AgentRow::episode_at()`.
  A pane that never had a turn end reads `since` alone, so every comparison degrades to exactly
  what it was. `turn_at` only ever advances on a completion while `since` advances on every
  transition, so outside an idle run `since` always dominates and no stale completion resurfaces.
- Free and correct fallout: `subscribe --events` now emits the `idle → done` edge for a second
  completion, because `StateToken::of` already classes idle+attention as `done`.
- **Six mutants, each caught by exactly the intended test**: `set_attention` back to
  `prev == Working` (the unit regression + the integration case), dropping the `!standing` gate
  (the codex-pair case + the integration case), daemon dedup back to `s.since`, `wait` back to
  `row.since`, pi's manifest forgetting `turn_end`, and `TURN_AT` missing from `REMOVABLE`.
- **Live end-to-end re-run after the fix**, same isolated socket, `notify.on = ["blocked","done"]`
  with an instrumented sink: turn 1 raises and fires once; the second channel's report of the same
  turn adds no `turn_at` and no second fire; the user clears; turn 2 raises again and fires
  (2 total); turn 3 with the mark still standing adds neither; deregister leaves zero `@agent_*`
  options behind, `@agent_turn_at` included.
- Suite: 1222 → **1235 passing, 0 failed** (`cargo fmt --all --check` clean, clippy silent).
- Worth a reviewer's eye: the `!standing` gate reads `@agent_attention` from the pane read taken at
  event time, so a seen-clear landing between that read and the write costs one raise. The race is
  one-directional **on the daemon path** (it can only fail to raise, never raise falsely) and is the
  same shape as the pre-existing clear-vs-dispatch race D4 narrowed. Narrowed by R-E: inbound frames
  serialize in the single accept loop (`serve.rs:509`), which is what makes it one-directional
  there. DAEMONLESS it is not — two concurrent `tma event` processes can both read attention absent
  and both raise. That is an over-raise, and the same shape as the pre-existing blocked
  read-modify-write, so it is recorded rather than fixed.

**E2 — verify the control-mode alert-suppression footgun.** `DONE`
- Claim: an attached control-mode client counts as a viewer, so tma's daemon may be silently
  clearing the user's own tmux activity/silence alert flags for the current window of each monitored
  session. The earlier probe never got its control client attached, so the claim was unverified.
- **Outcome: real, reproduced, documented, no behaviour change.** The earlier probe's client had not
  attached; with stdin held open (pipes, no tty, exactly `control.rs`) it attaches every time. Bell
  is affected as well as activity and silence, which the claim did not mention. See the new fact in
  §1 and DAEMON.md, "Known cost: the control client counts as a viewer" (measurements, materiality,
  and the four mitigations that were tried and failed).
- Materiality is narrow and that is why the daemon is unchanged: while the user is attached their own
  client already suppresses those flags, and a real re-attach clears them anyway, so the loss is only
  the outside view of a still-detached session (`choose-tree`, `list-windows`, a polling script).
  The sharper edge is `destroy-unattached`, which never fires while the daemon runs.

**E3 — dead code.** `DONE`
- `@agent_hash`'s value is write-only since A1: `can_reuse_stamp` (`cycle.rs:593`) reads only
  `p.hash.is_none()`, and no hash is compared anywhere. **Decision: keep, presence-only, and say so.**
  Removing it would not remove a write — `can_reuse_stamp` still needs an "ever captured" marker, so
  the option would come back under another name carrying strictly less information, for a saving of
  one FNV pass over a viewport already fetched by a `capture-pane` subprocess. It is also read as
  optional (`parse_opt_int`, `stamp.rs`), so a stamp without it already decodes and no reader breaks
  either way. Where it now says so: `snapshot.rs` (`tail_hash`), ARCHITECTURE.md's option table, and
  a new row in `docs/reference/pane-options-and-json.md` — it had never been listed on the public
  contract page at all, which was the real gap.
- `Source::ProcessFact` is declared (`evidence.rs:19`) and produced nowhere. **Decision: delete** —
  it is not a reserved slot. Process facts reach the fold as `SnapshotFacts`, and precedence 2
  publishes `Provenance::Process` directly (`fold.rs:154`), so no evidence record can ever carry this
  source and `tma-core` is `publish = false`, so nothing external sees the enum. Two of its four
  sites are in `fold.rs` (`:454` `source_name`, `:1165` the proptest generator), held by E1 for the
  duration; deleting only the other two would not compile. Filed as **E4** and commented in place, so
  the next reader neither re-derives it nor mistakes it for a reservation.
- `#{alternate_on}` is read into `PaneSnapshot` and consumed only by a debug line, while every agent
  pane is alt-screen. **Decision: diagnostic only, and record the blind spot it explains.** There is
  nothing to gate with it: the two candidates are already covered (`scroll_position` is read in the
  same format batch, so skipping it saves nothing, and identity does not need it). What was actually
  missing is the consequence: `PaneSnapshot::scrolled` now documents that an agent scrolling its own
  transcript never moves `#{scroll_position}`, so the freeze covers a human in tmux copy-mode and
  nothing else, and that what keeps that survivable is manifests anchoring on bottom-pinned chrome.
  Also recorded at REQUIREMENTS appendix A, next to the alt-screen observation that causes it.

**E4 — delete `Source::ProcessFact`.** `DONE`
- Decided in E3, blocked there only by file contention with E1. Six sites in three files, mechanical:
  `crates/tma-core/src/evidence.rs:19` (the variant and its doc comment), `:29` (the `provenance()`
  arm), `:166` (the assertion in `source_folds_to_provenance`), `crates/tma-core/src/fold.rs:454`
  (`source_name` arm), `:1165` (`arb_non_hook_source`), and
  `crates/tma-runtime/src/debug.rs:507` (`source_token` arm).
- Not a coverage loss: the proptest generator drops a variant that no producer can construct, and
  `Provenance::Process` (which IS produced, `fold.rs:154`) and its `FromStr`/token arms stay.
- Accept: `Source` has three variants, the workspace builds, and the suite count drops by nothing.

**E4 outcome.** E3's reasoning re-verified, not assumed: no `Source::ProcessFact` construction
exists anywhere in the workspace, `Source` derives no `Deserialize`/`FromStr` so nothing can build
one from a string either, and `crates/tma-core/Cargo.toml:9` is `publish = false`. All six sites
deleted. A **seventh** site the task did not list: ARCHITECTURE.md's evidence-record sketch
(`:230`) named the variant, and batch A's `4fd9237` set the precedent by editing that same line
when `ActivityDelta` went — so it follows here. `Provenance::Process` and `Provenance::Activity`
untouched. Suite **1235 passing, 0 failed** (unchanged: the deleted assertion lived inside an
existing test, so no test count moved); `cargo fmt --all --check` clean, clippy silent.

**E5 — repair the R-E findings.** `DONE`
- R-E returned **FAIL** on one blocking finding plus five smaller ones.
- ⚠️ **Finding 1 (blocking).** `--since` no longer escapes level-triggering. `wait.rs:73` compares
  `row.episode_at()`, `surfaces.rs:102` emits only `since_ms`, and the documented supervisor loop
  (`block-a-script-on-agent-state.md:112`, second copy at `custom-actions.md:154`) feeds `since_ms`
  back as the next `--since`. Once a pane has `turn_at > since` — the second-completion case E1
  built — the fed-back floor can never reach the compared quantity, so every later `wait`
  re-satisfies instantly and the loop dispatches its whole queue in a spin. R-E reproduced four
  consecutive SATISFIED laps with an unchanging fed-back `since_ms`; feeding the true episode
  instant blocks correctly (`exit=124`). Pre-E1 that pane STALLED, so it is not a pure regression —
  it converts "misses the second completion" into "fires forever".
  Expose the compared instant on the row so it can be fed back. **Do NOT change `since_ms`** —
  `since_ms == @agent_since` is pinned by the uptime column and `pane-options-and-json.md:95`.
  `AgentRow` has no `Serialize`; the JSON is hand-built in `surfaces.rs` and there are three
  key-set drift guards to keep honest. Also correct `cli.md:328` and
  `block-a-script-on-agent-state.md:119`, both of which now state the opposite of the truth.
  Regression test required, and it must fail on today's tree.
- **Finding 2 (medium).** `notify.rs:228` still passes `stored.since` as the episode start to
  `notification_for`, while the dedup (`:56`) and the marker clamp (`:204`) moved to `episode_at()`.
  For a second completion the payload's `since_ms` / `TMA_SINCE_MS` reads `now - <idle-run start>`,
  minutes instead of dispatch latency, contradicting `tma-runtime/src/notify.rs:192`, `:260` and
  `pane-options-and-json.md:178`. Fix: `stored.episode_at()`. The daemonless path already passes
  `now, now` (`event.rs:394`) and is correct — leave it.
- **Finding 3 (low).** The episode-reset arms (`render.rs:183-200` guarded, `:331-347` unguarded)
  unset `ATTENTION` and `NOTIFIED_AT` but not `TURN_AT`, so a completion recorded by a replaced
  agent survives into the new episode. Benign under a monotone clock, but a backward clock step
  would let the stale value win `episode_at()`. Add the unset to both arms and extend
  `render.rs:1100`, which only covers `render_remove`.
- **Finding 4 (low).** `event/mapping.rs:212`'s comment overstates: `standing` is read off
  `s.attention` regardless of `s.state`, so a BLOCKED pane with the mark up is conflated. Not a
  regression (pre-E1 `prev == Blocked` behaved the same). Comment only.
- **Finding 5 (low).** `manifests.rs:339-364` pins `turn_end == idle` correctly, but its failure
  message nudges a future author toward setting `turn_end = true` on claude's deliberately-unmapped
  idle REMINDER — the unclearable-mark failure E1 rejected. Put the rationale in the message.
- **Finding 6 (nit).** CHANGELOG does not name `subscribe --events` on the new `idle → done` edge.
- **Plan correction, no code.** E1's outcome note calls the `!standing` race one-directional. That
  holds on the DAEMON path (inbound frames serialize in the single accept loop, `serve.rs:509`) but
  not daemonless, where two concurrent `tma event` processes can both read attention absent and both
  raise. It is an over-raise, the same shape as a pre-existing blocked read-modify-write, so it is
  not fixed — the claim is narrowed instead.

**E5 outcome.**
- **Finding 1's shape: an additive `episode_ms` key on the schema-1 row, `since_ms` untouched.**
  The alternatives were weighed and rejected. Redefining `since_ms` as the episode instant is the
  one-key fix, but `since_ms == @agent_since` is what the uptime column and
  `pane-options-and-json.md` both promise, and it would silently change a value every existing
  consumer already reads. Teaching `wait` to accept a `--since-key`, or making `--since` compare
  `since` alone, both re-open the bug E1 closed. So the compared quantity gets its own name:
  `episode_ms = max(since_ms, @agent_turn_at)`, equal to `since_ms` on every pane that has never
  had a second completion, which is why nothing about the common case changes. `AgentRow` has no
  `Serialize`, so the one hand-built site (`surfaces.rs::write_row_fields`) serves `ls --json`,
  `wait --json` and the fleet `wait --json` alike; all three key-set drift guards were updated and
  the sorted lists keep them honest. `subscribe --events` needed nothing — an edge is not a row and
  carries no `since`.
- **Three tests for it, each verified RED on the pre-fix tree** (emission commented out, tests kept):
  `surfaces::episode_ms_is_the_wait_floor_and_diverges_from_since_ms` (the key exists and the two
  values part company), `wait::the_fed_back_floor_blocks_the_wait_the_row_came_from` (the loop
  closed through the REAL serializer and the real `Goal`, so emitted key and compared key cannot
  drift again), and `a_fed_back_episode_floor_blocks_the_next_lap` in `wait_integration.rs` — a live
  scratch server, the CLI's own JSON, the recipe's own extraction: lap 1 exits 0, the fed-back
  `episode_ms` exits 124, and the same lap fed `since_ms` exits 0, which is R-E's spin asserted as
  a contrast rather than described.
- Docs: the two false statements are gone (`cli.md`'s "the row's `since_ms` must be strictly
  greater", and the recipe's "which is why feeding back the row's own `since_ms` is correct").
  Both copies of the loop read `episode_ms`, the reference table has a row for it, and the two
  sample rows in the tutorial and the how-to carry the key (the tutorial's "twenty keys" is now
  twenty-one, checked by parsing the sample).
- **Finding 2** is `stored.episode_at()` at the one call site, plus
  `a_second_completions_payload_reports_the_turns_age_not_the_idle_runs`: an hour-old idle run with
  a fresh turn end, stamped directly, and the sink records `TMA_SINCE_MS`. Under the old argument it
  reports 3600087 ms. The daemonless path was left alone; it passes `now, now` and is correct.
- **Finding 3** adds `unset_pane(t, opt::TURN_AT)` to both episode-reset arms. The deregister test
  became `every_episode_teardown_clears_the_whole_episode_lane`, covering `render_remove`,
  `render_publish` and `render_publish_advisory`; each arm's omission was mutation-checked and
  fails with its own label. `REMOVABLE` already carried the key, so uninstall was never the gap.
- **Findings 4, 5, 6** are comment/message/CHANGELOG only, as R-E scoped them. Nothing about
  `standing`'s behaviour changed; the blocked-with-mark-up case is now stated rather than implied.
- Suite: 1235 → **1239 passing, 0 failed** (`cargo fmt --all --check` clean, clippy silent). The
  baseline was re-measured on a stashed tree rather than taken from the E4 note.
- Worth a reviewer's eye: `episode_ms` is derived at serialization time from a row field that only
  the hook intake ever writes, so a surface that builds an `AgentRow` without going through the
  stamp decode emits `episode_ms == since_ms` and nothing complains. That is the same precondition
  `write_row_fields` already documents for `repo`, and the drift guards pin keys rather than
  provenance.

---

**E6 — clear the R-E re-run findings.** `DONE`
- R-E re-run returned PASS WITH FINDINGS (4, none blocking). All cleared.
- **F1 was the sharpest and is the project's own bug class repeating**: E5 corrected the falsified
  `--since` sentence in `cli.md` but not the SHIPPED `--help` text at `crates/tma/src/cli.rs:524`,
  which still told users to feed back `since_ms` — the spin. There is no help↔`cli.md` drift test,
  which is why nothing caught it. Fixed; the missing guard is recorded below.
- F2: both definitions of the notify payload's `since_ms` still described the pre-fix arithmetic
  (`now - @agent_since`) after the code moved to `episode_at()`.
- F3 (code): `row_turn_at` extracted from `run_cycle` so it could be tested at all, with the
  episode-reset arm the review proved necessary — without it the row carries the REPLACED agent's
  turn instant for one cycle, and under a backward clock step that is what `episode_ms` and
  `wait --since` read. Three regression tests, the reset one mutation-checked (removing the arm
  fails it). The plan built through the real `plan_from_verdict`, since `Publish` is private to
  tma-tmux by design and a hand-rolled literal could drift from how the cycle builds one.
- F4 was informational and left as recorded: nothing pins the daemon payload's `since_ms` to a
  nonzero dispatch latency. Pre-existing, not opened by batch E.

> **Review gate R-E.** `PASSED` on re-run (first run FAILED; see E5). The re-run traced the emitted
> quantity to the compared quantity through the real serializer, mutation-checked both episode-reset
> arms separately, and confirmed no assertion was weakened anywhere in the batch.

---

## Batch F — the `switch-client` session-departure gap

Goal: close the one departure scope batch C left open, or prove on evidence that it should stay
open. `@agent_attention` clears when you leave a PANE (`after-select-pane`) and when you leave a
WINDOW (`session-window-changed`). Leaving a whole SESSION with a bare `switch-client -t <sess>`
fires neither, so a marker raised on the pane you were watching survives the switch. Not a
regression (nothing cleared on departure before batch C) and batch D's ordered-input clear takes it
down once you return and type, which is why it did not block v0.4.1.

**F1 — solve the `client-session-changed` mystery before designing anything.** `DONE`
- Probed before this batch (⚠️ **this reading is wrong — see the F1 outcome**): a bare
  `switch-client -t s2` fires `client-session-changed` TWICE, once per session context
  (`pane=%0 sess=s1`, then `pane=%1 sess=s2`), so the departed session's pane
  IS nameable at hook time. But installing tma's existing clear command on that hook did NOT clear
  the departed pane, while both invocations ran and both exited 0.
- `run_clear_attention` swallows every failure by design, so rc=0 proves nothing. Instrument the
  real tmux command (`render.rs` `unset_pane_option`, `read.rs`, `dispatch.rs`) rather than infer.
- Until the mechanism is known, any fix is guesswork. Report it whatever it turns out to be.

**F2 — decide the design on the probe, not the argument.** `DONE`
- Either wire the hook, or resolve the gap as WONTFIX and say why. **Keeping the gap is a legitimate
  outcome**: the marker means "finished, unreviewed", and batch C's premise (leaving = seen) is
  strongest for a pane you were staring at and weakest for a session you walked away from.
- If wired, it is held to batch C's bar: enumerate every way `client-session-changed` can fire
  (script, `choose-tree`, a second client, `attach`, nested tmux, detach) and prove each does not
  clear a pane the user never saw. Batch C shipped a defect of exactly that shape (C6).
- If not wired, remove the gap from ARCHITECTURE's outstanding list by RESOLVING it, not by leaving
  it dangling.

**F3 — tests + docs.** `DONE`
- Whatever ships, follow the witness-pane pattern (`crates/tma/tests/attention_integration.rs`) so a
  hook that silently does nothing fails loudly instead of passing vacuously. Mutation-check every
  new test.
- Docs: ARCHITECTURE, `install-agent-hooks.md`, `detection-model.md`, CHANGELOG as applicable.
- Gate as always; baseline 1242 passing, 0 failed at v0.4.1.

**F1 outcome — the mystery is that there was never a departure firing at all.** The pre-batch probe
installed its logging hook BEFORE the pty client attached, and an attach is itself a
`client-session-changed` (`cmd_attach_session` → `server_client_set_session`, `s != NULL`). So the
two lines read as "once per session context" were the ATTACH (`pane=%0 sess=s1`, `client_last_session`
EMPTY) and then the switch (`pane=%1 sess=s2`). Installing the hook after the attach gives exactly
ONE line per switch, always in the ARRIVAL session's context. That also explains why the clear "did
not land": nothing ever asked it to clear `%0`. The `FIRE pane=%0 … %0 attention STILL SET` line was
the attach's arrival clear running before the flag was raised, and `FIRE pane=%1 → cleared` was the
switch's arrival clear doing its job. Neither invocation was ever handed a departure. Confirmed by
re-running the same experiment with the hook installed after the attach, and by reading
`server_client_set_session` in the 3.6a source.
- Corollary the probe's reading hid: the departed pane IS resolvable at hook time, in ONE format —
  `#{S:#{?#{==:#{session_name},#{client_last_session}},#{W:#{?window_active,#{P:#{?pane_active,#{pane_id},}},}},}}`
  — and it resolves correctly through tma's own `display-message -p -t <arrival pane>`, no `-c`
  needed. The gap was never a resolution problem.

**F2 outcome — the gap STAYS OPEN, on evidence, and is now resolved rather than dangling.**
- **Why it cannot be wired: tmux notifies outside the changed-or-not test.**
  `server_client_set_session` (3.6a `server-client.c:390`) updates `c->last_session` only under
  `s != NULL && c->session != NULL && c->session != s`, then calls
  `notify_client("client-session-changed", c)` unconditionally. Identical in the 3.2 source
  (`cmd-switch-client.c:137-144`), so it holds across the supported range. A
  `switch-client -t <the session you are already on>` therefore fires the hook with a
  `client_last_session` naming a session left however long ago. **This is C6's defect verbatim, one
  scope up**, and nothing in the hook-time format vocabulary says "the session really changed".
  ⚠️ **F also claimed `client-session-changed` was the only notification a session change emits.
  That is FALSE — `pane-focus-out` fires too. Batch F2 measured it and re-decided on that basis;
  read F2 before building on anything in this outcome note.**
- **Measured, not argued.** On an isolated 3.6a socket with a real pty client, the departure clear
  wired up for real (the shipped hook-string shape, `TMA_HOOK_KIND=client-session-changed`, the live
  binary, `from_hook_name` temporarily mapping the name): a genuine `s1 → s2` switch correctly
  cleared `s1`'s pane; a **no-op `switch-client -t s1` cleared the done mark on `s2`'s current
  pane**; and the exact three-command sequence `Tmux::focus` issues for a cross-window jump inside
  `s1` cleared it too.
- **tma is the loudest producer of that no-op.** `Tmux::focus`
  (`crates/tma-tmux/src/tmux/display.rs`) runs `switch-client` unconditionally — C6 gave
  `select-window` a `#{window_active}` short-circuit and left this one alone — so every `tma jump` /
  picker Enter that stays inside the current session fires a false departure. Not changed here: the
  short-circuit is only reachable when the caller names a client (`focus(None, …)` is targetless),
  and `switch-client` also resets the client's key table, which skipping would make inconsistent
  between same-session and cross-session jumps. Recorded for whoever revisits the hook, since it
  makes the false-departure rate roughly "half of all jumps" rather than "the occasional
  `choose-tree`".
- **Other firing paths, all probed on 3.6a**: attach (fires, `client_last_session` empty →
  no departure, safe); `choose-tree` / `prefix s` onto the session you are on (fires, same no-op
  staleness — driven key-first through a pty client); `switch-client -l` / `-n` (genuine, safe);
  `switch-client -t sess:win` (fires CSC *and* `session-window-changed`, so the window departure is
  already handled and the CSC half is pure over-clear); detach (`server_client_set_session(c, NULL)`,
  no notification, `last_session` nulled); `kill-session` of an unattached session (nothing);
  `kill-session` of the attached one (`server_destroy_session` sets `c->last_session = NULL` before
  reassigning, so no false departure either way). Two clients on one session: client A switching
  away resolves the pane client B is **still displaying** — the same class as the pane/window hooks,
  but far likelier at session scope, since sharing a session between clients is what sessions are for.
- **The trade is one-sided, which is what decides it.** The residue held open is a mark standing on
  a session you walked out of; it is counted by `tma status` and offered by `prefix-j` until you
  return, and it comes down on your first keystroke there (batch D) or your next pane/window switch
  (batch C). The residue a fix would introduce is a silently destroyed record of a completion nobody
  ever saw. Every one-directional choice in this project runs the same way (§1's floored strict `>`,
  D2's never-false-block): fail to clear, never clear falsely.
- **The semantic case for closing it was weak anyway.** The mark means "finished, unreviewed", and
  walking out of a session is the clearest case of NOT having reviewed it. Batch C's premise
  (leaving = seen) is calibrated to the pane you were staring at; a session is a workspace you come
  back to, and cross-session "where did something finish" is what `tma status` and `prefix-j`
  exist to answer.
- **What a future attempt would need**, so nobody re-derives it: a per-client memo of the
  last-seen `#{session_id}` (rename-stable, unlike the name `client_last_session` gives). tmux has
  no per-client option scope, so it would live in server options keyed by `#{client_name}` — a tty
  path the NEXT client on that terminal reuses — with an unknown first fire after install, a
  read-modify-write on every session change, and its own GC. That is a new persistent state lane
  inside a hook that must never error, for a residue that self-heals on the next keystroke.

**F3 outcome — one guard, no behaviour change.**
- `DepartureKind::from_hook_name` gains `client-session-changed` to its unit test as an asserted
  `None`, with the reason in the failure message, and the reasoning in the doc comment beside the
  `after-select-window` paragraph it parallels.
- `a_hand_wired_session_hook_can_only_clear_the_pane_you_arrived_at`
  (`crates/tma/tests/attention_integration.rs`), the C6 shape: a real PTY client attached to `s1`,
  the real hook string installed on `client-session-changed` BEFORE the attach — the attach is
  itself a firing, so it doubles as a liveness sentinel that has to be consumed before the part
  under test begins (the F3 outcome originally recorded this backwards; R-F 2) — then a real
  `switch-client -t s2`. The arrival pane is the WITNESS — its flag must come off,
  so a hook that silently did nothing fails instead of passing. **Mutation-checked**: adding a
  `SessionChange` arm with the format above turns it red with the defect named. Non-vacuous by
  construction, since the client genuinely switched and `client_last_session` really names `s1`.
- Docs: ARCHITECTURE's "Known gap, unfixed" bullet is replaced by the decision record (mechanism,
  measurements, the `Tmux::focus` finding, the memo dead end); `detection-model.md` and
  `install-agent-hooks.md` both say plainly that departure means a pane or a window and never a
  session, and why; CHANGELOG carries the user-facing half.
- Suite: 1242 → **1243 passing, 0 failed**. No production code changed.
- **Trap worth knowing before the next mutation check in this repo**: reverting the mutated source
  with `cp` did not always make cargo rebuild `target/debug/tma`, so a "passing" run can be reading
  the mutated binary through `CARGO_BIN_EXE_tma`. It cost a false flake diagnosis here. `touch` the
  reverted file and confirm the artifact (`strings target/debug/tma | grep <mutant marker>`) before
  believing either colour.

> **Review gate R-F.** Focus: is the "leave it open" argument load-bearing or convenient? Does the
> new test fail against a real implementation of the fix (not just a stubbed one)? Is anything in
> the F2 outcome an inference rather than a measurement? Does any doc still promise a clear that
> does not happen?
> **Result: PASS WITH FINDINGS (7).** One of them reopens the batch — see F2 below.

---

## Batch F2 — reopen the session departure: `pane-focus-out`

Batch F concluded the gap must stay open because `client-session-changed` is the ONLY notification
tmux emits for a session change and it cannot tell a genuine switch from a no-op. The
no-op indistinguishability is real and re-verified. The "only notification" half is FALSE: R-F found
`pane-focus-out` also fires on a session switch, and a follow-up probe (isolated socket, real PTY
client, `focus-events` at its default OFF) saw it fire on exactly a genuine session change and on
nothing else, naming the departed pane directly in `#{pane_id}` with no nested format and no
`client_last_session`. R-F also reported a directional asymmetry the follow-up probe did not
reproduce. Whatever the outcome, batch F's false claim has to come out of all five places it was
written, because the obvious probe falsifies it in seconds.

**F2-1 — resolve the asymmetry and characterise the hook.** `DONE`
- Reconcile R-F's "one direction produced no `pane-focus-out`" against the follow-up probe's "both
  directions fired". One of the two mis-measured; which one changes the answer.
- Full characterisation, `focus-events` OFF **and** ON: every navigation kind (pane, window, no-op
  pane, no-op window, genuine session switch, no-op session switch), terminal focus loss/gain,
  detach, attach, nested tmux, a second client on the same session, and a programmatic switch from
  a script. Table it.
- The "explicitly rejected" entry for `pane-focus-out` says it fires when the CLIENT loses focus, so
  alt-tabbing to a browser would clear the flag. That is true only under `focus-events on`
  (default off). Weigh it against R-D finding 3: with `focus-events on` the focus-report bytes
  already move `client_activity`, so alt-tabbing already clears a marker today via batch D.

**F2-2 — decide, on the measurements.** `DONE`
- Wire it, or keep the gap. Both are legitimate; the decision record must rest on measured
  behaviour, and must state explicitly what changes for a `focus-events on` user vs a default user.
- If wired: batch C's bar. Enumerate and prove every firing path clears only a pane the user saw;
  prove it does not double-clear or fight `after-select-pane` / `session-window-changed`; decide
  config-gated (like `pane-focus-in` behind `[focus] events`) vs unconditional, on evidence.
- If not wired: correct the "only notification" claim in `crates/tma-tmux/src/tmux/read.rs`,
  `docs/internal/ARCHITECTURE.md` (twice), this plan, `crates/tma/tests/attention_integration.rs`,
  and `docs/how-to/install-agent-hooks.md`; and extend
  `a_hand_wired_session_hook_can_only_clear_the_pane_you_arrived_at` to cover `pane-focus-out`,
  which otherwise guards half the door.

**F2-3 — clear the remaining R-F findings.** `DONE`
- R-F 2: F3's outcome says the guard test installs its hook AFTER the attach; it installs it BEFORE
  (`attention_integration.rs:694-696`) and uses the attach's own firing as a liveness sentinel.
- R-F 3: batch F's facts never reached §1, breaking the plan's convention (C6 and E2 both added
  theirs). Add the CSC facts and the `pane-focus-out` characterisation.
- R-F 4: `the_retired_window_hook_name_carries_no_departure` (`read.rs`) now also asserts the CSC
  case; the name no longer covers its contents.
- R-F 5: the comment at `attention_integration.rs:694-698` leads with a race claim R-F could not
  reproduce in 10 runs. Its second reason is the strong one; reorder or drop the unproven claim.
- R-F 6: batch C's gap note (this file, ~line 460) dangles with no pointer to F, and still says
  `switch-client` "fires neither focus hook".
- R-F 7 (nit): `CHANGELOG.md` grew a `### Documentation` heading; every other section is
  Added/Fixed/Changed/Breaking.

**F2-1 outcome — the asymmetry was real, and its cause is the thing that decides the batch.**
Both probes were right. `pane-focus-out` is emitted only when NO attached, focused client still has
that window current (`window_pane_update_focus`, 3.6a `window.c:481`), so with one client a genuine
switch fires in both directions, and with a second client parked on one of the two sessions the
switch away from THAT session fires nothing while the other direction does. Reproduced on demand:
same socket, same commands, one PTY client → symmetric; add a second client (real or `tmux -C`) on
`s1` → `s1 → s2` silent, `s2 → s1` fires. R-F's probe is not in front of me, so this does not prove
what its setup was, only that the asymmetry it reported has a mechanism and is reproducible on
demand. What is settled either way: the hook is not disqualified by `focus-events` contingency the
way R-F framed it, and the suppression is not directional — it is per-viewer.

**Characterisation, tmux 3.6a, isolated socket, real PTY client** (`### STEP` markers driving a
logging hook on every candidate; the two runs differ only in `set -g focus-events`):

| event | `focus-events off` | `focus-events on` |
| --- | --- | --- |
| attach | focus-IN only | focus-IN only |
| pane switch | — | PFO on the departed pane |
| no-op `select-pane` | — | — |
| window switch | — | PFO on the departed window's pane |
| no-op `select-window` | — | — |
| session switch (out-of-band, key-driven, from `run-shell`, `-t sess:win`) | **PFO on the departed pane** | same |
| no-op `switch-client -t <current>` | — | — |
| session switch, another client on the departed session | — | — |
| session switch, a `tmux -C` client on the departed session | — | — |
| terminal focus-out report reaching the client | PFO | PFO |
| … but does the terminal send one? | no (tmux emits DECSET 1004 only under the option) | yes |
| `display-menu` / `display-popup` / `display-panes` open | **PFO** | **PFO** |
| overlay close | focus-IN | focus-IN |
| clean `detach-client` | **PFO** | **PFO** |
| client SIGKILLed (dropped link) | — | — |
| nested tmux, outer navigates away (outer at either setting) | inner fires iff the INNER server has `focus-events on` | same |

`#{pane_id}` at PFO time is the departed pane; there is no nested format and no
`client_last_session` involved. Below tmux 3.3 the whole table's left column is empty: 3.2 runs the
focus check from the server loop behind `if (focus)` (`server-client.c:1368`), and 3.3 moved focus
to event call sites (CHANGES, "Change focus to be driven by events rather than scanning panes").
The 3.3–3.5 shape was not verified — only 3.2 and 3.6a sources were read.

**F2-2 outcome — the gap STAYS OPEN, on different and better grounds than batch F gave.**
Batch F's central claim was false and is corrected everywhere. The mechanism objection is gone:
`pane-focus-out` distinguishes a genuine session switch from every no-op, which is exactly what F
said no hook could do, and it does it without the stale-name problem. It is still not wired, for
four measured reasons, in descending weight:
1. **Inert exactly where tma is most instrumented.** A control-mode client counts as an attached,
   focused viewer (E2) and the daemon parks one per monitored session, so the clear would never
   fire for daemon users while the pane and window clears kept working. A departure rule that
   exists only when the daemon is off cannot be stated, and it would make the daemon subtractive.
2. **A clean detach fires the same edge** and takes the mark down on the way out — the end-of-day
   flow the mark exists for — while a KILLED client fires nothing, so closing your terminal and
   losing your ssh link would behave differently. No hook-time fact separates the two.
3. **Every overlay fires it, ungated by `focus-events`** — including tma's own `prefix-a`
   `display-popup` picker, so opening the list of done marks would clear the one on the pane you
   are sitting on. Weakest of the four: the keystroke that opened it already clears the same mark
   through batch D within a cycle. But it is a second, unconditional path, and a script-opened
   popup reaches it with no input at all.
4. **Nothing below tmux 3.3**, so the clear would be present or absent by tmux version.
The semantic case that closed F stands unchanged and is now the tiebreak rather than the argument:
a session is a workspace you come back to, and "where did something finish" across sessions is what
`tma status` and `prefix-j` answer.
- **What changes for a `focus-events on` user vs a default user, stated plainly, since nothing
  shipped:** nothing. tma installs no `pane-focus-out` hook under either setting. The one thing
  `focus-events on` still changes is what batch D already documented: the focus report your
  terminal sends on alt-tab is input, so it clears the mark on the pane a client of yours is
  displaying, exactly as a keystroke would (R-D 3).
- The `Tmux::focus` no-op `switch-client` finding from F is untouched and still recorded: it makes
  the `client-session-changed` false-departure rate "half of all jumps". It does not bear on
  `pane-focus-out`, which ignores that no-op.

**F2-3 outcome — what shipped.** No production behaviour changed.
- R-F 4 cleared: `read.rs`'s `the_retired_window_hook_name_carries_no_departure` is now
  `only_the_two_departure_hooks_map_to_a_departure`, which is what its body has asserted since F3
  added the `client-session-changed` case and F2 added `pane-focus-out`.
- The "only notification" claim is gone from all five places R-F listed: `read.rs`'s
  `from_hook_name` doc (replaced with the `pane-focus-out` reasoning), ARCHITECTURE ×2, this plan's
  F2 outcome, the integration guard's doc comment, and `install-agent-hooks.md` (rewritten to tell
  a user what BOTH hand-wirable hooks actually do).
- `install.rs` gains `pane_focus_out_is_not_a_hook_tma_installs`, asserting the name is in neither
  `desired_hooks(false)`, `desired_hooks(true)`, nor `ALL_TMUX_HOOKS` — the last deliberately, since
  the uninstall sweep strips our content from every name in that list and this is a hook users
  legitimately wire themselves. **The refusal has to live at the install set**: unlike every other
  refusal in `from_hook_name`, refusing the NAME buys nothing here, because the hook hands the
  departed pane over as `#{pane_id}` and the plain arrival clear would take it from there. The
  `from_hook_name` unit test now asserts that too, with that reason in the message.
- Two characterisation guards in `attention_integration.rs`, both with a real PTY client:
  `a_hand_wired_focus_out_hook_clears_a_pane_you_only_detached_from` and
  `a_control_mode_client_suppresses_the_session_departure_focus_out` (whose liveness half is the
  C6 pattern — a second switch with the control client gone must clear — because the correct
  behaviour under the control client is that no hook runs at all). Each fails loudly if tmux ever
  stops behaving that way, which is the condition under which the decision should be reopened.
- **Mutation-checked, four mutants, all caught.** (a) `dispatch.rs`'s arrival unset replaced with an
  unset of a different key: both new tests fail (plus four existing). (b) the control client parked
  on `s2` instead of `s1`: the suppression assertion fails with its own message. (c) and (d) the
  detach test's hook installed on `pane-focus-in` and on `session-window-changed`: fails both ways.
- **A vacuity trap found and closed while mutation-checking, worth knowing.** The detach test
  originally installed its hook BEFORE the attach, mirroring the `client-session-changed` guard.
  With `pane-focus-out` that is wrong: an attach fires `pane-focus-in`, and a `pane-focus-in` hook
  already on the server leaves a `tma clear-attention` in flight that lands ~200 ms later, AFTER the
  test raises its flag — so mutant (c) PASSED, with the attach's clear credited to the detach. The
  `client-session-changed` guard wants the pre-attach install (the attach's own firing is its
  liveness sentinel); this one wants the opposite, and both reasons are now in the comments.
- Suite: 1243 → **1246 passing, 0 failed**.

> **Review gate R-F2.** Focus: is the characterisation table reproducible, and does the decision
> follow from it rather than from batch F's momentum? If wired, does any firing path clear a pane
> the user never saw? If not wired, is the "only notification" claim gone from all five places?

---

## Guard map: where this comes back, and what stands watch

Left by the R-E re-run, extended by batches F and F2. Four places a future maintainer most likely
reintroduces one of these bugs.

1. **Collapsing `since_ms` and `episode_ms` into one key.** Two adjacent JSON numbers, equal on
   almost every row, read like a mistake. Deleting `episode_ms` restores the supervisor-loop spin;
   redefining `since_ms` breaks the uptime column. **Code guard is strong** — `crates/tma/src/wait.rs`
   closes the emitted→compared loop through the real serializer AND the real `Goal`, and
   `crates/tma/tests/wait_integration.rs` pins the `since_ms` spin as an asserted contrast.
   **Doc guard: ADDED** — `crates/tma/src/cli.rs`,
   `every_description_of_the_since_floor_names_the_same_row_key`. It pins the three descriptions of
   the `--since` floor to each other: clap's rendered `--help`, the `cli.md` flag row, and every
   `sed` recipe in the two how-tos. Mutation-checked against all three drift sites.

   **Note what it took to make it real**, because two plausible forms of this test do not work.
   A "does this key exist" check misses the bug entirely — `since_ms` is a perfectly real key and
   the defect was naming the wrong real one. And asserting the help merely CONTAINS `episode_ms`
   also misses it: the shipped-wrong help read "`since_ms` must be strictly greater — NOT
   `episode_ms`", which mentions the right key while instructing the wrong one. That version of the
   test was written, run against the exact drift it was for, and PASSED. The guard therefore pins
   the INSTRUCTION ("feed the row's own `episode_ms`") and asserts the negative. Rewording that
   sentence fails the test on purpose — update the pin only after confirming the meaning survived.
2. **Deriving `turn_end` from `state == "idle"`.** Zero-schema, correct for all six bundled
   manifests today, and it will look obviously right. It also makes every idle-claiming hook a
   re-raiser — the unclearable-mark failure the fold is barred from causing. Guard:
   `crates/tma-runtime/src/manifests.rs`'s drift test, whose failure message names claude's
   deliberately-unmapped idle reminder as the counterexample. **Know its limit**: it pins today's
   equivalence, so it catches a manifest author flipping one entry, but it goes vacuous along with
   the field if someone deletes `turn_end` and derives it. The guard against THAT is prose.
3. **Wiring a session departure clear** (batches F and F2). Two hooks tempt, for opposite reasons.
   `client-session-changed` looks like the last obvious hole in seen-on-leave and the departed pane
   IS resolvable in one format, so a first cut passes every hand test — the no-op that breaks it is
   `switch-client -t <the session you are already on>`, which nobody types on purpose but which
   `Tmux::focus` issues on every same-session jump. `pane-focus-out` has none of that staleness and
   on a bare one-client server it looks flawless, which is the more dangerous trap: it is inert
   whenever another client (tma's own daemon control client included) is attached to the session
   you left, it fires on a clean detach and on every popup, and it does not exist below tmux 3.3.
   Guards, all in `crates/tma/tests/attention_integration.rs` unless noted:
   `a_hand_wired_session_hook_can_only_clear_the_pane_you_arrived_at` (mutation-checked against a
   working implementation) and the `from_hook_name` unit assertion for the first;
   `a_hand_wired_focus_out_hook_clears_a_pane_you_only_detached_from`,
   `a_control_mode_client_suppresses_the_session_departure_focus_out`, and
   `install.rs`'s `pane_focus_out_is_not_a_hook_tma_installs` for the second. **Know their limit**:
   they pin tma's own install set and mapping, so they say nothing about a user who wires either
   hook by hand. The guard against that is the prose in ARCHITECTURE and `install-agent-hooks.md`.
   Note the asymmetry: refusing the NAME is enough for `client-session-changed` and worthless for
   `pane-focus-out`, which hands the departed pane straight to the arrival clear as `#{pane_id}`.
4. **Removing the `!standing` gate** in `crates/tma-runtime/src/event/mapping.rs`. Someone will hit
   the held gap ("my second completion didn't raise while the first mark was up"), see the gate, and
   drop it — ringing the desktop twice per codex turn, since codex reports one turn end on both
   channels and opencode maps two SDK events onto one `stop` token. Guard is good: a unit test and
   an integration test, both mutation-checked.

## Explicitly rejected (do not resurrect)

- **Suppress-at-set** (never raise on a pane the viewer is on, herdr's design). `wait --until done`
  reads `is_done` directly (`wait.rs:67` → `row.rs:98`), so a pane completing while focused would
  never satisfy `--until done`, and `--since` cannot rescue it because `row.since` is write-once.
  A script would work or hang depending on which pane a human sat in.
- **Any windowed presence test** ("cleared if the user typed within the last N seconds"). Large N
  destroys the walk-away signal; small N does not fix quiet reading. Ordered beats windowed.
- **`pane-focus-out` as the leave hook.** Rejected in design for one reason (it fires when the
  *client* loses focus, so alt-tabbing to a browser would clear the flag), re-examined in F2, and
  kept rejected for four better ones. The original reason has largely expired: tmux enables
  DECSET 1004 only under `focus-events on`, and with that option on the focus-report bytes already
  clear the mark through batch D (R-D 3). What kills it is measured elsewhere — it is suppressed
  whenever any other attached client (a control-mode one included, so tma's own daemon) still has
  that window current, so the clear is inert exactly where tma is most instrumented; it fires on a
  clean detach; it fires on every `display-menu` / `display-popup` / `display-panes`, tma's own
  `prefix-a` picker included; and below tmux 3.3 it is not emitted at all at the default option.
  Guards: `pane_focus_out_is_not_a_hook_tma_installs`,
  `a_hand_wired_focus_out_hook_clears_a_pane_you_only_detached_from`,
  `a_control_mode_client_suppresses_the_session_departure_focus_out`. Full record in
  ARCHITECTURE.md. **Do not resurrect it on the strength of the default-config probe alone** — on a
  bare server with one client it looks perfect, which is exactly the trap.
- **`monitor-silence` / `alert-silence`.** Weaker than reading `window_activity` directly, and it
  requires mutating a user-facing global (`silence-action`, whose default `other` suppresses the
  alert for the current window anyway).
- **`#{pane_unseen_changes}` as a seen primitive.** It means copy-mode changes only.
- **Region- or chrome-scoped hashing.** Composer and working chrome share the footer, so no region
  split separates them; and a rule over that region is strictly more precise than a hash of it.
- **A departure clear on `client-session-changed`** (batch F). It fires for
  `switch-client -t <the session you are already on>` too, and
  `client_last_session` is stale there — so it clears done marks in sessions the user never touched.
  `Tmux::focus` issues exactly that no-op on every same-session jump. Measured with the fix wired up
  for real; see batch F's outcome and ARCHITECTURE. Leaving a session keeps its mark, on purpose.
- **A third `Claim` variant** for "something happened". Over-modelling: `SnapshotFacts` is the
  documented vehicle for non-claim facts, and the semantics are not wanted once activity is deleted.
