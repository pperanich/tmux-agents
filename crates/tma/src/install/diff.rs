//! Unified, optionally colorized config diffs for `install-hooks`. `print_diff`'s renderer:
//! `similar` line-diffs `old` vs `new`, groups changes into hunks with 3 context lines, and
//! formats git-style `@@ -a,b +c,d @@` headers with word-level emphasis inside replaced lines.

use similar::udiff::UnifiedHunkHeader;
use similar::{ChangeTag, TextDiff};

// Hand-rolled ANSI escapes (no color crate): hunk headers cyan, deletions red, insertions
// green, intra-line emphasis bold. RESET clears color; UNBOLD drops just the bold weight.
const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const CYAN: &str = "\x1b[36m";
const BOLD: &str = "\x1b[1m";
const UNBOLD: &str = "\x1b[22m";
const RESET: &str = "\x1b[0m";

/// Render a unified diff of `old` vs `new` with 3 context lines per hunk. When `color`, hunk
/// headers are cyan, deletions red, insertions green, and word-level changes within replaced
/// lines are bold. Every emitted line carries a two-space indent (`  -old` / `  +new` /
/// `   context` / `  @@ ... @@`) and exactly one trailing newline. Returns "" when identical.
pub(super) fn render_diff(old: &str, new: &str, color: bool) -> String {
    let diff = TextDiff::from_lines(old, new);
    let mut out = String::new();
    for group in diff.grouped_ops(3) {
        let header = UnifiedHunkHeader::new(&group).to_string();
        if color {
            out.push_str(&format!("  {CYAN}{header}{RESET}\n"));
        } else {
            out.push_str(&format!("  {header}\n"));
        }
        for op in &group {
            for change in diff.iter_inline_changes(op) {
                let (sign, line_color) = match change.tag() {
                    ChangeTag::Delete => ('-', RED),
                    ChangeTag::Insert => ('+', GREEN),
                    ChangeTag::Equal => (' ', ""),
                };
                out.push_str("  ");
                out.push(sign);
                let paint = color && !line_color.is_empty();
                if paint {
                    out.push_str(line_color);
                }
                for (emphasized, value) in change.iter_strings_lossy() {
                    let value = strip_eol(&value);
                    if paint && emphasized {
                        out.push_str(BOLD);
                        out.push_str(value);
                        out.push_str(UNBOLD);
                    } else {
                        out.push_str(value);
                    }
                }
                if paint {
                    out.push_str(RESET);
                }
                out.push('\n');
            }
        }
    }
    out
}

/// Strip a single trailing line terminator (`\n` or `\r\n`); the renderer re-adds exactly one,
/// so `similar`'s per-line values never double or drop the newline.
fn strip_eol(value: &str) -> &str {
    match value.strip_suffix('\n') {
        Some(v) => v.strip_suffix('\r').unwrap_or(v),
        None => value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A ten-line JSON-ish doc; distinct lines so hunk boundaries are unambiguous.
    fn doc(lines: &[&str]) -> String {
        let mut s = lines.join("\n");
        s.push('\n');
        s
    }

    #[test]
    fn single_change_is_one_hunk_with_three_context_lines() {
        let old = doc(&[
            "{",
            "  \"a\": 1,",
            "  \"b\": 2,",
            "  \"c\": 3,",
            "  \"d\": 4,",
            "  \"e\": 5,",
            "  \"f\": 6,",
            "  \"g\": 7,",
            "  \"h\": 8",
            "}",
        ]);
        let new = old.replace("  \"e\": 5,", "  \"e\": 50,");
        let out = render_diff(&old, &new, false);
        // Exactly one hunk. The change sits at line 6, so 3 context lines flank it each side.
        assert_eq!(out.matches("@@ -").count(), 1, "{out}");
        assert!(out.contains("  @@ -3,7 +3,7 @@\n"), "{out}");
        assert!(out.contains("  -  \"e\": 5,\n"), "{out}");
        assert!(out.contains("  +  \"e\": 50,\n"), "{out}");
        for ctx in ["   \"b\": 2,", "   \"c\": 3,", "   \"d\": 4,"] {
            assert!(out.contains(ctx), "missing leading context {ctx}: {out}");
        }
        for ctx in ["   \"f\": 6,", "   \"g\": 7,", "   \"h\": 8"] {
            assert!(out.contains(ctx), "missing trailing context {ctx}: {out}");
        }
        // Lines beyond the 3-line radius are trimmed out.
        assert!(!out.contains("\"a\": 1,"), "{out}");
    }

    #[test]
    fn two_distant_changes_are_two_hunks() {
        let lines: Vec<String> = (0..20).map(|i| format!("line{i}")).collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let old = doc(&refs);
        let new = old
            .replace("line2\n", "line2x\n")
            .replace("line17\n", "line17x\n");
        let out = render_diff(&old, &new, false);
        assert_eq!(out.matches("@@ -").count(), 2, "{out}");
    }

    #[test]
    fn identical_inputs_render_empty() {
        let s = doc(&["one", "two", "three"]);
        assert_eq!(render_diff(&s, &s, false), "");
        assert_eq!(render_diff(&s, &s, true), "");
    }

    #[test]
    fn end_insertion_without_trailing_newline_is_sane() {
        let old = "a\nb\n";
        let new = "a\nb\nc"; // new final line has no terminator
        let out = render_diff(old, new, false);
        assert!(out.contains("  +c\n"), "{out}");
        // Every line ends in exactly one newline: no blank (doubled) lines.
        assert!(!out.contains("\n\n"), "{out}");
    }

    #[test]
    fn color_wraps_deletions_in_red_and_resets() {
        let old = doc(&["keep", "drop", "keep2"]);
        let new = doc(&["keep", "changed", "keep2"]);
        let out = render_diff(&old, &new, true);
        assert!(out.contains(RED), "no red escape: {out:?}");
        assert!(out.contains(GREEN), "no green escape: {out:?}");
        assert!(out.contains(CYAN), "no cyan header: {out:?}");
        assert!(out.contains(RESET), "no reset: {out:?}");
        // The reset closes the deletion line before its newline.
        assert!(out.contains(&format!("{RESET}\n")), "{out:?}");
    }

    #[test]
    fn inline_emphasis_bolds_only_the_changed_word() {
        let old = doc(&["hello world"]);
        let new = doc(&["hello there"]);
        let out = render_diff(&old, &new, true);
        // The unchanged "hello " stays plain; only the differing word is bolded.
        assert!(
            out.contains(&format!("{BOLD}world{UNBOLD}")),
            "delete emphasis: {out:?}"
        );
        assert!(
            out.contains(&format!("{BOLD}there{UNBOLD}")),
            "insert emphasis: {out:?}"
        );
        assert!(
            !out.contains(&format!("{BOLD}hello")),
            "context should not bold: {out:?}"
        );
    }
}
