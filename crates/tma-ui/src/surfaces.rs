//! One-shot read surfaces: `tma ls` (line + JSON), `tma status` (the ambient status-line one-liner
//! and its `--format` siblings), and the `tma subscribe --events` edge record. Every one renders a
//! [`CycleReport`]'s rows and nothing else — no clock, no tmux — so the wall-clock values the prom
//! and edge forms need are injected. `tma status`'s glyphs and colors come from the `[status]`
//! config section, which defaults every leaf to the documented value (`⚑` red / `●` yellow /
//! `○` green / `?` colour244), so zero-config is unchanged.

use tma_core::{AgentRow, AgentState, Edge, StateToken};
use tma_runtime::config::StatusStyles;
use tma_runtime::cycle::CycleReport;
use tma_runtime::json::{JsonWriter, JSON_SCHEMA};
use tma_runtime::origin::Origin;

/// One agent row as a `tma ls` tab-separated line (trailing newline), shared with `tma wait` so the
/// two never drift in column order: `pane<TAB>agent<TAB>state<TAB>detail<TAB>since<TAB>session:window.pane<TAB>title<TAB>attention<TAB>muted<TAB>repo<TAB>branch<TAB>worktree`.
/// The `attention` column is `1` for `@agent_attention` else empty (idle + `1` = "done"); `muted` is
/// the same marker for a live `@agent_mute_until`; state is the raw token.
///
/// The three repo columns are appended last, so a pipeline reading `$1`-`$9` is unaffected. All
/// three are empty for a pane in no git checkout, and `worktree` is the same `1`-or-empty marker,
/// set for a linked worktree. They require an [`annotate_rows`](tma_runtime::repo::annotate_rows)
/// caller: an unannotated row reads as a non-git pane rather than failing loudly, which is why
/// both call sites annotate first.
pub fn render_ls_row(r: &AgentRow) -> String {
    let marker = |on: bool| if on { "1" } else { "" };
    let repo = r.repo.as_ref();
    format!(
        "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
        r.pane_id,
        r.agent,
        r.state,
        r.detail.as_deref().unwrap_or(""),
        r.since,
        r.locator(),
        r.title,
        marker(r.attention),
        marker(r.muted),
        repo.map_or("", |l| l.name.as_str()),
        repo.map_or("", |l| l.branch.as_str()),
        marker(repo.is_some_and(|l| l.worktree)),
    )
}

/// `tma ls`: one [`render_ls_row`] line per agent pane, line-oriented so users can compose their
/// own fzf/awk pipelines.
pub fn render_ls_text(report: &CycleReport) -> String {
    let mut out = String::new();
    for r in &report.rows {
        out.push_str(&render_ls_row(r));
    }
    out
}

/// `tma ls --json`: a versioned, additive-only document (`"schema": 1`). `attention` is additive so
/// the schema stays `1`. `since`/`since_ms` carry the same epoch-ms transition value; prefer `since_ms`.
/// `episode_ms` is the distinct quantity `wait --since` compares against, and the one a supervisor
/// loop feeds back.
pub fn render_ls_json(report: &CycleReport, origin: &Origin) -> String {
    render_rows_document(&report.rows, origin)
}

/// `tma wait --all/--count --json`: the satisfied set as the same schema-1 `agents` document `ls
/// --json` emits, so a consumer parses one shape whether it listed or waited. A distinct
/// serialization site with its own drift test (the single-row `wait --json` object stays the
/// single-pane targets' emission).
pub fn render_wait_json_rows(rows: &[AgentRow], origin: &Origin) -> String {
    render_rows_document(rows, origin)
}

/// The shared `{"schema":1,"agents":[…]}` document behind `ls --json` and the fleet `wait --json`.
fn render_rows_document(rows: &[AgentRow], origin: &Origin) -> String {
    let mut j = JsonWriter::new();
    j.begin_object();
    j.number("schema", JSON_SCHEMA);
    j.key("agents");
    j.begin_array();
    for r in rows {
        j.begin_object();
        write_row_fields(&mut j, r, origin);
        j.end_object();
    }
    j.end_array();
    j.end_object();
    j.finish()
}

/// Write one agent row's fields (no enclosing object) into `j`, the key set defined once here so
/// `ls --json` and `wait --json` cannot disagree on keys/order/null handling (drift tests pin both).
///
/// PRECONDITION: the caller has run `tma_runtime::repo::annotate_rows` on the row. The `repo` label is
/// `None` both when unresolved AND when never annotated — the type cannot tell those apart, so a
/// serializer that skips annotation emits well-formed nulls the drift tests will not catch. Every
/// current caller (ls, wait's matched-row emit, the subscribe render closure) annotates first; a new
/// serializing surface must too.
fn write_row_fields(j: &mut JsonWriter, r: &AgentRow, origin: &Origin) {
    j.string("pane", &r.pane_id);
    j.string("agent", &r.agent);
    j.string("state", r.state.token());
    match &r.detail {
        Some(d) => j.string("detail", d),
        None => j.null("detail"),
    }
    j.number("since", r.since as i64);
    j.number("since_ms", r.since as i64);
    // Additive (schema stays 1): the instant `wait --since` actually compares against
    // ([`AgentRow::episode_at`] — the later of the transition and the last turn end). It is a
    // SEPARATE key because `since_ms` is pinned to `@agent_since` by the uptime column, and a
    // supervisor loop that feeds `since_ms` back as its next floor can never reach the compared
    // quantity once a second completion has moved `@agent_turn_at` past it — the loop then
    // re-satisfies on every lap. This is the key to feed back.
    j.number("episode_ms", r.episode_at() as i64);
    j.string("locator", &r.locator());
    j.string("title", &r.title);
    j.bool("attention", r.attention);
    // Additive (schema stays 1): the "done" surface (idle + attention) precomputed from the one core
    // definition, so consumers stop re-deriving it — and cannot re-derive it differently.
    j.bool("done", tma_core::is_done(r));
    // Additive (schema stays 1): the owning agent session id and the context-utilization metric
    // pair. All nullable — absent when the pane never registered a session or the agent
    // has no telemetry coverage.
    match &r.agent_session {
        Some(s) => j.string("session", s),
        None => j.null("session"),
    }
    match r.context_pct {
        Some(pct) => j.number("context", pct as i64),
        None => j.null("context"),
    }
    match r.context_at {
        Some(at) => j.number("context_at_ms", at as i64),
        None => j.null("context_at_ms"),
    }
    // Additive (schema stays 1): notifications are suppressed on this pane right now
    // (`@agent_mute_until` still ahead of the cycle's clock). Presentation and dispatch only —
    // the state, the counts, and every other key read exactly as they would unmuted.
    j.bool("muted", r.muted);
    // Additive (schema stays 1): the absolute the gauge is a percent of, `null` for an agent whose
    // channel reports none. `context_at_ms` ages it too — one observation stamps both.
    match r.tokens {
        Some(t) => j.number("tokens", t as i64),
        None => j.null("tokens"),
    }
    // Additive (schema stays 1): the account quota, nested because its three values are one fact,
    // a percent with no window token cannot be read. `null` when the pane's channel reports no
    // rate-limit block (API-key auth, or before the agent's first API response). It is
    // ACCOUNT-wide, not per-pane, so several rows carrying the same numbers is correct and summing
    // them means nothing. `resets_at_ms` is epoch ms, converted at the parser from the seconds both
    // vendors publish.
    match &r.quota {
        Some(q) => {
            j.key("quota");
            j.begin_object();
            j.number("pct", q.pct as i64);
            j.string("window", &q.window);
            match q.resets_at_ms {
                Some(at) => j.number("resets_at_ms", at as i64),
                None => j.null("resets_at_ms"),
            }
            j.end_object();
        }
        None => j.null("quota"),
    }
    // Additive (schema stays 1): the agent's own reported cost for THIS session, `null` when its
    // channel publishes none. tma reports which pane right now and aggregates nothing across
    // sessions, a spend total over time is `ccusage`'s job, not this row's.
    match r.cost_usd {
        Some(v) => j.money("cost_usd", v),
        None => j.null("cost_usd"),
    }
    // Additive (schema stays 1): the resolved repo/branch grouping keys. All three keys are
    // null together (the row's cwd never resolved to a repo); `worktree` is `false` for a resolved main
    // checkout, `true` for a linked worktree.
    match &r.repo {
        Some(label) => {
            j.string("repo", &label.name);
            j.string("branch", &label.branch);
            j.bool("worktree", label.worktree);
        }
        None => {
            j.null("repo");
            j.null("branch");
            j.null("worktree");
        }
    }
    // Additive (schema stays 1): the permission decision the pane is waiting on, from Claude's
    // `PermissionRequest` payload. All three keys are null together (nothing pending, or an agent
    // whose hooks carry none). `pending_summary` is AGENT-SUPPLIED text, a command line, a path,
    // so it is deliberately confined to this document and the pane options: it is in no
    // notification payload, no audit line, and no `TMA_*` env var.
    match &r.pending {
        Some(p) => {
            j.string("pending_tool", &p.tool);
            j.string("pending_call", &p.call);
            j.string("pending_summary", &p.summary);
        }
        None => {
            j.null("pending_tool");
            j.null("pending_call");
            j.null("pending_summary");
        }
    }
    // Additive (schema stays 1): where the row was observed. Resolved once per invocation and
    // repeated per row, because a merged multi-machine set has no other place to put it — two hosts'
    // rows collide on `%5` without them.
    j.string("server", &origin.server);
    j.string("host", &origin.host);
}

/// `tma wait --json`: the single matched row as a schema-1 object. Its keys are `write_row_fields`'s
/// plus a top-level `schema` (no `agents` wrapper), a distinct serialization site with its own drift test.
pub fn render_wait_json(r: &AgentRow, origin: &Origin) -> String {
    let mut j = JsonWriter::new();
    j.begin_object();
    j.number("schema", JSON_SCHEMA);
    write_row_fields(&mut j, r, origin);
    j.end_object();
    j.finish()
}

/// `tma subscribe --events`: one [`Edge`] as a schema-1 JSON object, one per line. Its own
/// serialization site (an edge is not a row: no `since`, no title, and the state pair replaces
/// `state`), with its own drift test. `at_ms` is when the stream OBSERVED the transition — the
/// diffing cycle's wall clock, injected by the caller — not necessarily when the agent changed.
///
/// `from`/`to` are the empty string at the open ends of a pane's life: `""` → `state` for a pane
/// that appeared, `state` → `""` for one that vanished. `""` is deliberately not `unknown`, which is
/// a state the pane can genuinely be observed in.
pub fn render_edge_json(edge: &Edge, at_ms: u64) -> String {
    let mut j = JsonWriter::new();
    j.begin_object();
    j.number("schema", JSON_SCHEMA);
    j.number("at_ms", at_ms as i64);
    j.string("pane", &edge.pane_id);
    j.string("agent", &edge.agent);
    j.string("from", edge.from.map(|s| s.token()).unwrap_or(""));
    j.string("to", edge.to.map(|s| s.token()).unwrap_or(""));
    match &edge.detail {
        Some(d) => j.string("detail", d),
        None => j.null("detail"),
    }
    j.string("locator", &edge.locator);
    match &edge.repo {
        Some(label) => {
            j.string("repo", &label.name);
            j.string("branch", &label.branch);
        }
        None => {
            j.null("repo");
            j.null("branch");
        }
    }
    j.end_object();
    j.finish()
}

/// The five `tma status` count classes. **The classes are disjoint**: an idle pane still carrying
/// `@agent_attention` counts as `done` and NOT as `idle`, so the five always sum to the pane total.
/// That is deliberately narrower than the JSON rows' `done` key, which is a subset of `state:"idle"`
/// (the row keeps its `idle` token); every `status` format reports the disjoint split.
struct StatusCounts {
    blocked: u32,
    working: u32,
    done: u32,
    idle: u32,
    unknown: u32,
}

impl StatusCounts {
    fn of(rows: &[AgentRow]) -> Self {
        let mut c = StatusCounts {
            blocked: 0,
            working: 0,
            done: 0,
            idle: 0,
            unknown: 0,
        };
        for r in rows {
            match r.state {
                AgentState::Blocked => c.blocked += 1,
                AgentState::Working => c.working += 1,
                AgentState::Idle if r.attention => c.done += 1,
                AgentState::Idle => c.idle += 1,
                AgentState::Unknown => c.unknown += 1,
            }
        }
        c
    }

    /// `(class, count, glyph, color)` per class in the fixed render order
    /// `blocked working done idle unknown`. The class token is the one the clickable-segment markup
    /// names (`tma:<class>`), carried here so the order and the names have a single source.
    fn styled<'a>(&self, styles: &'a StatusStyles) -> [(&'static str, u32, &'a str, &'a str); 5] {
        let (bg, bc) = styles.resolved(AgentState::Blocked);
        let (wg, wc) = styles.resolved(AgentState::Working);
        let (dg, dc) = styles.resolved_done();
        let (ig, ic) = styles.resolved(AgentState::Idle);
        let (ug, uc) = styles.resolved(AgentState::Unknown);
        [
            ("blocked", self.blocked, bg, bc),
            ("working", self.working, wg, wc),
            ("done", self.done, dg, dc),
            ("idle", self.idle, ig, ic),
            ("unknown", self.unknown, ug, uc),
        ]
    }
}

/// `tma status` (the default `--format tmux`): glyph + `#[fg=]` state counts, fixed order
/// `blocked working done idle unknown`, zero classes omitted, empty when none. Color from `[status]`
/// embeds verbatim; idle + `@agent_attention` = "done".
///
/// Each segment is wrapped in a `#[range=user|tma:<class>]…#[norange]` marker, which tmux honors on
/// the output of a `#()` status job: a root-table mouse binding then reads the clicked class from
/// `#{mouse_status_range}` (`tma install-keys --mouse`). Always emitted: without `mouse on` and
/// those bindings the markers are inert, and tmux draws exactly what it drew before.
pub fn render_status(report: &CycleReport, styles: &StatusStyles) -> String {
    let counts = StatusCounts::of(&report.rows);
    let parts: Vec<String> = counts
        .styled(styles)
        .iter()
        .filter(|(_, count, _, _)| *count > 0)
        .map(|(class, count, glyph, color)| {
            format!("#[range=user|tma:{class}]#[fg={color}]{glyph}{count}#[norange]")
        })
        .collect();
    parts.join(" ")
}

/// `tma status --format plain`: [`render_status`] with the tmux markup (`#[fg=]` and the clickable
/// range markers) dropped and nothing else changed: same fixed order, same configured glyphs, same
/// zero-class omission. The form an external bar (starship, sketchybar, waybar) wants, since those
/// apply their own coloring.
pub fn render_status_plain(report: &CycleReport, styles: &StatusStyles) -> String {
    let counts = StatusCounts::of(&report.rows);
    let parts: Vec<String> = counts
        .styled(styles)
        .iter()
        .filter(|(_, count, _, _)| *count > 0)
        .map(|(_, count, glyph, _)| format!("{glyph}{count}"))
        .collect();
    parts.join(" ")
}

/// `tma status --format json`: the same counts as a schema-1 document, so a bar that would otherwise
/// parse glyphs reads numbers. Every class is present even at zero (an omitted key would make a
/// consumer branch); see [`StatusCounts`] for why `done` and `idle` are disjoint here.
pub fn render_status_json(report: &CycleReport) -> String {
    let c = StatusCounts::of(&report.rows);
    let mut j = JsonWriter::new();
    j.begin_object();
    j.number("schema", JSON_SCHEMA);
    j.key("counts");
    j.begin_object();
    j.number("working", c.working as i64);
    j.number("blocked", c.blocked as i64);
    j.number("idle", c.idle as i64);
    j.number("unknown", c.unknown as i64);
    j.number("done", c.done as i64);
    j.end_object();
    j.end_object();
    j.finish()
}

/// `tma status --format prom`: the Prometheus text exposition format, for a node_exporter textfile
/// collector. Two gauge families — `tma_agents` (the [`StatusCounts`] classes) and
/// `tma_agent_state_seconds` (per pane, how long it has held its current state, from `since` against
/// `now_ms`). `now_ms` is injected rather than read here so the renderer stays a pure function.
pub fn render_status_prom(report: &CycleReport, now_ms: u64) -> String {
    let c = StatusCounts::of(&report.rows);
    let mut out = String::new();
    out.push_str(
        "# HELP tma_agents Agent panes in each state class. The classes are disjoint: an idle pane \
         with unreviewed output counts as done, not idle, so the five sum to the pane total.\n\
         # TYPE tma_agents gauge\n",
    );
    for (state, count) in [
        ("working", c.working),
        ("blocked", c.blocked),
        ("idle", c.idle),
        ("unknown", c.unknown),
        ("done", c.done),
    ] {
        out.push_str(&format!("tma_agents{{state=\"{state}\"}} {count}\n"));
    }
    out.push_str(
        "# HELP tma_agent_state_seconds Seconds the pane has held its current state (0 when the \
         transition timestamp is unknown or in the future). The state label uses the same disjoint \
         classes as tma_agents.\n\
         # TYPE tma_agent_state_seconds gauge\n",
    );
    for r in &report.rows {
        let held = now_ms.saturating_sub(r.since);
        // `since` is 0 on a pane whose transition was never stamped: report 0 rather than the
        // epoch-sized age that subtraction would give.
        let secs = if r.since == 0 {
            0.0
        } else {
            held as f64 / 1000.0
        };
        out.push_str(&format!(
            "tma_agent_state_seconds{{pane=\"{}\",agent=\"{}\",state=\"{}\"}} {secs:.3}\n",
            prom_label(&r.pane_id),
            prom_label(&r.agent),
            // The disjoint class, so summing the per-pane series by state reproduces `tma_agents`.
            StateToken::of(r).token(),
        ));
    }
    out
}

/// Escape a Prometheus label value (backslash, double quote, newline). Pane ids and agent names
/// carry none of these in practice; a user-named manifest could, and one stray quote would corrupt
/// the whole exposition.
fn prom_label(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for ch in v.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tma_core::{QuotaLabel, RepoLabel};

    fn row(pane: &str, agent: &str, state: AgentState, detail: Option<&str>) -> AgentRow {
        AgentRow {
            pane_id: pane.to_string(),
            agent: agent.to_string(),
            state,
            detail: detail.map(str::to_string),
            since: 100,
            turn_at: 0,
            session: "s".to_string(),
            window_index: 1,
            pane_index: 0,
            title: "a task".to_string(),
            attention: false,
            agent_session: None,
            context_pct: None,
            context_at: None,
            tokens: None,
            quota: None,
            cost_usd: None,
            muted: false,
            model: None,
            cwd: None,
            repo: None,
            pending: None,
        }
    }

    /// A row carrying the account quota and a cost, so the key-set pins reach the nested `quota`
    /// keys (an all-`null` row would leave them unpinned and a rename inside it would ship silently).
    fn quota_row(pane: &str) -> AgentRow {
        AgentRow {
            quota: Some(QuotaLabel {
                pct: 63,
                window: "spend".to_string(),
                resets_at_ms: Some(1_790_787_200_000),
            }),
            cost_usd: Some(3.4972),
            ..row(pane, "claude", AgentState::Blocked, Some("permission"))
        }
    }

    /// The repo labels are appended last so a consumer's `awk -F'\t'` indexes into `$1`-`$9` keep
    /// reading what they always did; the marker follows the attention/muted convention.
    #[test]
    fn ls_row_repo_columns_carry_the_git_labels() {
        let mut labelled = row("%2", "claude", AgentState::Idle, None);
        labelled.repo = Some(RepoLabel {
            name: "myrepo".to_string(),
            branch: "fix/timeout".to_string(),
            worktree: true,
        });
        let labelled = render_ls_row(&labelled);
        let cols: Vec<&str> = labelled.trim_end_matches('\n').split('\t').collect();
        assert_eq!(&cols[9..], &["myrepo", "fix/timeout", "1"]);
        // The nine original columns are untouched by the addition, so `$1`-`$9` pipelines still read
        // what they always did.
        assert_eq!(cols[0], "%2");
        assert_eq!(cols[8], "", "muted stays the ninth column");
    }

    /// An idle pane with `@agent_attention` set: the "done" surface.
    fn done_row(pane: &str) -> AgentRow {
        AgentRow {
            attention: true,
            ..row(pane, "claude", AgentState::Idle, None)
        }
    }

    /// A fixed [`Origin`] for the row documents; the real one reads tmux and `uname`.
    fn origin() -> Origin {
        Origin {
            server: "/tmp/tmux-501/default".to_string(),
            host: "box".to_string(),
        }
    }

    /// `render_ls_json` / `render_wait_json` against that origin (the tests are about the rows).
    fn render_ls_json_t(report: &CycleReport) -> String {
        render_ls_json(report, &origin())
    }

    fn render_wait_json_t(r: &AgentRow) -> String {
        render_wait_json(r, &origin())
    }

    fn report(rows: Vec<AgentRow>) -> CycleReport {
        CycleReport {
            rows,
            ..Default::default()
        }
    }

    #[test]
    fn ls_text_is_tab_separated_with_twelve_columns() {
        let r = report(vec![row(
            "%1",
            "claude",
            AgentState::Blocked,
            Some("permission"),
        )]);
        let text = render_ls_text(&r);
        // Strip only the newline: `trim_end` would eat the trailing empty attention column
        // (an unset flag is a blank field, as with detail), but a `-F'\t'` split keeps it.
        let cols: Vec<&str> = text.strip_suffix('\n').unwrap().split('\t').collect();
        assert_eq!(cols.len(), 12);
        assert_eq!(cols[0], "%1");
        assert_eq!(cols[1], "claude");
        assert_eq!(cols[2], "blocked");
        assert_eq!(cols[3], "permission");
        assert_eq!(cols[7], "", "attention column is empty when unset");
        assert_eq!(cols[8], "", "muted column is empty when unset");
        assert_eq!(
            &cols[9..],
            &["", "", ""],
            "a pane in no git checkout leaves the three repo columns empty"
        );
    }

    /// The mute marker follows the attention column's convention exactly: `1` when live, an empty
    /// field otherwise, so an awk pipeline reads both the same way.
    #[test]
    fn ls_text_muted_column_marks_a_suppressed_pane() {
        let mut r = row("%1", "claude", AgentState::Blocked, None);
        r.muted = true;
        let text = render_ls_text(&report(vec![r]));
        let cols: Vec<&str> = text.strip_suffix('\n').unwrap().split('\t').collect();
        assert_eq!(cols[8], "1");
        assert_eq!(
            cols[2], "blocked",
            "muting changes no state: only the marker column moves"
        );
    }

    #[test]
    fn ls_text_empty_detail_is_blank_column() {
        let r = report(vec![row("%1", "claude", AgentState::Idle, None)]);
        let text = render_ls_text(&r);
        let cols: Vec<&str> = text.trim_end().split('\t').collect();
        assert_eq!(cols[3], "", "absent detail is an empty column, not a token");
    }

    #[test]
    fn ls_text_done_row_sets_attention_column_but_keeps_idle_token() {
        let text = render_ls_text(&report(vec![done_row("%1")]));
        let cols: Vec<&str> = text.trim_end().split('\t').collect();
        assert_eq!(
            cols[2], "idle",
            "the state token stays idle (presentation only)"
        );
        assert_eq!(cols[7], "1", "idle + attention flags the done surface");
    }

    #[test]
    fn ls_json_carries_schema_and_null_detail() {
        let r = report(vec![row("%1", "claude", AgentState::Idle, None)]);
        let json = render_ls_json(&r, &origin());
        assert!(json.starts_with("{\"schema\":1"));
        assert!(json.contains("\"detail\":null"));
        assert!(json.contains("\"state\":\"idle\""));
        assert!(json.contains("\"attention\":false"));
    }

    #[test]
    fn ls_json_done_row_keeps_schema_1_and_exposes_attention() {
        let json = render_ls_json_t(&report(vec![done_row("%1")]));
        // Additive-only: the field is added, the schema is not bumped.
        assert!(json.starts_with("{\"schema\":1"));
        assert!(json.contains("\"state\":\"idle\""), "state token unchanged");
        assert!(
            json.contains("\"attention\":true"),
            "done exposed via the flag"
        );
        assert!(
            json.contains("\"done\":true"),
            "and precomputed on the done key"
        );
    }

    /// The `done` key is the core predicate, not a synonym for `attention`: a blocked pane carries
    /// the flag while a plain idle pane carries neither.
    #[test]
    fn done_key_tracks_idle_plus_attention_only() {
        let idle = render_ls_json_t(&report(vec![row("%1", "claude", AgentState::Idle, None)]));
        assert!(idle.contains("\"done\":false"), "plain idle is not done");

        let mut blocked = row("%2", "claude", AgentState::Blocked, None);
        blocked.attention = true;
        let json = render_ls_json_t(&report(vec![blocked]));
        assert!(
            json.contains("\"attention\":true") && json.contains("\"done\":false"),
            "an attention-flagged blocked pane is not done: {json}"
        );
    }

    /// `episode_ms` is the quantity `wait --since` compares against, and it is NOT `since_ms`: on a
    /// pane whose second completion moved `@agent_turn_at` past its write-once `@agent_since`, the
    /// two differ, and a supervisor loop feeding `since_ms` back would set a floor it can never
    /// clear. Both surfaces emit it, because both are things a loop reads a row from.
    #[test]
    fn episode_ms_is_the_wait_floor_and_diverges_from_since_ms() {
        let mut r = row("%1", "claude", AgentState::Idle, None);
        r.attention = true;
        r.since = 500; // the idle run began here and write-once pins it
        r.turn_at = 900; // a second completion landed without the pane leaving idle
        for json in [
            render_wait_json_t(&r),
            render_ls_json_t(&report(vec![r.clone()])),
        ] {
            assert!(
                json.contains("\"since_ms\":500"),
                "since_ms stays @agent_since: {json}"
            );
            assert!(
                json.contains("\"episode_ms\":900"),
                "episode_ms is the later of the transition and the turn end: {json}"
            );
        }
        // With no turn end recorded the two agree, so a pane that never had one reads identically.
        r.turn_at = 0;
        let json = render_wait_json_t(&r);
        assert!(json.contains("\"since_ms\":500") && json.contains("\"episode_ms\":500"));
    }

    /// The quota is one nested object, not three parallel scalars, and the cost renders with the
    /// same two decimals the `@agent_cost_usd` option carries. Both surfaces emit them, and both
    /// render an absent reading as `null`, never as a zero, which would read as "no quota used".
    #[test]
    fn the_quota_object_and_cost_render_together_or_as_null() {
        for json in [
            render_wait_json_t(&quota_row("%1")),
            render_ls_json_t(&report(vec![quota_row("%1")])),
        ] {
            assert!(
                json.contains(
                    r#""quota":{"pct":63,"window":"spend","resets_at_ms":1790787200000}"#
                ),
                "the three values ride one object: {json}"
            );
            assert!(
                json.contains(r#""cost_usd":3.50"#),
                "3.4972 renders as the money the option stores: {json}"
            );
        }
        // A channel with no rate-limit block and no cost: two explicit nulls, no zeros.
        let bare = render_wait_json_t(&row("%1", "codex", AgentState::Idle, None));
        assert!(bare.contains(r#""quota":null"#) && bare.contains(r#""cost_usd":null"#));

        // A window whose reset instant the channel did not state keeps the percent.
        let mut r = quota_row("%1");
        r.quota.as_mut().unwrap().resets_at_ms = None;
        assert!(render_wait_json_t(&r)
            .contains(r#""quota":{"pct":63,"window":"spend","resets_at_ms":null}"#));
    }

    /// The complete `ls --json` key inventory (additive-only): a dropped, renamed, or new key fails
    /// here. `since`/`since_ms` share a value; `since` is the compat key, `since_ms` names the unit.
    #[test]
    fn ls_json_pins_full_key_set() {
        let json = render_ls_json_t(&report(vec![quota_row("%1")]));
        assert_eq!(
            json_keys(&json),
            [
                "agent",
                "agents",
                "attention",
                "branch",
                "context",
                "context_at_ms",
                "cost_usd",
                "detail",
                "done",
                "episode_ms",
                "host",
                "locator",
                "muted",
                "pane",
                "pct",
                "pending_call",
                "pending_summary",
                "pending_tool",
                "quota",
                "repo",
                "resets_at_ms",
                "schema",
                "server",
                "session",
                "since",
                "since_ms",
                "state",
                "title",
                "tokens",
                "window",
                "worktree",
            ]
        );
    }

    /// `wait --json` is a distinct serialization site (a single object, no `agents` array), so its
    /// own exact-key-set pin: the shared row fields plus a top-level `schema`, not the `agents` wrapper.
    #[test]
    fn wait_json_pins_full_key_set() {
        let json = render_wait_json_t(&quota_row("%1"));
        assert!(json.starts_with("{\"schema\":1"));
        assert_eq!(
            json_keys(&json),
            [
                "agent",
                "attention",
                "branch",
                "context",
                "context_at_ms",
                "cost_usd",
                "detail",
                "done",
                "episode_ms",
                "host",
                "locator",
                "muted",
                "pane",
                "pct",
                "pending_call",
                "pending_summary",
                "pending_tool",
                "quota",
                "repo",
                "resets_at_ms",
                "schema",
                "server",
                "session",
                "since",
                "since_ms",
                "state",
                "title",
                "tokens",
                "window",
                "worktree",
            ]
        );
    }

    /// The fleet `wait --json` (`--all`/`--count`) is its own serialization site: the schema-1
    /// `agents` document, key-for-key the `ls --json` one, carrying every satisfied row.
    #[test]
    fn wait_json_rows_pins_full_key_set() {
        let rows = [quota_row("%1"), quota_row("%2")];
        let json = render_wait_json_rows(&rows, &origin());
        assert!(json.starts_with("{\"schema\":1,\"agents\":["));
        assert!(json.contains("\"pane\":\"%1\"") && json.contains("\"pane\":\"%2\""));
        assert_eq!(
            json_keys(&json),
            [
                "agent",
                "agents",
                "attention",
                "branch",
                "context",
                "context_at_ms",
                "cost_usd",
                "detail",
                "done",
                "episode_ms",
                "host",
                "locator",
                "muted",
                "pane",
                "pct",
                "pending_call",
                "pending_summary",
                "pending_tool",
                "quota",
                "repo",
                "resets_at_ms",
                "schema",
                "server",
                "session",
                "since",
                "since_ms",
                "state",
                "title",
                "tokens",
                "window",
                "worktree",
            ]
        );
        assert_eq!(
            json_keys(&render_wait_json_rows(&[], &origin())),
            ["agents", "schema"],
            "an empty set is still a well-formed document"
        );
    }

    /// The `wait --json` object and one `ls --json` `agents` element carry the same row fields (both
    /// via `write_row_fields`): the done surface (idle + attention) round-trips through both identically.
    #[test]
    fn wait_json_row_matches_ls_row_fields() {
        let json = render_wait_json_t(&done_row("%2"));
        assert!(json.contains("\"pane\":\"%2\""));
        assert!(
            json.contains("\"state\":\"idle\""),
            "state token stays idle"
        );
        assert!(json.contains("\"attention\":true"), "done via the flag");
    }

    /// The sorted, de-duplicated object keys of a JSON document (a quoted string whose next non-space
    /// char is `:`). Byte-scanned (structural bytes are ASCII), covering nested and repeated keys.
    fn json_keys(json: &str) -> Vec<String> {
        let b = json.as_bytes();
        let mut out = std::collections::BTreeSet::new();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'"' {
                let start = i + 1;
                let mut j = start;
                while j < b.len() {
                    match b[j] {
                        b'\\' => j += 2,
                        b'"' => break,
                        _ => j += 1,
                    }
                }
                let mut k = j + 1;
                while k < b.len() && b[k].is_ascii_whitespace() {
                    k += 1;
                }
                if k < b.len() && b[k] == b':' {
                    out.insert(json[start..j].to_string());
                }
                i = j + 1;
            } else {
                i += 1;
            }
        }
        out.into_iter().collect()
    }

    #[test]
    fn status_fixed_order_zeros_omitted() {
        let r = report(vec![
            row("%1", "c", AgentState::Idle, None),
            row("%2", "c", AgentState::Blocked, None),
            row("%3", "c", AgentState::Working, None),
            row("%4", "c", AgentState::Working, None),
        ]);
        assert_eq!(
            render_status(&r, &StatusStyles::default()),
            "#[range=user|tma:blocked]#[fg=red]⚑1#[norange] \
             #[range=user|tma:working]#[fg=yellow]●2#[norange] \
             #[range=user|tma:idle]#[fg=green]○1#[norange]"
        );
    }

    #[test]
    fn status_splits_done_from_idle_in_fixed_order() {
        let r = report(vec![
            row("%1", "c", AgentState::Blocked, None),
            done_row("%2"),
            done_row("%3"),
            row("%4", "c", AgentState::Idle, None),
        ]);
        // Order blocked → working → done → idle → unknown; done uses ✓ magenta, idle keeps ○.
        assert_eq!(
            render_status(&r, &StatusStyles::default()),
            "#[range=user|tma:blocked]#[fg=red]⚑1#[norange] \
             #[range=user|tma:done]#[fg=magenta]✓2#[norange] \
             #[range=user|tma:idle]#[fg=green]○1#[norange]"
        );
    }

    /// Every class at zero means nothing to say, so the whole segment is empty rather than chrome
    /// with no content behind it: an agent-less server's status line is the one it always had.
    #[test]
    fn status_is_empty_when_there_are_no_agents() {
        assert_eq!(render_status(&report(vec![]), &StatusStyles::default()), "");
    }

    #[test]
    fn status_unknown_uses_colour244() {
        let r = report(vec![row("%1", "c", AgentState::Unknown, None)]);
        assert_eq!(
            render_status(&r, &StatusStyles::default()),
            "#[range=user|tma:unknown]#[fg=colour244]?1#[norange]"
        );
    }

    /// Every emitted segment carries its own `tma:<class>` range, opened before the color and closed
    /// after the count, so a click anywhere on the glyph or the digits resolves to that class. The
    /// classes are the mouse bindings' vocabulary; a renamed one silently breaks them.
    #[test]
    fn status_wraps_each_class_in_its_own_clickable_range() {
        let r = report(vec![
            row("%1", "c", AgentState::Blocked, None),
            row("%2", "c", AgentState::Working, None),
        ]);
        let line = render_status(&r, &StatusStyles::default());
        assert_eq!(line.matches("#[range=user|tma:").count(), 2);
        assert_eq!(
            line.matches("#[norange]").count(),
            2,
            "every opened range is closed: {line}"
        );
        assert!(
            line.starts_with("#[range=user|tma:blocked]#[fg=red]"),
            "the range opens outside the color: {line}"
        );
    }

    /// The provenance pair is stamped on every row of every rows document, so a merged multi-machine
    /// set can tell two `%5`s apart.
    #[test]
    fn every_row_carries_its_server_and_host() {
        let rows = [
            row("%1", "claude", AgentState::Idle, None),
            row("%2", "codex", AgentState::Idle, None),
        ];
        let json = render_wait_json_rows(&rows, &origin());
        assert_eq!(
            json.matches("\"host\":\"box\"").count(),
            2,
            "both rows carry the host: {json}"
        );
        assert_eq!(
            json.matches("\"server\":\"/tmp/tmux-501/default\"").count(),
            2
        );
        // The single-row `wait --json` object carries the same pair.
        let one = render_wait_json_t(&rows[0]);
        assert!(
            one.contains("\"server\":\"/tmp/tmux-501/default\"")
                && one.contains("\"host\":\"box\"")
        );
    }

    /// The `subscribe --events` edge document: its own key inventory, additive-only like the row
    /// documents, and the open ends spell `""` rather than a state token.
    #[test]
    fn edge_json_pins_full_key_set_and_open_ends() {
        let prev = [row("%1", "claude", AgentState::Working, None)];
        let next = [row("%1", "claude", AgentState::Blocked, Some("permission"))];
        let edges = tma_core::diff_rows(&prev, &next);
        let json = render_edge_json(&edges[0], 1_700_000_000_000);

        assert!(json.starts_with("{\"schema\":1,\"at_ms\":1700000000000"));
        assert!(json.contains("\"from\":\"working\"") && json.contains("\"to\":\"blocked\""));
        assert!(json.contains("\"detail\":\"permission\""));
        assert!(json.contains("\"locator\":\"s:1.0\""));
        assert!(json.contains("\"repo\":null") && json.contains("\"branch\":null"));
        assert_eq!(
            json_keys(&json),
            [
                "agent", "at_ms", "branch", "detail", "from", "locator", "pane", "repo", "schema",
                "to",
            ]
        );

        let appeared = tma_core::diff_rows(&[], &next);
        let json = render_edge_json(&appeared[0], 0);
        assert!(
            json.contains("\"from\":\"\""),
            "an appearance has no prior state: {json}"
        );
        let vanished = tma_core::diff_rows(&next, &[]);
        let json = render_edge_json(&vanished[0], 0);
        assert!(
            json.contains("\"to\":\"\""),
            "a departure has no current state: {json}"
        );
    }

    /// The done pseudo-state reaches the wire as its own token (the edge vocabulary is the
    /// selector's, not the stored `@agent_state` one).
    #[test]
    fn edge_json_speaks_the_done_token() {
        let edges = tma_core::diff_rows(
            &[row("%1", "claude", AgentState::Working, None)],
            &[done_row("%1")],
        );
        let json = render_edge_json(&edges[0], 0);
        assert!(json.contains("\"from\":\"working\"") && json.contains("\"to\":\"done\""));
    }

    /// `--format plain` is `--format tmux` minus the color sequences: same order, same glyphs, same
    /// zero-class omission (an external bar applies its own coloring).
    #[test]
    fn status_plain_drops_colors_and_keeps_glyphs() {
        let r = report(vec![
            row("%1", "c", AgentState::Blocked, None),
            done_row("%2"),
            row("%3", "c", AgentState::Idle, None),
            row("%4", "c", AgentState::Working, None),
        ]);
        let plain = render_status_plain(&r, &StatusStyles::default());
        assert_eq!(plain, "⚑1 ●1 ✓1 ○1");
        assert!(!plain.contains("#[fg="), "no tmux markup: {plain}");
        assert_eq!(
            render_status_plain(&report(vec![]), &StatusStyles::default()),
            "",
            "no agents is still the empty string"
        );
    }

    /// Configured glyphs are honored in plain form; only the color is dropped.
    #[test]
    fn status_plain_honors_configured_glyphs() {
        let styles: StatusStyles =
            toml::from_str("blocked = { glyph = \"B\", color = \"colour196\" }").unwrap();
        let r = report(vec![row("%1", "c", AgentState::Blocked, None)]);
        assert_eq!(render_status_plain(&r, &styles), "B1");
    }

    /// `--format json` carries every class even at zero, and splits `done` out of `idle` exactly as
    /// the rendered line does (the classes sum to the pane total).
    #[test]
    fn status_json_counts_every_class_with_done_disjoint_from_idle() {
        let r = report(vec![
            row("%1", "c", AgentState::Blocked, None),
            done_row("%2"),
            done_row("%3"),
            row("%4", "c", AgentState::Idle, None),
        ]);
        let json = render_status_json(&r);
        assert!(json.starts_with("{\"schema\":1,\"counts\":{"));
        assert!(json.contains("\"blocked\":1"), "{json}");
        assert!(json.contains("\"done\":2"), "{json}");
        assert!(
            json.contains("\"idle\":1"),
            "the two done panes are NOT also counted as idle: {json}"
        );
        assert!(json.contains("\"working\":0") && json.contains("\"unknown\":0"));
    }

    /// The `status --format json` key inventory: its own serialization site, its own pin.
    #[test]
    fn status_json_pins_full_key_set() {
        assert_eq!(
            json_keys(&render_status_json(&report(vec![]))),
            ["blocked", "counts", "done", "idle", "schema", "unknown", "working"]
        );
    }

    /// `--format prom` emits both gauge families with HELP/TYPE, all five classes (zeros included, so
    /// a series never disappears), and one per-pane age series derived from `since` against `now`.
    #[test]
    fn status_prom_emits_both_families_with_help_and_type() {
        let mut blocked = row("%1", "claude", AgentState::Blocked, Some("permission"));
        blocked.since = 60_000;
        let text = render_status_prom(&report(vec![blocked, done_row("%2")]), 72_500);
        assert!(text.contains("# HELP tma_agents "), "{text}");
        assert!(text.contains("# TYPE tma_agents gauge\n"), "{text}");
        assert!(text.contains("tma_agents{state=\"blocked\"} 1"), "{text}");
        assert!(text.contains("tma_agents{state=\"done\"} 1"), "{text}");
        assert!(
            text.contains("tma_agents{state=\"idle\"} 0"),
            "a zero class keeps its series: {text}"
        );
        assert!(text.contains("# TYPE tma_agent_state_seconds gauge\n"));
        assert!(
            text.contains(
                "tma_agent_state_seconds{pane=\"%1\",agent=\"claude\",state=\"blocked\"} 12.500"
            ),
            "{text}"
        );
        assert!(
            text.contains("state=\"done\"} 72.400"),
            "a done pane reports the done class, not idle: {text}"
        );
        assert!(text.ends_with('\n'), "the exposition ends with a newline");
    }

    /// An unstamped transition (`since` 0) reports 0 seconds rather than the age of the epoch, and a
    /// clock that ran backwards clamps instead of underflowing.
    #[test]
    fn status_prom_handles_unknown_and_future_since() {
        let text = render_status_prom(&report(vec![row("%1", "c", AgentState::Idle, None)]), 0);
        assert!(text.contains("state=\"idle\"} 0.000"), "{text}");

        let mut future = row("%2", "c", AgentState::Idle, None);
        future.since = 9_000;
        let text = render_status_prom(&report(vec![future]), 1_000);
        assert!(text.contains("state=\"idle\"} 0.000"), "{text}");
    }

    /// A label value carrying a quote or backslash is escaped, so one oddly-named agent cannot
    /// corrupt the whole exposition.
    #[test]
    fn status_prom_escapes_label_values() {
        let text = render_status_prom(
            &report(vec![row("%1", "we\"ird\\name", AgentState::Idle, None)]),
            0,
        );
        assert!(text.contains(r#"agent="we\"ird\\name""#), "{text}");
    }

    /// A `[status]` override changes the emitted glyph + color; other classes keep defaults.
    #[test]
    fn status_honors_config_override() {
        let styles: StatusStyles =
            toml::from_str("blocked = { glyph = \"B\", color = \"colour196\" }").unwrap();
        let r = report(vec![
            row("%1", "c", AgentState::Blocked, None),
            row("%2", "c", AgentState::Working, None),
        ]);
        assert_eq!(
            render_status(&r, &styles),
            "#[range=user|tma:blocked]#[fg=colour196]B1#[norange] \
             #[range=user|tma:working]#[fg=yellow]●1#[norange]"
        );
    }
}
