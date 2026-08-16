//! The core's input alphabet; the shell maps terminal key events onto it.

/// The keys the fold understands; the shell owns the terminal-key mapping, keeping the input
/// backend out of the core.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    Up,
    Down,
    Enter,
    Esc,
    Backspace,
    Char(char),
    CtrlC,
    CtrlS,
}
