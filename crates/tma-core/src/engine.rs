//! The screen-rule engine: compile a [`Manifest`]'s matchers once, then evaluate a
//! [`PaneSnapshot`] into screen [`Evidence`] plus a per-rule match report for `tma debug
//! explain`. Pure — reads no clock; evidence timestamps are the snapshot's `captured_at`.
//! Regexes compile at [`RuleEngine::build`]; an invalid pattern is a build-time error.
//!
//! Evidence selection: a snapshot can match several rules, but the engine emits at most one
//! [`Evidence`] per [`AgentState`] (highest priority, ties by lower rule index), so the fold
//! gets one deterministic claim per state. A matched `skip_state_update` rule emits no evidence;
//! it raises [`Evaluation::history_view`]. Region semantics live on [`Region`].

use regex::Regex;

use crate::evidence::{Claim, Evidence, Source, StateClaim};
use crate::manifest::{Manifest, Matcher, Region};
use crate::snapshot::PaneSnapshot;
use crate::state::{AgentState, Detail};

/// A compiled screen-rule engine: the manifest's rules with their matchers compiled.
#[derive(Debug)]
pub struct RuleEngine {
    rules: Vec<CompiledRule>,
    /// Identity narrowing: the `[identity] title_patterns` regexes, compiled here so a bad
    /// pattern fails at build and the per-cycle resolver only does a cheap `is_match`.
    title_patterns: Vec<Regex>,
}

#[derive(Debug)]
struct CompiledRule {
    state: AgentState,
    detail: Option<Detail>,
    priority: i64,
    region: Region,
    skip_state_update: bool,
    matcher: CompiledMatcher,
}

/// A compiled matcher tree — mirrors [`Matcher`] with regexes compiled.
#[derive(Debug)]
enum CompiledMatcher {
    Contains(String),
    Regex(Regex),
    LineRegex(Regex),
    Any(Vec<CompiledMatcher>),
    All(Vec<CompiledMatcher>),
    Not(Box<CompiledMatcher>),
}

/// A build-time regex-compilation failure, naming the offending pattern's origin: a `[[rules]]`
/// matcher names its rule; a `[identity] title_patterns` entry names its index.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// A `[[rules]]` matcher regex failed to compile.
    #[error(
        "rule #{index} (state {state}, priority {priority}) has an invalid regex {pattern:?}: {source}"
    )]
    Rule {
        index: usize,
        state: AgentState,
        priority: i64,
        pattern: String,
        #[source]
        source: regex::Error,
    },
    /// An `[identity] title_patterns` regex failed to compile.
    #[error("[identity] title_patterns #{index} has an invalid regex {pattern:?}: {source}")]
    TitlePattern {
        index: usize,
        pattern: String,
        #[source]
        source: regex::Error,
    },
}

/// One rule's outcome against a snapshot, for `tma debug explain`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleReport {
    pub index: usize,
    pub state: AgentState,
    pub detail: Option<Detail>,
    pub priority: i64,
    pub region: Region,
    pub skip_state_update: bool,
    pub matched: bool,
}

/// The result of evaluating a snapshot against the engine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Evaluation {
    /// One deterministic screen claim per state (see module docs). Ordered by state.
    pub evidence: Vec<Evidence>,
    /// Every rule with its match outcome, in manifest order.
    pub reports: Vec<RuleReport>,
    /// A matched `skip_state_update` rule was seen — the pane shows history, freeze.
    pub history_view: bool,
}

impl RuleEngine {
    /// Compile a manifest's rules and identity title patterns. Fails if any regex is invalid,
    /// naming the rule or the title-pattern index.
    pub fn build(manifest: &Manifest) -> Result<RuleEngine, EngineError> {
        let mut rules = Vec::with_capacity(manifest.rules.len());
        for (index, rule) in manifest.rules.iter().enumerate() {
            let matcher = compile(&rule.match_).map_err(|(pattern, source)| EngineError::Rule {
                index,
                state: rule.state,
                priority: rule.priority,
                pattern,
                source,
            })?;
            rules.push(CompiledRule {
                state: rule.state,
                detail: rule.detail.clone(),
                priority: rule.priority,
                region: rule.region,
                skip_state_update: rule.skip_state_update,
                matcher,
            });
        }
        // Identity title patterns, compiled at the same build boundary as rule regexes.
        let mut title_patterns = Vec::with_capacity(manifest.identity.title_patterns.len());
        for (index, pat) in manifest.identity.title_patterns.iter().enumerate() {
            let re = Regex::new(pat).map_err(|source| EngineError::TitlePattern {
                index,
                pattern: pat.clone(),
                source,
            })?;
            title_patterns.push(re);
        }
        Ok(RuleEngine {
            rules,
            title_patterns,
        })
    }

    /// Whether this manifest declares any `[identity] title_patterns`. When true, a process-name
    /// match alone is not enough — the resolver also needs a title match or a stickiness hold.
    pub fn has_title_patterns(&self) -> bool {
        !self.title_patterns.is_empty()
    }

    /// Whether `title` matches any `[identity] title_patterns` regex. ANSI escapes are stripped
    /// first (like `Region::Title`); always `false` when no patterns are declared.
    pub fn title_matches(&self, title: &str) -> bool {
        if self.title_patterns.is_empty() {
            return false;
        }
        let text = strip_ansi(title);
        self.title_patterns.iter().any(|re| re.is_match(&text))
    }

    /// Evaluate a snapshot: match every rule, select one claim per state, flag history.
    pub fn evaluate(&self, snap: &PaneSnapshot) -> Evaluation {
        let mut reports = Vec::with_capacity(self.rules.len());
        let mut history_view = false;
        // (state, winning rule index): highest priority, lowest index on ties. A small linear
        // vec (states are ≤4) keeps `AgentState` free of an `Ord` derive.
        let mut best: Vec<(AgentState, usize)> = Vec::new();

        for (index, rule) in self.rules.iter().enumerate() {
            let region_text = extract_region(rule.region, snap);
            let matched = rule.matcher.matches(&region_text);
            reports.push(RuleReport {
                index,
                state: rule.state,
                detail: rule.detail.clone(),
                priority: rule.priority,
                region: rule.region,
                skip_state_update: rule.skip_state_update,
                matched,
            });
            if !matched {
                continue;
            }
            if rule.skip_state_update {
                history_view = true;
                continue;
            }
            match best.iter_mut().find(|(s, _)| *s == rule.state) {
                Some(entry) => {
                    if rule.priority > self.rules[entry.1].priority {
                        entry.1 = index;
                    }
                }
                None => best.push((rule.state, index)),
            }
        }

        // Deterministic evidence order regardless of rule declaration order.
        best.sort_by_key(|(state, _)| state.token());
        let evidence = best
            .iter()
            .map(|&(_, index)| {
                let rule = &self.rules[index];
                let source = match rule.region {
                    Region::Title => Source::Title,
                    Region::TailLines(_) | Region::BottomNonEmptyLines(_) | Region::Visible => {
                        Source::ScreenRule
                    }
                };
                Evidence {
                    source,
                    claim: Claim::State(StateClaim {
                        state: rule.state,
                        detail: rule.detail.clone(),
                    }),
                    at: snap.captured_at,
                    meta: format!(
                        "rule #{index} {} on {}",
                        rule.state,
                        region_label(rule.region)
                    ),
                }
            })
            .collect();

        Evaluation {
            evidence,
            reports,
            history_view,
        }
    }
}

/// Human-readable region label for explain output and evidence meta.
pub fn region_label(region: Region) -> String {
    match region {
        Region::Title => "title".to_string(),
        Region::Visible => "visible".to_string(),
        Region::TailLines(n) => format!("tail_lines({n})"),
        Region::BottomNonEmptyLines(n) => format!("bottom_non_empty_lines({n})"),
    }
}

/// Extract and clean the region text for matching. `capture-pane -e` interleaves ANSI escapes
/// with glyphs, which would split anchors like `❯ 1. Yes`; rules anchor on the text, so escapes
/// are stripped here (the snapshot's `tail_text` keeps them for the fixture record).
fn extract_region(region: Region, snap: &PaneSnapshot) -> String {
    match region {
        Region::Title => strip_ansi(&snap.title),
        Region::TailLines(n) => {
            let lines: Vec<&str> = snap.tail_text.lines().collect();
            let start = lines.len().saturating_sub(n);
            strip_ansi(&lines[start..].join("\n"))
        }
        // The same window as `TailLines`, re-anchored on the last line with content. Blankness is
        // tested PER LINE after stripping, because a visually empty row still carries its SGR
        // sequences (codex's composer background paints `ESC[48;2;…m` onto otherwise empty rows) —
        // testing the raw text would count those as content and defeat the whole region.
        Region::BottomNonEmptyLines(n) => {
            let lines: Vec<&str> = snap.tail_text.lines().collect();
            let Some(end) = lines.iter().rposition(|l| !strip_ansi(l).trim().is_empty()) else {
                return String::new();
            };
            let start = (end + 1).saturating_sub(n);
            strip_ansi(&lines[start..=end].join("\n"))
        }
        // The visible screen only — the last `visible_height` lines, clamping out the scrollback
        // that `capture-pane -S -50` reaches into. `None` height (synthetic) degrades to the whole tail.
        Region::Visible => {
            let lines: Vec<&str> = snap.tail_text.lines().collect();
            let start = match snap.visible_height {
                Some(h) => lines.len().saturating_sub(h as usize),
                None => 0,
            };
            strip_ansi(&lines[start..].join("\n"))
        }
    }
}

/// Remove ANSI control sequences (CSI `ESC[…<final>` and OSC `ESC]…(BEL|ESC\)`), leaving
/// the visible text. Pure; a plain byte-free string passes through unchanged.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            // CSI: ESC [ params/intermediates, terminated by a byte in 0x40..=0x7E.
            Some('[') => {
                chars.next();
                for f in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&f) {
                        break;
                    }
                }
            }
            // OSC: ESC ] ... terminated by BEL or ST (ESC \).
            Some(']') => {
                chars.next();
                while let Some(f) = chars.next() {
                    if f == '\u{07}' {
                        break;
                    }
                    if f == '\u{1b}' {
                        if matches!(chars.peek(), Some('\\')) {
                            chars.next();
                        }
                        break;
                    }
                }
            }
            // Other ESC-prefixed sequences (e.g. ESC ( charset): drop ESC and the next byte.
            Some(_) => {
                chars.next();
            }
            None => {}
        }
    }
    out
}

impl CompiledMatcher {
    fn matches(&self, text: &str) -> bool {
        match self {
            CompiledMatcher::Contains(s) => text.contains(s.as_str()),
            CompiledMatcher::Regex(re) => re.is_match(text),
            CompiledMatcher::LineRegex(re) => text.lines().any(|line| re.is_match(line)),
            CompiledMatcher::Any(v) => v.iter().any(|m| m.matches(text)),
            CompiledMatcher::All(v) => v.iter().all(|m| m.matches(text)),
            CompiledMatcher::Not(m) => !m.matches(text),
        }
    }
}

/// Compile a matcher, returning the failing pattern alongside the regex error so the
/// build-time error can name the exact leaf that failed (not just the whole rule).
fn compile(matcher: &Matcher) -> Result<CompiledMatcher, (String, regex::Error)> {
    Ok(match matcher {
        Matcher::Contains(s) => CompiledMatcher::Contains(s.clone()),
        Matcher::Regex(s) => CompiledMatcher::Regex(Regex::new(s).map_err(|e| (s.clone(), e))?),
        Matcher::LineRegex(s) => {
            CompiledMatcher::LineRegex(Regex::new(s).map_err(|e| (s.clone(), e))?)
        }
        Matcher::Any(v) => CompiledMatcher::Any(compile_all(v)?),
        Matcher::All(v) => CompiledMatcher::All(compile_all(v)?),
        Matcher::Not(b) => CompiledMatcher::Not(Box::new(compile(b)?)),
    })
}

fn compile_all(v: &[Matcher]) -> Result<Vec<CompiledMatcher>, (String, regex::Error)> {
    v.iter().map(compile).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(rules: &str) -> Manifest {
        let src = format!(
            "min_engine_version = \"0.1\"\n[identity]\nprocess_names=[\"x\"]\n[capture]\nvisible=[\"working\",\"idle\",\"blocked\"]\n{rules}"
        );
        Manifest::parse(&src, "t.toml").unwrap()
    }

    fn snap(title: &str, tail: &str) -> PaneSnapshot {
        PaneSnapshot {
            pane_id: "%1".to_string(),
            pid_tree: vec![],
            title: title.to_string(),
            tail_text: tail.to_string(),
            tail_hash: 0,
            alternate_on: true,
            scroll_position: None,
            visible_height: None,
            captured_at: 1000,
        }
    }

    fn snap_h(title: &str, tail: &str, height: Option<u32>) -> PaneSnapshot {
        PaneSnapshot {
            visible_height: height,
            ..snap(title, tail)
        }
    }

    #[test]
    fn contains_and_title_regions() {
        let m = manifest(
            "[[rules]]\nstate=\"idle\"\nregion=\"title\"\nmatch={ contains=\"✳\" }\n\
             [[rules]]\nstate=\"blocked\"\nregion=\"tail_lines(5)\"\nmatch={ contains=\"Do you want to proceed?\" }\n",
        );
        let eng = RuleEngine::build(&m).unwrap();
        let ev = eng.evaluate(&snap("✳ task", "line\nDo you want to proceed?\n"));
        // Both rules matched → one idle (title) + one blocked (screen) claim.
        assert_eq!(ev.evidence.len(), 2);
        assert!(ev.evidence.iter().any(|e| e.source == Source::Title
            && matches!(&e.claim, Claim::State(s) if s.state == AgentState::Idle)));
        assert!(ev.evidence.iter().any(|e| e.source == Source::ScreenRule
            && matches!(&e.claim, Claim::State(s) if s.state == AgentState::Blocked)));
        assert!(!ev.history_view);
    }

    #[test]
    fn tail_lines_scopes_to_last_n_lines() {
        // "target" is on line 1 of 4; tail_lines(2) only sees lines 3–4, so no match.
        let m = manifest(
            "[[rules]]\nstate=\"idle\"\nregion=\"tail_lines(2)\"\nmatch={ contains=\"target\" }\n",
        );
        let eng = RuleEngine::build(&m).unwrap();
        let ev = eng.evaluate(&snap("t", "target\nb\nc\nd\n"));
        assert!(ev.evidence.is_empty());
        assert!(!ev.reports[0].matched);
    }

    #[test]
    fn bottom_non_empty_lines_skips_trailing_blanks() {
        // The codex fresh-session shape: the composer sits above a screen the agent has not
        // filled. `tail_lines(2)` reads two of the blanks and misses it; the re-anchored window
        // does not.
        let tail = "composer\n\n\n\n";
        let missed = manifest(
            "[[rules]]\nstate=\"idle\"\nregion=\"tail_lines(2)\"\nmatch={ contains=\"composer\" }\n",
        );
        let found = manifest(
            "[[rules]]\nstate=\"idle\"\nregion=\"bottom_non_empty_lines(2)\"\nmatch={ contains=\"composer\" }\n",
        );
        assert!(RuleEngine::build(&missed)
            .unwrap()
            .evaluate(&snap("t", tail))
            .evidence
            .is_empty());
        assert_eq!(
            RuleEngine::build(&found)
                .unwrap()
                .evaluate(&snap("t", tail))
                .evidence
                .len(),
            1
        );
    }

    #[test]
    fn bottom_non_empty_lines_treats_an_ansi_only_line_as_blank() {
        // Codex paints its composer background onto empty rows, so a row that shows nothing still
        // carries SGR bytes. If blankness were tested before stripping, that row would count as
        // content and eat one of the two slots the anchor line needs.
        let m = manifest(
            "[[rules]]\nstate=\"idle\"\nregion=\"bottom_non_empty_lines(2)\"\nmatch={ contains=\"composer\" }\n",
        );
        let eng = RuleEngine::build(&m).unwrap();
        let tail = "composer\n\u{1b}[38;5;246m\u{1b}[39m\n\u{1b}[48;2;57;57;71m\n";
        assert_eq!(eng.evaluate(&snap("t", tail)).evidence.len(), 1);
    }

    #[test]
    fn bottom_non_empty_lines_handles_all_blank_and_oversized_windows() {
        let m = manifest(
            "[[rules]]\nstate=\"idle\"\nregion=\"bottom_non_empty_lines(50)\"\nmatch={ contains=\"composer\" }\n",
        );
        let eng = RuleEngine::build(&m).unwrap();
        // Nothing but blanks: an empty region, not a panic and not a match.
        assert!(eng
            .evaluate(&snap("t", "\n   \n\u{1b}[0m\n"))
            .evidence
            .is_empty());
        // A window wider than the capture clamps to the whole capture.
        assert_eq!(eng.evaluate(&snap("t", "composer\n\n")).evidence.len(), 1);
        // A zero window is empty, as `tail_lines(0)` is.
        let zero = manifest(
            "[[rules]]\nstate=\"idle\"\nregion=\"bottom_non_empty_lines(0)\"\nmatch={ contains=\"composer\" }\n",
        );
        assert!(RuleEngine::build(&zero)
            .unwrap()
            .evaluate(&snap("t", "composer\n\n"))
            .evidence
            .is_empty());
    }

    #[test]
    fn bottom_non_empty_lines_window_ends_at_the_last_content_line() {
        // The window is re-anchored, not widened: content further up than `n` lines above the
        // anchor is still out of scope, which is what keeps the codex transcript echoes excluded.
        let m = manifest(
            "[[rules]]\nstate=\"idle\"\nregion=\"bottom_non_empty_lines(2)\"\nmatch={ contains=\"echo\" }\n",
        );
        let eng = RuleEngine::build(&m).unwrap();
        let ev = eng.evaluate(&snap("t", "echo\nfiller\ncomposer\n\n\n"));
        assert!(ev.evidence.is_empty());
        assert!(!ev.reports[0].matched);
    }

    #[test]
    fn visible_region_clamps_to_pane_height_excluding_scrollback() {
        // A 12-row visible pane whose `-S -50` capture reaches into scrollback. A prior
        // turn's `Working...` sits in the scrollback portion (line 1); the visible screen (the
        // last 12 lines) shows only an idle composer. `visible` must scope to the last 12 lines
        // and NOT match the scrollback anchor — where a whole-screen `tail_lines(40)` would.
        let m = manifest(
            "[[rules]]\nstate=\"working\"\nregion=\"visible\"\nmatch={ contains=\"Working...\" }\n",
        );
        let eng = RuleEngine::build(&m).unwrap();
        let mut lines = vec!["Working... (prior turn, in scrollback)"];
        lines.extend(vec!["idle composer row"; 12]);
        let tail = format!("{}\n", lines.join("\n"));

        // Clamped to the 12 visible rows: the scrollback `Working...` is out of scope.
        assert!(
            eng.evaluate(&snap_h("t", &tail, Some(12)))
                .evidence
                .is_empty(),
            "visible region must not match prior-turn chrome sitting in scrollback"
        );
        // Control: without the clamp (height unknown ⇒ whole tail) the same anchor DOES match,
        // proving the fixture actually exercises the scrollback leak the clamp closes.
        assert_eq!(
            eng.evaluate(&snap_h("t", &tail, None)).evidence.len(),
            1,
            "the leak is real: unclamped evaluation matches the scrollback anchor"
        );
    }

    #[test]
    fn line_regex_matches_per_line() {
        // A whole-region regex with ^ anchor only matches because line_regex applies it
        // per line; a plain regex against the joined text would not (line 2 is not start).
        let m = manifest(
            "[[rules]]\nstate=\"working\"\nregion=\"tail_lines(5)\"\nmatch={ line_regex=\"^⏵⏵ \" }\n",
        );
        let eng = RuleEngine::build(&m).unwrap();
        let ev = eng.evaluate(&snap("t", "prompt\n⏵⏵ bypass permissions on\n"));
        assert_eq!(ev.evidence.len(), 1);
        assert_eq!(ev.evidence[0].source, Source::ScreenRule);
    }

    #[test]
    fn highest_priority_wins_per_state() {
        let m = manifest(
            "[[rules]]\nstate=\"idle\"\npriority=10\nregion=\"tail_lines(5)\"\nmatch={ contains=\"a\" }\n\
             [[rules]]\nstate=\"idle\"\npriority=100\ndetail=\"background\"\nregion=\"tail_lines(5)\"\nmatch={ contains=\"a\" }\n",
        );
        let eng = RuleEngine::build(&m).unwrap();
        let ev = eng.evaluate(&snap("t", "a\n"));
        assert_eq!(ev.evidence.len(), 1);
        assert_eq!(
            ev.evidence[0].claim,
            Claim::State(StateClaim {
                state: AgentState::Idle,
                detail: Some(Detail::new("background")),
            })
        );
    }

    #[test]
    fn skip_state_update_raises_history_view_and_emits_no_evidence() {
        let m = manifest(
            "[[rules]]\nstate=\"idle\"\nregion=\"tail_lines(50)\"\nskip_state_update=true\nmatch={ contains=\"transcript\" }\n",
        );
        let eng = RuleEngine::build(&m).unwrap();
        let ev = eng.evaluate(&snap("t", "transcript viewer\n"));
        assert!(ev.history_view);
        assert!(ev.evidence.is_empty());
        assert!(ev.reports[0].matched);
    }

    #[test]
    fn strips_ansi_so_split_anchors_match() {
        // The `-e` capture interleaves escapes: `❯[39m [38;5;246m1. [38;5;153mYes`.
        let raw = "\u{1b}[38;5;153m❯\u{1b}[39m \u{1b}[38;5;246m1. \u{1b}[38;5;153mYes\u{1b}[39m";
        assert_eq!(strip_ansi(raw), "❯ 1. Yes");
        // OSC hyperlink sequence removed too.
        let osc = "a\u{1b}]8;id=1;https://x\u{1b}\\b";
        assert_eq!(strip_ansi(osc), "ab");
    }

    #[test]
    fn rule_matches_across_escape_split_text() {
        let m = manifest(
            "[[rules]]\nstate=\"blocked\"\nregion=\"tail_lines(5)\"\nmatch={ regex=\"❯ 1\\\\. Yes\" }\n",
        );
        let eng = RuleEngine::build(&m).unwrap();
        let tail = "\u{1b}[38;5;153m❯\u{1b}[39m \u{1b}[38;5;246m1. \u{1b}[38;5;153mYes\u{1b}[39m\n";
        assert_eq!(eng.evaluate(&snap("t", tail)).evidence.len(), 1);
    }

    #[test]
    fn not_and_all_compose() {
        let m = manifest(
            "[[rules]]\nstate=\"idle\"\nregion=\"tail_lines(50)\"\nmatch={ all=[{contains=\"prompt\"},{not={contains=\"error\"}}] }\n",
        );
        let eng = RuleEngine::build(&m).unwrap();
        assert_eq!(eng.evaluate(&snap("t", "prompt here\n")).evidence.len(), 1);
        assert_eq!(eng.evaluate(&snap("t", "prompt error\n")).evidence.len(), 0);
    }

    #[test]
    fn invalid_regex_is_named_build_error() {
        let m = manifest(
            "[[rules]]\nstate=\"blocked\"\npriority=7\nregion=\"tail_lines(5)\"\nmatch={ regex=\"(unclosed\" }\n",
        );
        let err = RuleEngine::build(&m).unwrap_err();
        match err {
            EngineError::Rule {
                index,
                state,
                pattern,
                ..
            } => {
                assert_eq!(index, 0);
                assert_eq!(state, AgentState::Blocked);
                assert_eq!(pattern, "(unclosed");
            }
            other => panic!("expected Rule error, got {other:?}"),
        }
    }

    #[test]
    fn nested_invalid_regex_names_the_leaf_pattern() {
        let m = manifest(
            "[[rules]]\nstate=\"blocked\"\nregion=\"tail_lines(5)\"\nmatch={ any=[{contains=\"ok\"},{regex=\"[bad\"}] }\n",
        );
        let err = RuleEngine::build(&m).unwrap_err();
        match err {
            EngineError::Rule { pattern, .. } => assert_eq!(pattern, "[bad"),
            other => panic!("expected Rule error, got {other:?}"),
        }
    }

    // ---- identity title patterns ------------------------------------------------

    fn manifest_with_title_patterns(patterns: &str) -> Manifest {
        let src = format!(
            "min_engine_version = \"0.1\"\n[identity]\nprocess_names=[\"node\"]\ntitle_patterns=[{patterns}]\n[capture]\nvisible=[]\n"
        );
        Manifest::parse(&src, "t.toml").unwrap()
    }

    #[test]
    fn title_patterns_absent_never_match() {
        // A manifest with no title_patterns reports none and matches no title.
        let eng = RuleEngine::build(&manifest("")).unwrap();
        assert!(!eng.has_title_patterns());
        assert!(!eng.title_matches("Cursor Agent"));
        assert!(!eng.title_matches(""));
    }

    #[test]
    fn title_patterns_match_after_ansi_strip() {
        let eng = RuleEngine::build(&manifest_with_title_patterns("\"^Cursor Agent$\"")).unwrap();
        assert!(eng.has_title_patterns());
        assert!(eng.title_matches("Cursor Agent"));
        // An OSC/CSI-wrapped title still matches (styling stripped, like Region::Title).
        assert!(eng.title_matches("\u{1b}[1mCursor Agent\u{1b}[0m"));
        // A tool-name title (the flicker) does not match — stickiness, not this predicate,
        // holds identity across it.
        assert!(!eng.title_matches("Shell Command Output"));
    }

    #[test]
    fn invalid_title_pattern_is_named_build_error() {
        let m = manifest_with_title_patterns("\"(unclosed\"");
        match RuleEngine::build(&m).unwrap_err() {
            EngineError::TitlePattern { index, pattern, .. } => {
                assert_eq!(index, 0);
                assert_eq!(pattern, "(unclosed");
            }
            other => panic!("expected TitlePattern error, got {other:?}"),
        }
    }
}
