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

**Known gap, recorded for batch D/E, not a defect of C:** `switch-client` fires neither focus hook
(verified: `switch-client -t s2` from a client on `s1` produced nothing), so departing a whole
SESSION while an agent is finishing there leaves exactly the residue seen-on-leave set out to kill.
Batch D's ordered-input clear does not reach it either — the user is by then typing in a different
session. If it is worth closing, `client-session-changed` is the hook that fires, and the departed
session's current window's active pane is what it would have to resolve.

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

**E2 — verify the control-mode alert-suppression footgun.** `WIP e23-agent`
- Claim: an attached control-mode client counts as a viewer, so tma's daemon may be silently
  clearing the user's own tmux activity/silence alert flags for the current window of each monitored
  session. **Not reproduced** in our probe (the test control client never attached). Verify properly,
  and if real, decide whether to document or avoid.

**E3 — dead code.** `WIP e23-agent`
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
