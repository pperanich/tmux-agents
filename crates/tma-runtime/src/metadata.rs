//! Hook/API metadata has a process/session lifetime independent of the state winner.
use tma_core::persistence::{MetadataOwner, METADATA_KEYS};
use tma_core::snapshot::ProcessIdentity;
use tma_core::stamp::opt;
use tma_tmux::persistence::{self, CommitOutcome, OptionEdit, PaneRead, PersistenceError};
use tma_tmux::tmux::{sample_process_chain, sample_program_process, Tmux};

pub(crate) struct Binding {
    pub read: PaneRead,
    pub expected: Option<MetadataOwner>,
    pub next: MetadataOwner,
    processes: Vec<ProcessIdentity>,
}

fn current_processes(
    tmux: &Tmux,
    read: &PaneRead,
    manifest: Option<&crate::manifests::LoadedManifest>,
    registration: Option<&crate::identity::Registration>,
) -> Option<Vec<ProcessIdentity>> {
    let panes = tmux.list_panes().ok()?;
    let pane = panes.iter().find(|pane| pane.pane_id == read.pane_id)?;
    let known = read
        .owner()
        .map(|owner| owner.processes.agent().pid)
        .or_else(|| {
            read.options
                .get(opt::PID)?
                .parse()
                .ok()
                .filter(|pid| *pid != 0)
        });
    if let Some(pid) = known {
        if let Ok(chain) = sample_process_chain(pane.pane_pid, pid) {
            return Some(chain);
        }
        manifest?;
    }
    // A child binding stays valid before capture publishes PID. A root-only binding can be
    // earlier unattributed telemetry, so an intake may still recognize the actual child.
    let mut root_binding = None;
    if let Some(owner) = persistence::metadata_owner(read) {
        if let Ok(chain) = sample_process_chain(pane.pane_pid, owner.process.pid) {
            if chain.last() == Some(&owner.process) {
                if manifest.is_none() || chain.len() > 1 {
                    return Some(chain);
                }
                root_binding = Some(chain);
            }
        }
        if root_binding.is_none() {
            manifest?;
        }
    }
    if let Some(manifest) = manifest {
        // Couple manifest recognition to the native birth snapshot, then sample it again.
        let processes = tma_tmux::tmux::program_processes().ok()?;
        let anchor = read
            .options
            .get(opt::TITLE_MATCH_PID)
            .and_then(|raw| crate::identity::TitleAnchor::parse(raw));
        if let crate::identity::PaneIdentity::Agent(agent) = crate::identity::identify(
            pane.pane_pid,
            &pane.current_command,
            &pane.title,
            processes.processes(),
            std::slice::from_ref(manifest),
            anchor,
            registration,
        ) {
            if agent.agent_pid != 0 {
                let chain = sample_process_chain(pane.pane_pid, agent.agent_pid).ok()?;
                return chain
                    .iter()
                    .all(|process| processes.identity(process.pid).ok() == Some(*process))
                    .then_some(chain);
            }
        }
    }
    root_binding.or_else(|| sample_process_chain(pane.pane_pid, pane.pane_pid).ok())
}

/// A foreign delayed session cannot bind over a live same-process replacement. SessionStart
/// may bind after protocol acquisition; SessionEnd releases the binding before the next start.
pub(crate) fn bind(
    tmux: &Tmux,
    pane: &str,
    session: Option<&str>,
    subagent: bool,
) -> Option<Binding> {
    let read = persistence::read_pane(tmux, pane).ok()?;
    let binding = bind_observed(tmux, read, session, subagent, None)?;
    // Existing-binding consumers cannot promote a delayed operation to a new process birth.
    if binding
        .expected
        .as_ref()
        .is_some_and(|owner| owner.process != binding.next.process)
    {
        return None;
    }
    Some(binding)
}

pub(crate) fn bind_observed(
    tmux: &Tmux,
    read: PaneRead,
    session: Option<&str>,
    subagent: bool,
    manifest: Option<&crate::manifests::LoadedManifest>,
) -> Option<Binding> {
    let expected = persistence::metadata_owner(&read);
    let registration = read
        .options
        .get(opt::NAME)
        .zip(read.options.get(opt::SESSION))
        .map(|(name, session)| crate::identity::Registration {
            agent_name: name.clone(),
            session: Some(session.clone()),
        })
        .or_else(|| {
            if subagent {
                return None;
            }
            Some(crate::identity::Registration {
                agent_name: manifest?.name.clone(),
                session: Some(session?.to_owned()),
            })
        });
    let processes = current_processes(tmux, &read, manifest, registration.as_ref())?;
    let process = *processes.last()?;
    let current_session = expected
        .as_ref()
        .filter(|owner| owner.process == process)
        .map(|owner| owner.session.as_str())
        .or_else(|| {
            expected
                .is_none()
                .then(|| read.options.get(opt::SESSION).map(String::as_str))
                .flatten()
        });
    let current_session = current_session.filter(|session| *session != "<unattributed>");
    let session = if subagent {
        current_session
    } else {
        if matches!((current_session, session), (Some(current), Some(event)) if current != event) {
            return None;
        }
        session.or(current_session)
    }
    .unwrap_or("<unattributed>");
    let next = MetadataOwner {
        process,
        session: session.into(),
    };
    MetadataOwner::parse(&next.encode())?;
    Some(Binding {
        read,
        expected,
        next,
        processes,
    })
}

pub(crate) fn write(tmux: &Tmux, binding: &Binding, edits: &[OptionEdit], release: bool) -> bool {
    let next = (!release).then_some(&binding.next);
    let mut replacement = Vec::new();
    if binding
        .expected
        .as_ref()
        .is_some_and(|owner| owner != &binding.next)
    {
        replacement.extend(METADATA_KEYS.iter().map(|key| OptionEdit::unset(key)));
    }
    replacement.extend_from_slice(edits);
    let edits = &replacement;
    let mut refreshed = None;
    for attempt in 0..=1 {
        let current = tmux.list_panes().ok().and_then(|panes| {
            let pane = panes
                .iter()
                .find(|pane| pane.pane_id == binding.read.pane_id)?;
            sample_process_chain(pane.pane_pid, binding.next.process.pid).ok()
        });
        if current.as_ref() != Some(&binding.processes) {
            return false;
        }
        let observed = refreshed.as_ref().unwrap_or(&binding.read);
        match persistence::write_metadata(tmux, observed, binding.expected.as_ref(), next, edits) {
            Ok(CommitOutcome::Won(_)) => return true,
            Ok(CommitOutcome::Lost(current))
                if attempt == 0 && same_attribution(binding, &current) =>
            {
                // A cycle may refresh only the state receipt. Re-admit once within
                // the original binding; an applied metadata write changes its update fence.
                refreshed = Some(current);
            }
            Ok(CommitOutcome::Lost(_)) => return false,
            Err(PersistenceError::Unsupported) => {
                return persistence::advisory_metadata(
                    tmux,
                    observed,
                    binding.expected.as_ref(),
                    next,
                    edits,
                )
                .is_ok()
            }
            Err(_) => return false,
        }
    }
    false
}

fn same_attribution(binding: &Binding, current: &PaneRead) -> bool {
    current.pane_id == binding.read.pane_id
        && current.target == binding.read.target
        && METADATA_KEYS
            .iter()
            .copied()
            .chain([
                tma_core::persistence::opt::METADATA_OWNER,
                tma_core::persistence::opt::METADATA_UPDATE,
                tma_core::persistence::opt::ATTENTION_EPISODE,
                opt::PID,
                opt::NAME,
                opt::SINCE,
                opt::TURN_AT,
            ])
            .all(|key| current.options.get(key) == binding.read.options.get(key))
}

/// Presentation metadata cannot lend a replacement process its old session or requests.
/// Unmarked genuine legacy metadata remains compatible outside program ownership.
pub(crate) fn visible(options: &std::collections::HashMap<String, String>) -> bool {
    let program = tma_core::persistence::ProgramMetadata::from_options(options);
    let Some(raw) = options
        .get(tma_core::persistence::opt::METADATA_OWNER)
        .filter(|raw| !raw.is_empty())
    else {
        return !program.program_origin;
    };
    let Some(owner) = MetadataOwner::parse(raw) else {
        return false;
    };
    if options
        .get(opt::SESSION)
        .is_some_and(|session| session != &owner.session)
    {
        return false;
    }
    if program.program_origin {
        program
            .owner
            .is_some_and(|program| program.processes.agent() == owner.process)
    } else {
        let pid = options
            .get(opt::PID)
            .and_then(|pid| pid.parse::<u32>().ok())
            .unwrap_or(0);
        pid == 0 || pid == owner.process.pid
    }
}

pub(crate) fn live_for_actions(options: &std::collections::HashMap<String, String>) -> bool {
    if !visible(options) {
        return false;
    }
    let Some(raw) = options
        .get(tma_core::persistence::opt::METADATA_OWNER)
        .filter(|raw| !raw.is_empty())
    else {
        return true;
    };
    let Some(owner) = MetadataOwner::parse(raw) else {
        return false;
    };
    sample_program_process(owner.process.pid)
        .is_ok_and(|sample| sample.birth == owner.process.birth)
}

pub(crate) fn release(tmux: &Tmux, binding: &Binding) -> bool {
    let edits: Vec<_> = METADATA_KEYS
        .iter()
        .map(|key| OptionEdit::unset(key))
        .collect();
    write(tmux, binding, &edits, true)
}
