//! The resolved-agent-row DTO shared by every surface (`tma ls`, `tma status`, jump, the picker,
//! `watch`): one pane's detected state plus its locator vocabulary, produced by the runtime cycle.
//!
//! Accretion rule: a future multi-field annotation adds one cluster type (like [`RepoLabel`]), never
//! a set of parallel scalar `Option`s that must all be null together. Revisit decomposing the row
//! into sub-structs only if its top-level fields approach ~20.

use std::str::FromStr;

use crate::state::GrammarError;
use crate::AgentState;

/// The resolved repo annotation for one pane: present exactly when the pane's `cwd` resolved to a git
/// repo, absent otherwise. Clustering the three values makes "branch/worktree are set iff repo is set"
/// structural rather than a doc-comment invariant across three parallel `Option`s. `worktree` is
/// `false` for a resolved main checkout, `true` for a linked worktree.
#[derive(Clone, Debug)]
pub struct RepoLabel {
    /// Origin repo name (basename of the git common dir's parent); linked worktrees share it.
    pub name: String,
    /// Current branch label (`git rev-parse --abbrev-ref HEAD`); the literal `HEAD` when detached.
    pub branch: String,
    /// Whether the checkout is a linked worktree (git-dir differs from the common-dir).
    pub worktree: bool,
}

/// One agent pane's resolved state for a surface (`tma ls`, `tma status`, jump, the picker).
#[derive(Clone, Debug)]
pub struct AgentRow {
    pub pane_id: String,
    pub agent: String,
    pub state: AgentState,
    pub detail: Option<String>,
    /// Epoch of the current state's transition (`@agent_since`); 0 when unknown. The uptime
    /// column reads it, so it stays the state's own clock and never absorbs [`AgentRow::turn_at`].
    pub since: u64,
    /// Epoch of the last recorded turn end (`@agent_turn_at`); 0 when none. Only a hook the
    /// manifest marks `turn_end` writes it, and only when that turn end raised the done marker.
    /// Not part of the JSON row contract; `wait --since` reads it via [`AgentRow::episode_at`].
    pub turn_at: u64,
    pub session: String,
    pub window_index: u32,
    pub pane_index: u32,
    pub title: String,
    /// Presentation flag (`@agent_attention`): the current state is unreviewed. Surfaces render
    /// idle + `attention` as the "done" glyph; the `@agent_state` token stays `idle`.
    pub attention: bool,
    /// Owning agent session id (`@agent_session`), `None` when the pane never registered one. The
    /// `session` key of the JSON rows.
    pub agent_session: Option<String>,
    /// Context-utilization percent (`@agent_context_pct`), `None` when absent. The `context`
    /// key of the JSON rows.
    pub context_pct: Option<u8>,
    /// Epoch **ms** of the context evidence (`@agent_context_at`), `None` when absent. The
    /// `context_at_ms` key of the JSON rows, and the age of `tokens` too (both come from the one
    /// observation, so `@agent_tokens_at` would repeat it on the row).
    pub context_at: Option<u64>,
    /// Tokens currently in the context window (`@agent_tokens`), `None` when the agent's
    /// channel reports no count tma can call a footprint. The `tokens` key of the JSON rows; never a
    /// cumulative spend figure, so it is not summable across turns.
    pub tokens: Option<u64>,
    /// Whether `@agent_mute_until` is still in the future at the cycle's `now`: this pane's
    /// notifications are suppressed. A resolved boolean rather than the deadline, because every
    /// consumer asks the same question and only the cycle holds the clock. The `muted` key of the
    /// JSON rows and the `tma ls` marker column.
    pub muted: bool,
    /// Best-effort model label (`@agent_model`), `None` when never stamped. Presentation
    /// only (the watch table's optional model column); not part of the JSON row contract.
    pub model: Option<String>,
    /// The pane's working directory (`#{pane_current_path}`), `None` when tmux reports it empty.
    /// The repo/branch resolver reads it; `repo` below is its output.
    pub cwd: Option<String>,
    /// The resolved repo annotation (name/branch/worktree), `None` until resolved or when the pane's
    /// cwd is not a git repo. Set by `tma_runtime::repo::annotate_rows`.
    pub repo: Option<RepoLabel>,
}

impl AgentRow {
    /// The instant this row's episode last became noteworthy: its state transition, or the last
    /// turn end when a second completion landed inside an unchanged idle run. `wait --since`
    /// compares against this so a supervisor loop can see the NEXT completion on a pane that never
    /// left `idle` (`@agent_since` is write-once per state run and would pin it to the first).
    pub fn episode_at(&self) -> u64 {
        self.since.max(self.turn_at)
    }

    /// `session:window.pane` locator for surfaces and jump.
    pub fn locator(&self) -> String {
        format!("{}:{}.{}", self.session, self.window_index, self.pane_index)
    }

    /// The resolved branch label, or `None` when the pane's repo is unresolved.
    pub fn branch(&self) -> Option<&str> {
        self.repo.as_ref().map(|l| l.branch.as_str())
    }
}

/// Surface sort rank: attention-worthy states first.
pub fn sort_rank(state: AgentState) -> u8 {
    match state {
        AgentState::Blocked => 0,
        AgentState::Working => 1,
        AgentState::Idle => 2,
        AgentState::Unknown => 3,
    }
}

/// The one definition of "done": finished with output nobody has looked at yet. `@agent_state`
/// stays `idle` (the vocabulary is frozen); the attention flag is what makes it done. Every surface
/// that speaks of done — `wait --until done`, `--state done`, the JSON `done` key — routes here.
pub fn is_done(row: &AgentRow) -> bool {
    row.state == AgentState::Idle && row.attention
}

/// One state token in the selector vocabulary: a stored [`AgentState`], or the `done` pseudo-state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StateToken {
    /// One of the closed, stored `@agent_state` tokens.
    Closed(AgentState),
    /// idle + `@agent_attention`: finished, output unreviewed.
    Done,
}

impl StateToken {
    /// The tokens a user may write, for an error message naming the valid set.
    pub const VOCABULARY: &'static str = "idle, working, blocked, unknown, done";

    /// The same vocabulary as values, in the order [`StateToken::VOCABULARY`] prints them. The
    /// surfaces that enumerate rather than describe read this: `--state`/`--until` report it to
    /// clap as their possible values, which is what puts it in `--help` and in a generated shell
    /// completion script.
    pub const ALL: [StateToken; 5] = [
        StateToken::Closed(AgentState::Idle),
        StateToken::Closed(AgentState::Working),
        StateToken::Closed(AgentState::Blocked),
        StateToken::Closed(AgentState::Unknown),
        StateToken::Done,
    ];

    /// The row's one class in the DISJOINT reading of the vocabulary: `Done` for a finished,
    /// unreviewed pane, else its stored state. The inverse of [`StateToken::matches`], whose `idle`
    /// deliberately also covers a done pane; the surfaces that partition panes (the `tma status`
    /// counts, the `--events` edges) need each pane in exactly one class instead.
    pub fn of(row: &AgentRow) -> StateToken {
        if is_done(row) {
            StateToken::Done
        } else {
            StateToken::Closed(row.state)
        }
    }

    /// Whether `row` is in this state. `Closed` compares the raw stored token, so `idle` also
    /// matches a done pane; `Done` is the narrower [`is_done`].
    pub fn matches(self, row: &AgentRow) -> bool {
        match self {
            StateToken::Closed(state) => row.state == state,
            StateToken::Done => is_done(row),
        }
    }

    pub fn token(self) -> &'static str {
        match self {
            StateToken::Closed(state) => state.token(),
            StateToken::Done => "done",
        }
    }
}

impl FromStr for StateToken {
    type Err = GrammarError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "done" => Ok(StateToken::Done),
            other => other.parse::<AgentState>().map(StateToken::Closed),
        }
    }
}

/// Which agent rows a surface shows: the shared partition vocabulary behind `--session`, `--repo`,
/// `--branch`, `--agent`, and `--state`. Fields AND together; the multi-valued `state` ORs within
/// itself. An empty selector matches every row, so an unfiltered surface passes
/// [`Selector::default`] and behaves exactly as before.
///
/// Matching is exact string equality — no globbing, no case folding. `repo` and `branch` compare
/// against the row's resolved [`RepoLabel`] (the label surfaces render, so worktrees match their
/// origin's repo name), which means a pane whose cwd resolved to no repo matches neither.
///
/// The predicate is a *display* filter. Callers apply it to a finished cycle's rows; narrowing what
/// a cycle observes would stop refreshing the hidden panes.
#[derive(Clone, Debug, Default)]
pub struct Selector {
    pub session: Option<String>,
    pub repo: Option<String>,
    pub branch: Option<String>,
    pub agent: Option<String>,
    pub state: Vec<StateToken>,
}

impl Selector {
    /// Whether this selector narrows anything (no field set).
    pub fn is_empty(&self) -> bool {
        self.session.is_none()
            && self.repo.is_none()
            && self.branch.is_none()
            && self.agent.is_none()
            && self.state.is_empty()
    }

    /// Whether matching needs the resolved repo label, so a surface that skips the (bounded, but
    /// non-free) repo resolve knows when it must run it first.
    pub fn needs_repo(&self) -> bool {
        self.repo.is_some() || self.branch.is_some()
    }

    /// Whether `row` is in the selection.
    pub fn matches(&self, row: &AgentRow) -> bool {
        let eq = |want: &Option<String>, have: &str| want.as_ref().is_none_or(|w| w == have);
        eq(&self.session, &row.session)
            && eq(&self.agent, &row.agent)
            && eq(
                &self.repo,
                row.repo.as_ref().map(|l| l.name.as_str()).unwrap_or(""),
            )
            && eq(&self.branch, row.branch().unwrap_or(""))
            && (self.state.is_empty() || self.state.iter().any(|s| s.matches(row)))
    }

    /// Drop the rows outside the selection, in place.
    pub fn retain(&self, rows: &mut Vec<AgentRow>) {
        if self.is_empty() {
            return;
        }
        rows.retain(|r| self.matches(r));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(session: &str, agent: &str, state: AgentState) -> AgentRow {
        AgentRow {
            pane_id: "%1".to_string(),
            agent: agent.to_string(),
            state,
            detail: None,
            since: 0,
            turn_at: 0,
            session: session.to_string(),
            window_index: 0,
            pane_index: 0,
            title: String::new(),
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

    fn with_repo(mut r: AgentRow, name: &str, branch: &str) -> AgentRow {
        r.repo = Some(RepoLabel {
            name: name.to_string(),
            branch: branch.to_string(),
            worktree: false,
        });
        r
    }

    fn sel(f: impl FnOnce(&mut Selector)) -> Selector {
        let mut s = Selector::default();
        f(&mut s);
        s
    }

    #[test]
    fn empty_selector_matches_every_row() {
        let s = Selector::default();
        assert!(s.is_empty());
        assert!(s.matches(&row("work", "claude", AgentState::Idle)));
        assert!(s.matches(&row("other", "codex", AgentState::Blocked)));
    }

    #[test]
    fn each_field_matches_exactly() {
        let r = with_repo(row("work", "claude", AgentState::Working), "app", "main");
        assert!(sel(|s| s.session = Some("work".into())).matches(&r));
        assert!(!sel(|s| s.session = Some("wor".into())).matches(&r));
        assert!(sel(|s| s.agent = Some("claude".into())).matches(&r));
        assert!(!sel(|s| s.agent = Some("codex".into())).matches(&r));
        assert!(sel(|s| s.repo = Some("app".into())).matches(&r));
        assert!(!sel(|s| s.repo = Some("lib".into())).matches(&r));
        assert!(sel(|s| s.branch = Some("main".into())).matches(&r));
        assert!(!sel(|s| s.branch = Some("dev".into())).matches(&r));
        assert!(sel(|s| s.state = vec![StateToken::Closed(AgentState::Working)]).matches(&r));
        assert!(!sel(|s| s.state = vec![StateToken::Closed(AgentState::Idle)]).matches(&r));
    }

    /// A worktree carries its origin's repo name, so `--repo` selects the whole family and
    /// `--branch` splits it.
    #[test]
    fn repo_matches_the_rendered_label_across_worktrees() {
        let main = with_repo(row("a", "claude", AgentState::Idle), "app", "main");
        let mut tree = with_repo(row("b", "claude", AgentState::Idle), "app", "feature");
        tree.repo.as_mut().unwrap().worktree = true;
        let repo = sel(|s| s.repo = Some("app".into()));
        assert!(repo.matches(&main) && repo.matches(&tree));
        assert!(sel(|s| s.branch = Some("feature".into())).matches(&tree));
        assert!(!sel(|s| s.branch = Some("feature".into())).matches(&main));
    }

    /// An unresolved repo is not a wildcard: a repo/branch filter excludes it rather than letting
    /// every non-git pane through.
    #[test]
    fn unresolved_repo_matches_no_repo_or_branch_filter() {
        let bare = row("a", "claude", AgentState::Idle);
        assert!(!sel(|s| s.repo = Some("app".into())).matches(&bare));
        assert!(!sel(|s| s.branch = Some("main".into())).matches(&bare));
        assert!(Selector::default().matches(&bare));
    }

    #[test]
    fn fields_and_together_states_or_within() {
        let r = with_repo(row("work", "claude", AgentState::Blocked), "app", "main");
        let both = sel(|s| {
            s.session = Some("work".into());
            s.agent = Some("claude".into());
        });
        assert!(both.matches(&r));
        let one_wrong = sel(|s| {
            s.session = Some("work".into());
            s.agent = Some("codex".into());
        });
        assert!(!one_wrong.matches(&r), "fields AND");
        let states = sel(|s| {
            s.state = vec![
                StateToken::Closed(AgentState::Idle),
                StateToken::Closed(AgentState::Blocked),
            ]
        });
        assert!(states.matches(&r), "states OR");
    }

    /// `done` is idle + attention; the bare `idle` token is the broader set (it matches a done pane
    /// too), which is what `wait --until idle` has always meant.
    #[test]
    fn done_is_narrower_than_idle() {
        let mut done_row = row("a", "claude", AgentState::Idle);
        done_row.attention = true;
        let idle_row = row("a", "claude", AgentState::Idle);
        assert!(is_done(&done_row) && !is_done(&idle_row));

        let done = sel(|s| s.state = vec![StateToken::Done]);
        let idle = sel(|s| s.state = vec![StateToken::Closed(AgentState::Idle)]);
        assert!(done.matches(&done_row) && !done.matches(&idle_row));
        assert!(idle.matches(&done_row) && idle.matches(&idle_row));

        // Attention on a non-idle state is not done (a blocked pane also carries the flag).
        let mut blocked = row("a", "claude", AgentState::Blocked);
        blocked.attention = true;
        assert!(!is_done(&blocked) && !done.matches(&blocked));
    }

    /// `of` partitions: every row lands in exactly one token, with done split out of idle. The
    /// counterpart to `done_is_narrower_than_idle`, which pins the overlapping reading.
    #[test]
    fn of_assigns_one_disjoint_class_per_row() {
        let mut done_row = row("a", "claude", AgentState::Idle);
        done_row.attention = true;
        assert_eq!(StateToken::of(&done_row), StateToken::Done);
        assert_eq!(
            StateToken::of(&row("a", "claude", AgentState::Idle)),
            StateToken::Closed(AgentState::Idle)
        );
        // Attention on a non-idle state does not move the class.
        let mut blocked = row("a", "claude", AgentState::Blocked);
        blocked.attention = true;
        assert_eq!(
            StateToken::of(&blocked),
            StateToken::Closed(AgentState::Blocked)
        );
    }

    #[test]
    fn state_tokens_parse_and_round_trip() {
        assert_eq!(
            "done".parse::<StateToken>().unwrap(),
            StateToken::Done,
            "the pseudo-state parses"
        );
        assert_eq!(
            "blocked".parse::<StateToken>().unwrap(),
            StateToken::Closed(AgentState::Blocked)
        );
        assert!("running".parse::<StateToken>().is_err());
        for t in ["idle", "working", "blocked", "unknown", "done"] {
            assert_eq!(t.parse::<StateToken>().unwrap().token(), t);
            assert!(StateToken::VOCABULARY.contains(t), "vocabulary names {t}");
        }
    }

    /// `ALL` is what `--state`/`--until` hand clap as their possible values, and `VOCABULARY` is
    /// what the parse errors name. A state added to one and not the other would leave `--help` and
    /// the generated completion scripts disagreeing with the error message.
    #[test]
    fn state_token_all_matches_the_vocabulary_string() {
        let listed: Vec<&str> = StateToken::ALL.iter().map(|t| t.token()).collect();
        assert_eq!(listed.join(", "), StateToken::VOCABULARY);
    }

    #[test]
    fn needs_repo_only_for_the_repo_and_branch_fields() {
        assert!(!Selector::default().needs_repo());
        assert!(!sel(|s| s.session = Some("work".into())).needs_repo());
        assert!(sel(|s| s.repo = Some("app".into())).needs_repo());
        assert!(sel(|s| s.branch = Some("main".into())).needs_repo());
    }

    #[test]
    fn retain_keeps_only_matching_rows_and_is_a_noop_when_empty() {
        let mut rows = vec![
            row("work", "claude", AgentState::Blocked),
            row("home", "claude", AgentState::Idle),
            row("work", "codex", AgentState::Idle),
        ];
        Selector::default().retain(&mut rows);
        assert_eq!(rows.len(), 3, "an empty selector drops nothing");
        sel(|s| s.session = Some("work".into())).retain(&mut rows);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.session == "work"));
    }
}
