//! The write half of `[daemon] window_names`: the `rename-window` chains and their restore.
//!
//! Every write is a [`StampCommand`], so a rename and its bookkeeping commit as ONE chained tmux
//! invocation. That matters here for the same reason it does in [`crate::stamp`]: tmux redraws every
//! attached client on each `set-option`, and a rename that cost four invocations would cost four
//! full redraws.

use tma_core::render::StampCommand;
use tma_core::stamp::{opt, AUTORENAME_UNSET};

/// Build the rename chain for one window. `save_original` adds the two bookkeeping writes that make
/// the rename undoable: the pre-tma `window_name` and the window-scope `automatic-rename` value
/// (`None` ⇒ the [`AUTORENAME_UNSET`] sentinel, so the restore unsets rather than pinning an
/// inherited value). `@tma_window_name_last` records what tma wrote, which is how the next pass
/// tells its own name from one the user typed.
pub fn rename_commands(
    window_id: &str,
    name: &str,
    save_original: Option<SavedOriginal>,
) -> Vec<StampCommand> {
    let mut cmds = Vec::with_capacity(4);
    if let Some(saved) = save_original {
        // Saved BEFORE the rename: `rename-window` itself sets `automatic-rename off`.
        cmds.push(set_window_option(
            window_id,
            opt::WINDOW_NAME_ORIG,
            &saved.name,
        ));
        cmds.push(set_window_option(
            window_id,
            opt::WINDOW_AUTORENAME_ORIG,
            saved
                .automatic_rename
                .as_deref()
                .unwrap_or(AUTORENAME_UNSET),
        ));
    }
    cmds.push(StampCommand {
        argv: vec![
            "rename-window".into(),
            "-t".into(),
            window_id.into(),
            name.into(),
        ],
    });
    cmds.push(set_window_option(window_id, opt::WINDOW_NAME_LAST, name));
    cmds
}

/// What the first rename of a window preserves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedOriginal {
    /// The window's name before tma touched it.
    pub name: String,
    /// The window-scope `automatic-rename` value, `None` when the window inherited it.
    pub automatic_rename: Option<String>,
}

/// Build the restore chain: the saved name back, then `automatic-rename` (the rename above turns it
/// off, so it is restored after), then the three bookkeeping options dropped. Dropping them is what
/// takes tma out of the window: with no saved original, the next pass will not rename it again
/// until an agent pane returns.
pub fn restore_commands(
    window_id: &str,
    original_name: &str,
    original_automatic_rename: Option<&str>,
) -> Vec<StampCommand> {
    let mut cmds = vec![StampCommand {
        argv: vec![
            "rename-window".into(),
            "-t".into(),
            window_id.into(),
            original_name.into(),
        ],
    }];
    match original_automatic_rename {
        Some(value) if value != AUTORENAME_UNSET => {
            cmds.push(set_window_option(window_id, "automatic-rename", value));
        }
        // Unset at window scope before tma, or never recorded: put it back to inheriting.
        _ => cmds.push(unset_window_option(window_id, "automatic-rename")),
    }
    cmds.push(unset_window_option(window_id, opt::WINDOW_NAME_ORIG));
    cmds.push(unset_window_option(window_id, opt::WINDOW_AUTORENAME_ORIG));
    cmds.push(unset_window_option(window_id, opt::WINDOW_NAME_LAST));
    cmds
}

/// `set-option -w -t <window> <key> <value>`. [`tma_core::render`]'s builders are pane- and
/// server-scoped; this feature is the only window-scoped writer.
fn set_window_option(window_id: &str, key: &str, value: &str) -> StampCommand {
    StampCommand {
        argv: vec![
            "set-option".into(),
            "-w".into(),
            "-t".into(),
            window_id.into(),
            key.into(),
            value.into(),
        ],
    }
}

fn unset_window_option(window_id: &str, key: &str) -> StampCommand {
    StampCommand {
        argv: vec![
            "set-option".into(),
            "-w".into(),
            "-u".into(),
            "-t".into(),
            window_id.into(),
            key.into(),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv_of(c: &StampCommand) -> Vec<&str> {
        c.argv.iter().map(String::as_str).collect()
    }

    #[test]
    fn a_first_rename_saves_the_name_and_the_autorename_value_first() {
        let cmds = rename_commands(
            "@3",
            "tma:blocked",
            Some(SavedOriginal {
                name: "shell".into(),
                automatic_rename: Some("on".into()),
            }),
        );
        assert_eq!(
            cmds.iter().map(argv_of).collect::<Vec<_>>(),
            vec![
                vec![
                    "set-option",
                    "-w",
                    "-t",
                    "@3",
                    "@tma_window_name_orig",
                    "shell"
                ],
                vec![
                    "set-option",
                    "-w",
                    "-t",
                    "@3",
                    "@tma_window_autorename_orig",
                    "on"
                ],
                vec!["rename-window", "-t", "@3", "tma:blocked"],
                vec![
                    "set-option",
                    "-w",
                    "-t",
                    "@3",
                    "@tma_window_name_last",
                    "tma:blocked"
                ],
            ]
        );
    }

    #[test]
    fn an_inherited_autorename_is_saved_as_the_sentinel() {
        let cmds = rename_commands(
            "@3",
            "tma:idle",
            Some(SavedOriginal {
                name: "shell".into(),
                automatic_rename: None,
            }),
        );
        assert_eq!(argv_of(&cmds[1]).last(), Some(&AUTORENAME_UNSET));
    }

    #[test]
    fn a_later_rename_writes_only_the_name_and_the_marker() {
        let cmds = rename_commands("@3", "tma:idle", None);
        assert_eq!(
            cmds.iter().map(argv_of).collect::<Vec<_>>(),
            vec![
                vec!["rename-window", "-t", "@3", "tma:idle"],
                vec![
                    "set-option",
                    "-w",
                    "-t",
                    "@3",
                    "@tma_window_name_last",
                    "tma:idle"
                ],
            ]
        );
    }

    #[test]
    fn a_restore_puts_the_name_back_then_the_autorename_then_drops_the_bookkeeping() {
        let cmds = restore_commands("@3", "shell", Some("on"));
        assert_eq!(
            cmds.iter().map(argv_of).collect::<Vec<_>>(),
            vec![
                vec!["rename-window", "-t", "@3", "shell"],
                vec!["set-option", "-w", "-t", "@3", "automatic-rename", "on"],
                vec![
                    "set-option",
                    "-w",
                    "-u",
                    "-t",
                    "@3",
                    "@tma_window_name_orig"
                ],
                vec![
                    "set-option",
                    "-w",
                    "-u",
                    "-t",
                    "@3",
                    "@tma_window_autorename_orig"
                ],
                vec![
                    "set-option",
                    "-w",
                    "-u",
                    "-t",
                    "@3",
                    "@tma_window_name_last"
                ],
            ]
        );
    }

    #[test]
    fn the_sentinel_restores_by_unsetting() {
        for saved in [Some(AUTORENAME_UNSET), None] {
            let cmds = restore_commands("@3", "shell", saved);
            assert_eq!(
                argv_of(&cmds[1]),
                vec!["set-option", "-w", "-u", "-t", "@3", "automatic-rename"]
            );
        }
    }
}
