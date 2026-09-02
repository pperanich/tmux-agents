// tma OpenCode bridge, installed by `tma install-hooks opencode` into
// `~/.config/opencode/plugin/`. OpenCode auto-loads plugin modules from that directory and calls
// each exported factory with its plugin input; this module forwards the state-bearing events to
// tma's stable `tma-hook` wrapper, which resolves the `tma` binary at fire time.
//
// Sources: the plugin API (factory export, the `event` bus hook, `tool.execute.before`) is
// https://opencode.ai/docs/plugins/; the event property names are OpenCode's published OpenAPI
// schema (`EventPermissionAsked.properties.{id,sessionID}`,
// `EventPermissionReplied.properties.requestID`, `SessionStatus` = `{type: idle|busy|retry}`).
//
// API channel: the pending permission id rides `permission-required` as `request_id` and the
// serving base URL rides `session-start` as `api_endpoint`, so `tma act approve`/`deny` can POST
// `/permission/{requestID}/reply` instead of sending a keystroke. `permission.replied` forwards a
// clear so a spent id never reads as a pending request.
//
// The `@@TMA_HOOK@@` path is substituted with the resolved wrapper at install time
// (diff-before-write, so re-install is byte-identical).
//
// Fire-and-forget: the child is spawned detached with the payload on its stdin and unref'd, so it
// can neither block OpenCode's event loop nor keep it alive. Every failure path is swallowed,
// including a missing wrapper (mid-rebuild, uninstalled): the bridge must never break OpenCode.

import { spawn } from "node:child_process";

const TMA_HOOK = "@@TMA_HOOK@@";

// The last session id seen on the event bus. The hooks that carry no session of their own fire
// under it, and the load-time registration (before any event) fires with no session id at all.
let sessionId = "";

// The serving base URL from the plugin input, `undefined` when it names none: an empty string
// would stamp `@agent_api_endpoint` blank rather than leave it alone.
let apiEndpoint;

// Spawn `tma-hook opencode <event>` with `body` (JSON) on stdin, detached and unref'd.
function spawnHook(event, body) {
  try {
    const child = spawn(TMA_HOOK, ["opencode", event], {
      stdio: ["pipe", "ignore", "ignore"],
      detached: true,
    });
    child.on("error", () => {});
    child.stdin.on("error", () => {});
    child.stdin.end(JSON.stringify(body));
    child.unref();
  } catch {
    // OpenCode keeps running even if the wrapper is gone.
  }
}

// One wrapper token, carrying the session id when one is known plus any API-channel fields for
// this edge. `undefined` values drop out of the envelope, which is what keeps an absent field absent.
function fire(event, extra) {
  spawnHook(event, sessionId ? { session_id: sessionId, ...extra } : { ...extra });
}

// The event bus: OpenCode's own event names normalized to the wrapper tokens tma's manifest maps.
function onEvent(event) {
  const props = event?.properties ?? {};
  if (typeof props.sessionID === "string" && props.sessionID) sessionId = props.sessionID;
  switch (event?.type) {
    case "session.created":
      fire("session-start", { api_endpoint: apiEndpoint });
      break;
    case "session.idle":
      fire("stop");
      break;
    case "session.status":
      // `retry` is neither edge: a turn being retried has not started or finished.
      if (props.status?.type === "busy") fire("user-prompt-submit");
      else if (props.status?.type === "idle") fire("stop");
      break;
    // `permission.updated` is accepted as a synonym: the SDK typings and the shipped binary have
    // disagreed on the name, so a rename lands inert instead of silently dropping `blocked`.
    case "permission.asked":
    case "permission.updated":
      fire("permission-required", {
        permission: props.permission,
        request_id: props.id || props.requestID,
      });
      break;
    case "permission.replied":
      fire("permission-replied");
      break;
  }
}

export const TmaBridge = async (input) => {
  // Inert outside tmux: `tma event` binds to the pane named by $TMUX_PANE, and there is none.
  if (!process.env.TMUX_PANE) return {};

  if (typeof input?.serverUrl === "string" && input.serverUrl) apiEndpoint = input.serverUrl;
  // Register at load, not just on `session.created`: OpenCode emits that event for a brand-new
  // session only, so a TUI waiting at its prompt and `opencode --continue` would announce nothing.
  fire("session-start", { api_endpoint: apiEndpoint });

  return {
    event: async ({ event }) => onEvent(event),
    // A turn start the bus does not always announce. Three sources fire this token over one turn;
    // the intake re-stamps the same state each time, so the extra fires are inert.
    "chat.message": async () => fire("user-prompt-submit"),
    "tool.execute.before": async (input) => {
      if (typeof input?.sessionID === "string" && input.sessionID) sessionId = input.sessionID;
      fire("user-prompt-submit");
    },
  };
};
