//! `tma mute`: suppress a pane's notifications without touching what tma detects about it.
//!
//! One pane option (`@agent_mute_until`, an epoch-ms deadline) is the whole mechanism, which is what
//! makes a mute survive a tma restart, a daemon start/stop, and a config reload: the state lives in
//! tmux, not in a process. The notify layer reads it on the fire path
//! ([`tma_runtime::notify::muted`]); detection, stamping, the `tma status` counts, and every JSON key
//! but the additive `muted` are unchanged.
//!
//! Target resolution mirrors `act`: `--pane <ID>` names one, the selector flags mute every pane they
//! match, and with neither the current pane (`$TMUX_PANE`) is the target. Exit codes are the shared
//! contract: `0` applied, `2` usage, `3` nothing matched, `1` a runtime failure.

use std::process::ExitCode;

use tma_core::stamp::{opt, MUTE_FOREVER_MS};
use tma_core::{render, FoldConfig, Selector};
use tma_runtime::cycle;

use crate::cli_support;
use crate::config::Config;
use crate::tmux::{self, Tmux};

/// Everything `tma mute` needs, assembled by the bin's dispatch from the CLI args and loaded config.
pub(crate) struct MuteOpts {
    pub pane: Option<String>,
    pub selector: Selector,
    /// The `--for` window in milliseconds, already parsed by [`parse_duration`]. `None` mutes
    /// indefinitely (the [`MUTE_FOREVER_MS`] sentinel).
    pub for_ms: Option<u64>,
    pub clear: bool,
    pub server: tma_tmux::tmux::Server,
    pub manifest_dir: Option<std::path::PathBuf>,
    pub config: Config,
}

pub(crate) fn run(opts: MuteOpts) -> ExitCode {
    if opts.clear && opts.for_ms.is_some() {
        eprintln!("tma: --clear unmutes; drop --for (or drop --clear to set a new window)");
        return ExitCode::from(2);
    }
    let tmux = Tmux::connect(&opts.server);
    let panes = match resolve_targets(&opts, &tmux) {
        Ok(p) => p,
        Err(code) => return code,
    };

    // The deadline is resolved once, from one clock read, so a fan-out over ten panes cannot leave
    // them muted until ten slightly different instants.
    let until = opts
        .for_ms
        .map(|ms| tma_runtime::now_ms().saturating_add(ms));
    let commands: Vec<_> = panes
        .iter()
        .map(|pane| {
            if opts.clear {
                render::unset_pane_option(pane, opt::MUTE_UNTIL)
            } else {
                render::set_pane_option(
                    pane,
                    opt::MUTE_UNTIL,
                    &until.unwrap_or(MUTE_FOREVER_MS).to_string(),
                )
            }
        })
        .collect();
    match tmux.apply(&commands) {
        Ok(()) => {}
        Err(tmux::TmuxError::ServerGone) => return cli_support::no_server(),
        Err(err) => {
            eprintln!("tma: {err}");
            return ExitCode::FAILURE;
        }
    }

    let what = match (opts.clear, opts.for_ms) {
        (true, _) => "unmuted".to_string(),
        (false, None) => "muted indefinitely".to_string(),
        (false, Some(ms)) => format!("muted for {}", fmt_duration(ms)),
    };
    println!("tma: {what}: {}", panes.join(" "));
    ExitCode::SUCCESS
}

/// The panes to write. `--pane` names one; a selector mutes everything it matches (a mute is
/// idempotent and per-pane, so a fan-out needs no `--all` opt-in the way firing an action does);
/// neither is the current pane.
fn resolve_targets(opts: &MuteOpts, tmux: &Tmux) -> Result<Vec<String>, ExitCode> {
    if let Some(p) = &opts.pane {
        if !opts.selector.is_empty() {
            eprintln!(
                "tma: --pane names one pane; drop the selector flags \
                 (--session/--repo/--branch/--agent/--state)"
            );
            return Err(ExitCode::from(2));
        }
        return Ok(vec![p.clone()]);
    }
    if opts.selector.is_empty() {
        return match std::env::var("TMUX_PANE") {
            Ok(p) if !p.is_empty() => Ok(vec![p]),
            _ => {
                eprintln!(
                    "tma: not inside a tmux pane; name the target with --pane <ID> or a selector \
                     flag (--session/--repo/--branch/--agent/--state)"
                );
                Err(ExitCode::from(2))
            }
        };
    }

    let manifests = cli_support::load_manifests_or_exit(
        opts.manifest_dir.as_deref(),
        &opts.config.agent_overrides,
    )?;
    let cfg: FoldConfig = opts.config.fold_config();
    // Deferred, never inline: `--state done` is idle + `@agent_attention`, so an inline clear would
    // retract the mark out of the rows the selector matches on and mute nothing.
    let mut report = match cycle::run_cycle_with(tmux, &manifests, &cfg, cycle::SeenClear::Deferred)
    {
        Ok(r) => r,
        Err(tmux::TmuxError::ServerGone) => return Err(cli_support::no_server()),
        Err(err) => {
            eprintln!("tma: {err}");
            return Err(ExitCode::FAILURE);
        }
    };
    // A repo/branch selector needs the labels the cycle deliberately leaves unresolved.
    if opts.selector.needs_repo() {
        tma_runtime::repo::annotate_rows(&mut report.rows);
    }
    let ids: Vec<String> = report
        .rows
        .iter()
        .filter(|r| opts.selector.matches(r))
        .map(|r| r.pane_id.clone())
        .collect();
    // The cycle's clear, strictly after the selector read and before the no-match return below.
    if !report.deferred_seen.is_empty() {
        tma_runtime::seen::clear_seen(tmux, &report.deferred_seen);
    }
    if ids.is_empty() {
        eprintln!("tma: no agent pane matched the selector (exit 3)");
        return Err(ExitCode::from(3));
    }
    Ok(ids)
}

/// clap value parser for `--for`: an integer plus a unit (`45s`, `30m`, `2h`, `1d`), a bare number
/// being seconds. Returns milliseconds. A zero window is a usage error rather than a mute that is
/// over before it starts, and the error names the grammar so a typo is self-correcting.
pub(crate) fn parse_duration(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let bad = || {
        format!(
            "invalid duration {s:?}; use a number with an optional unit: 45s, 30m, 2h, 1d \
             (a bare number is seconds)"
        )
    };
    let (digits, unit_ms) = match s.chars().last() {
        None => return Err(bad()),
        Some('s') => (&s[..s.len() - 1], 1_000u64),
        Some('m') => (&s[..s.len() - 1], 60_000),
        Some('h') => (&s[..s.len() - 1], 3_600_000),
        Some('d') => (&s[..s.len() - 1], 86_400_000),
        Some(c) if c.is_ascii_digit() => (s, 1_000),
        Some(_) => return Err(bad()),
    };
    let value: u64 = digits.parse().map_err(|_| bad())?;
    if value == 0 {
        return Err(format!("a mute of {s:?} would be over before it began"));
    }
    // Saturating rather than overflowing: an absurd `--for 999999999999d` clamps to the indefinite
    // sentinel, which is what the user asked for in every sense that matters.
    Ok(value.saturating_mul(unit_ms).min(MUTE_FOREVER_MS))
}

/// The duration echoed back on success, in the unit the user most likely typed. Display only.
fn fmt_duration(ms: u64) -> String {
    let secs = ms / 1000;
    if secs.is_multiple_of(86_400) {
        format!("{}d", secs / 86_400)
    } else if secs.is_multiple_of(3_600) {
        format!("{}h", secs / 3_600)
    } else if secs.is_multiple_of(60) {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_duration_reads_each_unit() {
        assert_eq!(parse_duration("45s"), Ok(45_000));
        assert_eq!(parse_duration("30m"), Ok(1_800_000));
        assert_eq!(parse_duration("2h"), Ok(7_200_000));
        assert_eq!(parse_duration("1d"), Ok(86_400_000));
        // A bare number is seconds, and surrounding whitespace (a quoted shell value) is trimmed.
        assert_eq!(parse_duration("90"), Ok(90_000));
        assert_eq!(parse_duration(" 5m "), Ok(300_000));
    }

    #[test]
    fn parse_duration_rejects_what_it_cannot_mean() {
        for bad in ["", "m", "abc", "5x", "5 m", "-5m", "1.5h", "5min"] {
            assert!(
                parse_duration(bad).is_err(),
                "{bad:?} should not parse as a duration"
            );
        }
        // Zero is refused rather than silently writing a deadline already in the past.
        assert!(parse_duration("0").is_err() && parse_duration("0m").is_err());
        // The grammar rides the error, so a typo is fixable without reaching for the docs.
        assert!(parse_duration("5x").unwrap_err().contains("30m"));
    }

    #[test]
    fn parse_duration_clamps_an_absurd_window_to_the_sentinel() {
        // No overflow, and the result still means "muted until you clear it".
        assert_eq!(parse_duration("999999999999d"), Ok(MUTE_FOREVER_MS));
    }

    #[test]
    fn fmt_duration_echoes_the_coarsest_exact_unit() {
        assert_eq!(fmt_duration(45_000), "45s");
        assert_eq!(fmt_duration(1_800_000), "30m");
        assert_eq!(fmt_duration(7_200_000), "2h");
        assert_eq!(fmt_duration(86_400_000), "1d");
        assert_eq!(fmt_duration(90_000), "90s");
    }
}
