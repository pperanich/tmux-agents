// tma pi bridge — installed by `tma install-hooks pi` into `~/.pi/agent/extensions/`.
// pi auto-discovers extension modules there and calls the exported factory with its
// ExtensionAPI; this module forwards the state-bearing lifecycle events to tma's stable
// `tma-hook` wrapper, which resolves the `tma` binary at fire time.
//
// Event set VERIFIED live against pi 0.82.1 (2026-07-26) by driving trivial, tool-using,
// and trust-prompt turns under an isolated PI_CODING_AGENT_DIR with a logging extension; the
// five events below fired in both print (`-p`) and interactive (tui) modes. The full
// ExtensionAPI event set (~30 events) is enumerated in dist/core/extensions/types.d.ts; only
// these five carry a state/lifecycle transition tma needs:
//   session_start        -> lifecycle start (register the pane, rescuing its identity)
//   before_agent_start   -> working (turn start; user prompt accepted)
//   tool_execution_start -> working (a tool is running mid-turn)
//   agent_settled        -> idle (turn fully settled; fires once per turn, after agent_end)
//   session_shutdown     -> lifecycle end (deregister; reason "quit")
//
// Context telemetry: on the turn-settled `agent_settled` event the extension also forwards
// pi's own `ctx.getContextUsage()` to `tma event --kind context`. pi's ContextUsage carries a
// precomputed `percent` + absolute `contextWindow` (verified against earendil-works/pi, 2026-07-29),
// so tma needs no model/window table; the `pi-context-json` parser reads the percent.
//
// The `@@TMA_HOOK@@` path is substituted with the resolved wrapper at install time
// (diff-before-write, so re-install is byte-identical).
//
// Fire-and-forget (the wrapper's discipline): the child is spawned detached with the payload on
// its stdin and unref'd, so it can never block or crash pi. Every failure path is swallowed.
// Inert outside tmux: pi's own env carries $TMUX_PANE only when launched inside a pane, and
// `tma event` resolves the pane from it — with no $TMUX_PANE the extension registers nothing.

import { spawn } from "node:child_process";

const TMA_HOOK = "@@TMA_HOOK@@";

// pi's event payloads carry NO session id, and PI_SESSION_ID is injected only into bash-tool
// CHILD processes, not pi itself — so read it from the session manager and emit it under the
// snake_case `session_id` key tma's session guard (parse_session_id) reads.
function sessionId(ctx) {
  try {
    return ctx && ctx.sessionManager ? (ctx.sessionManager.getSessionId() ?? "") : "";
  } catch {
    return "";
  }
}

// Spawn `tma-hook pi <event>` fire-and-forget with `body` (JSON) on stdin: detached + unref'd so it
// can never block or crash pi, every failure path swallowed.
function spawnHook(event, body) {
  try {
    const child = spawn(TMA_HOOK, ["pi", event], {
      stdio: ["pipe", "ignore", "ignore"],
      detached: true,
    });
    child.on("error", () => {});
    child.stdin.on("error", () => {});
    child.stdin.end(JSON.stringify(body));
    child.unref();
  } catch {
    // pi keeps running even if the wrapper is missing.
  }
}

// State/lifecycle events carry only the session id; the manifest maps the event → state.
function fire(event, ctx) {
  spawnHook(event, { session_id: sessionId(ctx) });
}

// pi's current context usage for the active model, or null when unavailable (no model/window, or the
// method is missing on an older pi). `null` right after a `/compact` until a fresh assistant response.
function contextUsage(ctx) {
  try {
    return ctx && typeof ctx.getContextUsage === "function"
      ? (ctx.getContextUsage() ?? null)
      : null;
  } catch {
    return null;
  }
}

// Context telemetry: forward `{ session_id, context_usage }` to the `context` intake. The pane
// is resolved from the inherited $TMUX_PANE (same as every other pi forward). A null/absent usage
// leaves the stored gauge untouched (the parser stamps nothing) rather than clearing it.
function fireContext(ctx) {
  spawnHook("context", { session_id: sessionId(ctx), context_usage: contextUsage(ctx) });
}

export default function (pi) {
  // Inert outside tmux: without a pane there is nothing for `tma event` to bind to.
  if (!process.env.TMUX_PANE) return;

  pi.on("session_start", (_event, ctx) => fire("session_start", ctx));
  pi.on("before_agent_start", (_event, ctx) => fire("before_agent_start", ctx));
  pi.on("tool_execution_start", (_event, ctx) => fire("tool_execution_start", ctx));
  pi.on("agent_settled", (_event, ctx) => {
    fire("agent_settled", ctx);
    fireContext(ctx);
  });
  pi.on("session_shutdown", (_event, ctx) => fire("session_shutdown", ctx));
}
