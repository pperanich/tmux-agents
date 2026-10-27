# OSC 7501 Integration Work Order

Status: B1 through B8 accepted. Implementation, independent fixing reviews, native workspace checks, Rust 1.88, isolated Nix packaging and documentation checks are complete. On 2026-10-27 the user selected `prefer` as the default for v0.6.0, superseding the initial opt-in rollout below. Explicit `off` and `observe` remain available. Agent eligibility, hook retirement and daemonless removal are unchanged.

Prepared 2026-10-26 against `17470d5c1bdcf25376678f8ca0f98bd578588d89` on `main`. Revalidate the baseline before execution. This document is the repository copy of the planning Page; keep both synchronized when decisions change.

## Goal statement

**G1. Make OSC 7501 the daemon's preferred source of agent state when reporting and observation coverage are verified, using bounded byte parsing, typed Rust domain models, explicit ownership, and deterministic recovery. Preserve the existing public state contract and action safeguards, make per-agent state hooks unnecessary for verified configurations, and leave a clean path to retiring daemonless operation and redundant state hooks later.**

The feature is complete when the production daemon, rather than the scratch observer, passes the acceptance matrix below. A working parser or a green suite with live cases skipped does not establish that outcome. The initial verified agent target is Pi; Claude Code receives its own activation and coverage gate.

## Scope and constraints

| Code | Constraint |
| --- | --- |
| C1 | Planning is complete. The user activated G1 and instructed continued work toward it on 2026-10-26, authorizing implementation. Commit, push, release, and hook uninstallation remain outside this work order. |
| C2 | Use the existing crate boundaries. `tma-core` stays pure; `tma-tmux` owns tmux subprocesses; runtime owns policy and integration; daemon owns scheduling. Preserve the current tier and UI boundary checks. |
| C3 | Build one daemon-owned OSC collector. Retain existing daemonless fallback behavior for this feature, but add no daemonless OSC collector, PTY proxy, native tmux patch, or replacement event framework. Actual daemonless removal is a separate migration. |
| C4 | Keep telemetry, session/transcript binding, pending requests, and structured permission replies working. Do not uninstall hooks or user configuration automatically. State-hook retirement follows evidence, per agent and version. |
| C5 | Prefer enums, validated newtypes, borrowing, explicit ownership, and static dispatch. Strings remain appropriate for open identifiers and human text. No new async runtime, trait-object pipeline, broad process library, repository-wide identifier rewrite, or unsafe shortcut without a demonstrated requirement. |
| C6 | Tests belong in workspace packages under `crates/`, including shared harness helpers. Do not add tests for `examples/` or `scripts/`. Invoke Python through `uv` when it is needed. Preserve unrelated work. |

### Evidence already established

| Code | Observation | Consequence |
| --- | --- | --- |
| F1 | Real Pi 1.3.0 under tmux 3.6a emitted eight reports covering idle, working, done, blocked/permission, error, and clear with `PI_PROGRAM_STATUS=1`. The HTTP model response was deterministic local test data. | This establishes transport feasibility and several real Pi lifecycle transitions, not complete agent coverage. |
| F2 | Normal activation and a control-mode-injected reply each produced only a capability query. Pi stops accepting the reply after tmux's device-attributes answer. | Do not implement asynchronous reply injection as the activation strategy. |
| F3 | The daemon already reads control-mode pane output, and the core already arbitrates typed evidence. | Extend these components and retain the current UI state projection. |
| F4 | Existing hooks also supply session identity, context usage, transcript pointers, and action correlation. | Retiring state hooks and retiring every agent adapter are separate outcomes. |

The scratch evidence is currently in `/tmp/tma-osc7501.2plMcb/`: `prototype.py`, `results.json`, and the three run directories. B1 preserves the minimal redacted protocol fixtures and provenance needed by tests; do not commit the scratch configuration or entire terminal transcript.

## Architecture and typed interfaces

### D4 Typed boundaries and crate ownership

The target flow is:

`tmux control bytes -> streaming transport decoder -> terminal scanner -> typed protocol command -> record reducer -> episode eligibility and selection -> authority-aware verdict -> guarded publication -> existing consumers`

| Component | Owner and proposed home | Responsibilities |
| --- | --- | --- |
| Protocol grammar and domain | `tma-core/src/program_status/{mod,parse}.rs` | Parse bounded input into validated values; keep protocol commands separate from manifest `Claim`. |
| Records and projection | `tma-core/src/program_status/{store,project}.rs` | Pure reduction, lifecycle rules, app inheritance, selection, completion effects; injected clocks and identities only. |
| Control decoding and terminal scanning | `tma-tmux/src/control.rs` and focused submodules | Decode raw control bytes without whole-line UTF-8 conversion; emit ordered typed events and explicit gaps. |
| Coverage and process binding | `tma-runtime/src/program_status.rs` or a small module directory | Own per-pane stores, validate episodes, handle app eligibility and loss, prepare publication intents. |
| Authority and persistence | Existing `evidence.rs`, `verdict.rs`, `stamp.rs`, `render.rs`, and `tma-tmux/src/stamp.rs` | Carry real source independently of legacy provenance; apply one ownership policy to every writer. |
| Scheduling and diagnostics | `tma-daemon/src/daemon/serve.rs`, runtime config/debug, CLI doctor | Drain immediate reports, reconcile coverage, renew collector ownership, explain fallback, and wake existing consumers. |

The interfaces below describe the intended type shape, not a requirement to copy names verbatim. B1 freezes concrete signatures for downstream workers. Keep shared identifiers in one lower-layer module; do not create a new crate merely to hold them.

```rust
enum ProgramStatus {
    Idle,
    Working { progress: Option<Progress> },
    Blocked { kind: Option<BlockedKind>, progress: Option<Progress> },
    Done,
    Error,
}

enum BlockedKind { Permission, Question, Auth }
struct Progress(u8); // private field, checked construction

enum ProgramCommandRef<'a> {
    Replace(RecordRef<'a>),
    Clear(RecordIdRef<'a>),
    Query,
}

struct PaneId(u32);
struct ConnectionEpoch(u64);
struct ObservationSequence(u64);

enum CoverageState {
    Unobserved,
    Active(ProgramAuthority),
    Recovering(RecoveryReason),
}
```

**Z1. Parse once.** Parse state, kind, configuration mode, pane/session IDs, and lifecycle causes at the boundary. Downstream control flow uses exhaustive matches. Replace the existing `source != HookEvent` screen filters with explicit screen/title matching before adding a new source. Labels used by debug output must never drive behavior.

**Z2. Borrow before retaining.** The parser borrows tokens and uses bounded caller-owned decoding scratch. The store owns accepted records after complete validation. Stream ordinary output without allocating a `String` or owned output notification for every chunk. Allocations for retained text, tmux command rendering, and JSON serialization are expected and measured, not described as zero cost.

**Z3. Keep dynamic identifiers honest.** `RecordId`, `AppId`, and safe single-line text encapsulate validation and operations such as ancestry. Open agent names and `Detail` remain extensible. Do not force user-defined manifest names into a closed agent enum or replace all string fields across unrelated crates.

**Z4. Prefer concrete structures.** A bounded vector is a suitable initial record store at this scale. Use explicit update ordinals for eviction and tie breaking. Add indexing or pooling only if measurements justify it. Use a small established base64 decoder with reviewed features/MSRV rather than inventing a decoder; the dependency decision belongs to B1.

### D1 Public state projection

| Protocol value | Existing agent projection | Attention policy |
| --- | --- | --- |
| idle | idle | No new result attention |
| working | working | Start or continue a work generation |
| blocked | blocked, optional permission/question/auth detail | Existing blocked attention semantics |
| done | idle | Completion effect, subject to D8 |
| error | idle with error detail | Failure-result attention, subject to D8 |
| clear | Release the applicable record authority | Re-evaluate the remaining records or fallback |

An error report means execution failed and stopped; it does not prove an actionable input prompt exists. Keep this distinction in the typed model and action gates. Do not turn an error result into an approval target or count it as a successful completion. Progress and text can be retained internally and shown in debug diagnostics; a new progress UI is outside this feature.

### D2 Authority precedence

First validate observation coverage, the live process episode, exclusions, and record eligibility. While that validation supplies a selected program record in prefer mode, its state wins over fresh hooks, screen/title claims, hook decay, capture dwell, copy-mode/history freezes, and a pager or editor temporarily owning the tty. Those screen constraints still govern fallback. A report does not age out merely because the agent becomes silent. Clear, lifetime events, ownership loss, or an ineligible selection release authority before fallback runs; a valid collector lease alone does not make a record eligible.

### D5 Source, authority, and compatibility projection

`ProgramStatus` must be a real internal evidence origin. A live `ProgramAuthority` is a separate validity fact. The published `Provenance` remains a compatibility encoding. Never infer protocol lifetime from `Provenance::Hook`.

Preserve the closed public `@agent_source` vocabulary. At the external boundary, project valid program reports into the existing `hook` bucket, whose documented meaning is direct application evidence. Distinguish the actual origin with private `@tma_program_*` ownership fields and typed internal/debug output. Do not silently publish a new source token or rename JSON fields. Any later native public source token needs a separately versioned migration.

An ownership guard composes with existing timestamp and state guards. It applies to publication, holds, removals, session-end cleanup, attention, turn markers, process identity fields, and companions that depend on the winning state. No existing `Unconditional` branch may bypass ownership accidentally. Independent context/quota telemetry keeps its own episode checks. Session/transcript binding, model/API endpoint data, subagent bookkeeping, and pending request metadata remain hook/API data with explicit session and process-episode ownership rules. Accept valid updates in these lanes even when accompanying hook state loses arbitration; a valid SessionStart can bind metadata after protocol state has already won. A same-process SessionEnd can release that session's metadata and requests without removing live program state.

Current review targets include `guard_for_verdict`, `render_publish`, `StampPlan::Remove`, `EndSession`, demoted capture, broker reverification, cycle reuse, direct event fallback, the registration reaper, rollout/context model writes, `seen::clear_seen`, runtime UI acknowledgement, and CLI focus/departure clears. Some currently update identity or freshness even when their state write is suppressed. User acknowledgement remains permitted for the current result, but a delayed clear must compare the observed owner and result generation before clearing a newer one. Rollups and returned rows must reflect the winning persisted tuple, not a losing writer's intended verdict.

Picker and watch clears retain the displayed row's identity. A separate focus hook may acknowledge a result already visible on arrival, preserving existing focus behavior. At hook dispatch, capture a bounded session-qualified revision and presentation for both arrival and departure; delayed handlers must match that capture against fresh validated state before using the existing exact acknowledgement operation. This guarantee begins at tmux hook dispatch and format expansion, not the physical navigation event. Known hook invocations without a capture must not acknowledge program-owned state; unmarked older or custom hook strings require regeneration for this guarantee. Explicit direct clears retain their current behavior. The internal hook interface passed a separate narrow review before B5 acceptance. Installation comparisons preserve shell payload bytes while decoding tmux's outer serialization, so broken quoting cannot masquerade as a current command.

Keep a guard's decision stable across its command chain. In particular, releasing an owner token before its dependent fields are cleared must not suppress the rest of that same cleanup; acquiring/replacing ownership must not expose a half-published new tuple. Define the private commit-marker order and winning-write read-back contract in B5 and exercise partial application/failure in real tmux. A submitted command list alone is not evidence that publication won.

Notification permission has a separate claim-specific confirmation. A harmless receipt refresh after a guarded claim may defeat full transaction confirmation while this invocation's unique claim still identifies the same current observation. Confirm that exact token, typed identity, original live coverage, attention and metadata binding, with no matching result ACK or invalidation. This authorizes only the notification effect; it cannot promote a losing publication or advance presentation. The daemon may retry admission once for the same observation after a receipt refresh. Native tests reproduce both sides of the existing 500 ms hook-delivery timeout without changing that timeout or the general transaction contract.

B5a uses `if-shell -F` with a body restricted to literal, explicitly targeted option mutations. A tmux `after-set-option` hook can yield between those mutations, so any effective nonempty hook disables protocol authority for that target. Probe this behavior and recheck the effective hook at write time, including the target context of server lease writes; never clear or override user hooks. Capability loss stops publication for the affected target and revokes its matching pane commit with a single-mutation guard. A collector may renew through another supported target while healthy panes remain covered; revoke its exact lease token when collector-wide capability or all supported coverage is lost. If revocation fails or the collector has crashed, lease expiry permits fallback recovery. B5b must preserve legacy fallback in this configuration without allowing advisory writes to bypass a still-live program owner.

Keep ordinary transactions on the existing argument transport. If a complete ownership guard exceeds tmux's command-message limit, send the same quoted command through `source-file -` using bounded standard input, with a 256 KiB generated-script cap. Loading finishes before the inner guard evaluates, so competing writes during loading must still be caught by its exact comparisons. Probe this transport behavior and retain full commit/readback verification; no temporary files, shell expansion, truncated ancestry, or unguarded retry. The transport addition receives a separate narrow Sol xhigh audit under O4 before B5a acceptance.

Compatibility here means existing readers and documented JSON/pane contracts. An older tma binary that actively writes state does not know the new ownership guard. Upgrade all tma producers together; do not claim mixed-version writer safety. Third-party producers that directly write the same options must also adopt the ownership rule or be disabled for authoritative panes. Arbitrary tmux option writes cannot be prevented by this collector.

### D6 Observation ownership and recovery

Use one selected control-stream owner per pane. Preserve it while valid; use stable session IDs and deterministic selection for a new owner. Linked windows must not cause duplicate parsing or concatenation of fragments from different clients. Every client replacement or owner transfer starts a new connection epoch.

A newly observed stream, including collection enabled after off, starts with unknown terminal framing. A successful tmux attach response does not prove the pane was outside an OSC/DCS string. Require an unambiguous seven-bit ST before accepting a subsequent complete report, accounting for an initially pending escape and repeated escapes. CAN/SUB cannot establish unknown framing because tmux treats them as data inside a DCS payload, as shown by the [tmux 3.7b parser tables](https://github.com/tmux/tmux/blob/3.7b/input.c#L640-L658). The first ST-terminated sequence may therefore establish framing only; BEL-only emitters can remain on fallback until a safe delimiter appears. Preserve known framing across local process-binding boundaries, including the cancellation behavior of each known string kind. Native Pi's report/query body bytes followed by ST provide the recovery path; do not inject synchronization input into an agent.

Keep full protocol records in daemon memory for the current observation session. Persist only the normalized projection, acknowledgement state, and ownership metadata required by independent producers. No new record checkpoint database is needed. On restart or a stream gap, retained/persisted values are historical observations, not proof of present state. Release authority and use fallback until a new report arrives. A restart is not a terminal reset.

A private server-level collector token and lease let same-version fallback producers recover after a daemon crash. Proposed initial lease: renew at most every 5 seconds while coverage is healthy, expire after 15 seconds, and reject an expiry more than 17 seconds into the reader's future. Use monotonic time for scheduling, epoch milliseconds only at the persistence boundary. A detected wall-clock discontinuity or missed lease deadline invalidates the old coverage epoch before renewal. These are collector-liveness bounds; they never require the agent to resend its status.

The server lease establishes collector liveness; each pane additionally needs its matching process and stream epoch. Renew the collector lease for healthy silent panes without refreshing their report timestamps. A gap in one pane releases that pane's authority even while the collector remains healthy for other panes. When the collector misses its own lease deadline, renew only after revoking every old pane authority and creating a new coverage epoch, since fallback writers may already have committed.

Only the matching collector token may renew or release its ownership. A restarted collector uses a new token, so delayed old work cannot renew the new lease. Clear or invalidate per-pane ownership immediately on known stream loss, incompatible reload, ignore/identity changes, or process departure. A stale fallback computation must not clear a newer owner's fields. Do not reconstruct an expired program projection as an ordinary protected hook claim.

Protocol records have their own lifetime reducer. Keep terminal lifetime events distinct from process-episode changes and observer gaps. Completed/error records may remain historical records within the observation session, but they cannot resurrect an exited agent row or become authority for its replacement. No new completed-task browser is in scope.

RIS removes every terminal record. DECSTR and primary/alternate-screen changes preserve them. OSC 133 A and exit of the terminal's attached process remove working/blocked records; choose to remove idle there as well, while retaining done/error only as history. Departure of the independently recognized agent invalidates its projection even if the pane's shell remains alive. Neither agent departure nor observer reconnect is a full terminal reset.

### D7 Episode identity and eligible records

Bind agent projection to independently recognized local process identity and an observation epoch. Add a typed process birth token at the existing process-read boundary, rather than declaring PID alone sufficient. B4 must validate the smallest portable safe source on macOS and Linux, including precision and permissions. Prefer extending the existing process snapshot; use safe platform access only where necessary. If trustworthy binding is unavailable, mark protocol ownership unavailable for that pane and fall back. Do not silently weaken the identity guarantee or add direct libc calls.

Establish that identity before the collector's eligible observation boundary. On cold bind and every detected replacement, advance the pane epoch, discard older queued events and partial frames, resynchronize terminal framing, and require a fresh complete eligible report. Compare birth identity before and after processing and before delayed publication; a detected change discards that batch. Do not adopt bytes already held by the collector before binding merely because a later process sample matches an alias. This orders our own observations; tmux and the PTY can still contain bytes emitted before a handoff, and neither a reattach nor a process snapshot authenticates their emitter. Keep that irreducible in-flight ambiguity inside the documented same-pane trust boundary, fail closed for detected conflicts, and do not introduce a PTY wrapper to claim an exact-emitter guarantee.

Manifest app aliases are optional validated configuration, proposed as `[program_status] apps = ["pi"]`. Compile them when manifests load. An app name is a grouping key, not authentication. It cannot register an unknown agent, cross an ignore/remote/nested-multiplexer exclusion, or grant action capabilities. An honest child tool reporting another app cannot take over the agent projection.

Require a matching app alias to establish protocol ownership. Legal anonymous records can be retained, but affect the agent only after an owner has been established for that live episode and no conflicting app is inherited. Clear without app is accepted for an established owner. Parent changes trigger effective-app re-evaluation for children. Because all processes sharing a pane can write its output, this is collision avoidance and episode binding, not proof of which process emitted each byte.

Track anonymous eligibility by record lineage rather than a permanent pane-wide "app seen" flag. Replacing the root with a foreign app excludes children inheriting it and closes anonymous adoption in that lineage until fresh matching-app evidence re-establishes it. An explicitly matching child can remain eligible independently. Replacing or clearing an ancestor, or evicting its record, recomputes inheritance before selecting state. Never copy an inherited app into a child as though the child explicitly reported it.

### D8 Record selection and completion identity

Within eligible records, use explicit urgency order: blocked, working, error, done, idle. Thus an active child can keep an idle/done root from declaring the whole pane settled. Acknowledged or superseded terminal results remain retained history and contribute only a settled idle baseline; an old child error cannot mask a later fresh root result. Break equal-rank ties by newest update ordinal, then deterministic record ID order. This is tma presentation policy, not protocol ordering. Keep every original typed state so projection does not alter lifetime.

Track completion acknowledgement per record and work generation independently from aggregate selected state. The first observed terminal result can raise attention. Reconciliation and an identical repeated result cannot re-arm it after acknowledgement. A new observed working cycle, or explicit idle reset followed by a new result, can establish a new generation. A dialog temporarily changing done to blocked and restoring done must not manufacture another completion. Aggregate selection switching back to an old completed record is also not a new result.

Result effects do not announce aggregate completion while any eligible record is working or blocked. Record the child result internally, then make the pane result eligible only when the aggregate settles; do not add a public childDone event. If selection later returns to a result already presented or acknowledged, it remains spent. A new aggregate work cycle supersedes terminal results from the prior settled cycle, without deleting their protocol history. Correlate the existing persisted attention/notification markers and focus/input acknowledgements to the owner plus result generation, including the case where acknowledgement and the next report arrive in one daemon wake. A clear acknowledges only the result actually presented, never a deferred child result merely because attention is absent during working/blocked. A new visible result must advance the existing turn marker when needed even if the projected state stays idle.

Keep failure-result attention distinct from successful completion internally. The existing notification trigger maps any noteworthy idle state to `done`; extend its decision input to retain the result kind. Preserve its public trigger/config vocabulary: a configured `done` sink may carry a terminal result with existing `detail=error`, but display text must identify failure and must not say it succeeded. Do not add an `error` state/trigger or infer failure from `msg`. B5/B6 must test this path through notification construction, `wait --since`, and acknowledgement, rather than only testing the pure reducer.

OSC has no completion event identifier. Identical results without intervening observed work cannot be distinguished from retransmission. Choose idempotence and record that limit in the coverage ledger. Hooks do not create an extra completion while protocol authority is active. For fallback after a gap, preserve existing acknowledgement/turn markers where identity is still valid and avoid manufacturing a transition from the stale projection. Restart reconciliation retains the latest normalized presented terminal result and its exact acknowledgement/notification disposition. Match that history only after a fresh eligible report and independent full process-birth-chain validation, using record identity and terminal kind rather than generation counters from different observer epochs. Preserve spent effects when rebinding to a new owner. Reducer-defined new work, a reset or clear affecting the relevant record, and full reset invalidate the corresponding history match; an unrelated idle record does not. History alone never restores authority, and no full-record checkpoint database is introduced.

### D9 Activation and rollout modes

Use a typed configuration mode with `off`, `observe`, and `prefer` values at the configuration boundary. Initial default is `off`; enabling the feature is explicit. Observe mode collects diagnostics while existing detection owns publication. Prefer mode activates the authority rules above. Neither mode changes agent environment or installed hooks silently.

Use runtime `[program_status] mode = "off" | "observe" | "prefer"`, distinct from the manifest's optional `[program_status] apps` aliases. Missing configuration stays off. Off retains the existing control-mode activity service but allocates no protocol stores, parses no protocol payloads, and acquires no collector lease. Observe produces no protocol state, attention, turn-marker, notification, or action effects. Entering prefer starts a fresh eligible observation epoch, so a record observed before the mode change cannot appear as a newly completed result. Document that a silent application may remain on fallback until its next native report.

Pi activation starts with the documented per-process `PI_PROGRAM_STATUS=1 pi` launch. The collector must already cover the session. Recognition of a capability query alone never marks a pane supported. Do not answer queries through `send-keys`, change `TERM`, or rely on `allow-passthrough` to make bare OSC negotiation work.

Claude activation must be tested independently. A changelog or embedded binary string is insufficient evidence. If no reliable supported activation method works inside tmux, preserve Claude's fallback and explicitly limit the verified rollout to Pi. This permits the generic integration to complete while leaving Claude enablement and hook retirement gated. Do not invent an environment switch or silently patch the installed agent.

### D10 Bounded work and diagnostics

Use the protocol's bounded record model and bounded frame scratch. The control decoder streams long output notifications rather than discarding their tail at the old activity-line cap. Retain the existing per-wake read budget so a chatty pane cannot starve other work. Do not queue full raw output.

Add collector-wide record/decoded-text byte accounting. The initial budget is 32 MiB of retained protocol data, with at most 256 records per tracked pane. Allocation overhead is measured separately. If the global budget cannot admit a pane, evict its protocol coverage explicitly, release its authority, and schedule fallback; never retain a possibly incomplete authoritative aggregate. B7 verifies these defaults under churn and may adjust them with recorded evidence.

Immediate typed events preserve per-pane order. Bound both queued event count and retained payload bytes; pending decoded text counts toward the global byte budget. Queue overflow or unknown byte loss invalidates coverage for the affected pane. The gap signal must survive the overflow it reports, for example through a bounded per-pane invalidation flag rather than the saturated event queue. If framing loss prevents attribution to a pane, invalidate all panes covered by that client. Ordinary duplicate reports can avoid redundant tmux writes, but coalescing cannot erase completion, clear, or ownership boundaries. Diagnostics report mode, observed emission, owner/episode, last report, recovery reason, rejected-frame counts, and bounded-queue/budget events without logging raw prompts or arbitrary screen output.

Bound synchronous semantic work as well as bytes. B6 starts with admission limits of 8 expensive operations and 2 seconds of charged I/O per pane per run, plus 32 operations or 5 seconds per run. Cheap duplicate reports consume no semantic I/O budget. Pane exhaustion creates an explicit work-budget gap and requires fresh evidence while healthy peers continue. These are admission limits: an already-admitted bounded operation may finish later. Long batches refresh process facts at the one-second cadence before delayed publication; ordinary short batches retain two fleet snapshots.

On collector-wide exhaustion or shutdown, retire the exact collector lease before bounded cleanup. Cleanup admits at most 4 cached owners or 250 milliseconds per turn; shutdown does not wait for an arbitrarily large cache. Preserve healthy peers when ordinary exact pane cleanup succeeds. Unproved pane revocation stops renewal of that collector token; failed lease retirement relies on natural expiry and must be reported as uncertain. Stale private markers may remain after bounded cleanup, but a revoked lease cannot authorize them.

## Work batches

Execution is serial by default: implement, review and fix, verify, accept, then start the next batch. B2 and B3 could run independently after B1, but the initial work order deliberately avoids concurrent edits to shared interfaces and lockfiles. Current status is recorded in the execution receipts.

Every batch uses a fresh **Sol high** implementer and a different fresh **Sol xhigh** reviewer. In this environment those are `model="gpt-6.1-sol"` with `reasoning_effort="high"` and `"xhigh"`, respectively, both with `fork_turns="none"`. Do not silently substitute a model if unavailable.

| Batch | Deliverable | Prerequisite | Main acceptance | Relative risk |
| --- | --- | --- | --- | --- |
| B1 | Typed protocol model and parser | Plan authorized, baseline recorded | A1 | Medium |
| B2 | Pure records, projection, and attention effects | B1 accepted | A3, A8 | Medium |
| B3 | Streaming tmux transport and pane ownership | B1 and B2 accepted | A2, A10 | High |
| B4 | Process episodes and runtime coverage | B2 and B3 accepted | A4, A6, A7 | High |
| B5 | Shared authority and guarded persistence | B4 accepted | A5, A9, A12 | Highest |
| B6 | Daemon integration, controls, and diagnostics | B5 accepted | A6, A8, A9, A13 | High |
| B7 | Production-path live and adversarial acceptance | B6 accepted | A1 through A13 | High |
| B8 | Documentation, rollout ledger, and release readiness | B7 accepted | A12, final gate | Medium |

### B1 Typed protocol model and parser

**Objective:** introduce the reusable pure protocol boundary without changing production detection behavior.

**Owned paths:** new `tma-core/src/program_status/{mod,parse}.rs`, required exports, focused package tests/fixtures, and the minimal workspace dependency/lockfile change if a decoder is required.

**Work:** freeze the command/record/newtype API and cross-crate event/lifecycle/epoch contracts needed by B2 through B4; reference revision 0.4 rather than duplicating its specification in code comments. Distinguish skipped pairs, invalid optional fields, report rejection, clear, and query. Validate the complete input before exposing a mutation command, including invalid overwritten duplicate fields. Preserve only redacted prototype sequences with agent/tmux versions and the mock-model provenance. No authority, daemon configuration, or hook changes yet.

**Exit evidence:** A1 parser cases and mutation-safety properties pass; rejected input does not allocate retained records; the original manifest `Claim` grammar remains strict. Run core tests with all features and formatting/clippy appropriate to changed packages.

**Reviewer focus:** malformed-input recovery, integer/text boundaries, unintended serde grammar changes, unnecessary allocations, dependency/MSRV fit. The reviewer fixes defects in these owned paths and reruns affected checks.

### B2 Pure records, projection, and attention effects

**Objective:** produce deterministic, typed state decisions from validated commands and injected lifecycle facts.

**Owned paths:** `program_status/{store,project}.rs`, relevant core evidence/verdict/fold types, package tests. Serialize any shared export change with the orchestrator.

**Work:** implement full replacement, record eviction, inheritance and selection; keep records independent of manifest claims and live-agent row lifetime. Add typed completion/failure effects and acknowledgement generations. Carry actual evidence origin independently from legacy provenance. Replace negative source filtering with exhaustive matches. Add a typed fold entry point with an explicit optional program-selection/authority input and retain a compatibility entry point for existing callers until integration. Without program authority behavior remains unchanged. Make only the minimal exhaustive-match/call-site adjustments needed outside the owned core paths to keep every batch buildable, under orchestrator ownership.

**Exit evidence:** A3 and A8, plus pure precedence cases against fresh hooks, capture blockers, copied/history view, and a pager foreground with a still-live owner. Large injected time advances do not decay program state. Record exit/history policy follows D6, not whatever the old tuple happened to encode.

**Reviewer focus:** child aggregation, dialog restoration, duplicate terminal reports, no accidental blocked/error action equivalence, and semantic changes to existing fallback tests.

### B3 Streaming tmux transport and pane ownership

**Objective:** deliver ordered typed observations without losing bytes or mixing sources.

**Owned paths:** `tma-tmux/src/control.rs`, focused control submodules, shared lower-layer ID types agreed in B1, `tma-tmux/tests/control_integration.rs` and new package tests as needed.

**Work:** decode headers separately from payload for `%output` and `%extended-output`, including future header fields up to the standalone colon; stream octal escapes and terminal bytes across arbitrary read boundaries. Bound command-response framing separately so text inside a `%begin`/`%end` or `%error` block cannot spoof a notification. Use a terminal state machine that skips unrelated OSC/DCS/APC/PM strings, handles cancellation and escape termination, and recognizes protocol/lifetime events. A fully delimited invalid or oversized report is an atomic rejection; transport/framing loss is a coverage gap. Add connection epochs, deterministic per-pane stream ownership, topology updates keyed by stable session IDs, and typed discontinuities, including `%pause`/`%continue`, malformed octal and EOF/error with a partial frame. Resume alone never revalidates lost coverage. Preserve existing activity hints for fallback. Extended-output backlog age is metadata, not sender event time. Freeze numeric event-count/header limits and their fairness tests in this batch. Expose events without publishing agent state yet.

**Exit evidence:** A2 and A10, including the actual Pi frame split across two notifications, huge ordinary output with a valid report near its tail, non-UTF-8 output, linked windows, and EOF in an escape. Query handling remains observational and injects no input into an agent.

**Reviewer focus:** bounded buffers, terminal resynchronization, fake OSC inside OSC/DCS text, fairness, connection handoff, and regressions to control-client attach/recovery behavior.

### B4 Process episodes and runtime coverage

**Objective:** decide whether a retained report belongs to the current agent and whether observation remains complete enough to trust it.

**Owned paths:** new runtime program-status module, `identity.rs`, `tma-core/src/snapshot.rs`, the process-read adapter, manifest app-alias schema/loading, the shipped Pi manifest, and corresponding package tests. B4 also owns the narrow B3 transport admission-budget seam needed to enforce the combined bound while bytes arrive; runtime owns the 32 MiB policy and supplies the collector's remaining allowance.

**Work:** add process birth tokens with explicit macOS/Linux evidence; bind record stores to agent/stream episodes through the collector queue/frame boundaries and process rechecks in D7; implement app eligibility, anonymous-record policy, ignore/exclusion checks, loss/reset/prompt behavior, and global storage accounting. Add Pi's app alias from the native B1 fixture without enabling runtime collection by default. Sample once around a bounded processing batch and before delayed publication, never per byte/report. Retain typed historical terminal records separately from current-agent authority. On inability to establish ownership, return a recovery/fallback reason instead of guessing. Record the same-pane in-flight attribution limit explicitly.

**Exit evidence:** A4, A6 and A7 without requiring publication yet. Demonstrate PID reuse/replacement behavior and platform birth-token limits. A foreign child tool, stale queued record, or new agent in the same pane cannot inherit old authority. Reconnect alone never restores coverage.

**Reviewer focus:** process sampling races, unsupported platform behavior, stale ownership after topology/config changes, memory budgets, and truthful limits of app attribution. If portable birth identity cannot meet the stated gate, escalate that concrete design choice to the orchestrator before implementing B5; do not substitute PID silently.

### B5 Shared authority and guarded persistence

**Objective:** make all production writers obey the same ownership rule and recover safely after the owner disappears.

**Owned paths:** core stamp/render/verdict adapters, `tma-tmux/src/stamp.rs` and option/read adapters, runtime cycle/capture/event/identity/broker/rollout/seen/UI seams, CLI focus/departure acknowledgement, notification result-kind adapters, and focused integration tests. This batch owns the shared write contract and runs with one writer at a time.

**Work:** add private collector/episode/stream authority fields, lease acquisition/renewal/release, typed guard composition, and legacy source projection. Audit every publish/hold/remove/end-session/acknowledgement path and every field it writes. State-dependent fields share the winning guard; the metadata lanes listed in D5 retain explicit independent episode/session rules. Correlate guarded acknowledgement and notification commits to the result generation. Derive rows and rollups from the actual winner. Decode an expired or mismatched protocol projection as stale, never as an ordinary indefinitely protected hook claim.

**Exit evidence:** A5, A9 and A12 in scratch tmux with controlled conflicting write orders, including demotion and nominally unconditional paths. Verify lease expiry, token replacement, backward/forward clock movement, late old cleanup, and independent metadata updates. When safe conditional writes are unavailable, including an effective `after-set-option` hook, disable protocol authority with a diagnostic and retain fallback; do not degrade this lane to unguarded writes.

**Reviewer focus:** complete-tuple suppression, old-session cleanup, hook direct fallback, freshness fields, companion metadata, observed-vs-intended summaries, and write-time ownership. Generated-format string tests alone are insufficient.

**Execution split:** B5 runs as two serial work batches, each with a fresh Sol high implementer followed by a different Sol xhigh fixing reviewer. B5a implements typed persisted ownership, collector leases, frozen conditional command groups, capability probing, commit/readback, and exact acknowledgement/notification primitives in core/tmux with native tests. B5b adopts these at every state producer and result consumer, preserves independent hook metadata lanes, and derives rows/rollups from the persisted winner. Root accepts B5a before dispatching B5b. The combined A5/A9/A12 gate remains required before B6 can enable protocol authority.

### B6 Daemon integration, controls, and diagnostics

**Objective:** make the feature operate through the real serve loop with prompt publication and predictable recovery.

**Owned paths:** daemon serve/lifecycle, runtime program-status/capture/cycle/config/debug, CLI doctor, related JSON output sites/tests if additive diagnostics require them.

**Work:** wire immediate events before the next blocking wait; coordinate hook, protocol, fallback and notification ordering through the shared authority model. Drain each pane in order, preserve semantic effects before coalescing writes, wake waiters and update rollups from the committed result. Skip unnecessary fallback capture for healthy authoritative panes. Implement off/observe/prefer, collector lease scheduling, reload/disable/stop/crash recovery, and bounded diagnostics. Invalid reload leaves the previous coherent configuration active.

**Exit evidence:** A6, A8, A9 and A13. Blocked publication precedes the quiet threshold. Off/observe do not acquire authority or duplicate outward notifications. Disable releases ownership promptly. Continuous output cannot starve reports, other panes, liveness checks or shutdown. No loop repeatedly spawns a process for unchanged reports.

**Reviewer focus:** ready-fd ordering, state changes arriving near hook IPC, publication versus notification consistency, completion deduplication, lease renewal with unhealthy coverage, and shutdown cleanup.

### B7 Production-path live and adversarial acceptance

**Objective:** establish real coverage and failure behavior of the integrated candidate.

**Owned paths:** package integration tests, fixture provenance, narrowly scoped `tma-test-support` helpers, and fixes to the integrated paths required by a demonstrated failure. No new product scope.

**Work:** recreate the Pi prototype using the production daemon and isolated agent configuration. Keep the local deterministic HTTP model and a minimal dialog extension that only triggers real Pi UI; do not synthesize OSC in that extension. Exercise working, done, permission dialog, error, cancellation, rapid turns, exit and clear. Add focused tests for gaps, overflow, linked sessions, process replacement, conflicting writers, acknowledgements, and spoofed message/action text. Investigate Claude activation separately and record exactly what ran. Test CPU/memory/write behavior for silent, duplicate-heavy, and ordinary-output-heavy panes.

**Exit evidence:** every applicable A-row has a concrete result tied to the final candidate. Pi's live lane must actually execute with hooks absent. Claude is either verified with a documented activation path or explicitly left on fallback with no coverage claim. Record observed latency and resource figures; cap tests must prove bounded retention and fair service, not claim an unmeasured zero-allocation pipeline.

**Reviewer focus:** tests that can pass while production code is bypassed, silent skips, mock emission mistaken for native emission, incomplete teardown, copied credentials, and omitted known failure paths. Fix any in-scope implementation failures before the gate closes.

### B8 Documentation, rollout ledger, and release readiness

**Objective:** leave a coherent, reviewable feature with accurate operational guidance and a clear retirement path.

**Owned paths:** detection model, agent coverage, configuration, pane/JSON contracts, daemon how-to, doctor guidance, changelog, fixture README, and this work order's execution receipts. Update `docs/SUMMARY.md` only if book pages are added or moved.

**Work:** document activation, mode changes, coverage loss, compatibility projection, mixed-version/third-party writer limitations, result/failure acknowledgement and notification policy, and optional metadata/control adapters. Correct older Pi coverage text that assumes no blocked dialog signal exists. Publish the agent/version/activation coverage ledger. Keep the feature explicitly opt-in until evidence supports a separate default-change decision. Prepare the final diff summary and evidence; do not push, release or uninstall hooks without a later instruction authorizing those actions.

**Exit evidence:** required workspace checks and doc checks pass on the final reviewed tree, all batch receipts reconcile, no unresolved correctness finding remains, and the user can enable and disable the feature using the documented commands/settings. Unsupported or unverified cases have an explicit fallback, not a claimed pass.

**Reviewer focus:** documentation versus final behavior, schema key/token promises, honest live coverage, cleanup instructions, and scope creep into daemonless or hook removal.

## Verification and acceptance

| Code | Required scenario | Evidence |
| --- | --- | --- |
| A1 | Protocol grammar and bounded atomic validation | Pure package tests reference revision 0.4, including valid/invalid boundaries, optional fields and duplicates; rejection leaves retained data and eviction order unchanged. |
| A2 | Incremental bytes and control framing | All split positions for representative frames, property-generated chunking, octal splits/errors, arbitrary terminal bytes, unrelated control strings, command-block spoofing, pause/resume, oversized input and recovery. Distinguish rejected complete reports from lost observation. |
| A3 | Records and projection | Parent/child/sibling cases, similarly prefixed IDs, replacements, inheritance changes, ancestor eviction, deterministic selection, foreign-root collisions and anonymous eligibility revocation. |
| A4 | Episode and app attribution | Reused PID, replaced agent, stale collector-queued output before identity sampling, cold-launch/attach races, unknown process birth, anonymous records, foreign tool apps, ignored and excluded panes. Demonstrate epoch/frame boundaries and process rechecks on each supported platform; distinguish those guarantees from unknown in-flight PTY data. |
| A5 | Conflicting writers | Real tmux ordering tests assert the entire tuple, attention, request companions and summaries; include hook/capture/OSC races, equal clocks, old episodes and late cleanup. |
| A6 | Observation gaps and collector loss | Late attach, dropped client, daemon restart/crash, queue/budget loss, owner transfer and lease expiry produce explicit fallback until new valid evidence. |
| A7 | Lifetime | Prompt/full/soft reset, alternate screen, agent exit, pane close, server loss, and replacement preserve the distinction between terminal history and live authority. |
| A8 | Completion and acknowledgement | Duplicate terminal reports after acknowledgement, dialog restore, two work generations, child result while root works, aggregate settlement, concurrent delayed focus/input clear, hook coexistence, restart and gap recovery cannot duplicate or invent completion. Error attention retains failure identity through notification and wait consumers. |
| A9 | Action safeguards | Program text/kind alone cannot create a request ID, tool call or approval. Existing binder/recheck/single-flight checks and same-episode hook metadata still work. |
| A10 | Multiple sessions and fairness | Linked-window streams yield one owner and one logical event; no fragment splicing; session rename and owner transfer are deterministic. |
| A11 | Live activation | Real Pi through the production daemon, isolated config and no state hooks; native emission distinguished from the deterministic model and test dialog. Claude has a separate verdict. |
| A12 | Compatibility and operational modes | Existing tokens/keys and boundary tests, same-version daemonless fallback, unsupported tmux capability behavior, off/observe/prefer, reload and disable. |
| A13 | Scheduling and resource bounds | Blocked state arrives before quiet-edge capture; hot panes cannot starve others; silence does not decay active records; storage/queue caps and subprocess counts are measured. |

Run focused checks as each batch changes code. Before narrowed cross-package integration tests, build the workspace so tests do not execute a stale `tma` binary. Never set `TMA_TEST_BIN_NO_STALE_CHECK` to hide that condition. In this environment use the repository's Nix development shell because Cargo is not on the outer shell PATH.

Final required commands are `cargo build --workspace`, `TMA_REQUIRE_TMUX=1 mise run test`, and `mise run lint` inside that shell. The test task includes all workspace features; lint includes clippy, fmt, and rustdoc. Run `mdbook build` for book edits and preserve the Rust 1.88 MSRV check. Run the specifically named Pi integration test with `TMA_LIVE_AGENTS=1`; an opted-in lane fails if its required binary/setup is unavailable. Record live execution separately from skipped cases.

Execution preference: build and verify natively on this machine through the Nix development shell. The user explicitly excluded Docker builds during B1; the preliminary scratch container has been removed. Platform-specific behavior that cannot execute natively must be identified as such rather than claimed from an unexecuted check.

Normal tests use synthetic emitters for adversarial bytes, while the live lane verifies native agent behavior. Neither replaces the other. No test or script copies user credentials or changes real agent settings. All tmux sessions and local model servers are scratch resources with bounded lifetime and teardown. Every tmux test uses an explicit scratch socket selector, with `TMUX` and `TMUX_PANE` removed from test commands; the user's running server is never a test target.

## Orchestration and review protocol

| Code | Orchestrator obligation |
| --- | --- |
| O1 | Before a batch, check the actual branch/diff, rebase the plan onto current code, record baseline failures, and name its prerequisite receipts. Do not overwrite user work. |
| O2 | Send a self-contained brief to a fresh Sol high worker: G1, applicable decisions, exact allowed paths, interface contracts, acceptance IDs, commands, forbidden scope, and expected handoff. Workers do not spawn their own teams. |
| O3 | Give the fresh Sol xhigh reviewer the original requirements, integrated diff and neighboring consumers. It has write ownership for in-scope fixes after the implementer stops. It must report findings and the fixes applied, not only suggestions. |
| O4 | Inspect reviewer fixes and run checks appropriate to the final candidate. Evidence predating a fix does not certify it. A material design introduced during review receives independent review; small corrections do not trigger endless blanket passes. |
| O5 | Advance only when acceptance evidence matches the reviewed tree and substantive findings are resolved. Maintain statuses planned, implementing, reviewing, verifying, accepted, or blocked, with a concrete blocker. |
| O6 | Keep changes serialized across shared interfaces, manifests, Cargo files and stamp formats. Use worktrees only when isolation is actually helpful and all prerequisite changes are present. Preserve a single integrated candidate. |
| O7 | Update the work order and coverage ledger after each accepted batch. A requirement change invalidates only affected acceptance evidence. Do not silently redefine success to match the implementation. |
| O8 | Stop after the agreed feature is complete, or at a concrete blocker needing a user decision. Execution follows the active user goal, with no implied permission to publish or retire existing operation modes. |

Every handoff contains the batch ID, base revision plus working-diff identity, changed files, design decisions, commands/results, acceptance IDs, reviewer fixes, remaining findings, and next-batch readiness. The root remains accountable for integration and cannot treat a sub-agent's success message as sufficient evidence.

Use the requested model configurations for every implementation/review pair. Reviewer and implementer must be distinct agents. If one becomes unavailable, report the model constraint rather than substituting silently. Clean Conventional Commit messages, with no AI attribution, apply if committing is later authorized.

## Migration and retirement criteria

**D3. Hookless state monitoring remains the long-term target.** This feature establishes the generic path and a per-agent coverage ledger. Removing daemonless operation and retiring hook integrations are later decisions backed by that ledger.

| Code | Later milestone | Gate |
| --- | --- | --- |
| M1 | Prefer OSC by default for a verified configuration | Native emission, timing, lifecycle, restart/gap recovery and resource acceptance pass; activation is documented and reproducible. |
| M2 | Stop installing redundant state mappings for an agent/version range | Every formerly supported state path has verified coverage, fallback behavior is acceptable, and metadata/action callbacks are split from state callbacks. |
| M3 | Remove daemonless operation | Separate compatibility decision covers CLI startup/availability, supervision, remote/container behavior, packaging, failure UX and migration. Keep protocol collection isolated so this is subtraction rather than a second redesign. |
| M4 | Remove the remaining per-agent hooks/adapters | Session identity, telemetry, transcript and action capabilities each have another verified source, or the user explicitly retires the feature. OSC status alone does not satisfy this gate. |

The ledger records agent version, tmux version/platform, activation method, tested states/dialogs, completion fidelity, nested/background work, collector recovery, retained adapters, and executed versus unverified scenarios. A capability query or release note cannot substitute for those observations.

## Decisions and references

### Review points and known limits

| Code | Risk or decision | Planned treatment |
| --- | --- | --- |
| R1 | Reliable ownership across competing producers is the largest integration risk. | B4 establishes identity; B5 owns one complete write policy; B7 exercises the actual races. |
| R2 | Claude activation under tmux is unverified, and Pi's normal negotiation fails in the prototype. | D9 uses verified forced Pi emission and a separate Claude gate. No async reply injection. |
| R3 | Historical records cannot establish current state after observation loss. | D6 clears authority and falls back; no claim of complete recovery without a new report. |
| R4 | Same-pane output lacks emitter identity and completion IDs, including bytes already in flight during a process handoff. | D7 rejects known stale collector epochs and detected process conflicts without claiming exact byte attribution; D8 makes identical terminal results idempotent; A9 preserves action safeguards. |
| R5 | Process birth-token portability and collector lease defaults need platform evidence. | B4 verifies birth identity before B5; B7 records resource/recovery measurements. Failed identity binding falls back. |

The proposed defaults are concrete review targets: compatibility projection D5, loss policy D6, selection/attention D8, rollout mode D9, and the lease/global memory bounds. Changing one during planning updates dependent batch contracts before work starts. No implementation approval is implied by accepting a wording correction in the review page.

### Primary sources and code seams

**S1.** [OSC 7501 revision 0.4](https://www.superlogical.com/rex/docs/build/program-status) is the normative wire reference. Its syntax, limits and record-lifetime rules govern B1/B2; tma-specific ownership, projection and rollout choices are defined above.

**S2.** [Pi 1.3.0 terminal implementation](https://github.com/earendil-works/pi/blob/v1.3.0/packages/tui/src/terminal.ts#L288-L311), [its late-reply regression test](https://github.com/earendil-works/pi/blob/v1.3.0/packages/tui/test/terminal.test.ts#L273-L289), and [Pi's terminal setup documentation](https://github.com/earendil-works/pi/blob/v1.3.0/packages/coding-agent/docs/terminal-setup.md#program-status) establish the activation behavior and supported override used by the prototype.

**S3.** [Claude Code 2.1.295 release notes](https://github.com/anthropics/claude-code/releases/tag/v2.1.295) announce protocol support. They do not establish working tmux activation or complete lifecycle coverage for this integration.

**S4.** Repository contracts: `CONTRIBUTING.md`, `mise.toml`, `docs/reference/agent-state-contract.md`, `docs/explanation/detection-model.md`, `docs/how-to/install-agent-hooks.md`, and `docs/reference/agent-coverage.md`. Existing tier/UI boundary and JSON/pane exact-key tests remain acceptance constraints.

**S5.** High-risk implementation seams: `crates/tma-tmux/src/control.rs`; `crates/tma-core/src/{evidence,fold,stamp,render,verdict}.rs`; `crates/tma-runtime/src/{cycle,capture,event,identity}.rs`; `crates/tma-runtime/src/broker/tmux_io.rs`; `crates/tma-daemon/src/daemon/serve.rs`; `crates/tma/assets/pi-extension.js`.

**S6.** The [tmux control-mode contract](https://man.openbsd.org/tmux.1#CONTROL_MODE) defines command-response blocks, octal-escaped pane output, extended-output headers and pause/continue notifications. B3 pins behavior against the tested tmux versions, rather than treating every percent-prefixed line as pane output.

### Execution receipts

Planning inputs came from separate architecture, transport, and acceptance agents. A final independent Sol xhigh reviewer applied corrections to this work order; the orchestrator reviewed those corrections. No production code or tests were changed or executed during planning.

Execution baseline (2026-10-26): `main` at `17470d5c1bdcf25376678f8ca0f98bd578588d89`; only the planning document and its HTML review artifact were untracked. `nix develop --command cargo test -p tma-core --all-features` passed before implementation. No applicable filesystem `AGENTS.md` or `CLAUDE.md` files were found; session instructions and `CONTRIBUTING.md` apply.

The native full-workspace baseline ran against an isolated copy of accepted B1 while B2 was being implemented. It reported 1775 passed and one existing timing failure in `window_name_integration::a_rename_round_trips_an_inherited_automatic_rename`: automatic renaming changed the restored name before a later read. A focused rerun passed. B7 must address that fixture race before the final gate; this full run is recorded as failed. Live-agent gates were not enabled.

| Batch | Status | Implementer | Reviewer | Final evidence |
| --- | --- | --- | --- | --- |
| B1 | Accepted | `osc_b1_implement`, Sol high | `osc_b1_review`, Sol xhigh | 391 core tests (21 protocol), clippy/fmt/rustdoc, root diff inspection, native Rust 1.88 all-features workspace check pass. Reviewer fixed title construction bounds and partial base64 padding. Candidate `04aa3b72fb86c1f5101ef5b3c1114d4b1158437dbddab4680a954b78d5f4d929`. |
| B2 | Accepted | `osc_b2_implement`, Sol high | `osc_b2_review`, Sol xhigh | 431 core tests (40 new B2), clippy/fmt, workspace all-target/all-feature checks and rustdoc pass. Reviewer fixed deferred-result adoption, revoked-result settlement, foreign same-ID generation resets, and mutable validated selections. Root inspected fixes and passed native Rust 1.88 workspace/all-target/all-feature check. Candidate `21cdd704c790ddf68c2687c35757c580d47728e1e82a6ffc902fcc4099ce27ad`. |
| B3 | Accepted | `osc_b3_implement`, Sol high | `osc_b3_review`, Sol xhigh; narrow `osc_b3_framing_audit`, Sol xhigh | Final 100 unit and 10 native control integration tests pass; workspace check, clippy/fmt/rustdoc and native Rust 1.88 all-target/all-feature check pass. Reviews fixed escape/framing, quarantine, owner isolation, command cap and prompt-lifetime defects; root inspected fixes and replayed the framing model. Cold recovery follows corrected D6. Candidate `c8e5b0a35e4e579de2ef29a2d69d6d916896ab4dfbef57ef985c52159100faf4`. |
| B4 | Accepted | `osc_b4_implement`, Sol high | `osc_b4_review`, Sol xhigh | 433 runtime, 431 core and 111 tmux unit tests; 11 native integrations and 2 source-guard tests pass. Review fixed undrained-stream publication, budget recovery across rebinding/sample failure, and exited-process identity. Root inspected fixes and corrected ancestry evidence, and passed native Rust 1.88 all-target/all-feature check. Candidate `9da1e9a044f783414fdbd88ea8b6fe103a24f92d3053209ec90f9dba7c1ffe29`. |
| B5 | Accepted | `osc_b5a_implement` and `osc_b5b_implement`, Sol high | `osc_b5a_review`, `osc_b5a_transport_audit`, `osc_b5b_review` and `osc_b5b_focus_audit`, Sol xhigh | B5a primitives accepted at `8a188797001025e227d8a244196da21b3f2f8ff5df15887f1aea7eede8f00fa3`. Final combined candidate `6092591c5614e11665309f5c91581244e017e8e047cf3b57bb60c735388ef587` (94 cumulative files) adopts ownership across producers and consumers, preserves independent metadata and result history, and adds audited focus capture. Main review passes 443 core, 433 runtime, 52 ownership, selected legacy, 232 CLI, 55 daemon, 152 tmux and UI gates. Focus audit passes 9 focused native cases, with the final 4 changed rendezvous cases refreshed, plus 23 installer cases serially and affected unit/schema checks. Root inspected all fixes, matched the final snapshot and passed native Rust 1.88 workspace/all-target/all-feature check. Static checks and mdBook pass; retained failures and execution limits follow below. |
| B6 | Accepted | `osc_b6_implement`, Sol high | `osc_b6_review`, Sol xhigh | Candidate `e1cdf71bf56569d841035ac0eca641da5732bc398714637dd68dc8abc5c5445c` (108 cumulative files) connects production modes, scheduling, ordered effects, recovery and diagnostics. Review corrected continuation boundaries, facts refreshed after slow ACK reads, notification/progress admission and pruning after fresh pane absence. Final selected checks and isolated replays total 751 passing executions; two broad commands failed before their scenario premises and remain recorded below. Build/check/clippy/fmt/rustdoc pass. Root inspected the 11-path review delta, matched the frozen source and passed native Rust 1.88 workspace/all-target/all-feature check in 3.16s. Both workers stopped. |
| B7 | Accepted | `osc_b7_implement`, Sol high; stopped | `osc_b7_review`, Sol xhigh; stopped; independent Sol xhigh RF14 and RF17/RF18 audits complete | Candidate `c1833d7a6a2c029560339d37fe650b20aea356f1bcfba0d5138ad283590b0ef5` (136 cumulative files) matches the independent root acceptance snapshot. Full workspace, lint, separate native Pi and root Rust 1.88 checks pass; final scoped cleanup finds no owned processes. Both narrow audits remain clean. The final results and historical failed runs are recorded below. Claude activation and native Clear consumption retain their explicit limits. |
| B8 | Accepted | `osc_b8_implement`, Sol high; stopped | `osc_b8_review`, Sol xhigh; stopped | Final candidate `8fac5b8765b9b8f479a12f92283442a7c743c28675e461852782d3a37b2f8ca5` (149 cumulative files) matches root's independent snapshot. The final B8 delta covers 16 documentation files, Pi manifest comments and a four-line default-documentation test helper. Parsed Pi values and runtime logic are unchanged from B7. Review fixes DR1 through DR5 are resolved; 552 selected test bodies, 327 rendered local links/anchors, build, lint and mdBook pass. Root verified the complete deltas, evidence hashes, final Rust 1.88 check and isolated native Nix package. |

B5 validation limits: one main-review SessionStart test missed its initial state; the unchanged-assertion event suite then passed all 11 cases, with the cause unproved. The focus audit's parallel installer run passed 18 cases and failed 5 during setup: 2 explicit three-second tmux timeouts and 3 failed statuses without stderr. All 23 passed serially. These failed runs remain recorded, and B7 must investigate recurrence during full acceptance. The 64 KiB summary snapshot bound can retain an oversized scope's previous hint; a diagnostic reports it, while pane publication and rows remain independently guarded.

B5 execution incident: an early fixture invoked `clear-attention` three times without a scratch socket selector. The user was informed; external effects remain unknown and no live-server inspection or restoration was attempted. The fixture now always selects its owned socket. A temporary execution guard rejects non-scratch tmux calls before execution and fails the enclosing run on any new rejection. Subsequent acceptance runs used that guard with no new rejection; historical blocked fixture calls are distinct from the three executed calls.

B6 isolation finding: a broader daemon run reported 104 passing test bodies but failed its execution guard because 60 unqualified TPM-style tmux calls were blocked before real tmux execution. Source inspection points to a scratch-server recreation loading ambient startup configuration; the exact origin remains an inference, and other configuration-script effects were not assessed. The shared harness now isolates child home/config directories. An owned default-configuration sentinel, affected harness/CLI/installer checks, and the refreshed 112-case daemon gate passed without new guard rejections. Independent review also completed with no guard growth. The historical guard log is retained at 95 lines and 7,850 bytes. B7 audits separate command factories before full-suite execution.

B6 validation limits: the original ten-second shutdown timeout and same-wake final-state failure have no established cause; later passes do not diagnose them. A separate post-exit stale-marker assertion failed after refactoring. A deterministic test now proves the permitted unconfirmed-write window: an applied publication can leave a private COMMIT after shutdown, while the exact collector token is retired with expiry zero, live authority and attention permission are absent, and no notification fires. The final shutdown test retains its ten-second exit bound and checks those authority guarantees. Semantic-work thresholds are admission limits; a currently admitted bounded operation may finish later. Independent review corrected and verified the boundaries between operations, exact cleanup, delayed ACK facts and notification ordering. Its pane-removal regression proves stale-target IO after fresh membership; the initially suspected broader collector-loss outcome remains unproved.

B6 final review retained two additional failed broad commands. Runtime unit tests passed 434 cases and ownership passed 54 of 55; a legacy claim command hit its three-second timeout before the claim-written rendezvous. Its unchanged isolated replay passed in 5.04s. Daemon unit, legacy and notification targets passed 43, 34 and 17 cases; production passed 22 of 23, with one fixture reaching the 45-second initial attach/bind ceiling before its ACK scenario. Empty status/log and the missing child exit status do not distinguish early exit from startup hang. Its unchanged isolated replay passed in 7.17s and reached the intended scenario. Both causes remain unproved. Tmux checks passed 114 unit, 29 persistence and 2 source-guard cases. B7 retains recurrence investigation and the full-workspace gate; these individual passes do not turn either failed broad command into a green run.

B7 native evidence: Pi 1.3.0 on native Darwin 25.6 arm64 with tmux 3.7b exercised working, three completions, permission dialog/restore, HTTP failure and cancellation through the production daemon. The corrected candidate passes the named native test in 23.94 seconds. Exact pane/episode notification checks passed; cancellation produced idle with no result, attention or completion effect while the local model response was held. In this run blocked state arrived in 1,116 ms before the configured 10-second quiet edge; shutdown took 340 ms. Exit preserved the pane shell and released authority, but native Clear consumption was not established. Claude Code 2.1.303 reached first-run theme onboarding during its separate bounded probe; no model or lifecycle coverage was verified, and cold decoder framing prevents an inference of no raw OSC emission.

B7 full-gate result: the unchanged implementation candidate completed the all-feature workspace command in 2,121.15 seconds, exiting 101. Its 91 target summaries report 2,030 returned passes and 10 failures; returned passes include four disabled-live bodies and a doctor readiness skip. The named native Pi execution is separate. Ownership passed 55 cases, persistence 29, and all five new production cases passed. Final lint and diff checks passed, with no growth in the scratch tmux guard log. Review covers fixture identity and isolation (RF6 through RF9, RF11, RF15), the vanished-pane exit-code regression (RF10), daemon ACK/startup evidence (RF12 and RF13), and the extra polling write (RF14). Two exact owned leaked test children were verified and reaped. The old per-Python uv shim is retired; the replacement runs the whole test command through uv and the revised broker fixture verifies that Python is the Rust-owned direct child, with bounded readiness and cleanup on unwinding (RF16). Its two focused tests pass in 0.12 seconds. No failed command or unproved cause is reclassified as a pass.

RF14 reviewed correction: a dedicated typed advisory poll hint is batched with the pane write, guarded by its exact fresh receipt and target-local hook capability. The hint means the complete server-side write executed; a later concurrent receipt replacement may leave it present even when client readback returns Lost. It grants no state, result or effect permission. Full pane readback and Won rules remain unchanged, and arbitrary trailing commands cannot enter restricted persistence groups. Native checks prove one writing invocation for a normal working cycle and zero option writes for a quiet cycle, plus ownership/capability refusal and Lost readback with a retained advisory hint. A separate Sol xhigh audit completed cleanly after a wording correction. These checks do not measure attached-client redraws.

B7 review progress: affected action, doctor and wait targets pass 29, 10 and 16 cases. Doctor now reaches its remote assertions instead of silently skipping. RF10 has a deterministic fail-before and a typed vanished-pane correction that requires a complete fresh pane inventory; malformed live reads still fail. RF13 has a deterministic socket-resolution error test and preserves intentional no-server and best-effort ensure behavior, but the original intermittent startup cause remains unproved. The timestamp fixture and targeted startup/ACK replays pass. After RF17/RF18, all seven subscription cases pass in 31.08 seconds. The corrected candidate passes a genuine workspace rebuild, full lint and the native Pi gate. The completed final workspace gate below follows these selected checks.

RF17 metadata race: the subscribe suite still failed after actual subscriber baseline readiness was witnessed. A deterministic native reproduction then gated the first metadata write and changed only its shared receipt with a fallback refresh. The event returned Applied while state stayed idle and its session and metadata owner were absent. The correction permits one metadata re-admission only when the original owner, metadata values/update token, public PID/name and episode remain unchanged, with the complete native process birth chain rechecked before each attempt. Existing evidence guards and protocol ownership remain in force. Expanded native checks prove receipt-only recovery and refusal after a competing session, metadata field, private episode or actual process replacement. An already-applied write that later loses readback is not replayed. A separate Sol xhigh audit is clean on the frozen correction. This does not establish the cause of every earlier intermittent failure.

RF18 sessionless state: a separate native reproduction shows a first blocked event with an empty payload cannot publish because no metadata edits establish the private binding required by state publication. A working event binds through existing request-clear edits. The correction establishes the already validated private binding for a state-stamp plan when it differs from the stored binding, even when its metadata edits are empty. Native checks now prove working and blocked publication while public session, tool and request fields remain absent. Already bound empty events avoid the extra metadata write. RF17/RF18's three expanded regressions pass in 20.00 seconds, and the joint independent audit reports no correctness finding.

RF19 fixture isolation: three later broad runs each triggered 20 rejected unqualified TPM-style tmux calls, bringing the retained guard log to 155 lines and 13,706 bytes. None of those rejected calls reached native tmux. The first two runs' exact subprocess ancestry was not captured, and the initial picker correlation was disproved when the calls recurred before that target. Each run was stopped after its active target completed normal cleanup: wait 16/16, serve 28/28, then slots 10/10. They returned 428, 358 and 370 passing bodies respectively, including two disabled-live bodies each, but none completed the suite. The second and third whole-wrapper exit receipts are 125. Owned coordinator processes were verified gone; the history is preserved.

RF19 correction evidence: a private-profile sentinel proved bare pane startup reads that profile. The two init fixtures were still using ambient native starts before their separately isolated tma child; a targeted probe recorded two private-profile reads before the fix and none afterward, while all init assertions passed. Interactive picker/watch/wait/action panes now use explicit isolated shells, and idle doctor/cleanup/init plus later split/window fixtures use explicit idle processes. Linked-session operations remain unchanged. Sixty affected fixture checks and five final membership/edge-removal/init checks pass without new guard entries. The guard's selector policy is unchanged; a bounded read-only ancestry sidecar captured the third run's actual TPM parent: an owned install-keys scratch probe server. The bindings-only test sourced a generated file that launched an unowned background daemon. It now passes `--no-daemon` while retaining all source-file and key-table assertions. Three native install-keys cases and two daemon-launcher rendering cases pass. The probe ancestry is proved; its precise scheduling relative to teardown remains inferred, and other startup-script effects were not assessed.

RF20 fixture context: the isolated shell initially removed its own native TMUX_PANE, so watch rendered but could not advertise its PID. The helper now preserves only the pane identity assigned inside that newly created scratch pane; parent ambient tmux context remains scrubbed and CLI selectors remain explicit. The failed watch run is retained, and all six watch tests pass after the correction. No production UI behavior or deadline was changed.

B7 accepted final gate: the corrected all-feature workspace command completed in 1,876.87 seconds with whole-wrapper exit 0. Its 91 summaries report 2,046 returned passes and no failures or ignored cases. Exactly four returned passes are disabled-live bodies, with native Pi execution separately recorded above; doctor has no readiness skip. Root independently counted the raw log and inspected every target summary. Production protocol tests pass 28, ownership 58, persistence 30, redraw 3 and stamp 7. The stamp stress uses two writer threads and 800 stamp invocations, with full-load and settled-reader assertions. Final lint and diff checks pass; root's native Rust 1.88 workspace/all-target/all-feature locked check passes in 3.06 seconds. No owned fixture processes remain, and the successful run adds no scratch-guard entries. All earlier failures retain their original status.

B7 final resource observations: 2,004 ms of silence and 200 duplicate reports each caused zero publications; the duplicate workload took 3,146 ms. Their daemon tmux invocation counts were 7 and 26, including reads and maintenance. After 2,000 ms of ordinary output, blocked state arrived in 726 ms. The combined sample used 32 process snapshots and 107 daemon tmux invocations; shutdown took 499 ms. The queue sample held zero events and payload bytes, with two retained logical bytes. The separate 6,000-query workload caused five observed overflows and recovered after a fresh report. Logical limits remain 1,024 queued events, 1 MiB of collector payload and 32 MiB of combined retained/pending runtime payload. CPU/RSS readings are per-daemon samples; these measurements do not establish peak or process-tree memory, total allocation, or hard latency guarantees.

B8 fixing review: DR1 corrects stale Pi and daemon guidance; DR2 separates SessionEnd metadata cleanup from live program state and limits the supported-read contract to public options; DR3 explains Clear's subtree removal and re-selection; DR4 names OpenCode's existing question mapping; DR5 documents the 64 KiB rollup snapshot bound and advisory poll freshness. The independent reviewer changed seven Markdown files and added no runtime behavior. Root inspected every correction. The unchanged production and native fixture bytes retain B7's full-workspace and native-agent evidence; the final documentation consumers were rerun, without claiming another full-workspace or live-agent execution.

Final native package gate: root's Rust 1.88 workspace/all-target/all-feature locked check passed in 1.72 seconds. An isolated git-archive plus verified overlay included all new product files; all 460 files selected into the Nix source matched it byte-for-byte. `nix build --no-link --print-out-paths --option sandbox true` passed on aarch64-darwin, with 180 tma-tmux cases, zero failures or ignored cases, and installed Bash, Fish and Zsh completions plus the matching tma-hook body. The installed ARM64 binary reports tma 0.5.16. Package output: `/nix/store/m1mb1v57z6k4hsys0cc87maixwaz96jc-tma-0.5.16`. No global Nix setting or packaging dependency change was needed.

Completion audit: the orchestrator reconciled G1, D1 through D10, Z1 through Z4, A1 through A13, C1 through C6, O1 through O8, all batch receipts and M1 through M4 against the reviewed source and retained evidence. No unresolved in-scope correctness finding or running worker/native job remains; scoped cleanup found no owned fixture processes and required no signals. The scratch guard history remains 155 lines and 13,706 bytes. Historical failed runs and execution incidents above remain part of the record. No commit, push, release, hook uninstallation or daemonless removal was performed. The supported starting point is the documented per-process Pi activation with the daemon already observing; Claude activation, native Clear consumption and native Linux/historical-tmux production execution retain their stated limits.
