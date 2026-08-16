//! `tma jump`: move focus to an agent pane across sessions in one action. A jump is
//! `switch-client` + `select-window` + `select-pane`, the only pane-affecting action `tma` performs;
//! the modes are on [`JumpKind`]. Origin is resolved via client/active-pane queries, **never
//! `$TMUX_PANE`** (a hidden popup pane under `display-popup`); the acting client is passed in by the
//! keybinding, so `switch-client -c <client>` moves the client that actually invoked the jump.
//! Return trail: origins live in a server option keyed by sanitized client name + a raw-name hash
//! (tmux has no client-scoped options), a bounded `TRAIL_CAP` stack of locators, oldest first,
//! newline-joined (a legacy single-entry value with no newline parses as a one-deep stack).

use tma_core::stamp::opt;
use tma_core::{sort_rank, AgentRow, AgentState, FoldConfig, Selector};
use tma_runtime::{escape_menu_label, ui, MenuItem, Server, Tmux, TmuxError};

use tma_runtime::config::PickerStyles;
use tma_runtime::cycle;
use tma_runtime::manifests::LoadedManifest;
use tma_ui_core::render::{fmt_since, truncate, truncate_locator};

/// Which jump to perform.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JumpKind {
    /// Focus one named agent pane (`--pane %5`), the target the menu entries fire. Behaves like the
    /// picker's Enter: it records the origin and clears the destination's attention flag.
    Pane(String),
    /// The next agent that wants you: blocked (longest-blocked first), then finished-unreviewed
    /// (idle with attention), advancing from the current pane and wrapping (the attention cursor).
    Attention,
    /// The longest-blocked agent.
    Blocked,
    /// The next agent after the current pane, cycling by session → window → pane.
    Next,
    /// Return one step along the trail (the previous jump's origin).
    Back,
    /// Return to the pre-triage origin (the bottom of the trail) and clear the trail.
    Home,
}

/// The return trail's cap: the most origins a client keeps for `--back`/`--home`. Bounded and
/// disposable (a deeper history is not worth the option churn); past the cap the oldest are dropped.
const TRAIL_CAP: usize = 8;

/// The result of a jump attempt, for the CLI to report.
pub struct JumpOutcome {
    /// The locator jumped to, or `None` when there was no target.
    pub jumped_to: Option<String>,
    /// A human message (why nothing happened, or what was chosen).
    pub message: String,
}

/// A pane to jump to, resolved to the tmux targets `focus` needs.
struct Destination {
    session: String,
    window_target: String,
    pane_target: String,
    locator: String,
}

impl Destination {
    fn from_row(r: &AgentRow) -> Destination {
        Destination {
            session: r.session.clone(),
            window_target: format!("{}:{}", r.session, r.window_index),
            // A pane id is a stable, unambiguous select-pane target.
            pane_target: r.pane_id.clone(),
            locator: r.locator(),
        }
    }

    /// Parse a stored origin locator `session:window.pane` back into jump targets.
    fn from_locator(loc: &str) -> Option<Destination> {
        let (session, rest) = loc.split_once(':')?;
        let (window, _pane) = rest.split_once('.')?;
        Some(Destination {
            session: session.to_string(),
            window_target: format!("{session}:{window}"),
            pane_target: loc.to_string(),
            locator: loc.to_string(),
        })
    }
}

/// Run a jump: one poll cycle for fresh state, resolve the destination, record the origin for
/// `--back`, focus. `acting_client` `Some` resolves/focuses that exact client, `None` best-effort.
/// `selector` scopes the candidates of the forward jumps (scoped triage); `--back`/`--home` replay
/// the trail and ignore it. The cycle runs unscoped — filtering only narrows what may be picked.
pub fn run_jump(
    tmux: &Tmux,
    manifests: &[LoadedManifest],
    cfg: &FoldConfig,
    kind: JumpKind,
    selector: &Selector,
    acting_client: Option<&str>,
) -> Result<JumpOutcome, TmuxError> {
    let origin = ui::active_locator(tmux, acting_client);
    let client = resolve_client(tmux, acting_client);
    let key = origin_key(&client);
    // Read the return trail once. `--back`/`--home` resolve their destination from it (pruning any
    // malformed entry off the consumed end); a forward jump pushes onto it below.
    let mut trail = read_trail(tmux, &key)?;

    let dest = match kind {
        // `--back` consumes from the top, `--home` from the bottom (the pre-triage origin). Both
        // parse a stored locator back into jump targets; a malformed entry at the consumed end is
        // pruned (not merely skipped) and the next candidate tried, so a corrupt locator can never
        // wedge the return.
        JumpKind::Back => pop_valid_from_top(&mut trail),
        JumpKind::Home => pop_valid_from_bottom(&mut trail),
        // An explicit pane target ignores the selector: the caller (a menu entry, a script) already
        // named the pane, so narrowing it again could only turn a valid target into a miss.
        JumpKind::Pane(ref pane_id) => cycle::run_cycle(tmux, manifests, cfg)?
            .rows
            .iter()
            .find(|r| &r.pane_id == pane_id)
            .map(Destination::from_row),
        JumpKind::Attention | JumpKind::Blocked | JumpKind::Next => {
            let mut report = cycle::run_cycle(tmux, manifests, cfg)?;
            // Scope the candidates after the cycle stamped them all. Repo labels cost a bounded,
            // memoized resolve, so they are only fetched when the selector reads them.
            if selector.needs_repo() {
                tma_runtime::repo::annotate_rows(&mut report.rows);
            }
            selector.retain(&mut report.rows);
            let here = parse_locator(&origin);
            let here = here.as_ref().map(|(s, w, p)| (s.as_str(), *w, *p));
            let chosen = match kind {
                JumpKind::Attention => pick_attention(&report.rows, here),
                JumpKind::Blocked => pick_blocked(&report.rows),
                JumpKind::Next => here
                    .and_then(|o| pick_next(&report.rows, o))
                    .or_else(|| report.rows.first()),
                JumpKind::Back | JumpKind::Home | JumpKind::Pane(_) => unreachable!(),
            };
            chosen.map(Destination::from_row)
        }
    };

    let Some(dest) = dest else {
        // `--back`/`--home` may have pruned malformed entries while failing to resolve; persist the
        // pruned trail so a corrupt entry cannot wedge the next return.
        if matches!(kind, JumpKind::Back | JumpKind::Home) {
            let _ = write_trail(tmux, &key, &trail);
        }
        // A scoped miss says so, else "no blocked agents" reads as a lie about the whole server.
        let scope = if selector.is_empty() { "" } else { " in scope" };
        return Ok(JumpOutcome {
            jumped_to: None,
            message: match kind {
                JumpKind::Attention => format!("no agents waiting for you{scope}"),
                JumpKind::Blocked => format!("no blocked agents{scope}"),
                JumpKind::Next => format!("no agents to jump to{scope}"),
                JumpKind::Back | JumpKind::Home => "no jump origin recorded".to_string(),
                JumpKind::Pane(id) => format!("no agent in pane {id}"),
            },
        });
    };

    persist_trail(tmux, &key, &kind, &mut trail, &origin);

    ui::focus_pane(
        tmux,
        acting_client,
        &dest.session,
        &dest.window_target,
        &dest.pane_target,
    )?;
    // Clear the destination's attention flag only on the "go deal with this" jumps
    // (`--attention`/`--blocked`/`--pane`), the same way the picker's Enter path does: focusing a
    // waiting pane reviews it, so it leaves the attention queue. `--next` is pure positional cycling
    // and must NOT clear attention (that would silently mark a finished-unreviewed agent reviewed);
    // `--back`/`--home` resolve to a locator target rather than a real pane id, so both are excluded.
    if matches!(
        kind,
        JumpKind::Attention | JumpKind::Blocked | JumpKind::Pane(_)
    ) {
        let _ = ui::clear_attention(tmux, &dest.pane_target);
    }
    Ok(JumpOutcome {
        message: format!("jumped to {}", dest.locator),
        jumped_to: Some(dest.locator),
    })
}

/// Update the return trail after the destination resolved. A forward jump pushes the current
/// location; `--back` already popped the entry it consumed during resolution, so it just persists the
/// remainder; `--home` clears the trail (it returned to the bottom). Writes are best-effort.
fn persist_trail(tmux: &Tmux, key: &str, kind: &JumpKind, trail: &mut Vec<String>, origin: &str) {
    let forward = matches!(
        kind,
        JumpKind::Attention | JumpKind::Blocked | JumpKind::Next | JumpKind::Pane(_)
    );
    match kind {
        JumpKind::Back => {
            let _ = write_trail(tmux, key, trail);
        }
        JumpKind::Home => {
            let _ = write_trail(tmux, key, &[]);
        }
        _ if forward && !origin.is_empty() => {
            push_origin(trail, origin);
            let _ = write_trail(tmux, key, trail);
        }
        _ => {}
    }
}

/// The client name to key the origin option by: the acting client when the keybinding passed
/// one, else the targetless `#{client_name}` best-effort fallback.
fn resolve_client(tmux: &Tmux, acting_client: Option<&str>) -> String {
    match acting_client {
        Some(c) => c.to_string(),
        None => ui::active_client_name(tmux),
    }
}

/// Focus a specific agent pane (the picker's Enter path): record the current location as the
/// `--back` origin, then jump. Reuses `run_jump`'s origin machinery to move the same acting client.
pub(crate) fn focus_agent(
    tmux: &Tmux,
    row: &AgentRow,
    acting_client: Option<&str>,
) -> Result<(), TmuxError> {
    let origin = ui::active_locator(tmux, acting_client);
    let client = resolve_client(tmux, acting_client);
    if !origin.is_empty() {
        let key = origin_key(&client);
        // Only push when the trail reads cleanly. A transient (non-`ServerGone`) read error must not
        // clobber the whole stack with a single-entry default (what `unwrap_or_default` did); skip
        // the push and keep the jump, matching the best-effort writes elsewhere in this path.
        if let Ok(mut trail) = read_trail(tmux, &key) {
            push_origin(&mut trail, &origin);
            let _ = write_trail(tmux, &key, &trail);
        }
    }
    ui::focus_pane(
        tmux,
        acting_client,
        &row.session,
        &format!("{}:{}", row.session, row.window_index),
        &row.pane_id,
    )
}

// ---- --menu --------------------------------------------------------------------------------

/// What `--menu` did, for the CLI to report. The menu itself is a tmux overlay: once it renders,
/// selecting an entry is tmux's job, so there is nothing further to await here.
pub enum JumpMenuOutcome {
    /// The menu rendered with this many entries.
    Shown(usize),
    /// No agent to list (after the selector and the self-pane exclusion), so nothing was rendered.
    NoAgents,
    /// No client and no `$TMUX_PANE`, so there is nowhere to render a menu.
    NoClient,
}

/// Agent column width in a menu label, and the locator's. Wide enough for the usual `claude`/`codex`
/// and a `session:window.pane`, short enough that the menu stays a menu.
const MENU_AGENT_W: usize = 10;
const MENU_LOCATOR_W: usize = 20;

/// The `display-menu` entries for `rows`: `<glyph> <agent> <locator> <since>` per agent, each firing
/// the same focus the picker's Enter performs, via `run-shell '<bin> jump --pane <id>'`. The acting
/// client is resolved into the command here rather than left as `#{client_name}`: this string is
/// built by a process that already knows it. The first nine carry a `1`..`9` quick-select mnemonic,
/// as the action menu's entries do.
pub(crate) fn menu_items(
    rows: &[AgentRow],
    now: u64,
    styles: &PickerStyles,
    bin: &str,
    server: &Server,
    client: Option<&str>,
) -> Vec<MenuItem> {
    let socket = server.shell_flag();
    let client_flag = match client.filter(|c| !c.is_empty()) {
        Some(c) => format!(" --client '{c}'"),
        None => String::new(),
    };
    rows.iter()
        .enumerate()
        .map(|(i, r)| {
            let glyph = if tma_core::is_done(r) {
                styles.resolved_done_str().0
            } else {
                styles.resolved_str(r.state).0
            };
            let label = format!(
                "{glyph} {:<MENU_AGENT_W$} {:<MENU_LOCATOR_W$} {}",
                truncate(&r.agent, MENU_AGENT_W),
                truncate_locator(&r.locator(), MENU_LOCATOR_W),
                fmt_since(now, r.since)
            );
            MenuItem {
                label: escape_menu_label(&label),
                key: if i < 9 {
                    ((b'1' + i as u8) as char).to_string()
                } else {
                    String::new()
                },
                command: format!(
                    "run-shell \"'{bin}' jump --pane {}{client_flag}{socket}\"",
                    r.pane_id
                ),
            }
        })
        .collect()
}

/// `tma jump --menu`: one poll cycle, the picker's ordering and self-pane exclusion, then a tmux
/// `display-menu` on the acting client whose entries jump. `selector` scopes the listed agents the
/// same way it scopes a forward jump.
#[allow(clippy::too_many_arguments)]
pub fn run_jump_menu(
    tmux: &Tmux,
    manifests: &[LoadedManifest],
    cfg: &FoldConfig,
    styles: &PickerStyles,
    selector: &Selector,
    bin: &str,
    server: &Server,
    acting_client: Option<&str>,
) -> Result<JumpMenuOutcome, TmuxError> {
    // The pane the menu is opened from, resolved through the client (inside a popup `$TMUX_PANE` is
    // the popup's hidden pane): both the row to hide and the menu's target. An unresolvable pane
    // hides nothing and falls back to `$TMUX_PANE` as the target.
    let self_pane = ui::active_pane_id(tmux, acting_client);
    let mut report = cycle::run_cycle(tmux, manifests, cfg)?;
    if selector.needs_repo() {
        tma_runtime::repo::annotate_rows(&mut report.rows);
    }
    selector.retain(&mut report.rows);
    if let Some(pane) = &self_pane {
        report.rows.retain(|r| &r.pane_id != pane);
    }
    // The picker's order: blocked → working → idle → unknown, longest-in-state first.
    report.rows.sort_by(|a, b| {
        sort_rank(a.state)
            .cmp(&sort_rank(b.state))
            .then_with(|| a.since.cmp(&b.since))
    });
    if report.rows.is_empty() {
        return Ok(JumpMenuOutcome::NoAgents);
    }
    let Some(target) =
        self_pane.or_else(|| std::env::var("TMUX_PANE").ok().filter(|p| !p.is_empty()))
    else {
        return Ok(JumpMenuOutcome::NoClient);
    };

    let items = menu_items(
        &report.rows,
        crate::picker::unix_now(),
        styles,
        bin,
        server,
        acting_client,
    );
    crate::menu::show(tmux, &target, "tma jump", &items)?;
    Ok(JumpMenuOutcome::Shown(items.len()))
}

/// The longest-blocked agent: the smallest `@agent_since` among blocked panes.
pub(crate) fn pick_blocked(rows: &[AgentRow]) -> Option<&AgentRow> {
    rows.iter()
        .filter(|r| r.state == AgentState::Blocked)
        .min_by_key(|r| r.since)
}

/// The next agent after `origin`, cycling by session name → window index → pane index.
/// Wraps to the first agent when the origin is at or past the last.
pub(crate) fn pick_next<'a>(
    rows: &'a [AgentRow],
    origin: (&str, u32, u32),
) -> Option<&'a AgentRow> {
    if rows.is_empty() {
        return None;
    }
    let mut sorted: Vec<&AgentRow> = rows.iter().collect();
    sorted.sort_by(|a, b| key(a).cmp(&key(b)));
    sorted
        .iter()
        .copied()
        .find(|r| key(r) > origin)
        .or_else(|| sorted.first().copied())
}

/// Whether a row "wants you": blocked, or finished-unreviewed (idle with the attention flag
/// still set, the "done" surface). The predicate the attention cursor advances through.
fn wants_attention(r: &AgentRow) -> bool {
    r.state == AgentState::Blocked || (r.state == AgentState::Idle && r.attention)
}

/// The attention cursor: the next agent that wants you, from the current pane and wrapping. If the
/// current pane is in the queue, advance past it; else start at the front (the highest-priority row).
pub(crate) fn pick_attention<'a>(
    rows: &'a [AgentRow],
    origin: Option<(&str, u32, u32)>,
) -> Option<&'a AgentRow> {
    let mut queue: Vec<&AgentRow> = rows.iter().filter(|r| wants_attention(r)).collect();
    if queue.is_empty() {
        return None;
    }
    queue.sort_by(|a, b| attention_key(a).cmp(&attention_key(b)));
    // The cursor: the current pane's slot in the queue, else before the front so we pick index 0.
    let next = match origin.and_then(|o| queue.iter().position(|r| key(r) == o)) {
        Some(i) => (i + 1) % queue.len(),
        None => 0,
    };
    Some(queue[next])
}

/// Sort key for the attention queue: blocked rows first (bucket 0, longest-blocked first via `since`
/// ascending), then finished-unreviewed (bucket 1, by locator), locator the final tiebreak in both.
/// `since` is keyed only for blocked rows so idle+attention rows sort purely positionally.
fn attention_key(r: &AgentRow) -> (u8, u64, &str, u32, u32) {
    let blocked = r.state == AgentState::Blocked;
    (
        u8::from(!blocked),
        if blocked { r.since } else { 0 },
        r.session.as_str(),
        r.window_index,
        r.pane_index,
    )
}

fn key(r: &AgentRow) -> (&str, u32, u32) {
    (r.session.as_str(), r.window_index, r.pane_index)
}

/// Read a client's return trail from its server option, oldest entry first. A missing option is an
/// empty trail; a legacy single-entry value (no newline) parses as a one-deep trail.
fn read_trail(tmux: &Tmux, key: &str) -> Result<Vec<String>, TmuxError> {
    Ok(parse_trail(
        ui::trail_read(tmux, key)?.as_deref().unwrap_or_default(),
    ))
}

/// Parse a stored trail option value into its stack of locators. Newline-separated; blank lines are
/// dropped, so an empty value is an empty trail and a legacy single-entry value is a one-deep trail.
fn parse_trail(raw: &str) -> Vec<String> {
    raw.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// Write a client's return trail back to its server option (oldest first, newline-joined). An empty
/// trail writes an empty value, which reads back as no trail.
fn write_trail(tmux: &Tmux, key: &str, trail: &[String]) -> Result<(), TmuxError> {
    ui::trail_write(tmux, key, &trail.join("\n"))
}

/// Push `origin` onto the trail: skip a consecutive duplicate of the top (re-triaging from the same
/// spot must not grow it), then drop the oldest so it never exceeds [`TRAIL_CAP`].
fn push_origin(trail: &mut Vec<String>, origin: &str) {
    if trail.last().map(String::as_str) == Some(origin) {
        return;
    }
    trail.push(origin.to_string());
    if trail.len() > TRAIL_CAP {
        let overflow = trail.len() - TRAIL_CAP;
        trail.drain(0..overflow);
    }
}

/// Pop off the top until one parses into a [`Destination`] (what `--back` consumes), discarding
/// malformed entries above it so a corrupt locator cannot wedge the return. `None` is "no origin".
fn pop_valid_from_top(trail: &mut Vec<String>) -> Option<Destination> {
    while let Some(loc) = trail.pop() {
        if let Some(dest) = Destination::from_locator(&loc) {
            return Some(dest);
        }
    }
    None
}

/// Remove off the bottom until one parses into a [`Destination`], dropping malformed entries below
/// it. `--home` clears the trail afterward anyway, so this only skips a corrupt bottom entry.
fn pop_valid_from_bottom(trail: &mut Vec<String>) -> Option<Destination> {
    while !trail.is_empty() {
        let loc = trail.remove(0);
        if let Some(dest) = Destination::from_locator(&loc) {
            return Some(dest);
        }
    }
    None
}

fn parse_locator(loc: &str) -> Option<(String, u32, u32)> {
    let (session, rest) = loc.split_once(':')?;
    let (window, pane) = rest.split_once('.')?;
    Some((
        session.to_string(),
        window.parse().ok()?,
        pane.parse().ok()?,
    ))
}

/// Server-option key for a client's jump origin: `@tma_origin_<sanitized name>_<hash>`. Sanitizing
/// alone collides for punctuation-only-differing names (`/dev/ttys003` vs `.dev.ttys003` both give
/// `_dev_ttys003`), so an 8-hex FNV-1a hash of the *raw* name disambiguates (a clash: one stale `--back`).
fn origin_key(client: &str) -> String {
    let sanitized: String = client
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    // FNV-1a (32-bit) over the raw bytes, written inline to avoid a dependency.
    let mut hash: u32 = 0x811c_9dc5;
    for b in client.as_bytes() {
        hash ^= u32::from(*b);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    format!("{}{sanitized}_{hash:08x}", opt::ORIGIN_PREFIX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(session: &str, w: u32, p: u32, state: AgentState, since: u64) -> AgentRow {
        AgentRow {
            pane_id: format!("%{w}{p}"),
            agent: "claude".to_string(),
            state,
            detail: None,
            since,
            session: session.to_string(),
            window_index: w,
            pane_index: p,
            title: "t".to_string(),
            attention: false,
            agent_session: None,
            context_pct: None,
            context_at: None,
            tokens: None,
            muted: false,
            model: None,
            cwd: None,
            repo: None,
        }
    }

    #[test]
    fn blocked_picks_longest_blocked() {
        let rows = vec![
            row("a", 0, 0, AgentState::Working, 10),
            row("b", 0, 0, AgentState::Blocked, 300), // blocked more recently
            row("c", 0, 0, AgentState::Blocked, 100), // blocked longer (smaller since)
        ];
        let picked = pick_blocked(&rows).unwrap();
        assert_eq!(picked.since, 100, "longest-blocked (smallest since) wins");
        assert_eq!(picked.session, "c");
    }

    #[test]
    fn blocked_none_when_no_blocked() {
        let rows = vec![row("a", 0, 0, AgentState::Idle, 10)];
        assert!(pick_blocked(&rows).is_none());
    }

    #[test]
    fn next_cycles_by_session_then_window_then_pane() {
        let rows = vec![
            row("alpha", 0, 0, AgentState::Idle, 0),
            row("alpha", 1, 0, AgentState::Idle, 0),
            row("beta", 0, 0, AgentState::Idle, 0),
        ];
        // After alpha:0.0 → alpha:1.0.
        assert_eq!(pick_next(&rows, ("alpha", 0, 0)).unwrap().window_index, 1);
        // After alpha:1.0 → beta:0.0.
        assert_eq!(pick_next(&rows, ("alpha", 1, 0)).unwrap().session, "beta");
        // After the last (beta:0.0) → wrap to the first (alpha:0.0).
        let wrapped = pick_next(&rows, ("beta", 0, 0)).unwrap();
        assert_eq!(
            (wrapped.session.as_str(), wrapped.window_index),
            ("alpha", 0)
        );
    }

    #[test]
    fn next_from_before_all_picks_first() {
        let rows = vec![row("m", 5, 0, AgentState::Idle, 0)];
        // Origin sorts before every agent → first agent.
        assert_eq!(pick_next(&rows, ("a", 0, 0)).unwrap().session, "m");
    }

    #[test]
    fn origin_key_sanitizes_client_name() {
        // The key is the sanitized name plus an underscore and an 8-hex FNV-1a disambiguator of the
        // raw name, so it starts with the sanitized prefix and is exactly 9 chars (`_` + 8 hex) longer.
        let k = origin_key("/dev/ttys003");
        assert!(k.starts_with("@tma_origin__dev_ttys003_"), "got {k}");
        assert_eq!(k.len(), "@tma_origin__dev_ttys003".len() + 9);
        assert!(origin_key("client-1").starts_with("@tma_origin_client_1_"));
    }

    #[test]
    fn origin_key_disambiguates_punctuation_only_difference() {
        // `/dev/ttys003` and `.dev.ttys003` sanitize identically (`_dev_ttys003`); the raw-name hash
        // must keep their trails from cross-contaminating.
        assert_ne!(origin_key("/dev/ttys003"), origin_key(".dev.ttys003"));
    }

    #[test]
    fn destination_from_locator_round_trips() {
        let d = Destination::from_locator("work:2.3").unwrap();
        assert_eq!(d.session, "work");
        assert_eq!(d.window_target, "work:2");
        assert_eq!(d.pane_target, "work:2.3");
        assert!(Destination::from_locator("garbage").is_none());
    }

    // --- the jump menu --------------------------------------------------------------------------

    /// Each entry fires the same targetless focus the picker's Enter does, through `tma jump --pane`,
    /// with the acting client and the invoking server resolved into the command (a menu entry runs
    /// long after the surface that built it). The first nine get a quick-select digit.
    #[test]
    fn menu_entries_jump_to_their_pane_with_the_client_and_server() {
        let rows = vec![
            row("work", 1, 0, AgentState::Blocked, 100),
            row("home", 0, 2, AgentState::Idle, 0),
        ];
        let items = menu_items(
            &rows,
            0,
            &PickerStyles::default(),
            "/usr/bin/tma",
            &Server::named(Some("scratch".to_string())),
            Some("/dev/ttys003"),
        );
        assert_eq!(items.len(), 2);
        assert_eq!(
            items[0].command,
            "run-shell \"'/usr/bin/tma' jump --pane %10 --client '/dev/ttys003' \
             --socket-name scratch\""
        );
        assert_eq!(items[0].key, "1");
        assert_eq!(items[1].key, "2");
        // No client to resolve: the entry omits the flag and lets `tma` find the acting client.
        let items = menu_items(
            &rows,
            0,
            &PickerStyles::default(),
            "tma",
            &Server::default(),
            None,
        );
        assert_eq!(items[0].command, "run-shell \"'tma' jump --pane %10\"");
    }

    /// The label carries the state glyph, the agent, the locator and the time in state, and a `#` in
    /// a session name is escaped (a menu label is a tmux format string).
    #[test]
    fn menu_labels_show_the_row_and_escape_the_format_marker() {
        let mut r = row("we#ird", 1, 0, AgentState::Blocked, 1_000);
        r.agent = "claude".to_string();
        let items = menu_items(
            &[r],
            61_000,
            &PickerStyles::default(),
            "tma",
            &Server::default(),
            None,
        );
        let label = &items[0].label;
        assert!(label.starts_with("⚑ claude"), "{label}");
        assert!(label.contains("we##ird:1.0"), "the `#` is doubled: {label}");
        assert!(label.ends_with(" 1m"), "time in state: {label}");
    }

    /// A finished-unreviewed row (idle + attention) shows the "done" glyph, not the idle one, the
    /// same split every other surface makes.
    #[test]
    fn menu_labels_split_done_from_idle() {
        let items = menu_items(
            &[done("s", 0, 0)],
            0,
            &PickerStyles::default(),
            "tma",
            &Server::default(),
            None,
        );
        assert!(items[0].label.starts_with('✓'), "{}", items[0].label);
    }

    /// Past the ninth entry there is no quick-select digit left (tmux would take a two-char key as
    /// something else), matching the action menu's rule.
    #[test]
    fn menu_tenth_entry_has_no_mnemonic() {
        let rows: Vec<AgentRow> = (0..11)
            .map(|i| row("s", i, 0, AgentState::Idle, 0))
            .collect();
        let items = menu_items(
            &rows,
            0,
            &PickerStyles::default(),
            "tma",
            &Server::default(),
            None,
        );
        assert_eq!(items[8].key, "9");
        assert_eq!(items[9].key, "");
    }

    // --- attention cursor -----------------------------------------------------------------------

    fn done(session: &str, w: u32, p: u32) -> AgentRow {
        // Finished-unreviewed: idle with the attention flag still set (the "done" surface).
        let mut r = row(session, w, p, AgentState::Idle, 0);
        r.attention = true;
        r
    }

    /// The queue is blocked-first (longest-blocked first), then finished-unreviewed; working panes
    /// and plain idle panes (idle without attention) are skipped entirely.
    #[test]
    fn attention_orders_blocked_longest_first_then_done_skips_others() {
        let rows = vec![
            row("aa", 0, 0, AgentState::Blocked, 300), // blocked more recently
            row("bb", 0, 0, AgentState::Blocked, 100), // blocked longest (smaller since)
            done("cc", 0, 0),                          // finished-unreviewed
            row("dd", 0, 0, AgentState::Working, 5),   // skipped
            row("ee", 0, 0, AgentState::Idle, 5),      // plain idle (no attention): skipped
        ];
        // From outside the queue (no current-pane match) the highest-priority row wins.
        assert_eq!(pick_attention(&rows, None).unwrap().session, "bb");
        // Advance through the queue: bb (longest blocked) → aa (blocked) → cc (done) → wrap to bb.
        assert_eq!(
            pick_attention(&rows, Some(("bb", 0, 0))).unwrap().session,
            "aa"
        );
        assert_eq!(
            pick_attention(&rows, Some(("aa", 0, 0))).unwrap().session,
            "cc"
        );
        assert_eq!(
            pick_attention(&rows, Some(("cc", 0, 0))).unwrap().session,
            "bb",
            "wraps from the last queue entry to the first"
        );
    }

    /// A current pane that is not itself in the queue (a working or non-agent pane) starts at the
    /// front, so `--attention` from anywhere lands on the highest-priority waiter.
    #[test]
    fn attention_from_pane_outside_queue_picks_front() {
        let rows = vec![
            row("work", 0, 0, AgentState::Working, 0),
            row("blk", 0, 0, AgentState::Blocked, 50),
        ];
        assert_eq!(
            pick_attention(&rows, Some(("work", 0, 0))).unwrap().session,
            "blk"
        );
    }

    /// Blocked rows with the same `since` fall back to the positional locator, so the cursor still
    /// advances deterministically.
    #[test]
    fn attention_ties_break_on_locator() {
        let rows = vec![
            row("z", 0, 0, AgentState::Blocked, 100),
            row("a", 0, 0, AgentState::Blocked, 100),
        ];
        assert_eq!(pick_attention(&rows, None).unwrap().session, "a");
        assert_eq!(
            pick_attention(&rows, Some(("a", 0, 0))).unwrap().session,
            "z"
        );
    }

    #[test]
    fn attention_empty_when_nothing_wants_you() {
        let rows = vec![
            row("a", 0, 0, AgentState::Working, 0),
            row("b", 0, 0, AgentState::Idle, 0), // no attention flag
        ];
        assert!(pick_attention(&rows, None).is_none());
        assert!(pick_attention(&rows, Some(("a", 0, 0))).is_none());
    }

    // --- return trail ---------------------------------------------------------------------------

    #[test]
    fn trail_parses_legacy_single_entry() {
        // An option written by the old single-level `--back` (one locator, no newline) is a
        // one-deep trail.
        assert_eq!(parse_trail("home:0.0"), vec!["home:0.0".to_string()]);
        assert!(parse_trail("").is_empty());
    }

    #[test]
    fn trail_round_trips_multiple_entries() {
        let trail = vec![
            "a:0.0".to_string(),
            "b:1.2".to_string(),
            "c:3.4".to_string(),
        ];
        let raw = trail.join("\n");
        assert_eq!(parse_trail(&raw), trail);
    }

    #[test]
    fn push_origin_dedups_consecutive_duplicate() {
        let mut trail = vec!["home:0.0".to_string()];
        push_origin(&mut trail, "home:0.0");
        assert_eq!(
            trail,
            vec!["home:0.0".to_string()],
            "no growth from re-triage"
        );
        push_origin(&mut trail, "work:1.0");
        push_origin(&mut trail, "work:1.0");
        assert_eq!(
            trail,
            vec!["home:0.0".to_string(), "work:1.0".to_string()],
            "a non-consecutive value still pushes; the consecutive repeat does not"
        );
    }

    #[test]
    fn push_origin_caps_dropping_oldest() {
        let mut trail: Vec<String> = Vec::new();
        for i in 0..(TRAIL_CAP + 3) {
            push_origin(&mut trail, &format!("s:{i}.0"));
        }
        assert_eq!(trail.len(), TRAIL_CAP, "never exceeds the cap");
        // The three oldest entries (0,1,2) were dropped; the bottom is now entry 3.
        assert_eq!(trail.first().unwrap(), "s:3.0");
        assert_eq!(trail.last().unwrap(), &format!("s:{}.0", TRAIL_CAP + 2));
    }

    #[test]
    fn back_consumes_top_and_prunes_malformed() {
        let mut trail = vec![
            "a:0.0".to_string(),
            "garbage".to_string(),
            "b:1.2".to_string(),
        ];
        // A valid top is consumed; the remainder is left intact.
        assert_eq!(pop_valid_from_top(&mut trail).unwrap().locator, "b:1.2");
        assert_eq!(trail, vec!["a:0.0".to_string(), "garbage".to_string()]);
        // The malformed entry now on top is pruned, not returned, then `a:0.0` is consumed — so a
        // corrupt entry can never wedge `--back` (the bug in finding 1).
        assert_eq!(pop_valid_from_top(&mut trail).unwrap().locator, "a:0.0");
        assert!(trail.is_empty());
        assert!(pop_valid_from_top(&mut trail).is_none());
    }

    #[test]
    fn home_skips_malformed_bottom_entries() {
        let mut trail = vec!["junk".to_string(), "a:0.0".to_string(), "b:1.2".to_string()];
        // The corrupt bottom entry is dropped and the next valid one becomes the target, so a bad
        // bottom entry cannot wedge `--home`.
        assert_eq!(pop_valid_from_bottom(&mut trail).unwrap().locator, "a:0.0");
        // An all-malformed trail empties entirely and yields no origin.
        let mut bad = vec!["x".to_string(), "y".to_string()];
        assert!(pop_valid_from_bottom(&mut bad).is_none());
        assert!(bad.is_empty());
    }
}
