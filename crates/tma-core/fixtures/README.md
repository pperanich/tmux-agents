# Fixtures

Captured-screen test data in the fixture format (see `src/fixture.rs`):

```
# agent: <name>
# state: idle|working|blocked|unknown
# title: <pane title>
# command: <pane_current_command>
# pid: <u32>
# captured_at: <epoch seconds>
---
<raw capture text, byte-for-byte>
```

Real per-agent rule fixtures land here alongside their manifests (Claude first,
then Codex/Gemini/Cursor). Every one is evidence-first: captured from a real
agent via `tma debug capture`, run through `tma debug redact`, and never
hand-edited. `_harness_smoke.txt` is synthetic loader-self-test data, not a rule
fixture — do not derive detection rules from it.

## Synthesized headers

Four fixtures came from the mobile-planning capture corpus
(`docs/internal/mobile/research/captures/claude/`), which recorded **capture
bodies only** — no pane title, pid or timestamp:

- `claude_blocked_plan_w{60,200}.txt`
- `claude_blocked_trust_w200.txt`
- `claude_blocked_permission_bash_w60.txt`

Their bodies are the real captures verbatim (already ANSI-stripped at capture
time; `claude_blocked_permission_bash_w60.txt` additionally has the account
holder's name redacted width-preservingly out of the welcome banner). Their
**headers are synthesized**, so no test may assert on the title, pid or
`captured_at` of these four. `plan_verdict_survives_an_idle_marker_in_the_title`
authors a `✳` title onto the plan body deliberately and says so.
