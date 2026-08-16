//! The picker/watch row palette: a glyph + ratatui `Color` per agent state (plus the "done" pair),
//! resolved from the `(glyph, colour-name)` string pairs the runtime config emits. Keeps ratatui's
//! colour vocabulary in the UI crate, off the runtime config.

use ratatui::style::Color;
use tma_core::AgentState;

/// Resolved glyph + colour for each agent state plus the "done" surface (idle + `@agent_attention`).
/// Built once per draw from the config's resolved string pairs; an unparseable colour string falls
/// back to the state's default (a picker render must never fail on a typo).
#[derive(Debug, Clone)]
pub struct RowPalette {
    blocked: (String, Color),
    working: (String, Color),
    idle: (String, Color),
    unknown: (String, Color),
    done: (String, Color),
}

/// The resolved `(glyph, colour-name)` string pairs per state that [`RowPalette::new`] maps to
/// colours (the shape `PickerStyles::resolved_str`/`resolved_done_str` emit). Named fields, because
/// all five share the `(&str, &str)` type: positional args let a caller transpose two states silently.
pub struct RowStyles<'a> {
    pub blocked: (&'a str, &'a str),
    pub working: (&'a str, &'a str),
    pub idle: (&'a str, &'a str),
    pub unknown: (&'a str, &'a str),
    pub done: (&'a str, &'a str),
}

impl RowPalette {
    /// Build from the resolved string pairs. Each colour string parses via [`parse_color`], falling
    /// back to the state's default ratatui colour on a typo (a picker render must never fail).
    pub fn new(styles: RowStyles) -> RowPalette {
        RowPalette {
            blocked: resolve(styles.blocked, Color::Red),
            working: resolve(styles.working, Color::Yellow),
            idle: resolve(styles.idle, Color::Green),
            unknown: resolve(styles.unknown, Color::DarkGray),
            done: resolve(styles.done, Color::Magenta),
        }
    }

    /// The `(glyph, colour)` for a plain agent state.
    pub fn state(&self, state: AgentState) -> (String, Color) {
        match state {
            AgentState::Blocked => self.blocked.clone(),
            AgentState::Working => self.working.clone(),
            AgentState::Idle => self.idle.clone(),
            AgentState::Unknown => self.unknown.clone(),
        }
    }

    /// The `(glyph, colour)` for the "done" surface (idle + attention).
    pub fn done(&self) -> (String, Color) {
        self.done.clone()
    }
}

impl Default for RowPalette {
    /// The zero-config picker palette (⚑ red, ● yellow, ○ green, ? darkgray, ✓ magenta); `unknown`
    /// is `darkgray`, the pre-config picker value (not status's `colour244`). Mirrors `PickerStyles`'
    /// zero-config defaults (runtime `config.rs`) and is test-only: any drift there surfaces as a
    /// palette test failure, so it stays a deliberate, caught edit.
    fn default() -> RowPalette {
        RowPalette::new(RowStyles {
            blocked: ("⚑", "red"),
            working: ("●", "yellow"),
            idle: ("○", "green"),
            unknown: ("?", "darkgray"),
            done: ("✓", "magenta"),
        })
    }
}

/// Resolve one `(glyph, colour-name)` pair to `(glyph, Color)`, falling back to `fallback` when the
/// colour string does not parse.
fn resolve((glyph, color): (&str, &str), fallback: Color) -> (String, Color) {
    (glyph.to_string(), parse_color(color).unwrap_or(fallback))
}

/// Map a config color string to a ratatui [`Color`]: named ANSI colors, `colourNNN`/`colorNNN`
/// indexes (the tmux spelling, so a status color ports to the picker), and `#rrggbb`. `None` if
/// unrecognized.
pub fn parse_color(name: &str) -> Option<Color> {
    let n = name.trim().to_ascii_lowercase();
    // `colourNNN` / `colorNNN` (or a bare index) → a 256-color palette entry.
    let idx = n
        .strip_prefix("colour")
        .or_else(|| n.strip_prefix("color"))
        .unwrap_or(&n);
    if let Ok(i) = idx.parse::<u8>() {
        return Some(Color::Indexed(i));
    }
    if let Some(hex) = n.strip_prefix('#') {
        // ASCII first: `len` counts bytes, so slicing a multi-byte char would panic on a char
        // boundary, and this value comes straight from the user's config.
        if hex.len() == 6 && hex.is_ascii() {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            return Some(Color::Rgb(r, g, b));
        }
        return None;
    }
    Some(match n.as_str() {
        "reset" | "default" => Color::Reset,
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "gray" | "grey" | "white" => Color::Gray,
        "darkgray" | "darkgrey" => Color::DarkGray,
        "brightblack" => Color::DarkGray,
        "brightred" | "lightred" => Color::LightRed,
        "brightgreen" | "lightgreen" => Color::LightGreen,
        "brightyellow" | "lightyellow" => Color::LightYellow,
        "brightblue" | "lightblue" => Color::LightBlue,
        "brightmagenta" | "lightmagenta" => Color::LightMagenta,
        "brightcyan" | "lightcyan" => Color::LightCyan,
        "brightwhite" => Color::White,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default picker palette maps its zero-config colour strings to the right ratatui colours;
    /// `unknown` is `DarkGray` (the picker default), not status's `colour244`. Ported from the
    /// config-crate `zero_config` assertions when the ratatui vocabulary moved here.
    #[test]
    fn default_palette_maps_strings_to_colors() {
        let p = RowPalette::default();
        assert_eq!(p.state(AgentState::Blocked), ("⚑".to_string(), Color::Red));
        assert_eq!(
            p.state(AgentState::Unknown),
            ("?".to_string(), Color::DarkGray)
        );
        assert_eq!(p.done(), ("✓".to_string(), Color::Magenta));
    }

    /// An unparseable colour string falls back to the state's default colour (a picker render must
    /// never fail on a typo); the glyph is still carried through.
    #[test]
    fn unparseable_color_falls_back_to_state_default() {
        let p = RowPalette::new(RowStyles {
            blocked: ("⚑", "not-a-color"),
            working: ("●", "yellow"),
            idle: ("○", "green"),
            unknown: ("?", "darkgray"),
            done: ("✓", "magenta"),
        });
        assert_eq!(p.state(AgentState::Blocked), ("⚑".to_string(), Color::Red));
    }

    #[test]
    fn parse_color_covers_names_indexes_and_hex() {
        assert_eq!(parse_color("red"), Some(Color::Red));
        assert_eq!(parse_color("DarkGray"), Some(Color::DarkGray));
        assert_eq!(parse_color("colour244"), Some(Color::Indexed(244)));
        assert_eq!(parse_color("color12"), Some(Color::Indexed(12)));
        assert_eq!(parse_color("#ff8800"), Some(Color::Rgb(255, 136, 0)));
        assert_eq!(parse_color("not-a-color"), None);
    }

    /// A hex value carrying multi-byte chars is a typo like any other: `None`, never a panic on a
    /// char boundary (the picker rebuilds this palette from hot-reloaded config on every draw).
    #[test]
    fn non_ascii_hex_falls_back_instead_of_panicking() {
        assert_eq!(parse_color("#€123"), None); // 6 bytes, 4 chars
        assert_eq!(parse_color("#ff88é"), None); // 6 bytes, 5 chars
        assert_eq!(parse_color("#夕焼け"), None); // 9 bytes
        let p = RowPalette::new(RowStyles {
            blocked: ("⚑", "#€123"),
            working: ("●", "yellow"),
            idle: ("○", "green"),
            unknown: ("?", "darkgray"),
            done: ("✓", "magenta"),
        });
        assert_eq!(p.state(AgentState::Blocked), ("⚑".to_string(), Color::Red));
    }
}
