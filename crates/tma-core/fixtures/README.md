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
