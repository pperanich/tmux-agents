//! What makes a pane an agent pane: identity = observation ∪ self-registration.
//!
//! *observed* = the process-tree walk finds a known agent binary (zero setup); the walk is authority
//! because wrappers hide the binary. *registered* = a SessionStart-class hook claimed the pane,
//! reconstructed each cycle from `@agent_session`/`@agent_name`. Two fold-visible carve-outs: a
//! remote-shell foreground (ssh/mosh/docker) or a nested multiplexer client (tmux/zellij/screen) is
//! out of scope ([`PaneIdentity::Remote`], [`PaneIdentity::Multiplexer`]) *unless* a live
//! registration claims the pane ([`Identified::behind`]), and [`Identified::foreground_is_agent`]
//! caps screen evidence at `unknown` when the agent is nested under a shell/editor. Title narrowing
//! and its pid-anchored flicker stickiness live on [`identify`].

use tma_core::render::{self, StampCommand};
use tma_core::snapshot::ProcInfo;
use tma_core::stamp::opt;

use tma_tmux::tmux::normalize_comm;

use crate::manifests::LoadedManifest;

/// Foreground commands whose real work runs on an unreachable host/container. Matched against the
/// normalized foreground basename.
const REMOTE_SHELLS: &[&str] = &["ssh", "mosh", "mosh-client", "docker", "podman", "kubectl"];

/// Foreground commands that are a terminal multiplexer *client*. Whatever runs inside is a child of
/// the inner server, not of this client, so the walk finds nothing and the outer pane's screen is a
/// composite of the inner ones — which screen rules would happily match. Matched like
/// [`REMOTE_SHELLS`], before the walk, so a nested pane is named rather than silently invisible.
const MULTIPLEXERS: &[&str] = &["tmux", "zellij", "screen", "dvtm", "abduco"];

/// Why a pane is out of scope for detection. Carries the matched foreground command so
/// `tma debug explain` and `tma doctor` can name it and say what to do instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutOfScope {
    /// A remote shell: the agent, if any, runs on a host tma cannot see.
    RemoteShell(&'static str),
    /// A nested multiplexer client: the agent's state lives on the inner server.
    Multiplexer(&'static str),
}

impl OutOfScope {
    /// The matched foreground command.
    pub fn command(self) -> &'static str {
        match self {
            OutOfScope::RemoteShell(cmd) | OutOfScope::Multiplexer(cmd) => cmd,
        }
    }

    /// Stable category token for the `--json` surfaces.
    pub fn token(self) -> &'static str {
        match self {
            OutOfScope::RemoteShell(_) => "remote_shell",
            OutOfScope::Multiplexer(_) => "nested_multiplexer",
        }
    }

    /// The matched foreground named by kind: `remote shell docker`, `nested multiplexer tmux`.
    pub fn label(self) -> String {
        match self {
            OutOfScope::RemoteShell(cmd) => format!("remote shell {cmd}"),
            OutOfScope::Multiplexer(cmd) => format!("nested multiplexer {cmd}"),
        }
    }

    /// The one-line explanation both `tma debug explain` and `tma doctor` print.
    pub fn hint(self) -> String {
        match self {
            OutOfScope::RemoteShell(_) => format!("{}, out of scope", self.label()),
            OutOfScope::Multiplexer(_) => format!(
                "{} — agent state lives on the inner server; run tma there",
                self.label()
            ),
        }
    }
}

/// How a pane's agent identity was established.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentitySource {
    /// Found by the process-tree walk. The zero-setup source: works for every agent.
    Observed,
    /// Claimed by a hook self-registration. Corroborates an observation (marking the pane
    /// hook-capable) or stands alone before the agent's process is walkable.
    Registered,
}

/// A hook self-registration, reconstructed each cycle from the stored `@agent_session` (set at
/// SessionStart, cleared at SessionEnd) plus `@agent_name`; no stored session yields `None`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Registration {
    /// The agent name the registration claims (must match a manifest to be honored).
    pub agent_name: String,
    /// Owning agent session id (the subagent guard reads it).
    pub session: Option<String>,
}

/// A recognized agent owning a pane.
#[derive(Clone, Copy)]
pub struct Identified<'a> {
    /// The manifest whose `process_names` matched a process in the pane's tree.
    pub manifest: &'a LoadedManifest,
    /// The matched agent process (group leader among matches); feeds the fold's episode-boundary
    /// pid comparison.
    pub agent_pid: u32,
    /// Foreground-cap input: is the pane's foreground command the agent itself? False for wrapper-TUI
    /// panes caps screen evidence at `unknown`.
    pub foreground_is_agent: bool,
    /// Provenance of this identification.
    pub source: IdentitySource,
    /// Flicker anchor: `Some(pid)` for a title-narrowed match (fresh or held), so the producer sets
    /// `@tma_title_match_pid == pid`. `None` for a process-only manifest: leave the anchor untouched.
    pub title_match_pid: Option<u32>,
    /// The out-of-scope foreground this pane's registration outranks (an agent in a container, or on
    /// the far side of an inner multiplexer server). `Some(..)` means nothing local is walkable and no
    /// capture crosses the boundary: the hook path is this pane's only evidence source.
    pub behind: Option<OutOfScope>,
}

/// The result of identifying one pane.
pub enum PaneIdentity<'a> {
    /// A recognized agent owns the pane.
    Agent(Identified<'a>),
    /// A remote shell (ssh/mosh/docker) owns the pane — out of scope for v1. Carries the matched
    /// command for `tma debug explain`.
    Remote(&'static str),
    /// A nested multiplexer client (tmux/zellij/screen) owns the pane. Treated exactly like
    /// [`PaneIdentity::Remote`] by the cycle — no stamp, no row — because the agent belongs to the
    /// inner server, which is where tma should run.
    Multiplexer(&'static str),
    /// No recognized agent, and neither a remote shell nor a nested multiplexer.
    None,
}

impl PaneIdentity<'_> {
    /// The out-of-scope classification, or `None` for an agent pane / an ordinary unrecognized one.
    pub fn out_of_scope(&self) -> Option<OutOfScope> {
        match self {
            PaneIdentity::Remote(cmd) => Some(OutOfScope::RemoteShell(cmd)),
            PaneIdentity::Multiplexer(cmd) => Some(OutOfScope::Multiplexer(cmd)),
            PaneIdentity::Agent(_) | PaneIdentity::None => None,
        }
    }
}

/// Whether the user has taken this pane out of detection with `@agent_ignore` (any non-empty
/// value). Checked by every caller *before* [`identify`], since the walk is exactly what a
/// false-positive pane fools: three bundled manifests match the generic process `node` narrowed by
/// title, so a dev server can look like an agent and this is how one pane opts out.
pub fn is_ignored(options: &std::collections::HashMap<String, String>) -> bool {
    options.get(opt::IGNORE).is_some_and(|v| !v.is_empty())
}

/// Resolve the pane's agent, if any, from its process tree, foreground command, and title.
/// `title_match_pid` is the stored `@tma_title_match_pid` flicker anchor; `registration` is the
/// hook-registered claim when a stored `@agent_session` names one.
pub fn identify<'a>(
    pane_pid: u32,
    current_command: &str,
    pane_title: &str,
    procs: &[ProcInfo],
    manifests: &'a [LoadedManifest],
    title_match_pid: Option<u32>,
    registration: Option<&Registration>,
) -> PaneIdentity<'a> {
    let foreground = normalize_comm(current_command);

    // The out-of-scope carve-outs, both checked before the walk. A remote-shell foreground runs its
    // real work where tma cannot see it, and a stray local child must not flip such a pane into a
    // false agent. A nested multiplexer client is the same shape one level down: the inner server's
    // processes are not in this pane's tree, so the walk finds nothing while the composited inner
    // screen sits there for a screen rule to match by coincidence.
    if let Some(scope) = out_of_scope_foreground(foreground) {
        // A live registration outranks both. It is proof a hook fired IN this pane, so the boundary
        // hides the agent's process, not the agent: honor it pid-less (nothing local is walkable, and
        // no capture crosses the boundary) and let the dead-registration reaper bound its life. With
        // no registration the carve-out stands and the pane stays invisible, stamps and all.
        return match registered_manifest(registration, manifests) {
            Some(lm) => PaneIdentity::Agent(Identified {
                manifest: lm,
                agent_pid: 0,
                foreground_is_agent: false,
                source: IdentitySource::Registered,
                title_match_pid: None,
                behind: Some(scope),
            }),
            None => match scope {
                OutOfScope::RemoteShell(cmd) => PaneIdentity::Remote(cmd),
                OutOfScope::Multiplexer(cmd) => PaneIdentity::Multiplexer(cmd),
            },
        };
    }

    let subtree = subtree(pane_pid, procs);

    // Observation: the first manifest with a matching process in the pane's tree.
    for lm in manifests {
        let names = &lm.manifest.identity.process_names;
        let Some(agent_pid) = leader_pid(&subtree, names) else {
            continue;
        };
        let registered_here = matches!(registration, Some(r) if r.agent_name == lm.name);
        // Title narrowing + flicker stickiness. A title-narrowed manifest (cursor runs as a bare
        // `node`) claims the pane only when the title matches a pattern NOW or the sticky hold is
        // live (agent pid unchanged since a prior match); satisfying neither, fall through so a bare
        // `node` pane is not mis-claimed. A hook registration for THIS manifest bypasses the gate:
        // it is authoritative identity, claimed with the real walkable pid and no anchor.
        let title_match = if lm.engine.has_title_patterns() {
            if lm.engine.title_matches(pane_title) || title_match_pid == Some(agent_pid) {
                Some(agent_pid)
            } else if registered_here {
                None
            } else {
                continue;
            }
        } else {
            None
        };
        let foreground_is_agent = names.iter().any(|n| n == foreground);
        // A registration corroborating this observation upgrades the provenance
        // (marks the pane hook-capable); otherwise it is plain observation.
        let source = if registered_here {
            IdentitySource::Registered
        } else {
            IdentitySource::Observed
        };
        return PaneIdentity::Agent(Identified {
            manifest: lm,
            agent_pid,
            foreground_is_agent,
            source,
            title_match_pid: title_match,
            behind: None,
        });
    }

    // Registration with no walkable process (not yet up, or died without SessionEnd). Honor it with
    // pid 0; the cycle's `agent_pid == 0` branch then reaps a truly-dead pane (shell-only subtree)
    // but holds a live pid-less agent (gemini, a non-shell process in the subtree).
    if let Some(lm) = registered_manifest(registration, manifests) {
        return PaneIdentity::Agent(Identified {
            manifest: lm,
            agent_pid: 0,
            foreground_is_agent: false,
            source: IdentitySource::Registered,
            // A pid-less registered identity holds no title anchor (no live pid to anchor).
            title_match_pid: None,
            behind: None,
        });
    }

    PaneIdentity::None
}

/// The carve-out a foreground command falls under, if any (see [`REMOTE_SHELLS`], [`MULTIPLEXERS`]).
fn out_of_scope_foreground(foreground: &str) -> Option<OutOfScope> {
    if let Some(&shell) = REMOTE_SHELLS.iter().find(|&&s| s == foreground) {
        return Some(OutOfScope::RemoteShell(shell));
    }
    MULTIPLEXERS
        .iter()
        .find(|&&m| m == foreground)
        .map(|&mux| OutOfScope::Multiplexer(mux))
}

/// The manifest a registration names, `None` when there is no registration or no manifest answers to
/// its agent name (an uninstalled agent's leftover claim is not identity).
fn registered_manifest<'a>(
    registration: Option<&Registration>,
    manifests: &'a [LoadedManifest],
) -> Option<&'a LoadedManifest> {
    let r = registration?;
    manifests.iter().find(|m| m.name == r.agent_name)
}

/// Reconcile the `@tma_title_match_pid` anchor. `desired` is [`Identified::title_match_pid`], or
/// `None` to clear a stale anchor. Returns a set/unset command only when it differs from `stored`,
/// so a process-only pane or an already-correct anchor issues no write.
pub(crate) fn title_anchor_command(
    pane: &str,
    stored: Option<&str>,
    desired: Option<u32>,
) -> Option<StampCommand> {
    let want = desired.map(|p| p.to_string());
    if stored.map(str::to_string) == want {
        return None;
    }
    Some(match want {
        Some(v) => render::set_pane_option(pane, opt::TITLE_MATCH_PID, &v),
        None => render::unset_pane_option(pane, opt::TITLE_MATCH_PID),
    })
}

/// The pid to attribute to the agent among subtree processes matching `names`: prefer a group
/// leader (`pid == pgid`), else the lowest matched pid for determinism. `None` when none match.
fn leader_pid(subtree: &[&ProcInfo], names: &[String]) -> Option<u32> {
    let mut matches: Vec<&ProcInfo> = subtree
        .iter()
        .copied()
        .filter(|p| names.iter().any(|n| normalize_comm(&p.comm) == n))
        .collect();
    if matches.is_empty() {
        return None;
    }
    // Group leaders first (pid == pgid), then lowest pid.
    matches.sort_by_key(|p| (p.pid != p.pgid, p.pid));
    Some(matches[0].pid)
}

/// Shells that can be a pane's root process. A subtree of nothing but these means the registered
/// agent's process is GONE (a live agent always leaves a non-shell process behind). Matched against
/// the normalized comm with a leading `-` stripped (login shells report `-zsh`); `login` covers a
/// fresh macOS login pane (`login -> -zsh`) before any command runs.
const SHELLS: &[&str] = &[
    "sh", "bash", "zsh", "fish", "dash", "ksh", "mksh", "tcsh", "csh", "ash", "login",
];

/// Does `subtree` carry no process that could be a live agent? True when every process is a
/// [`SHELLS`] entry (or the subtree is empty). Conservative on purpose: a single non-shell process
/// (gemini's `node`, an editor) makes it false, so a live pid-less agent holds forever and only a
/// truly-dead pane (shell fallback) is eligible for the timed reap.
pub(crate) fn subtree_is_shell_only(subtree: &[&ProcInfo]) -> bool {
    subtree.iter().all(|p| {
        let comm = normalize_comm(&p.comm).trim_start_matches('-');
        SHELLS.contains(&comm)
    })
}

/// The process facts in a pane: every process reachable from `root` via the ppid chain,
/// including `root` itself. The pane's tree, stored on the snapshot.
pub(crate) fn subtree(root: u32, procs: &[ProcInfo]) -> Vec<&ProcInfo> {
    let mut out: Vec<&ProcInfo> = Vec::new();
    let mut frontier: Vec<u32> = vec![root];
    let mut seen: Vec<u32> = Vec::new();
    while let Some(pid) = frontier.pop() {
        if seen.contains(&pid) {
            continue;
        }
        seen.push(pid);
        for p in procs.iter().filter(|p| p.pid == pid || p.ppid == pid) {
            if p.pid == pid {
                out.push(p);
            }
            if p.ppid == pid && p.pid != pid && !seen.contains(&p.pid) {
                frontier.push(p.pid);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tma_core::Manifest;

    fn proc(pid: u32, ppid: u32, pgid: u32, comm: &str) -> ProcInfo {
        ProcInfo {
            pid,
            ppid,
            pgid,
            comm: comm.to_string(),
        }
    }

    fn manifest(name: &str, names: &str) -> LoadedManifest {
        let src = format!(
            "min_engine_version=\"0.1\"\n[identity]\nprocess_names=[{names}]\n[capture]\nvisible=[\"working\"]\n"
        );
        let manifest = Manifest::parse(&src, "t.toml").unwrap();
        let engine = tma_core::RuleEngine::build(&manifest).unwrap();
        LoadedManifest {
            name: name.to_string(),
            manifest,
            engine,
        }
    }

    /// A title-narrowed manifest (the cursor shape): a generic `process_names` plus
    /// `title_patterns` that gate it.
    fn titled_manifest(name: &str, names: &str, patterns: &str) -> LoadedManifest {
        let src = format!(
            "min_engine_version=\"0.1\"\n[identity]\nprocess_names=[{names}]\ntitle_patterns=[{patterns}]\n[capture]\nvisible=[\"working\"]\n"
        );
        let manifest = Manifest::parse(&src, "t.toml").unwrap();
        let engine = tma_core::RuleEngine::build(&manifest).unwrap();
        LoadedManifest {
            name: name.to_string(),
            manifest,
            engine,
        }
    }

    #[test]
    fn is_ignored_reads_any_non_empty_value() {
        let mut options = std::collections::HashMap::new();
        assert!(!is_ignored(&options), "an absent option ignores nothing");
        // tmux reports an unset option as empty, and the read path drops empty values; treating one
        // as "ignore" would silence every pane on a server that ever set it.
        options.insert(opt::IGNORE.to_string(), String::new());
        assert!(!is_ignored(&options));
        for value in ["1", "0", "false", "dev server"] {
            options.insert(opt::IGNORE.to_string(), value.to_string());
            assert!(is_ignored(&options), "{value:?} is a non-empty value");
        }
    }

    fn agent<'a>(id: &'a PaneIdentity<'a>) -> &'a Identified<'a> {
        match id {
            PaneIdentity::Agent(i) => i,
            _ => panic!("expected Agent"),
        }
    }

    // --- procps format (bare comm token) ----------------------------------------

    #[test]
    fn procps_direct_launch_is_foreground_agent() {
        // procps `ps -eo pid,ppid,pgid,comm`: comm is a bare token.
        let procs = vec![proc(200, 100, 200, "claude")];
        let ms = vec![manifest("claude", "\"claude\"")];
        let id = identify(200, "claude", "", &procs, &ms, None, None);
        let i = agent(&id);
        assert_eq!(i.agent_pid, 200);
        assert!(i.foreground_is_agent);
        assert_eq!(i.source, IdentitySource::Observed);
    }

    #[test]
    fn procps_nested_agent_found_by_walk_not_foreground() {
        // Shell foreground, claude nested (wrapper). The walk finds it; the foreground cap applies.
        let procs = vec![
            proc(100, 1, 100, "zsh"),
            proc(200, 100, 100, "claude"),
            proc(300, 200, 100, "node"),
        ];
        let ms = vec![manifest("claude", "\"claude\"")];
        let id = identify(100, "zsh", "", &procs, &ms, None, None);
        let i = agent(&id);
        assert_eq!(i.agent_pid, 200);
        assert!(!i.foreground_is_agent, "foreground is the shell");
    }

    // --- BSD/macOS format (comm carries argv[0] + args) -------------------------

    #[test]
    fn bsd_comm_with_path_and_args_normalizes_to_basename() {
        // macOS `ps` reports comm as the full invocation; normalize_comm takes the basename
        // of the first token. `/opt/homebrew/bin/claude --foo` ⇒ `claude`.
        let procs = vec![
            proc(100, 1, 100, "-zsh"),
            proc(220, 100, 220, "/opt/homebrew/bin/claude --resume"),
        ];
        let ms = vec![manifest("claude", "\"claude\"")];
        let id = identify(100, "zsh", "", &procs, &ms, None, None);
        assert_eq!(agent(&id).agent_pid, 220);
    }

    // --- process-group-leader preference -----------------------------------

    #[test]
    fn prefers_process_group_leader_among_matches() {
        // Two matching processes; the group leader (pid==pgid) wins over a lower-pid child.
        let procs = vec![
            proc(100, 1, 100, "zsh"),
            proc(150, 200, 200, "claude"), // child worker, not the leader
            proc(200, 100, 200, "claude"), // the group leader
        ];
        let ms = vec![manifest("claude", "\"claude\"")];
        let id = identify(100, "zsh", "", &procs, &ms, None, None);
        assert_eq!(
            agent(&id).agent_pid,
            200,
            "group leader preferred over lower pid"
        );
    }

    #[test]
    fn falls_back_to_lowest_pid_when_no_leader_matches() {
        let procs = vec![
            proc(100, 1, 100, "zsh"),
            proc(300, 100, 100, "claude"),
            proc(250, 100, 100, "claude"),
        ];
        let ms = vec![manifest("claude", "\"claude\"")];
        let id = identify(100, "zsh", "", &procs, &ms, None, None);
        assert_eq!(agent(&id).agent_pid, 250);
    }

    // --- remote shells --------------------------------------------------------

    #[test]
    fn ssh_foreground_is_out_of_scope_not_an_agent() {
        // Even if a stray local child matches, an ssh pane must not be claimed.
        let procs = vec![proc(100, 1, 100, "ssh"), proc(200, 100, 100, "claude")];
        let ms = vec![manifest("claude", "\"claude\"")];
        match identify(100, "ssh", "", &procs, &ms, None, None) {
            PaneIdentity::Remote("ssh") => {}
            _ => panic!("ssh pane must be Remote(ssh)"),
        }
    }

    #[test]
    fn docker_and_mosh_are_remote() {
        let ms = vec![manifest("claude", "\"claude\"")];
        assert!(matches!(
            identify(1, "docker", "", &[proc(1, 0, 1, "docker")], &ms, None, None),
            PaneIdentity::Remote("docker")
        ));
        assert!(matches!(
            identify(
                1,
                "mosh-client",
                "",
                &[proc(1, 0, 1, "mosh-client")],
                &ms,
                None,
                None
            ),
            PaneIdentity::Remote("mosh-client")
        ));
    }

    // --- nested multiplexers ----------------------------------------------------

    #[test]
    fn a_nested_multiplexer_client_is_named_not_invisible() {
        // The inner server's processes are not in this pane's tree, so the walk finds nothing.
        // Before the guard that yielded PaneIdentity::None: no row, no explanation.
        let procs = vec![proc(100, 1, 100, "tmux")];
        let ms = vec![manifest("claude", "\"claude\"")];
        for mux in ["tmux", "zellij", "screen", "dvtm", "abduco"] {
            let procs = vec![proc(100, 1, 100, mux)];
            let id = identify(100, mux, "", &procs, &ms, None, None);
            assert!(
                matches!(id, PaneIdentity::Multiplexer(m) if m == mux),
                "{mux} foreground must classify as a nested multiplexer, not None"
            );
            assert_eq!(
                id.out_of_scope().map(|s| s.token()),
                Some("nested_multiplexer")
            );
        }
        // The guard runs before the walk, so a coincidental local child cannot claim the pane —
        // the composited inner screen is not this agent's, and its rules must not match it.
        let mut with_child = procs.clone();
        with_child.push(proc(200, 100, 100, "claude"));
        assert!(matches!(
            identify(100, "tmux", "", &with_child, &ms, None, None),
            PaneIdentity::Multiplexer("tmux")
        ));
    }

    // --- a registration outranks the carve-outs ---------------------------------

    #[test]
    fn a_registration_claims_a_pane_behind_a_remote_shell() {
        // `docker exec … claude`: the agent's process lives in the container, so the walk sees only
        // the client. The hook that stamped `@agent_session` fired IN this pane, which is what makes
        // the claim honest — the pane is an agent pane whose evidence arrives over the hook path.
        let procs = vec![proc(100, 1, 100, "docker")];
        let ms = vec![manifest("claude", "\"claude\"")];
        let reg = Registration {
            agent_name: "claude".into(),
            session: Some("sess-1".into()),
        };
        let id = identify(100, "docker", "", &procs, &ms, None, Some(&reg));
        let i = agent(&id);
        assert_eq!(i.agent_pid, 0, "nothing walkable this side of the boundary");
        assert_eq!(i.source, IdentitySource::Registered);
        assert_eq!(i.behind, Some(OutOfScope::RemoteShell("docker")));
        assert_eq!(
            id.out_of_scope(),
            None,
            "a claimed pane is in scope: the cycle must not remove its stamps"
        );
    }

    #[test]
    fn a_registration_claims_a_pane_behind_a_nested_multiplexer() {
        let procs = vec![proc(100, 1, 100, "tmux")];
        let ms = vec![manifest("claude", "\"claude\"")];
        let reg = Registration {
            agent_name: "claude".into(),
            session: Some("sess-1".into()),
        };
        let id = identify(100, "tmux", "", &procs, &ms, None, Some(&reg));
        assert_eq!(agent(&id).behind, Some(OutOfScope::Multiplexer("tmux")));
    }

    #[test]
    fn an_unregistered_pane_behind_a_carveout_stays_invisible() {
        // The non-regression that keeps the carve-outs worth having: with no registration (or one
        // naming an agent no manifest answers to) the pane is out of scope — no identity, no row,
        // and the cycle removes any stamp left on it.
        let ms = vec![manifest("claude", "\"claude\"")];
        let stale = Registration {
            agent_name: "uninstalled-agent".into(),
            session: Some("sess-1".into()),
        };
        for (cmd, kind) in [("tmux", "nested_multiplexer"), ("docker", "remote_shell")] {
            let procs = vec![proc(100, 1, 100, cmd)];
            let bare = identify(100, cmd, "", &procs, &ms, None, None);
            assert_eq!(bare.out_of_scope().map(|s| s.token()), Some(kind));
            let leftover = identify(100, cmd, "", &procs, &ms, None, Some(&stale));
            assert_eq!(
                leftover.out_of_scope().map(|s| s.token()),
                Some(kind),
                "a claim for an agent with no manifest is not identity"
            );
        }
    }

    #[test]
    fn out_of_scope_hints_name_the_command_and_the_fix() {
        let remote = identify(1, "ssh", "", &[proc(1, 0, 1, "ssh")], &[], None, None)
            .out_of_scope()
            .expect("ssh is out of scope");
        assert_eq!(remote.command(), "ssh");
        assert_eq!(remote.token(), "remote_shell");
        assert!(remote.hint().contains("remote shell ssh"));

        let mux = identify(1, "tmux", "", &[proc(1, 0, 1, "tmux")], &[], None, None)
            .out_of_scope()
            .expect("a nested tmux is out of scope");
        assert_eq!(mux.command(), "tmux");
        assert!(
            mux.hint().contains("inner server") && mux.hint().contains("run tma there"),
            "the hint says where the state actually lives: {}",
            mux.hint()
        );
    }

    // --- non-agent / registration slot ------------------------------------------

    #[test]
    fn plain_shell_pane_is_none() {
        let procs = vec![proc(100, 1, 100, "zsh"), proc(200, 100, 100, "vim")];
        let ms = vec![manifest("claude", "\"claude\"")];
        assert!(matches!(
            identify(100, "zsh", "", &procs, &ms, None, None),
            PaneIdentity::None
        ));
    }

    #[test]
    fn registration_corroborates_observation_as_registered() {
        let procs = vec![proc(200, 100, 200, "claude")];
        let ms = vec![manifest("claude", "\"claude\"")];
        let reg = Registration {
            agent_name: "claude".into(),
            session: Some("sess-1".into()),
        };
        let id = identify(200, "claude", "", &procs, &ms, None, Some(&reg));
        assert_eq!(agent(&id).source, IdentitySource::Registered);
    }

    #[test]
    fn registration_without_process_is_registered_with_unknown_pid() {
        // Agent registered via hook but no walkable process yet: honored as registered.
        let procs = vec![proc(100, 1, 100, "zsh")];
        let ms = vec![manifest("claude", "\"claude\"")];
        let reg = Registration {
            agent_name: "claude".into(),
            session: None,
        };
        let id = identify(100, "zsh", "", &procs, &ms, None, Some(&reg));
        let i = agent(&id);
        assert_eq!(i.source, IdentitySource::Registered);
        assert_eq!(i.agent_pid, 0);
    }

    // --- title narrowing + flicker stickiness ---------------------------

    #[test]
    fn shipped_process_only_manifest_never_sets_title_anchor() {
        // A process-only manifest (claude etc.) reports title_match_pid == None, so the producer
        // leaves @tma_title_match_pid untouched — the drift-critical invariant.
        let procs = vec![proc(200, 100, 200, "claude")];
        let ms = vec![manifest("claude", "\"claude\"")];
        let id = identify(200, "claude", "Cursor Agent", &procs, &ms, None, None);
        assert_eq!(agent(&id).title_match_pid, None);
    }

    #[test]
    fn title_narrowed_manifest_needs_title_match() {
        // process matches (`node`) but the title does not and no anchor is held: NOT this agent.
        let procs = vec![proc(300, 100, 300, "node")];
        let ms = vec![titled_manifest("cursor", "\"node\"", "\"Cursor Agent\"")];
        assert!(matches!(
            identify(300, "node", "some other title", &procs, &ms, None, None),
            PaneIdentity::None
        ));
        // With a matching title, it is claimed and reports the anchor pid to stamp.
        let id = identify(300, "node", "Cursor Agent", &procs, &ms, None, None);
        assert_eq!(agent(&id).agent_pid, 300);
        assert_eq!(agent(&id).title_match_pid, Some(300));
    }

    #[test]
    fn title_hold_survives_flicker_while_pid_unchanged() {
        // The pane flickered to a tool-name title, but the stored anchor equals the live agent
        // pid, so the match holds (identity does not drop mid-action).
        let procs = vec![proc(300, 100, 300, "node")];
        let ms = vec![titled_manifest("cursor", "\"node\"", "\"Cursor Agent\"")];
        let id = identify(
            300,
            "node",
            "Shell Command Output",
            &procs,
            &ms,
            Some(300),
            None,
        );
        assert_eq!(agent(&id).agent_pid, 300);
        assert_eq!(
            agent(&id).title_match_pid,
            Some(300),
            "the hold re-affirms the anchor for the same pid"
        );
    }

    #[test]
    fn title_hold_releases_on_pid_change() {
        // A new agent pid (agent restarted) with a non-matching title: the stale anchor (old pid)
        // no longer holds, so the pane is not claimed and the producer will clear the anchor.
        let procs = vec![proc(400, 100, 400, "node")];
        let ms = vec![titled_manifest("cursor", "\"node\"", "\"Cursor Agent\"")];
        assert!(
            matches!(
                identify(
                    400,
                    "node",
                    "Shell Command Output",
                    &procs,
                    &ms,
                    Some(300),
                    None
                ),
                PaneIdentity::None
            ),
            "a stale anchor for a dead pid must not hold identity on a new pid"
        );
    }

    #[test]
    fn registration_claims_title_narrowed_pane_without_title_match() {
        // A hook-registered cursor pane whose title is not "Cursor Agent" and has no anchor is
        // still claimed via the observation loop with its REAL walkable pid (registration is
        // authoritative identity), reporting no title anchor.
        let procs = vec![proc(300, 100, 300, "node")];
        let ms = vec![titled_manifest("cursor", "\"node\"", "\"Cursor Agent\"")];
        let reg = Registration {
            agent_name: "cursor".into(),
            session: Some("s1".into()),
        };
        let id = identify(
            300,
            "node",
            "Shell Command Output",
            &procs,
            &ms,
            None,
            Some(&reg),
        );
        let i = agent(&id);
        assert_eq!(
            i.agent_pid, 300,
            "claimed with the real pid, not the pid-less register branch"
        );
        assert_eq!(i.source, IdentitySource::Registered);
        assert_eq!(
            i.title_match_pid, None,
            "a registered pane needs no title anchor"
        );
    }

    #[test]
    fn hookless_gemini_pane_classifies_via_title_while_bare_node_does_not() {
        // gemini runs as `node` on both read paths, narrowed by its state titles. A hookless
        // gemini pane (node process + a real gemini title, NO registration) must classify via
        // title + process; an identically-shaped bare `node` pane with a plain title must not.
        let procs = vec![proc(500, 100, 500, "node")];
        let gemini = vec![titled_manifest(
            "gemini",
            "\"node\"",
            "\"^◇  Ready \", \"^✦  Working…\", \"^✋  Action Required \"",
        )];
        // Every observed gemini state title claims the pane (idle/working/blocked all covered).
        for title in [
            "◇  Ready (work)",
            "✦  Working… (work)",
            "✋  Action Required (work)",
        ] {
            let id = identify(500, "node", title, &procs, &gemini, None, None);
            let i = agent(&id);
            assert_eq!(i.agent_pid, 500, "gemini title {title:?} claims the pane");
            assert_eq!(i.source, IdentitySource::Observed, "no hook: observed only");
            assert_eq!(
                i.title_match_pid,
                Some(500),
                "a title-narrowed claim reports the anchor pid to stamp"
            );
        }
        // A bare node pane (a dev server, a REPL) with a plain title is NOT gemini: process alone
        // never claims a title-narrowed manifest. This is why ["node"] is safe here.
        assert!(matches!(
            identify(500, "node", "node", &procs, &gemini, None, None),
            PaneIdentity::None
        ));
        assert!(matches!(
            identify(500, "node", "my-dev-server", &procs, &gemini, None, None),
            PaneIdentity::None
        ));
    }

    #[test]
    fn title_anchor_command_is_idempotent_and_scoped() {
        // No write when the stored anchor already equals the desired pid.
        assert!(title_anchor_command("%1", Some("300"), Some(300)).is_none());
        // No write when a process-only pane (desired None) has no stored anchor.
        assert!(title_anchor_command("%1", None, None).is_none());
        // A set on a fresh/changed match, a clear on a stale anchor.
        assert!(title_anchor_command("%1", None, Some(300)).is_some());
        assert!(title_anchor_command("%1", Some("300"), Some(400)).is_some());
        assert!(title_anchor_command("%1", Some("300"), None).is_some());
    }

    // --- shell-only classification (the dead-registration reaper's discriminator) --------

    #[test]
    fn shell_only_subtree_is_a_dead_agent() {
        // The agent process is gone; only the pane's shell remains → eligible for the timed reap.
        let sub = [proc(100, 1, 100, "zsh")];
        let refs: Vec<&ProcInfo> = sub.iter().collect();
        assert!(subtree_is_shell_only(&refs));
    }

    #[test]
    fn login_shell_dash_prefix_is_a_shell() {
        // A macOS login pane: `login` then `-zsh`. Both are shells → still shell-only.
        let sub = [proc(90, 1, 90, "login"), proc(100, 90, 100, "-zsh")];
        let refs: Vec<&ProcInfo> = sub.iter().collect();
        assert!(subtree_is_shell_only(&refs));
    }

    #[test]
    fn live_unnamed_agent_subtree_is_not_shell_only() {
        // gemini's steady state: comm is `node` (matches no `process_names`), but that non-shell
        // process is always in the subtree — so the reaper must hold it forever, never reap.
        let sub = [proc(100, 1, 100, "zsh"), proc(200, 100, 200, "node")];
        let refs: Vec<&ProcInfo> = sub.iter().collect();
        assert!(!subtree_is_shell_only(&refs));
    }

    #[test]
    fn empty_subtree_is_shell_only() {
        // The pane's root process is gone entirely (no ProcInfo for it): definitely dead.
        assert!(subtree_is_shell_only(&[]));
    }

    #[test]
    fn direct_non_shell_root_is_not_shell_only() {
        // A pane launched with a direct command (e.g. an editor) is never reaped: a non-shell
        // root is not shell-only, so a stale registration on it holds rather than being cleared.
        let sub = [proc(100, 1, 100, "vim")];
        let refs: Vec<&ProcInfo> = sub.iter().collect();
        assert!(!subtree_is_shell_only(&refs));
    }

    #[test]
    fn subtree_does_not_escape_to_siblings() {
        let procs = vec![
            proc(1, 0, 1, "init"),
            proc(100, 1, 100, "zsh"),
            proc(200, 100, 200, "claude"),
            proc(999, 1, 999, "other-claude"),
        ];
        let sub = subtree(100, &procs);
        let pids: Vec<u32> = sub.iter().map(|p| p.pid).collect();
        assert!(pids.contains(&100) && pids.contains(&200));
        assert!(!pids.contains(&999) && !pids.contains(&1));
    }
}
