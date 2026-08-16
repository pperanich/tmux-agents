//! Fixture redaction: strip paths, emails, and user-supplied secrets from a capture before it
//! enters the repo. Width-preserving: each redacted span becomes a placeholder of the same terminal
//! display-column width (by display width, not char count, so a wide CJK token does not shrink the
//! line), keeping box-drawing chrome and column alignment intact. Deterministic, so redacting twice
//! is byte-identical. Built-in path/email detectors are hand-rolled scanners; user `--pattern` flags
//! are repeatable regexes compiled with the `regex` crate.

use regex::Regex;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// An invalid `--pattern` regex, naming the offending pattern so the caller can report it.
#[derive(Debug, thiserror::Error)]
#[error("invalid --pattern regex {pattern:?}: {source}")]
pub(crate) struct RedactError {
    pattern: String,
    #[source]
    source: regex::Error,
}

/// Redact `input`: replace each user `pattern` (a regex), then scan for paths and emails. Line
/// display widths and non-token chars are preserved. Errors if a `pattern` is not a valid regex.
pub(crate) fn redact(input: &str, patterns: &[String]) -> Result<String, RedactError> {
    let compiled = patterns
        .iter()
        .map(|p| {
            Regex::new(p).map_err(|source| RedactError {
                pattern: p.clone(),
                source,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut s = input.to_string();
    for re in &compiled {
        s = redact_regex(&s, re);
    }
    Ok(redact_runs(&s))
}

/// A width-preserving placeholder: `[category]` left-anchored, padded (or truncated) to
/// exactly `width` terminal display columns with `x` (each `x` is one column).
fn placeholder(category: &str, width: usize) -> String {
    let label = format!("[{category}]");
    let mut out = String::new();
    let mut cols = 0;
    for c in label.chars() {
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        if cols + cw > width {
            break;
        }
        out.push(c);
        cols += cw;
    }
    while cols < width {
        out.push('x');
        cols += 1;
    }
    out
}

/// Replace every regex match with a display-width-preserving `[redacted]` placeholder. `find_iter`
/// walks non-overlapping matches left to right, so a placeholder is never re-scanned.
fn redact_regex(s: &str, re: &Regex) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last = 0;
    for m in re.find_iter(s) {
        out.push_str(&s[last..m.start()]);
        out.push_str(&placeholder("redacted", m.as_str().width()));
        last = m.end();
    }
    out.push_str(&s[last..]);
    out
}

/// Whether `c` can be part of a path/email token. Unicode `is_alphanumeric` plus
/// [`is_combining_mark`] keep both NFC and NFD accented text (`é` = U+00E9, or `e` + U+0301) as one
/// token, so a decomposed (macOS) username's tail cannot split off and leak past the detector.
/// Excludes `:` and whitespace so `key: value` and box chrome split cleanly.
fn is_token_char(c: char) -> bool {
    c.is_alphanumeric()
        || is_combining_mark(c)
        || matches!(c, '.' | '_' | '/' | '@' | '~' | '+' | '-')
}

/// Whether `c` is a Unicode combining mark in one of the common blocks. Explicit code-point ranges
/// (not a full category table) keep the module dependency-free; coverage is honest-but-partial (the
/// Latin/Cyrillic/symbol diacritics that appear in terminal captures, not every historic mark).
/// Combining marks are zero-width, so keeping them in a token does not disturb placeholder sizing.
fn is_combining_mark(c: char) -> bool {
    matches!(c as u32,
        0x0300..=0x036F   // Combining Diacritical Marks (NFD accents: e + U+0301, etc.)
        | 0x0483..=0x0489 // Cyrillic combining marks
        | 0x1AB0..=0x1AFF // Combining Diacritical Marks Extended
        | 0x1DC0..=0x1DFF // Combining Diacritical Marks Supplement
        | 0x20D0..=0x20FF // Combining Diacritical Marks for Symbols
        | 0xFE20..=0xFE2F // Combining Half Marks
    )
}

/// Scan `s` for path/email tokens and redact them, leaving all other characters
/// (whitespace, box-drawing chrome, punctuation) untouched.
fn redact_runs(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        if is_token_char(chars[i]) {
            let start = i;
            while i < chars.len() && is_token_char(chars[i]) {
                i += 1;
            }
            let token: String = chars[start..i].iter().collect();
            match classify(&token) {
                Some(category) => out.push_str(&placeholder(category, token.as_str().width())),
                None => out.push_str(&token),
            }
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// Classify a token as an email or a path, or `None` to leave it alone.
fn classify(token: &str) -> Option<&'static str> {
    if is_email(token) {
        Some("email")
    } else if is_path(token) {
        Some("path")
    } else {
        None
    }
}

fn is_email(token: &str) -> bool {
    if token.matches('@').count() != 1 {
        return false;
    }
    let (local, domain) = token.split_once('@').expect("one @ present");
    if local.is_empty() || domain.is_empty() {
        return false;
    }
    match domain.rsplit_once('.') {
        Some((host, tld)) => {
            !host.is_empty() && tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic())
        }
        None => false,
    }
}

fn is_path(token: &str) -> bool {
    (token.starts_with('/') && token.len() > 1) || token.starts_with("~/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn golden_paths_and_emails() {
        let input = "line: /Users/alice/proj and mail alice@example.com ok";
        let expected = "line: [path]xxxxxxxxxxx and mail [email]xxxxxxxxxx ok";
        assert_eq!(redact(input, &[]).unwrap(), expected);
    }

    #[test]
    fn preserves_line_width() {
        let input = "│ ~/work/agent.log  bob@corp.io │";
        let out = redact(input, &[]).unwrap();
        assert_eq!(out.width(), input.width());
    }

    #[test]
    fn is_deterministic_and_idempotent() {
        let input = "run /var/log/x.txt then /var/log/y.txt for dev@ex.org";
        let once = redact(input, &[]).unwrap();
        assert_eq!(redact(input, &[]).unwrap(), once, "not deterministic");
        assert_eq!(redact(&once, &[]).unwrap(), once, "not idempotent");
    }

    #[test]
    fn leaves_chrome_untouched() {
        // A pure-chrome line with no path/email tokens must pass through verbatim.
        let chrome = "╭──────────╮\n│ ❯        │\n⏵⏵ bypass permissions on (shift+tab)";
        assert_eq!(redact(chrome, &[]).unwrap(), chrome);
    }

    #[test]
    fn redacts_user_pattern() {
        // A pattern with no regex metacharacters behaves like the old literal match.
        let input = "token=hunter2 and again hunter2";
        let out = redact(input, &["hunter2".to_string()]).unwrap();
        // "hunter2" is 7 columns → "[redact" (truncated to width 7).
        assert_eq!(out, "token=[redact and again [redact");
        assert_eq!(out.width(), input.width());
    }

    #[test]
    fn redacts_variable_shaped_secrets_with_one_regex() {
        // Finding 2: one regex redacts multiple different-valued API keys in a pass.
        let input = "key sk-AAA111 and sk-BBB222 done";
        let out = redact(input, &["sk-[A-Za-z0-9]+".to_string()]).unwrap();
        assert!(!out.contains("sk-AAA111"), "first key leaked: {out}");
        assert!(!out.contains("sk-BBB222"), "second key leaked: {out}");
        assert!(out.contains("[redact"), "no placeholder: {out}");
    }

    #[test]
    fn invalid_pattern_errors_and_names_it() {
        // Finding 2: a bad regex is reported, never silently ignored.
        let err = redact("anything", &["sk-[".to_string()]).unwrap_err();
        assert!(
            err.to_string().contains("sk-["),
            "error omits pattern: {err}"
        );
    }

    #[test]
    fn redacts_non_ascii_path_and_email() {
        // Finding 1: an accented path tail and an accented email local part must not leak.
        let input = "path /Users/alice/Résumé-Draft.txt and mail josé@example.com end";
        let out = redact(input, &[]).unwrap();
        assert!(!out.contains("Résumé"), "path tail leaked: {out}");
        assert!(!out.contains("josé"), "email leaked: {out}");
        assert!(out.contains("[path]"), "path not redacted: {out}");
        assert!(out.contains("[email]"), "email not redacted: {out}");
    }

    #[test]
    fn redacts_nfd_decomposed_email() {
        // macOS captures are commonly NFD, so `josé` arrives as `jose` + U+0301 (combining acute).
        // Without the combining-mark clause the token splits and the whole address leaks.
        let input = "mail jose\u{301}@example.com end";
        let out = redact(input, &[]).unwrap();
        assert!(
            !out.contains("jose\u{301}"),
            "NFD email local part leaked: {out:?}"
        );
        assert!(
            !out.contains("@example.com"),
            "email domain leaked: {out:?}"
        );
        assert!(out.contains("[email]"), "email not redacted: {out:?}");
        assert_eq!(out.width(), input.width(), "width not preserved: {out:?}");
    }

    #[test]
    fn preserves_display_width_of_wide_chars() {
        // Finding 3: a 3-char CJK token is 6 columns; the placeholder must occupy 6
        // columns too (char-count sizing would give 3 and shift the layout).
        let input = "东京市"; // 3 chars, 6 display columns
        assert_eq!(input.width(), 6);
        let out = redact(input, &["东京市".to_string()]).unwrap();
        assert!(!out.contains('东'), "CJK token leaked: {out}");
        assert_eq!(out.width(), 6, "placeholder is not 6 columns: {out:?}");
    }

    #[test]
    fn non_path_slash_words_survive() {
        // "and/or" does not start with '/', so it is not a path and stays put.
        assert_eq!(redact("and/or maybe", &[]).unwrap(), "and/or maybe");
    }

    #[test]
    fn bare_at_is_not_email() {
        assert_eq!(redact("@here team", &[]).unwrap(), "@here team");
    }
}
