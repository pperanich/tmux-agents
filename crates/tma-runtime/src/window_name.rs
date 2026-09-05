//! State-derived tmux window names: the pure half of `[daemon] window_names`.
//!
//! Two pieces, both free of I/O. [`NameTemplate`] parses the configured `format` once (an unknown
//! token is a config error, not a literal that silently survives into every window name) and renders
//! it against one window's winning agent pane. [`decide`] answers what to do with a window from its
//! stored bookkeeping alone, so the rename/restore rules are testable without tmux.

use std::fmt;

use serde::Deserialize;

/// The `format` default: repo plus the window's highest-attention state. Short enough to leave room
/// for tmux's own `#I:` prefix in a status line.
pub const DEFAULT_FORMAT: &str = "{repo}:{state}";

/// Cap on a rendered name. A window name is drawn once per window in the status line, so an
/// unbounded branch name would push every other window off the row.
pub const NAME_MAX: usize = 64;

/// Characters that make a literal a *separator* between tokens. A literal made only of these is
/// dropped when the token it introduces renders empty, so `{repo}:{state}` on a pane outside a
/// checkout reads `idle`, not `:idle`. Only whole literals are tested, so a `-` inside a branch name
/// is never touched.
const SEPARATORS: &[char] = &[' ', '\t', ':', '/', '|', ',', '-', '_', '@', '.'];

/// One substitutable field. The vocabulary is closed: a token outside it fails config load.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Agent,
    State,
    Detail,
    Repo,
    Branch,
}

impl Field {
    fn parse(token: &str) -> Option<Field> {
        match token {
            "agent" => Some(Field::Agent),
            "state" => Some(Field::State),
            "detail" => Some(Field::Detail),
            "repo" => Some(Field::Repo),
            "branch" => Some(Field::Branch),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Segment {
    Literal(String),
    Field(Field),
}

/// Why a `format` string is not a template.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TemplateError {
    #[error(
        "unknown token `{{{0}}}`; window_names.format takes {{agent}}, {{state}}, {{detail}}, \
         {{repo}} and {{branch}}"
    )]
    UnknownToken(String),
    #[error("unterminated `{{` in window_names.format")]
    Unterminated,
}

/// A parsed `[daemon] window_names` format. Parsed at config load, so a bad token is reported once
/// with the file and key rather than per rename.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub struct NameTemplate {
    segments: Vec<Segment>,
    source: String,
}

impl Default for NameTemplate {
    fn default() -> NameTemplate {
        NameTemplate::parse(DEFAULT_FORMAT).expect("the default format parses")
    }
}

impl fmt::Display for NameTemplate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.source)
    }
}

impl TryFrom<String> for NameTemplate {
    type Error = TemplateError;

    fn try_from(value: String) -> Result<NameTemplate, TemplateError> {
        NameTemplate::parse(&value)
    }
}

impl NameTemplate {
    /// Parse `format`. `{` opens a token and must close with `}`; everything else is a literal.
    pub fn parse(format: &str) -> Result<NameTemplate, TemplateError> {
        let mut segments = Vec::new();
        let mut literal = String::new();
        let mut rest = format;
        while let Some(open) = rest.find('{') {
            literal.push_str(&rest[..open]);
            let after = &rest[open + 1..];
            let Some(close) = after.find('}') else {
                return Err(TemplateError::Unterminated);
            };
            let token = &after[..close];
            let Some(field) = Field::parse(token) else {
                return Err(TemplateError::UnknownToken(token.to_string()));
            };
            if !literal.is_empty() {
                segments.push(Segment::Literal(std::mem::take(&mut literal)));
            }
            segments.push(Segment::Field(field));
            rest = &after[close + 1..];
        }
        literal.push_str(rest);
        if !literal.is_empty() {
            segments.push(Segment::Literal(literal));
        }
        Ok(NameTemplate {
            segments,
            source: format.to_string(),
        })
    }

    /// Whether the template names `{repo}` or `{branch}`. A template that does not lets the pass
    /// skip [`crate::repo`] resolution entirely, so no `git rev-parse` is spawned for a format that
    /// could not use the answer.
    pub fn needs_repo(&self) -> bool {
        self.segments.iter().any(|s| {
            matches!(
                s,
                Segment::Field(Field::Repo) | Segment::Field(Field::Branch)
            )
        })
    }

    /// Render one window's name. An empty field contributes nothing and takes the separator literal
    /// that introduced it with it, so an unresolved repo or branch leaves no dangling punctuation.
    /// The result is stripped of control bytes and capped at [`NAME_MAX`]; it can be empty, which
    /// [`decide`] reads as "leave this window alone".
    pub fn render(&self, fields: &NameFields<'_>) -> String {
        let mut out = String::new();
        // A separator literal is held back until something non-empty follows it; a later separator
        // replaces a held one, so `{repo}/{branch}:{state}` with no branch renders `repo:state`.
        let mut pending: Option<&str> = None;
        for segment in &self.segments {
            let value = match segment {
                Segment::Literal(text) if is_separator(text) => {
                    pending = Some(text);
                    continue;
                }
                Segment::Literal(text) => text.as_str(),
                Segment::Field(field) => fields.get(*field),
            };
            if value.is_empty() {
                continue;
            }
            if let Some(sep) = pending.take() {
                if !out.is_empty() {
                    out.push_str(sep);
                }
            }
            out.push_str(value);
        }
        sanitize(&out)
    }
}

/// Whether a literal is made only of [`SEPARATORS`] (and is not empty).
fn is_separator(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|c| SEPARATORS.contains(&c))
}

/// Drop control bytes and cap the length. A repo directory or branch could in principle carry one,
/// and a window name goes straight into the status line every attached client redraws.
fn sanitize(name: &str) -> String {
    name.chars()
        .filter(|c| !c.is_control())
        .take(NAME_MAX)
        .collect::<String>()
        .trim()
        .to_string()
}

/// The substitution inputs for one window: its highest-attention agent pane. Every field is already
/// a plain string, so the renderer has no opinion about where they came from.
#[derive(Clone, Copy, Debug, Default)]
pub struct NameFields<'a> {
    pub agent: &'a str,
    pub state: &'a str,
    pub detail: &'a str,
    pub repo: &'a str,
    pub branch: &'a str,
}

impl<'a> NameFields<'a> {
    fn get(&self, field: Field) -> &'a str {
        match field {
            Field::Agent => self.agent,
            Field::State => self.state,
            Field::Detail => self.detail,
            Field::Repo => self.repo,
            Field::Branch => self.branch,
        }
    }
}

/// What the pass should do with one window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WindowNameAction {
    /// Nothing to write: no agent and nothing saved, the name is already right, or the user has
    /// renamed the window by hand.
    Leave,
    /// Rename to `name`. `save_original` marks the first rename, which also records the pre-tma
    /// name and `automatic-rename` value.
    Rename { name: String, save_original: bool },
    /// The last agent pane is gone: put the saved name and `automatic-rename` back and drop the
    /// bookkeeping options.
    Restore,
}

/// Decide one window's action from what is stored on it.
///
/// `desired` is the rendered name, `None` when the window holds no agent pane. `current` is the
/// window's name right now, `saved_original` whether tma has already recorded the pre-tma name, and
/// `last_written` the name tma wrote last (`@tma_window_name_last`).
///
/// Two rules keep tma out of the user's way. A `current` that differs from `last_written` means the
/// user renamed the window themselves, and tma leaves it alone until the next restore; a `desired`
/// equal to `last_written` writes nothing at all, which is what keeps a per-poll redraw off the
/// table.
pub fn decide(
    desired: Option<&str>,
    current: &str,
    saved_original: bool,
    last_written: Option<&str>,
) -> WindowNameAction {
    let desired = match desired {
        // The last agent pane is gone: restore only what tma actually took over.
        None if saved_original => return WindowNameAction::Restore,
        None => return WindowNameAction::Leave,
        // A format that rendered empty: an agent is still there, so this is not a restore.
        Some("") => return WindowNameAction::Leave,
        Some(d) => d,
    };
    if last_written.is_some_and(|last| last != current) {
        return WindowNameAction::Leave; // renamed by hand since tma last wrote
    }
    if last_written == Some(desired) {
        return WindowNameAction::Leave; // already what tma wants: no write, no redraw
    }
    WindowNameAction::Rename {
        name: desired.to_string(),
        save_original: !saved_original,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields<'a>(repo: &'a str, branch: &'a str, state: &'a str) -> NameFields<'a> {
        NameFields {
            agent: "claude",
            state,
            detail: "permission",
            repo,
            branch,
        }
    }

    #[test]
    fn every_token_substitutes() {
        let t = NameTemplate::parse("{agent} {state} {detail} {repo} {branch}").unwrap();
        assert_eq!(
            t.render(&fields("tma", "main", "blocked")),
            "claude blocked permission tma main"
        );
    }

    #[test]
    fn the_default_format_is_repo_and_state() {
        let t = NameTemplate::default();
        assert_eq!(t.to_string(), "{repo}:{state}");
        assert_eq!(t.render(&fields("tma", "main", "working")), "tma:working");
    }

    #[test]
    fn an_unknown_token_is_an_error() {
        assert_eq!(
            NameTemplate::parse("{repo}:{model}"),
            Err(TemplateError::UnknownToken("model".to_string()))
        );
        assert_eq!(
            NameTemplate::parse("{repo"),
            Err(TemplateError::Unterminated)
        );
    }

    #[test]
    fn an_empty_field_takes_its_separator_with_it() {
        let t = NameTemplate::parse("{repo}:{state}").unwrap();
        assert_eq!(t.render(&fields("", "main", "idle")), "idle");

        let t = NameTemplate::parse("{repo}/{branch}:{state}").unwrap();
        assert_eq!(t.render(&fields("tma", "", "blocked")), "tma:blocked");
        assert_eq!(t.render(&fields("", "", "blocked")), "blocked");
        assert_eq!(
            t.render(&fields("tma", "main", "blocked")),
            "tma/main:blocked"
        );
    }

    #[test]
    fn a_non_separator_literal_always_survives() {
        let t = NameTemplate::parse("[{repo}] {state}").unwrap();
        assert_eq!(t.render(&fields("", "main", "idle")), "[] idle");
    }

    #[test]
    fn a_rendered_name_is_bounded_and_free_of_control_bytes() {
        let t = NameTemplate::parse("{branch}").unwrap();
        let long = "x".repeat(NAME_MAX * 2);
        assert_eq!(t.render(&fields("tma", &long, "idle")).len(), NAME_MAX);
        assert_eq!(t.render(&fields("tma", "a\x1bb\nc", "idle")), "abc");
    }

    #[test]
    fn only_a_repo_or_branch_template_asks_for_git() {
        assert!(NameTemplate::parse("{repo}:{state}").unwrap().needs_repo());
        assert!(NameTemplate::parse("{branch}").unwrap().needs_repo());
        assert!(!NameTemplate::parse("{agent} {state}").unwrap().needs_repo());
    }

    #[test]
    fn a_format_with_no_tokens_renders_itself() {
        let t = NameTemplate::parse("agents").unwrap();
        assert_eq!(t.render(&fields("tma", "main", "idle")), "agents");
    }

    #[test]
    fn the_first_rename_saves_the_original() {
        assert_eq!(
            decide(Some("tma:working"), "shell", false, None),
            WindowNameAction::Rename {
                name: "tma:working".to_string(),
                save_original: true,
            }
        );
    }

    #[test]
    fn a_later_rename_keeps_the_saved_original() {
        assert_eq!(
            decide(
                Some("tma:blocked"),
                "tma:working",
                true,
                Some("tma:working")
            ),
            WindowNameAction::Rename {
                name: "tma:blocked".to_string(),
                save_original: false,
            }
        );
    }

    #[test]
    fn an_unchanged_name_writes_nothing() {
        assert_eq!(
            decide(
                Some("tma:working"),
                "tma:working",
                true,
                Some("tma:working")
            ),
            WindowNameAction::Leave
        );
    }

    #[test]
    fn a_hand_renamed_window_is_left_alone() {
        assert_eq!(
            decide(Some("tma:blocked"), "mine", true, Some("tma:working")),
            WindowNameAction::Leave
        );
    }

    #[test]
    fn the_last_agent_leaving_restores_only_what_tma_took_over() {
        assert_eq!(
            decide(None, "tma:idle", true, Some("tma:idle")),
            WindowNameAction::Restore
        );
        // Never touched by tma: no saved original, no agent pane, nothing to do.
        assert_eq!(decide(None, "shell", false, None), WindowNameAction::Leave);
    }

    #[test]
    fn a_format_that_renders_empty_leaves_the_window_alone() {
        assert_eq!(
            decide(Some(""), "tma:idle", true, None),
            WindowNameAction::Leave
        );
    }
}
