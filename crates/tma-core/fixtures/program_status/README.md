# OSC 7501 protocol fixtures

`pi-1.3.0.frames` contains the eight native Pi report bodies observed on 2026-10-26
with Pi 1.3.0 and tmux 3.6a on macOS, using per-process `PI_PROGRAM_STATUS=1`.
Each line was originally framed by `ESC ] 7501 ;` and `ESC \`. Tests restore
those exact framing bytes. The bodies contain no user prompts, credentials, or
filesystem paths. No model output or full terminal transcript is retained.

Provenance: `/tmp/tma-osc7501.2plMcb/results.json`, `forced/events.json`,
`forced/control.log`, and `prototype.py`. The local OpenAI-compatible HTTP model
returned deterministic test data, including a deliberate failure. The isolated
Pi extension only opened Pi's real confirmation UI. It did not emit OSC reports.
The blocked and error messages are native Pi's encoding of this test UI/model
data, not reports manufactured by the model or extension.

`pi-1.3.0.split.control` preserves only the two adjacent control notifications
carrying the initial native frame. It intentionally includes the original split
after `state=idle:`. Unrelated terminal output has been removed after the ST.
The pane ID is normalized to `%0` (also its original value). This preserves
prototype transport input; production acceptance is separate.

The baseline and attempted asynchronous handshake runs emitted only a query.
These fixtures establish native transport and the listed transitions, not
production collector coverage or complete agent lifecycle coverage.

Production follow-up on 2026-10-27 used Pi 1.3.0, tmux 3.7b and Darwin 25.6
arm64. The package test
`native_pi_osc7501_through_production_daemon_without_state_hooks` drives the real
daemon with native reports, isolated home/config/project, a fake-key loopback
model and no tma state hooks. Its extension only opens Pi's real confirmation
UI. The reviewed execution passed 1/1 in 23.94 seconds: working, three successful
completions, two blocked dialogs, spent-result restore, HTTP failure and
cancellation. Cancellation was fresh idle with no result or effect while the
model response stayed gated. Shell-surviving exit revoked authority, but did not
prove consumption of native Clear. These later tests do not change the eight
historical fixture bodies or their tmux version. The current coverage/limitations
ledger is in `docs/reference/agent-coverage.md`.

Normative parser reference: [OSC 7501 revision 0.4](https://www.superlogical.com/rex/docs/build/program-status).
Synthetic adversarial frames live in the package tests and are not native captures.
