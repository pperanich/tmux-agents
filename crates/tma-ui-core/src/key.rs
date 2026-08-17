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
    /// The picker's action-menu key. A key no query can contain, because every printable character
    /// belongs to the fuzzy query — an agent named `auth` has to be typeable.
    Tab,
    CtrlC,
    CtrlS,
}
