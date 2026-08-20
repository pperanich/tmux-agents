// tma OpenCode bridge — installed by `tma install-hooks opencode` into
// `~/.config/opencode/plugin/`. OpenCode loads plugins from that dir and calls the
// exported hooks; this module forwards the state-bearing events to tma's stable
// `tma-hook` wrapper, which resolves the `tma` binary at fire time.
//
// Adapted (MIT) from tmux-agent-sidebar's plugin bridge
// (github.com/.../tmux-agent-sidebar/.opencode/plugins/tmux-agent-sidebar.js),
// trimmed to the four state tokens tma consumes. The event set is verified live
// against OpenCode 1.17.15 (session.created / session.status{busy|idle} /
// session.idle / permission.asked, plus the chat.message + tool.execute.before
// hooks). The `@@TMA_HOOK@@` path is substituted with the resolved wrapper at
// install time (diff-before-write, so re-install is byte-identical).
//
// API-channel actions: the plugin also forwards the pending permission
// `request_id` on permission.asked, the serving base URL (`PluginInput.serverUrl`)
// as `api_endpoint` at session-start, and a `permission-replied` clear on
// permission.replied. Property shapes verified against the @opencode-ai/sdk v2
// types shipped with 1.18.0 (EventPermissionAsked.properties.{id,requestID},
// PluginInput.serverUrl, POST /permission/{requestID}/reply {reply}).
//
// Fire-and-forget: OpenCode does not await the hook's returned promise, so the
// subprocess is spawned detached with the payload written to its stdin and unref'd
// (matching pi-extension.js, so it can never keep OpenCode's event loop alive or
// block it). Every failure path is swallowed — the bridge must never break OpenCode.

import { spawn } from "node:child_process";

const TMA_HOOK = "@@TMA_HOOK@@";

const fire = (event, payload) => {
  try {
    const child = spawn(TMA_HOOK, ["opencode", event], {
      stdio: ["pipe", "ignore", "ignore"],
      detached: true,
    });
    child.on("error", () => {});
    child.stdin.on("error", () => {});
    child.stdin.end(JSON.stringify(payload));
    child.unref();
  } catch {
    // OpenCode keeps running even if the wrapper is missing.
  }
};

// tma's session guard reads `session_id` (snake_case) from the payload; emit that key.
const sessionId = (value) =>
  value && typeof value.sessionID === "string" ? value.sessionID : "";

// The pending permission id the broker replies to. The `permission.asked` edge carries it as
// `id` (v2 SDK) or `requestID` (earlier captures); accept either so a version skew is inert.
const requestId = (value) =>
  value && typeof value.requestID === "string"
    ? value.requestID
    : value && typeof value.id === "string"
      ? value.id
      : "";

export const TmaBridge = async (input) => {
  // `PluginInput.serverUrl` is the base URL this OpenCode instance serves on. The server
  // pins its own port, so this is the only reliable source; stamped at registration as
  // `@agent_api_endpoint`, trailing slash trimmed. Absent (older API) ⇒ the broker's config fallback.
  const apiEndpoint =
    input && input.serverUrl ? String(input.serverUrl).replace(/\/+$/, "") : "";

  // Register at plugin load, before any session event. `session.created` fires only for a
  // BRAND-NEW session, so a TUI sitting at the prompt and `opencode --continue` (a restored
  // session) both emitted nothing at all — and OpenCode's `[capture] visible` is `blocked` only,
  // so with no hook claim the fold floor left those panes at `unknown` ("?" in the status bar)
  // until the first message. Both reproduced live on 1.18.18. Registration alone stamps idle
  // (event::decide maps Register ⇒ idle), which is the honest state for a waiting prompt.
  // Session-less: the id is unknown here, and the `session.created` / `session.status` edges that
  // follow carry the real one. Plugin load happens once per process, so this cannot loop.
  fire("session-start", { session_id: "", api_endpoint: apiEndpoint });

  return {
    event: async ({ event }) => {
      if (!event || !event.type) return;
      const props = event.properties ?? {};
      const session_id = sessionId(props);
      switch (event.type) {
        case "session.created":
          fire("session-start", { session_id, api_endpoint: apiEndpoint });
          return;
        case "session.status": {
          const type = props.status?.type;
          if (type === "busy") fire("user-prompt-submit", { session_id });
          else if (type === "idle") fire("stop", { session_id });
          return;
        }
        case "session.idle":
          fire("stop", { session_id });
          return;
        // `permission.asked` is what the 1.18.18 binary emits (verified: the shipped
        // `@opencode-ai/sdk` typings name a `permission.updated` the runtime has no string for).
        // Accept both so a rename lands inert rather than silently dropping `blocked`.
        case "permission.asked":
        case "permission.updated":
          fire("permission-required", {
            session_id,
            permission: typeof props.permission === "string" ? props.permission : "",
            request_id: requestId(props),
          });
          return;
        case "permission.replied":
          // The prompt was answered (by tma or the TUI): clear the stamped request id.
          fire("permission-replied", { session_id });
          return;
      }
    },

    "chat.message": async (input) => {
      fire("user-prompt-submit", { session_id: sessionId(input) });
    },

    "tool.execute.before": async (input) => {
      fire("user-prompt-submit", { session_id: sessionId(input) });
    },
  };
};
