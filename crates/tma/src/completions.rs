//! `tma completions <shell>`: the ahead-of-time shell completion scripts, generated from the same
//! clap tree the parser uses so a new flag is covered the moment it is declared.

use std::process::ExitCode;

use clap::CommandFactory;

use crate::cli::{Cli, CompletionsArgs};

pub(crate) fn run(args: CompletionsArgs) -> ExitCode {
    let mut cmd = completion_tree();
    clap_complete::generate(args.shell, &mut cmd, "tma", &mut std::io::stdout());
    ExitCode::SUCCESS
}

/// The tree the scripts are generated from: `Cli`'s, minus everything marked `hide`.
///
/// clap_complete's generators filter hidden *values* but not hidden subcommands or args, so
/// generating straight from `Cli::command()` would offer `tma supervise`, `tma event`,
/// `tma clear-attention` and `tma daemon --sweep-ms` — three internal verbs and five test hooks,
/// none of which anyone should be typing. clap has no way to drop an item from a built `Command`,
/// so each node is rebuilt from its visible parts instead.
///
/// Rebuilding means the command-level properties are copied by hand, which is what
/// `completion_tree_renders_the_same_help_as_the_parse_tree` guards: removing only hidden items
/// cannot change any help output, so a property this misses surfaces there as a diff.
fn completion_tree() -> clap::Command {
    visible_only(&Cli::command())
}

/// Whether anything at or below `cmd` is marked `hide`. A node with nothing to prune is cloned
/// verbatim below, which is what keeps the hand-copied property list from mattering for the rest of
/// the tree: in tma's, only the root (three internal verbs) and `daemon` (five test hooks) are
/// rebuilt at all.
fn hides_anything(cmd: &clap::Command) -> bool {
    cmd.get_arguments().any(clap::Arg::is_hide_set)
        || cmd
            .get_subcommands()
            .any(|sub| sub.is_hide_set() || hides_anything(sub))
}

fn visible_only(cmd: &clap::Command) -> clap::Command {
    use clap::builder::{Resettable, Str};
    if !hides_anything(cmd) {
        return cmd.clone();
    }
    let owned = |s: Option<&str>| Resettable::from(s.map(|s| Str::from(s.to_owned())));
    let args: Vec<clap::Arg> = cmd
        .get_arguments()
        .filter(|a| !a.is_hide_set())
        .cloned()
        .collect();
    // clap's derive puts every field of an `Args` struct into one implicit group named after it, so
    // dropping a hidden arg leaves the group naming an argument that is no longer there — which
    // clap's own debug assertions catch. Prune the members alongside.
    let kept: Vec<&clap::Id> = args.iter().map(clap::Arg::get_id).collect();
    let groups = cmd.get_groups().map(|group| {
        let mut group = group.clone();
        clap::ArgGroup::new(group.get_id().clone())
            .args(group.get_args().filter(|id| kept.contains(id)).cloned())
            .required(group.is_required_set())
            .multiple(group.is_multiple())
    });
    clap::Command::new(cmd.get_name().to_owned())
        .about(Resettable::from(cmd.get_about().cloned()))
        .long_about(Resettable::from(cmd.get_long_about().cloned()))
        .version(owned(cmd.get_version()))
        .long_version(owned(cmd.get_long_version()))
        .visible_aliases(cmd.get_visible_aliases().map(str::to_owned))
        .groups(groups.collect::<Vec<_>>())
        .args(args)
        .subcommands(
            cmd.get_subcommands()
                .filter(|sub| !sub.is_hide_set())
                .map(visible_only),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Generate for one shell and hand back the script.
    fn script(shell: clap_complete::Shell) -> String {
        let mut out = Vec::new();
        clap_complete::generate(shell, &mut completion_tree(), "tma", &mut out);
        String::from_utf8(out).expect("the generators write utf-8")
    }

    /// The whole point of rebuilding the tree. `supervise`, `event` and `clear-attention` are
    /// spawned by tmux hooks and by tma itself, never typed, and `daemon`'s five test hooks steer
    /// the sweep cadence and the detach staging. clap_complete would list every one of them.
    #[test]
    fn hidden_subcommands_and_args_reach_no_generated_script() {
        let tree = completion_tree();
        let offered: Vec<&str> = tree.get_subcommands().map(|s| s.get_name()).collect();
        for name in ["event", "clear-attention", "supervise"] {
            assert!(!offered.contains(&name), "the tree still carries {name}");
        }
        // The flags are distinctive enough to look for in the scripts themselves, which is where a
        // generator that started emitting hidden args again would show up.
        let hidden = [
            "--status-file",
            "--probe-cross-session",
            "--sweep-ms",
            "--detach-stage2",
            "--detach-session",
        ];
        for shell in [
            clap_complete::Shell::Bash,
            clap_complete::Shell::Zsh,
            clap_complete::Shell::Fish,
            clap_complete::Shell::Elvish,
            clap_complete::Shell::PowerShell,
        ] {
            let script = script(shell);
            for name in hidden {
                assert!(
                    !script.contains(name),
                    "{shell} completion offers the hidden {name}"
                );
            }
        }
    }

    /// `visible_only` rebuilds each node by hand, so a command-level property it forgets to copy
    /// would silently vanish from the completion scripts. Nothing hidden appears in help either, so
    /// the two trees must render identical help at every node — a missed property shows up here.
    #[test]
    fn completion_tree_renders_the_same_help_as_the_parse_tree() {
        fn compare(parsed: &mut clap::Command, generated: &mut clap::Command, path: &str) {
            assert_eq!(
                parsed.render_long_help().to_string(),
                generated.render_long_help().to_string(),
                "help differs at `{path}`: visible_only dropped a command property"
            );
            for sub in parsed
                .get_subcommands()
                .filter(|s| !s.is_hide_set())
                .cloned()
                .collect::<Vec<_>>()
            {
                let name = sub.get_name().to_owned();
                let mut generated_sub = generated
                    .find_subcommand(&name)
                    .unwrap_or_else(|| {
                        panic!("`{path} {name}` is missing from the completion tree")
                    })
                    .clone();
                compare(
                    &mut sub.clone(),
                    &mut generated_sub,
                    &format!("{path} {name}"),
                );
            }
        }
        compare(&mut Cli::command(), &mut completion_tree(), "tma");
    }

    /// The vocabulary `--state`/`--until` report as possible values is what a completion script can
    /// offer for them; a plain function value parser reports none, which is the gap
    /// `cli_support::StateListParser` closes.
    #[test]
    fn the_state_vocabulary_reaches_the_generated_scripts() {
        let script = script(clap_complete::Shell::Zsh);
        assert!(script.contains(":STATES:(idle working blocked unknown done)"));
    }
}
