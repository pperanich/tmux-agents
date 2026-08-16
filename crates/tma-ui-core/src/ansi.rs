//! SGR-to-ratatui converter for the picker/watch preview: tmux `capture-pane -e` interleaves ANSI
//! escapes with glyphs, and `Paragraph::new(String)` would render them as literal garbage. Hand-
//! rolled (like `tma_runtime::json`) to keep the dependency surface tight and cover only the SGR
//! subset tmux emits; the lexer mirrors `tma_core::strip_ansi` (CSI/OSC framing) but keeps the SGR
//! payload. Supported: 0/1/2/3/4/7 and resets 22/23/24/27; 30-37/40-47 basic and 39/49 default
//! fg/bg; 90-97/100-107 bright; 38;5;n / 48;5;n indexed; 38;2 / 48;2 rgb. Everything else is
//! consumed silently, never leaked as text. Style carries across newlines (SGR spans lines
//! legitimately); each `Line` still gets explicit per-span styles, so nothing bleeds past a widget.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};

/// Convert a string carrying ANSI SGR escapes into styled ratatui text. Non-SGR sequences and
/// unknown/truncated params are dropped; never panics, never emits raw escape bytes as text.
pub(crate) fn ansi_to_text(input: &str) -> Text<'static> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut cur = String::new();
    let mut style = Style::default();
    let mut chars = input.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.peek() {
                // CSI: ESC [ params/intermediates, terminated by a byte in 0x40..=0x7e.
                // Only the SGR final ('m') carries style; every other final is ignored.
                Some('[') => {
                    chars.next();
                    let mut params = String::new();
                    let mut final_byte = None;
                    for f in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&f) {
                            final_byte = Some(f);
                            break;
                        }
                        params.push(f);
                    }
                    if final_byte == Some('m') {
                        // Flush the accumulated run under the *old* style before switching.
                        if !cur.is_empty() {
                            spans.push(Span::styled(std::mem::take(&mut cur), style));
                        }
                        apply_sgr(&mut style, &params);
                    }
                    // final_byte == None → truncated at EOF, drop silently.
                }
                // OSC: ESC ] ... terminated by BEL or ST (ESC \). Consumed, never rendered.
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
                // Other ESC-prefixed sequences (e.g. ESC ( charset): drop ESC + next byte.
                Some(_) => {
                    chars.next();
                }
                // Bare ESC at EOF: drop.
                None => {}
            },
            '\n' => {
                if !cur.is_empty() {
                    spans.push(Span::styled(std::mem::take(&mut cur), style));
                }
                lines.push(Line::from(std::mem::take(&mut spans)));
            }
            // Carriage returns from CRLF captures would otherwise render as stray glyphs.
            '\r' => {}
            _ => cur.push(c),
        }
    }

    if !cur.is_empty() {
        spans.push(Span::styled(cur, style));
    }
    lines.push(Line::from(spans));
    Text::from(lines)
}

/// Fold one SGR sequence's params (the substring between `ESC[` and `m`) into the running style.
/// An empty string means `ESC[m`, i.e. SGR 0 (reset).
fn apply_sgr(style: &mut Style, params: &str) {
    let codes: Vec<u16> = if params.is_empty() {
        vec![0]
    } else {
        params
            .split(';')
            .filter_map(|p| {
                if p.is_empty() {
                    // An omitted parameter defaults to 0 per ECMA-48 (e.g. `ESC[;m`, `ESC[1;;4m`).
                    Some(0)
                } else {
                    // A non-empty unparsable/out-of-range (>u16) param is SKIPPED, not folded to
                    // 0: folding would act as SGR 0 (reset) and wipe style set earlier in the run.
                    p.parse::<u16>().ok()
                }
            })
            .collect()
    };

    let mut i = 0;
    while i < codes.len() {
        match codes[i] {
            0 => *style = Style::default(),
            1 => *style = style.add_modifier(Modifier::BOLD),
            2 => *style = style.add_modifier(Modifier::DIM),
            3 => *style = style.add_modifier(Modifier::ITALIC),
            4 => *style = style.add_modifier(Modifier::UNDERLINED),
            7 => *style = style.add_modifier(Modifier::REVERSED),
            22 => *style = style.remove_modifier(Modifier::BOLD | Modifier::DIM),
            23 => *style = style.remove_modifier(Modifier::ITALIC),
            24 => *style = style.remove_modifier(Modifier::UNDERLINED),
            27 => *style = style.remove_modifier(Modifier::REVERSED),
            n @ 30..=37 => *style = style.fg(basic_color(n - 30)),
            39 => *style = style.fg(Color::Reset),
            n @ 40..=47 => *style = style.bg(basic_color(n - 40)),
            49 => *style = style.bg(Color::Reset),
            n @ 90..=97 => *style = style.fg(Color::Indexed(8 + (n - 90) as u8)),
            n @ 100..=107 => *style = style.bg(Color::Indexed(8 + (n - 100) as u8)),
            38 => {
                if let Some(color) = extended_color(&codes, &mut i) {
                    *style = style.fg(color);
                }
            }
            48 => {
                if let Some(color) = extended_color(&codes, &mut i) {
                    *style = style.bg(color);
                }
            }
            // Any other SGR parameter (blink, conceal, 58 underline color, …): ignored.
            _ => {}
        }
        i += 1;
    }
}

/// Parse a `38`/`48` extended color at `codes[*i]`: `5;n` indexed, `2;r;g;b` truecolor. On success
/// advances `*i` past the args; a malformed/truncated tail returns `None` and leaves `*i` unchanged.
fn extended_color(codes: &[u16], i: &mut usize) -> Option<Color> {
    match codes.get(*i + 1) {
        Some(5) => {
            let n = *codes.get(*i + 2)?;
            *i += 2;
            // The 256-color palette is exactly 0..=255; clamp an out-of-range index rather than
            // truncating (`n as u8` wraps 256→0, 300→44 — a wildly wrong color).
            Some(Color::Indexed(n.min(255) as u8))
        }
        Some(2) => {
            let r = *codes.get(*i + 2)?;
            let g = *codes.get(*i + 3)?;
            let b = *codes.get(*i + 4)?;
            *i += 4;
            // Each channel is 0..=255; clamp rather than truncate (`as u8` wraps 256→0).
            let ch = |c: u16| c.min(255) as u8;
            Some(Color::Rgb(ch(r), ch(g), ch(b)))
        }
        _ => None,
    }
}

/// Map a basic ANSI color offset (0..=7) to a ratatui named color. 7 is `Gray`, the dim
/// "white" of the base palette; bright white is index 15, reached via the 90-series.
fn basic_color(offset: u16) -> Color {
    match offset {
        0 => Color::Black,
        1 => Color::Red,
        2 => Color::Green,
        3 => Color::Yellow,
        4 => Color::Blue,
        5 => Color::Magenta,
        6 => Color::Cyan,
        7 => Color::Gray,
        _ => Color::Reset,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Flatten styled text back to its visible characters, for escape-leak assertions.
    fn flatten(text: &Text) -> String {
        let mut out = String::new();
        for (i, line) in text.lines.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            for span in &line.spans {
                out.push_str(&span.content);
            }
        }
        out
    }

    /// The single styled span on a single-line `Text`, for asserting style + content.
    fn only_span<'a>(text: &'a Text<'static>) -> &'a Span<'static> {
        assert_eq!(text.lines.len(), 1, "expected one line");
        assert_eq!(text.lines[0].spans.len(), 1, "expected one span");
        &text.lines[0].spans[0]
    }

    #[test]
    fn plain_text_passes_through() {
        let text = ansi_to_text("hello world");
        let span = only_span(&text);
        assert_eq!(span.content, "hello world");
        assert_eq!(span.style, Style::default());
    }

    #[test]
    fn basic_foreground_color() {
        let text = ansi_to_text("\u{1b}[31mred");
        let span = only_span(&text);
        assert_eq!(span.content, "red");
        assert_eq!(span.style.fg, Some(Color::Red));
    }

    #[test]
    fn bright_foreground_color() {
        let text = ansi_to_text("\u{1b}[91mbright");
        let span = only_span(&text);
        assert_eq!(span.style.fg, Some(Color::Indexed(9)));
    }

    #[test]
    fn bright_background_color() {
        let text = ansi_to_text("\u{1b}[100mbg");
        let span = only_span(&text);
        assert_eq!(span.style.bg, Some(Color::Indexed(8)));
    }

    #[test]
    fn indexed_256_color() {
        let text = ansi_to_text("\u{1b}[38;5;196mx");
        let span = only_span(&text);
        assert_eq!(span.style.fg, Some(Color::Indexed(196)));
    }

    #[test]
    fn truecolor_rgb() {
        let text = ansi_to_text("\u{1b}[38;2;10;20;30mx\u{1b}[48;2;1;2;3my");
        assert_eq!(
            text.lines[0].spans[0].style.fg,
            Some(Color::Rgb(10, 20, 30))
        );
        assert_eq!(text.lines[0].spans[1].style.bg, Some(Color::Rgb(1, 2, 3)));
    }

    #[test]
    fn unparsable_param_is_skipped_not_a_reset() {
        // A junk / overflowing (>u16) parameter between a bold and a color must be SKIPPED, not
        // folded to 0: folding to 0 would act as SGR 0 and wipe the bold set just before it.
        let text = ansi_to_text("\u{1b}[1;99999;31mx");
        let span = only_span(&text);
        assert_eq!(span.content, "x");
        assert!(
            span.style.add_modifier.contains(Modifier::BOLD),
            "the overflowing param must not reset the bold"
        );
        assert_eq!(span.style.fg, Some(Color::Red));
    }

    #[test]
    fn empty_param_still_defaults_to_zero_reset() {
        // ECMA-48: an omitted parameter defaults to 0. `ESC[1;;m` — the empty middle param is a 0
        // (reset), so the trailing state is default, unlike a skipped junk param.
        let text = ansi_to_text("\u{1b}[31m\u{1b}[1;;mx");
        let span = only_span(&text);
        assert_eq!(
            span.style,
            Style::default(),
            "the empty 0 param reset the style"
        );
    }

    #[test]
    fn out_of_range_indexed_is_clamped_not_truncated() {
        // `38;5;300`: 300 > 255. `300 as u8` would wrap to 44 (a wrong color); clamp to 255.
        let text = ansi_to_text("\u{1b}[38;5;300mx");
        let span = only_span(&text);
        assert_eq!(span.style.fg, Some(Color::Indexed(255)));
    }

    #[test]
    fn out_of_range_rgb_channels_are_clamped() {
        // `48;2;300;10;400`: channels past 255 (but still parseable as u16) clamp rather than
        // wrapping via `as u8` (`300 as u8 == 44`, `400 as u8 == 144`).
        let text = ansi_to_text("\u{1b}[48;2;300;10;400mx");
        let span = only_span(&text);
        assert_eq!(span.style.bg, Some(Color::Rgb(255, 10, 255)));
    }

    #[test]
    fn bold_plus_color_then_reset_midline() {
        // "AB" bold+red, then reset, then "C" plain — three runs on one line.
        let text = ansi_to_text("\u{1b}[1;31mAB\u{1b}[0mC");
        let spans = &text.lines[0].spans;
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content, "AB");
        assert!(spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(spans[0].style.fg, Some(Color::Red));
        assert_eq!(spans[1].content, "C");
        assert_eq!(spans[1].style, Style::default());
    }

    #[test]
    fn style_carries_across_newline() {
        let text = ansi_to_text("\u{1b}[32mfirst\nsecond");
        assert_eq!(text.lines.len(), 2);
        assert_eq!(text.lines[0].spans[0].content, "first");
        assert_eq!(text.lines[0].spans[0].style.fg, Some(Color::Green));
        // The green SGR is still in effect on the next line (no reset was emitted).
        assert_eq!(text.lines[1].spans[0].content, "second");
        assert_eq!(text.lines[1].spans[0].style.fg, Some(Color::Green));
    }

    #[test]
    fn unknown_sgr_is_ignored() {
        // 53 (overline) is outside the supported subset; the color must still apply and no
        // escape byte may leak into the text.
        let text = ansi_to_text("\u{1b}[53;34mx");
        let span = only_span(&text);
        assert_eq!(span.content, "x");
        assert_eq!(span.style.fg, Some(Color::Blue));
        assert!(!flatten(&text).contains('\u{1b}'));
    }

    #[test]
    fn truncated_escape_at_eof_is_dropped() {
        let text = ansi_to_text("visible\u{1b}[38;5");
        assert_eq!(flatten(&text), "visible");
        assert!(!flatten(&text).contains('\u{1b}'));
    }

    #[test]
    fn osc_sequence_is_consumed() {
        // OSC 0 title set, BEL-terminated, then a plain glyph.
        let text = ansi_to_text("\u{1b}]0;window title\u{07}done");
        assert_eq!(flatten(&text), "done");
        assert!(!flatten(&text).contains('\u{1b}'));
    }

    #[test]
    fn non_sgr_csi_is_consumed() {
        // ESC[2J (clear screen) has a non-'m' final and must not style or leak.
        let text = ansi_to_text("\u{1b}[2Jtext");
        let span = only_span(&text);
        assert_eq!(span.content, "text");
        assert_eq!(span.style, Style::default());
    }

    #[test]
    fn real_fixture_leaves_no_escape_bytes() {
        // A real `capture-pane -e` body from the tma-core fixtures: after conversion the
        // flattened text must carry no raw escape bytes.
        let raw = include_str!("../../tma-core/fixtures/claude_working_title.txt");
        let body = raw.split_once("\n---\n").map(|(_, b)| b).unwrap_or(raw);
        assert!(body.contains('\u{1b}'), "fixture should contain escapes");
        let text = ansi_to_text(body);
        assert!(
            !flatten(&text).contains('\u{1b}'),
            "converted preview leaked escape bytes"
        );
    }

    /// Drift guard: this lexer's CSI/OSC/other-ESC framing must match `tma_core::strip_ansi` over a
    /// shared corpus (same grammar, SGR payload kept vs. dropped). `\r`, dropped here, is normalized out.
    #[test]
    fn escape_lexer_agrees_with_core_strip_ansi() {
        let fixture = {
            let raw = include_str!("../../tma-core/fixtures/claude_working_title.txt");
            raw.split_once("\n---\n")
                .map(|(_, b)| b.to_string())
                .unwrap_or_else(|| raw.to_string())
        };
        let corpus: &[&str] = &[
            "",
            "plain text, no escapes",
            "\u{1b}[31mred\u{1b}[0m normal",
            "\u{1b}[1;38;5;196mbold indexed\u{1b}[m",
            "\u{1b}[38;2;10;20;30mtruecolor\u{1b}[39m",
            "\u{1b}[2Jclear then \u{1b}[Htext", // non-SGR CSI finals
            "\u{1b}]0;window title\u{07}after OSC",
            "\u{1b}]8;;http://x\u{1b}\\link\u{1b}]8;;\u{1b}\\", // OSC ST-terminated
            "charset \u{1b}(Bthen text",                        // other-ESC
            "line one\nline two\nline three",
            "trailing newline\n",
            "truncated at eof\u{1b}[38;5",
            &fixture,
        ];
        for input in corpus {
            let via_spans = flatten(&ansi_to_text(input));
            let via_strip = tma_core::engine::strip_ansi(input).replace('\r', "");
            assert_eq!(
                via_spans, via_strip,
                "escape lexers disagree on visible text for {input:?}"
            );
        }
    }
}
