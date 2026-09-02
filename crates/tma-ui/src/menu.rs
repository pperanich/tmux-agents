//! `tma act --menu`: the keyboard-only parity surface. A tmux `display-menu` of the actions
//! fireable on a pane right now, each entry wired to `tma act <name> --pane <id>` so selecting it
//! fires through the same broker every other surface uses. Two helpers in the picker's discipline:
//! [`action_menu_items`] builds the entries (pure, unit-tested) and [`show`] renders them through
//! `tma_runtime::ui` (the crate's only tmux touch; no `tma-tmux` edge).

use tma_runtime::ui;
use tma_runtime::{escape_menu_label, MenuItem, Server, Tmux, TmuxError};

/// Build the `display-menu` entries for `fireable` (each `(name, label)`), invoking
/// `<bin> act <name> --pane <pane_id>` via `run-shell` (entries call the same CLI verb). The
/// invoking server's `--socket-name` is forwarded so a menu on a named socket fires on that server.
/// The first nine entries get a `1`..`9` quick-select mnemonic; the rest have none. `bin` is
/// single-quoted and the label escaped, as in [`act_menu_command`] and the jump menu.
///
/// Each entry sets `TMA_ACT_SOURCE=menu` on the command line, which is what puts `menu` in the act
/// audit line's `source`: the entry runs the same `tma act` a person types, so without it the
/// keyboard-only surface would be indistinguishable from a shell invocation after the fact.
pub fn action_menu_items(
    bin: &str,
    server: &Server,
    pane_id: &str,
    fireable: &[(String, String)],
) -> Vec<MenuItem> {
    let socket = server.shell_flag();
    fireable
        .iter()
        .enumerate()
        .map(|(i, (name, label))| {
            // Quick-select digit `1`..`9` for the first nine; tmux fires the entry on that keypress.
            let key = if i < 9 {
                ((b'1' + i as u8) as char).to_string()
            } else {
                String::new()
            };
            MenuItem {
                // `bin` comes from `current_exe()` and may hold a space; the label is a manifest
                // string that may hold a `#`, which tmux would read as a format marker.
                label: escape_menu_label(label),
                key,
                command: format!(
                    "run-shell \"TMA_ACT_SOURCE=menu '{bin}' act {name} --pane {pane_id}{socket}\""
                ),
            }
        })
        .collect()
}

/// The shell command that opens this menu for `pane_id` from a surface that is not standing in it:
/// `<bin> act --menu --pane <id>` on the invoking server, for the dashboards' `a` key. Handed to
/// tmux's `run-shell -b` rather than spawned as a child, so the menu belongs to the tmux server and
/// outlives the surface (a popup-hosted picker is closed by the menu overlay that replaces it).
/// `bin` is single-quoted: it comes from `current_exe()` and may hold a space.
pub fn act_menu_command(bin: &str, server: &Server, pane_id: &str) -> String {
    format!(
        "'{bin}' act --menu --pane {pane_id}{socket}",
        socket = server.shell_flag()
    )
}

/// Render the fireable-action menu on the client viewing `pane_id`. `title` heads the menu; `items`
/// come from [`action_menu_items`]. An empty `items` is a caller error (tmux rejects an empty menu),
/// so callers report "no fireable actions" before reaching here.
pub fn show(tmux: &Tmux, pane_id: &str, title: &str, items: &[MenuItem]) -> Result<(), TmuxError> {
    ui::display_menu(tmux, pane_id, title, items)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_run_shell_entries_with_quick_select_keys() {
        let fireable = vec![
            ("approve".to_string(), "Approve".to_string()),
            ("deny".to_string(), "Deny".to_string()),
        ];
        let items = action_menu_items("tma", &Server::default(), "%5", &fireable);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].label, "Approve");
        assert_eq!(items[0].key, "1");
        assert_eq!(
            items[0].command,
            "run-shell \"TMA_ACT_SOURCE=menu 'tma' act approve --pane %5\""
        );
        assert_eq!(items[1].key, "2");
        assert_eq!(
            items[1].command,
            "run-shell \"TMA_ACT_SOURCE=menu 'tma' act deny --pane %5\""
        );
    }

    #[test]
    fn a_binary_path_with_a_space_survives_the_quoting() {
        let fireable = vec![("approve".to_string(), "Approve".to_string())];
        let items = action_menu_items("/opt/my tools/tma", &Server::default(), "%5", &fireable);
        assert_eq!(
            items[0].command,
            "run-shell \"TMA_ACT_SOURCE=menu '/opt/my tools/tma' act approve --pane %5\""
        );
    }

    #[test]
    fn a_hash_in_a_manifest_label_is_escaped() {
        // Unescaped, tmux reads `#{` / `#[` in a label as the start of a format expansion.
        let fireable = vec![("pick".to_string(), "Pick #{1}".to_string())];
        let items = action_menu_items("tma", &Server::default(), "%5", &fireable);
        assert_eq!(items[0].label, "Pick ##{1}");
    }

    #[test]
    fn forwards_the_target_server() {
        let fireable = vec![("approve".to_string(), "Approve".to_string())];
        let named = Server::named(Some("scratch".to_string()));
        let items = action_menu_items("/usr/bin/tma", &named, "%1", &fireable);
        assert_eq!(
            items[0].command,
            "run-shell \"TMA_ACT_SOURCE=menu '/usr/bin/tma' act approve --pane %1 --socket-name scratch\""
        );

        // A socket-path server forwards `--socket-path` instead, quoted so a space survives.
        let by_path = Server {
            socket_path: Some(std::path::PathBuf::from("/tmp/tmate-501/sock")),
            ..Server::default()
        };
        let items = action_menu_items("tma", &by_path, "%1", &fireable);
        assert_eq!(
            items[0].command,
            "run-shell \"TMA_ACT_SOURCE=menu 'tma' act approve --pane %1 --socket-path '/tmp/tmate-501/sock'\""
        );
    }

    #[test]
    fn act_menu_command_targets_the_pane_and_the_server() {
        assert_eq!(
            act_menu_command("tma", &Server::default(), "%5"),
            "'tma' act --menu --pane %5"
        );
        // A named server rides along, so a watcher on a scratch socket menus on that socket.
        assert_eq!(
            act_menu_command(
                "/opt/my tools/tma",
                &Server::named(Some("s".to_string())),
                "%5"
            ),
            "'/opt/my tools/tma' act --menu --pane %5 --socket-name s",
            "a binary path with a space survives the quoting"
        );
    }

    #[test]
    fn tenth_and_later_entries_have_no_mnemonic() {
        let fireable: Vec<(String, String)> = (0..11)
            .map(|i| (format!("a{i}"), format!("A{i}")))
            .collect();
        let items = action_menu_items("tma", &Server::default(), "%2", &fireable);
        assert_eq!(items[8].key, "9");
        assert_eq!(items[9].key, "", "the tenth entry has no quick-select key");
        assert_eq!(items[10].key, "");
    }
}
