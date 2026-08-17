//! The read path: `list-panes`/`PaneRecord`, `capture-pane`, `list-sessions`, the `ps` process
//! walk, and the single-option `list-panes` read. No pane-affecting effects live here.

use std::collections::HashMap;
use std::process::Command;

use tma_core::snapshot::ProcInfo;
use tma_core::stamp::opt;

use super::{SessionInfo, Tmux, TmuxError, SEP};

/// The per-pane `@agent_*` options read back in the `list-panes` format: exactly the
/// [`tma_core::StampedState`] tuple keys, in encode order, built from the [`tma_core::stamp::opt`]
/// registry so the read set cannot drift from the write set. Each is its own `-F` field (tmux `-F`
/// has no glob). The window-scoped [`opt::SUMMARY`] and the session-scoped [`opt::SESSION_SUMMARY`]
/// are read separately via inheritance, not here.
const AGENT_OPTIONS: &[&str] = &[
    opt::NAME,
    opt::STATE,
    opt::DETAIL,
    opt::SOURCE,
    opt::EVIDENCE_AT,
    opt::SINCE,
    opt::STAMPED_AT,
    opt::ATTENTION,
    opt::NOTIFIED_AT,
    opt::HASH,
    opt::PID,
    opt::SESSION,
    opt::SUBAGENTS,
];

/// Per-pane bookkeeping options read into the same `options` map but NOT part of the
/// [`StampedState`] tuple, so keeping them separate leaves [`AGENT_OPTIONS`] "exactly the tuple
/// keys": the flicker-stickiness anchor, the dead-registration reaper marker, the user-set
/// `@agent_ignore` escape hatch and `@agent_mute_until` deadline (both read here so their gates cost
/// no second round-trip), the context metric
/// pair (a parallel lane, not part of the state tuple; surfaces read it for the JSON rows),
/// the model label (`@agent_model`, read by `tma doctor`'s recognized-model check), and the OpenCode
/// API-channel pair (`@agent_permission_request` / `@agent_api_endpoint`, read by the action
/// broker and `tma doctor`). Appended after [`AGENT_OPTIONS`] (order fixed by [`parse_pane_line`]).
const EXTRA_PANE_OPTIONS: &[&str] = &[
    opt::TITLE_MATCH_PID,
    opt::REG_DEAD_SINCE,
    opt::IGNORE,
    opt::MUTE_UNTIL,
    opt::CONTEXT_PCT,
    opt::CONTEXT_AT,
    opt::TOKENS,
    opt::TOKENS_AT,
    opt::CONTEXT_NOTIFIED_AT,
    opt::MODEL,
    opt::PERMISSION_REQUEST,
    opt::API_ENDPOINT,
];

/// One pane's read-side facts from `list-panes -a -F`.
#[derive(Clone, Debug)]
pub struct PaneRecord {
    pub pane_id: String,
    pub pane_pid: u32,
    pub session: String,
    pub window_index: u32,
    pub pane_index: u32,
    pub current_command: String,
    /// `#{window_activity}` epoch seconds (0 when tmux reports it empty).
    pub window_activity: u64,
    pub alternate_on: bool,
    /// `None` outside copy-mode; `Some(n)` in copy-mode with the viewport `n` lines above the
    /// live screen (`Some(0)` at the bottom, which is still the live screen).
    pub scroll_position: Option<u32>,
    /// `#{pane_height}`: visible-screen row count, so `Region::Visible` rules can clamp
    /// `capture-pane -S -50` (which reaches into scrollback) to the visible screen. `0` if empty.
    pub pane_height: u32,
    /// `#{pane_current_path}`: the pane's working directory, `None` when tmux reports it empty. The
    /// repo/branch resolver keys off it; the broker reads its own separate `TMA_CWD`.
    pub cwd: Option<String>,
    /// Present, non-empty `@agent_*` options keyed by their full name.
    pub options: HashMap<String, String>,
    /// The pane's *window* `@agent_summary`, resolved via option inheritance from the pane context.
    /// `None` when unset; read by the end-of-cycle summary reconciliation to skip no-op writes.
    pub window_summary: Option<String>,
    /// The pane's *session* `@agent_session_summary`, resolved the same way. Its own key rather
    /// than `@agent_summary` at session scope, so a window read never inherits the session rollup.
    pub session_summary: Option<String>,
    pub title: String,
}

impl PaneRecord {
    /// `session:window.pane` locator for surfaces and jump.
    pub fn locator(&self) -> String {
        format!("{}:{}.{}", self.session, self.window_index, self.pane_index)
    }
}

impl Tmux {
    /// Enumerate live sessions by `session_id` (`list-sessions -F`): the pool's membership basis.
    /// `session_id` (`$N`) is stable across renames, so the pool keys on it.
    pub(crate) fn list_sessions(&self) -> Result<Vec<SessionInfo>, TmuxError> {
        let out = self.run(&["list-sessions", "-F", "#{session_id}"])?;
        let mut sessions = Vec::new();
        for line in out.lines() {
            if line.is_empty() {
                continue;
            }
            sessions.push(SessionInfo {
                id: line.to_string(),
            });
        }
        Ok(sessions)
    }

    /// Enumerate every pane in every session.
    pub fn list_panes(&self) -> Result<Vec<PaneRecord>, TmuxError> {
        let format = list_panes_format();
        let out = self.run(&["list-panes", "-a", "-F", &format])?;
        let mut records = Vec::new();
        for line in out.lines() {
            if line.is_empty() {
                continue;
            }
            records.push(parse_pane_line(line)?);
        }
        Ok(records)
    }

    /// Read one user option across every pane (`list-panes -a`), returning `(pane_id, value)` where
    /// set. The nudge sender walks this for `@tma_watch_pid` (present only where `tma watch` set it).
    pub fn list_pane_option(&self, key: &str) -> Result<Vec<(String, String)>, TmuxError> {
        let format = format!("#{{pane_id}}{SEP}#{{{key}}}");
        let out = self.run(&["list-panes", "-a", "-F", &format])?;
        let mut found = Vec::new();
        for line in out.lines() {
            let Some((pane, value)) = line.split_once(SEP) else {
                continue;
            };
            if !value.is_empty() {
                found.push((pane.to_string(), value.to_string()));
            }
        }
        Ok(found)
    }

    /// Enumerate attached clients by name (`list-clients -F`), empty when none. `tma doctor` reads
    /// it: `#()` status jobs only run while a client is drawing the status line, so a server with no
    /// client has no ambient polling floor.
    pub fn list_clients(&self) -> Result<Vec<String>, TmuxError> {
        let out = self.run(&["list-clients", "-F", "#{client_name}"])?;
        Ok(out
            .lines()
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect())
    }

    /// Capture the live-viewport tail with escapes. `-S -50` reaches up to 50 lines ABOVE the
    /// viewport into scrollback: on a pane shorter than ~50 rows (a split) that carries prior-turn
    /// scrollback above the visible screen, so whole-screen rules scope to `Region::Visible` (clamped
    /// to `#{pane_height}` in the core); bottom-anchored `tail_lines(N)` rules never look that far up.
    pub fn capture_pane(&self, pane_id: &str) -> Result<String, TmuxError> {
        self.run(&["capture-pane", "-p", "-e", "-t", pane_id, "-S", "-50"])
    }

    /// Capture a pane's visible screen with escapes (`-e`) for the picker preview: its ANSI decoder
    /// turns the SGR escapes into styled ratatui spans, so the preview shows the pane's real colors.
    /// Visible-only deliberately (no `-S`): a negative start prepends primary-screen scrollback,
    /// which for an alt-screen TUI (claude, codex) buries the live frame below stale shell output.
    pub fn capture_ansi(&self, pane_id: &str) -> Result<String, TmuxError> {
        self.run(&["capture-pane", "-p", "-e", "-t", pane_id])
    }
}

/// Parse `ps -eo pid,ppid,pgid,comm` into process facts, portable across procps and BSD. Not a tmux
/// call. BSD `comm` carries `argv[0]` plus arguments; [`normalize_comm`] extracts the basename.
pub fn ps_all() -> Result<Vec<ProcInfo>, TmuxError> {
    let output = Command::new("ps")
        .args(["-eo", "pid,ppid,pgid,comm"])
        .output()
        .map_err(|source| TmuxError::Spawn {
            cmd: "ps -eo pid,ppid,pgid,comm".to_string(),
            source,
        })?;
    if !output.status.success() {
        return Err(TmuxError::Failed {
            cmd: "ps -eo pid,ppid,pgid,comm".to_string(),
            code: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(parse_ps(&text))
}

/// Extract the executable basename from a `comm` field: first whitespace token (BSD adds arguments),
/// then its path basename. `"npm exec …"` ⇒ `"npm"`, `"/sbin/launchd"` ⇒ `"launchd"`.
pub fn normalize_comm(comm: &str) -> &str {
    let first = comm.split_whitespace().next().unwrap_or(comm);
    first.rsplit('/').next().unwrap_or(first)
}

fn parse_ps(text: &str) -> Vec<ProcInfo> {
    let mut out = Vec::new();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let (Some(pid), Some(ppid), Some(pgid)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        let (Ok(pid), Ok(ppid), Ok(pgid)) =
            (pid.parse::<u32>(), ppid.parse::<u32>(), pgid.parse::<u32>())
        else {
            // Header row ("PID PPID PGID COMM") and any stray line: skip.
            continue;
        };
        // `comm` is every token after the three numeric columns. Rejoining on single
        // spaces collapses BSD's argument padding, which is irrelevant to name matching.
        let comm = it.collect::<Vec<_>>().join(" ");
        if comm.is_empty() {
            continue;
        }
        out.push(ProcInfo {
            pid,
            ppid,
            pgid,
            comm,
        });
    }
    out
}

fn list_panes_format() -> String {
    let mut fields: Vec<String> = vec![
        "#{pane_id}".to_string(),
        "#{pane_pid}".to_string(),
        "#{session_name}".to_string(),
        "#{window_index}".to_string(),
        "#{pane_index}".to_string(),
        "#{pane_current_command}".to_string(),
        "#{window_activity}".to_string(),
        "#{alternate_on}".to_string(),
        "#{scroll_position}".to_string(),
        "#{pane_height}".to_string(),
        "#{pane_current_path}".to_string(),
    ];
    for opt in AGENT_OPTIONS {
        fields.push(format!("#{{{opt}}}"));
    }
    // Non-tuple per-pane bookkeeping options (title anchor, reaper marker), read back into the
    // same `options` map right after the tuple keys.
    for opt in EXTRA_PANE_OPTIONS {
        fields.push(format!("#{{{opt}}}"));
    }
    // The window `@agent_summary` and the session `@agent_session_summary`, resolved from the pane
    // context by option inheritance (each is only ever set at its own scope, so a pane read yields
    // that scope's value).
    fields.push(format!("#{{{}}}", opt::SUMMARY));
    fields.push(format!("#{{{}}}", opt::SESSION_SUMMARY));
    // Title last: free-form, captured as the split remainder.
    fields.push("#{pane_title}".to_string());
    fields.join(&SEP.to_string())
}

/// Number of fixed fields before the `@agent_*` options.
const FIXED_FIELDS: usize = 11;

fn parse_pane_line(line: &str) -> Result<PaneRecord, TmuxError> {
    // Fixed fields, tuple options, non-tuple bookkeeping options, the two summary rollups, then
    // the title.
    let total = FIXED_FIELDS + AGENT_OPTIONS.len() + EXTRA_PANE_OPTIONS.len() + 3;
    let parts: Vec<&str> = line.splitn(total, SEP).collect();
    if parts.len() != total {
        return Err(TmuxError::Parse {
            cmd: "list-panes".to_string(),
            reason: format!("expected {total} fields, got {} in {line:?}", parts.len()),
        });
    }
    let parse_u32 = |s: &str, what: &str| -> Result<u32, TmuxError> {
        s.parse::<u32>().map_err(|_| TmuxError::Parse {
            cmd: "list-panes".to_string(),
            reason: format!("{what} {s:?} is not a number"),
        })
    };

    let mut options = HashMap::new();
    for (i, opt) in AGENT_OPTIONS.iter().enumerate() {
        let value = parts[FIXED_FIELDS + i];
        if !value.is_empty() {
            options.insert((*opt).to_string(), value.to_string());
        }
    }
    for (j, opt) in EXTRA_PANE_OPTIONS.iter().enumerate() {
        let value = parts[FIXED_FIELDS + AGENT_OPTIONS.len() + j];
        if !value.is_empty() {
            options.insert((*opt).to_string(), value.to_string());
        }
    }

    let summaries_at = FIXED_FIELDS + AGENT_OPTIONS.len() + EXTRA_PANE_OPTIONS.len();
    let optional = |s: &str| match s {
        "" => None,
        v => Some(v.to_string()),
    };
    let window_summary = optional(parts[summaries_at]);
    let session_summary = optional(parts[summaries_at + 1]);

    Ok(PaneRecord {
        pane_id: parts[0].to_string(),
        pane_pid: parse_u32(parts[1], "pane_pid")?,
        session: parts[2].to_string(),
        window_index: parse_u32(parts[3], "window_index")?,
        pane_index: parse_u32(parts[4], "pane_index")?,
        current_command: parts[5].to_string(),
        window_activity: parts[6].parse::<u64>().unwrap_or(0),
        alternate_on: parts[7] == "1",
        scroll_position: match parts[8] {
            "" => None,
            n => Some(parse_u32(n, "scroll_position")?),
        },
        pane_height: match parts[9] {
            "" => 0,
            n => parse_u32(n, "pane_height")?,
        },
        cwd: match parts[10] {
            "" => None,
            s => Some(s.to_string()),
        },
        options,
        window_summary,
        session_summary,
        title: parts[total - 1].to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_procps_and_bsd_ps() {
        // procps: comm is a bare token. BSD (macOS): comm carries argv[0] + arguments.
        let text = "  PID  PPID  PGID COMM\n\
                    100     1   100 zsh\n\
                    200   100   100 claude\n\
                    300   200   100 npm exec @claude-flow/cli@latest mcp start\n\
                    400     1   400 /sbin/launchd\n";
        let procs = parse_ps(text);
        assert_eq!(procs.len(), 4, "header skipped, 4 rows kept");
        assert_eq!(procs[1].comm, "claude");
        assert_eq!(procs[1].ppid, 100);
        assert_eq!(procs[2].comm, "npm exec @claude-flow/cli@latest mcp start");
        assert_eq!(normalize_comm(&procs[2].comm), "npm");
        assert_eq!(normalize_comm(&procs[3].comm), "launchd");
        assert_eq!(normalize_comm(&procs[1].comm), "claude");
    }

    #[test]
    fn parses_pane_line_with_options_and_title_containing_colon() {
        let mut fields: Vec<String> = vec![
            "%13".into(),
            "4242".into(),
            "work".into(),
            "2".into(),
            "1".into(),
            "claude".into(),
            "1721500000".into(),
            "1".into(),
            "".into(),           // scroll_position empty → live viewport
            "40".into(),         // pane_height
            "/home/work".into(), // pane_current_path
        ];
        // @agent_* options: state + pid set, rest empty.
        for opt in AGENT_OPTIONS {
            fields.push(match *opt {
                "@agent_state" => "working".into(),
                "@agent_pid" => "4242".into(),
                _ => "".into(),
            });
        }
        for opt in EXTRA_PANE_OPTIONS {
            // Non-tuple bookkeeping options: set the title anchor so the read-back is covered.
            fields.push(match *opt {
                "@tma_title_match_pid" => "4242".into(),
                _ => "".into(),
            });
        }
        fields.push("blocked:1".into()); // window @agent_summary
        fields.push("blocked:1 idle:2".into()); // session @agent_session_summary
        fields.push("⠂ Do a thing: now".into()); // title with a colon and a spinner
        let line = fields.join(&SEP.to_string());

        let rec = parse_pane_line(&line).unwrap();
        assert_eq!(rec.pane_id, "%13");
        assert_eq!(rec.pane_pid, 4242);
        assert_eq!(rec.locator(), "work:2.1");
        assert_eq!(rec.current_command, "claude");
        assert_eq!(rec.window_activity, 1_721_500_000);
        assert!(rec.alternate_on);
        assert_eq!(rec.scroll_position, None);
        assert_eq!(rec.pane_height, 40);
        assert_eq!(rec.cwd.as_deref(), Some("/home/work"));
        assert_eq!(
            rec.options.get("@agent_state").map(String::as_str),
            Some("working")
        );
        assert_eq!(
            rec.options.get("@agent_pid").map(String::as_str),
            Some("4242")
        );
        assert!(!rec.options.contains_key("@agent_detail"));
        // Non-tuple bookkeeping option read back into the same map (title anchor / reaper marker).
        assert_eq!(
            rec.options.get("@tma_title_match_pid").map(String::as_str),
            Some("4242")
        );
        assert!(!rec.options.contains_key("@tma_reg_dead_since"));
        assert_eq!(rec.window_summary.as_deref(), Some("blocked:1"));
        assert_eq!(rec.session_summary.as_deref(), Some("blocked:1 idle:2"));
        assert_eq!(rec.title, "⠂ Do a thing: now");
    }

    #[test]
    fn scrolled_pane_reports_scroll_position() {
        let mut fields: Vec<String> = vec![
            "%1".into(),
            "5".into(),
            "s".into(),
            "0".into(),
            "0".into(),
            "zsh".into(),
            "0".into(),
            "0".into(),
            "12".into(),
            "24".into(), // pane_height
            "".into(),   // pane_current_path empty → None
        ];
        for _ in AGENT_OPTIONS {
            fields.push("".into());
        }
        for _ in EXTRA_PANE_OPTIONS {
            fields.push("".into());
        }
        fields.push("".into()); // window @agent_summary unset
        fields.push("".into()); // session @agent_session_summary unset
        fields.push("t".into());
        let rec = parse_pane_line(&fields.join(&SEP.to_string())).unwrap();
        assert_eq!(rec.scroll_position, Some(12));
        assert_eq!(rec.pane_height, 24);
        assert_eq!(rec.cwd, None);
        assert!(!rec.alternate_on);
        assert_eq!(rec.window_summary, None);
        assert_eq!(rec.session_summary, None);
    }

    #[test]
    fn copy_mode_at_the_bottom_reports_zero_not_empty() {
        // tmux distinguishes "not in copy-mode" (empty) from "in copy-mode at the bottom" (`0`);
        // the freeze fact keys on that difference, so the parse must preserve it.
        let line = |scroll: &str| {
            let mut fields: Vec<String> = vec![
                "%1".into(),
                "5".into(),
                "s".into(),
                "0".into(),
                "0".into(),
                "zsh".into(),
                "0".into(),
                "0".into(),
                scroll.into(),
                "24".into(),
                "".into(),
            ];
            for _ in AGENT_OPTIONS {
                fields.push("".into());
            }
            for _ in EXTRA_PANE_OPTIONS {
                fields.push("".into());
            }
            fields.push("".into()); // window @agent_summary
            fields.push("".into()); // session @agent_session_summary
            fields.push("t".into());
            parse_pane_line(&fields.join(&SEP.to_string())).unwrap()
        };
        assert_eq!(line("0").scroll_position, Some(0));
        assert_eq!(line("").scroll_position, None);
    }
}
