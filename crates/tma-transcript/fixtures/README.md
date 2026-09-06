# Transcript store fixtures

Synthetic files that imitate what each agent writes. **Nothing here came off a real session.** The
record shapes, field names and nesting are copied from measured sessions; every string leaf is
`x`-filled or `synthetic-*`, every id is a made-up constant, and no path points at anything real.
That split is deliberate: the structure is the asset a reader has to keep working against, and the
text is a liability nobody needs committed.

## Layout

```
stores/VERSIONS.toml                 the pinned versions, and where each store stamps its own
stores/<agent>/<version>/<name>.jsonl        the fixture, named for the version it imitates
stores/<agent>/<version>/<name>/subagents/   nested-agent transcripts, served as their own sessions
stores/opencode/<version>/                   expectation only: the database is built by the suite
drift/                                       deliberately unknown record types (see below)
```

opencode is the one store with no fixture file. It keeps every session in one SQLite database, so
`src/tests/opencode.rs` builds a synthetic one (the schema is a `const` there, copied from the
measured columns) and only the expectation is committed. The version still has to agree in three
places: the directory name, the `version` the builder writes into the session row, and the
declaration in `VERSIONS.toml`.

A fixture's filename carries the version, the directory above it repeats it, and the version stamp
inside the file has to agree with both. `src/tests/corpus.rs` asserts all three, plus that the
emitted event sequence still equals the committed `<fixture>.expected.txt`. A store that starts
routing a tool result through a new role fails there rather than rendering the tool's output as the
assistant's prose.

## `drift/`

`claude-99.0.0-drift.jsonl` carries two invented record types at a version no adapter has seen. It
exists to pin the other half of the rule: an unseen **version** must parse happily and count what it
does not recognize, while an unseen **record type** must raise the unknown counter. The corpus under
`stores/` asserts `unknown == 0`; `drift/` asserts it is not zero. A reader that errored on either
would break every time an agent shipped a release.

## Refreshing one

Drive the agent, take the record shapes, replace every string leaf with a synthetic one of the same
shape, and rename the file for the version the session was written by. Then run the tests with
`TMA_TRANSCRIPT_BLESS=1` to rewrite the expectation files, and read the diff: that diff is the
drift, and it is the only reason these files are committed.
