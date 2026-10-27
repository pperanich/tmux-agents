//! Coherent per-process facts for protocol ownership. Legacy polling keeps its
//! cheap `ps_all` path; the optional protocol runtime requests this separately.

use std::collections::BTreeMap;
use tma_core::snapshot::{
    ProcInfo, ProcessBirth, ProcessIdentity, ProcessSample, ProcessUnavailable,
};

const MAX_PROCESS_SAMPLES: usize = 65_536;

/// Each successful row contains metadata and birth from one kernel read. A fleet
/// snapshot is not atomic: callers compare the complete ancestry around a batch.
#[derive(Clone, Debug, Default)]
pub struct ProcessSnapshot {
    processes: Vec<ProcInfo>,
    births: BTreeMap<u32, Result<ProcessBirth, ProcessUnavailable>>,
}

impl ProcessSnapshot {
    pub fn from_samples(
        samples: impl IntoIterator<Item = (u32, Result<ProcessSample, ProcessUnavailable>)>,
    ) -> Self {
        let mut snapshot = Self::default();
        for (pid, sample) in samples {
            match sample {
                Ok(sample) if sample.info.pid == pid => {
                    snapshot.births.insert(pid, Ok(sample.birth));
                    snapshot.processes.push(sample.info);
                }
                Ok(_) => {
                    snapshot
                        .births
                        .insert(pid, Err(ProcessUnavailable::Malformed));
                }
                Err(reason) => {
                    snapshot.births.insert(pid, Err(reason));
                }
            }
        }
        snapshot
    }

    pub fn processes(&self) -> &[ProcInfo] {
        &self.processes
    }

    pub fn identity(&self, pid: u32) -> Result<ProcessIdentity, ProcessUnavailable> {
        self.births
            .get(&pid)
            .copied()
            .unwrap_or(Err(ProcessUnavailable::NotFound))
            .map(|birth| ProcessIdentity { pid, birth })
    }
}

/// Fresh complete pane-root-to-agent lineage. A PID outside the target tree, an unavailable
/// birth, a loop or more than 256 processes cannot establish metadata/history continuity.
pub fn sample_process_chain(
    pane_pid: u32,
    agent_pid: u32,
) -> Result<Vec<ProcessIdentity>, ProcessUnavailable> {
    let mut chain: Vec<ProcessIdentity> = Vec::new();
    let mut pid = agent_pid;
    for _ in 0..256 {
        if pid == 0 || pane_pid == 0 {
            return Err(ProcessUnavailable::NotFound);
        }
        if chain.iter().any(|process| process.pid == pid) {
            return Err(ProcessUnavailable::Malformed);
        }
        let sample = sample_program_process(pid)?;
        chain.push(ProcessIdentity {
            pid,
            birth: sample.birth,
        });
        if pid == pane_pid {
            chain.reverse();
            return Ok(chain);
        }
        pid = sample.info.ppid;
    }
    Err(ProcessUnavailable::Malformed)
}

fn unavailable(error: std::io::Error) -> ProcessUnavailable {
    match error.kind() {
        std::io::ErrorKind::NotFound => ProcessUnavailable::NotFound,
        std::io::ErrorKind::PermissionDenied => ProcessUnavailable::PermissionDenied,
        _ if matches!(error.raw_os_error(), Some(3)) => ProcessUnavailable::NotFound,
        _ => ProcessUnavailable::ReadFailed,
    }
}

/// Read one PID without mixing `ps` metadata with a later PID incarnation.
#[cfg(target_os = "macos")]
pub fn sample_program_process(pid: u32) -> Result<ProcessSample, ProcessUnavailable> {
    let pid_arg = i32::try_from(pid).map_err(|_| ProcessUnavailable::NotFound)?;
    // This API returns an opaque English error, not an io::Error. Do not infer
    // errno by parsing it. Failure means unavailable, including exit/permissions.
    let info = libproc::proc_pid::pidinfo::<libproc::bsd_info::BSDInfo>(pid_arg, 0)
        .map_err(|_| ProcessUnavailable::ReadFailed)?;
    macos_sample(pid, &info)
}

#[cfg(target_os = "macos")]
fn macos_sample(
    pid: u32,
    info: &libproc::bsd_info::BSDInfo,
) -> Result<ProcessSample, ProcessUnavailable> {
    if info.pbi_pid != pid || info.pbi_start_tvusec >= 1_000_000 || info.pbi_start_tvsec == 0 {
        return Err(ProcessUnavailable::Malformed);
    }
    // sys/proc.h SZOMB: exited, awaiting its parent's collection. A stopped
    // process (SSTOP, 4) remains live and can still own a silent report.
    if info.pbi_status == 5 {
        return Err(ProcessUnavailable::NotFound);
    }
    let name = if info.pbi_name[0] == 0 {
        &info.pbi_comm[..]
    } else {
        &info.pbi_name[..]
    };
    let bytes: Vec<_> = name
        .iter()
        .take_while(|byte| **byte != 0)
        .map(|byte| *byte as u8)
        .collect();
    let comm = String::from_utf8(bytes).map_err(|_| ProcessUnavailable::Malformed)?;
    if comm.is_empty() {
        return Err(ProcessUnavailable::Malformed);
    }
    Ok(ProcessSample {
        info: ProcInfo {
            pid,
            ppid: info.pbi_ppid,
            pgid: info.pbi_pgid,
            tpgid: (info.e_tpgid > 0 && info.e_tpgid <= i32::MAX as u32).then_some(info.e_tpgid),
            comm,
        },
        birth: ProcessBirth::MacOsStart {
            seconds: info.pbi_start_tvsec,
            microseconds: info.pbi_start_tvusec as u32,
        },
    })
}

#[cfg(target_os = "linux")]
pub fn sample_program_process(pid: u32) -> Result<ProcessSample, ProcessUnavailable> {
    use std::io::Read;
    let file = std::fs::File::open(format!("/proc/{pid}/stat")).map_err(unavailable)?;
    let mut stat = String::new();
    file.take(16 * 1024 + 1)
        .read_to_string(&mut stat)
        .map_err(unavailable)?;
    if stat.len() > 16 * 1024 {
        return Err(ProcessUnavailable::Malformed);
    }
    parse_linux_stat(pid, &stat)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn sample_program_process(_pid: u32) -> Result<ProcessSample, ProcessUnavailable> {
    Err(ProcessUnavailable::Unsupported)
}

/// Start time is field 22 in raw kernel ticks. The command can contain both
/// spaces and closing parentheses, so numeric fields begin after its LAST `)`.
#[cfg(any(target_os = "linux", test))]
fn parse_linux_stat(pid: u32, stat: &str) -> Result<ProcessSample, ProcessUnavailable> {
    let malformed = || ProcessUnavailable::Malformed;
    let (prefix, fields) = stat.rsplit_once(')').ok_or_else(malformed)?;
    let (reported_pid, comm) = prefix.split_once(" (").ok_or_else(malformed)?;
    if reported_pid.parse::<u32>().ok() != Some(pid) || comm.is_empty() {
        return Err(malformed());
    }
    let fields: Vec<_> = fields.split_whitespace().collect();
    let number = |index: usize| {
        fields
            .get(index)
            .ok_or_else(malformed)?
            .parse::<u32>()
            .map_err(|_| malformed())
    };
    let tpgid = fields
        .get(5)
        .ok_or_else(malformed)?
        .parse::<i64>()
        .map_err(|_| malformed())?;
    let ticks = fields
        .get(19)
        .ok_or_else(malformed)?
        .parse::<u64>()
        .map_err(|_| malformed())?;
    if fields.first().is_none_or(|state| state.len() != 1) {
        return Err(malformed());
    }
    if matches!(fields[0], "Z" | "X" | "x") {
        return Err(ProcessUnavailable::NotFound);
    }
    Ok(ProcessSample {
        info: ProcInfo {
            pid,
            ppid: number(1)?,
            pgid: number(2)?,
            tpgid: u32::try_from(tpgid).ok().filter(|pid| *pid > 0),
            comm: comm.to_owned(),
        },
        birth: ProcessBirth::LinuxStartTicks(ticks),
    })
}

/// One bounded fleet read per batch edge, with failures kept distinct from
/// absent processes. Enumeration supplies only PIDs, never recognition metadata.
pub fn program_processes() -> Result<ProcessSnapshot, ProcessUnavailable> {
    #[cfg(target_os = "macos")]
    let mut pids: Vec<u32> = libproc::processes::pids_by_type(libproc::processes::ProcFilter::All)
        .map_err(unavailable)?
        .into_iter()
        .filter(|pid| *pid > 0)
        .collect();
    #[cfg(target_os = "linux")]
    let mut pids: Vec<u32> = std::fs::read_dir("/proc")
        .map_err(unavailable)?
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
        .collect();
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    return Err(ProcessUnavailable::Unsupported);
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        pids.sort_unstable();
        pids.dedup();
        if pids.len() > MAX_PROCESS_SAMPLES {
            return Err(ProcessUnavailable::ReadFailed);
        }
        Ok(ProcessSnapshot::from_samples(
            pids.into_iter()
                .map(|pid| (pid, sample_program_process(pid))),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_stat_keeps_raw_ticks_and_parentheses_in_comm() {
        let sample = parse_linux_stat(
            42,
            "42 (node (worker) done)) S 1 42 42 0 -1 0 0 0 0 0 0 0 0 0 0 0 0 0 1234567890123 0",
        )
        .unwrap();
        assert_eq!(sample.info.comm, "node (worker) done)");
        assert_eq!(sample.info.ppid, 1);
        assert_eq!(sample.info.pgid, 42);
        assert_eq!(sample.info.tpgid, None);
        assert_eq!(sample.birth, ProcessBirth::LinuxStartTicks(1234567890123));
        let foreground =
            parse_linux_stat(42, "42 (pi) S 1 42 42 0 42 0 0 0 0 0 0 0 0 0 0 0 0 0 7").unwrap();
        assert_eq!(foreground.info.tpgid, Some(42));
    }

    #[test]
    fn linux_stat_rejects_missing_mixed_or_malformed_rows() {
        for stat in [
            "",
            "42 (pi) S",
            "99 (pi) S 1 42 42 0 42 0 0 0 0 0 0 0 0 0 0 0 0 0 7",
            "42 pi S 1 42",
            "42 (pi) S 1 42 42 0 42 0 0 0 0 0 0 0 0 0 0 0 0 nope",
        ] {
            assert_eq!(
                parse_linux_stat(42, stat),
                Err(ProcessUnavailable::Malformed)
            );
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn native_child_birth_is_stable_and_disappears_after_reaping() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let first = sample_program_process(child.id()).unwrap();
        let second = sample_program_process(child.id()).unwrap();
        assert_eq!(first.identity(), second.identity());
        #[cfg(target_os = "macos")]
        assert!(
            matches!(first.birth, ProcessBirth::MacOsStart { seconds, microseconds } if seconds > 0 && microseconds < 1_000_000)
        );
        let fleet = program_processes().unwrap();
        assert_eq!(fleet.identity(child.id()).unwrap(), first.identity());
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(sample_program_process(child.id()).is_err());
    }

    #[test]
    fn snapshot_preserves_permission_and_unknown_birth_failures() {
        let snapshot =
            ProcessSnapshot::from_samples([(12, Err(ProcessUnavailable::PermissionDenied))]);
        assert_eq!(
            snapshot.identity(12),
            Err(ProcessUnavailable::PermissionDenied)
        );
        assert_eq!(snapshot.identity(13), Err(ProcessUnavailable::NotFound));
        assert!(snapshot.processes().is_empty());
    }

    #[test]
    fn linux_stat_does_not_bind_an_exited_unreaped_process() {
        for state in ["Z", "X", "x"] {
            assert_eq!(
                parse_linux_stat(
                    42,
                    &format!("42 (pi) {state} 1 42 42 0 42 0 0 0 0 0 0 0 0 0 0 0 0 0 7")
                ),
                Err(ProcessUnavailable::NotFound)
            );
        }
        for state in ["T", "t"] {
            assert!(parse_linux_stat(
                42,
                &format!("42 (pi) {state} 1 42 42 0 42 0 0 0 0 0 0 0 0 0 0 0 0 0 7")
            )
            .is_ok());
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_exited_unreaped_child_is_not_live_protocol_identity() {
        use std::time::{Duration, Instant};
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "read ignored; exit 0"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        assert!(sample_program_process(child.id()).is_ok());
        drop(child.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(2);
        let result = loop {
            let sample = sample_program_process(child.id());
            if sample.is_err() {
                break sample;
            }
            if Instant::now() >= deadline {
                child.kill().ok();
                child.wait().unwrap();
                panic!("child did not reach unreaped exited status");
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        child.wait().unwrap();
        // Darwin can reject the whole BSDInfo query for a zombie, which the
        // opaque libproc error maps to ReadFailed rather than a parsed errno.
        assert!(matches!(
            result,
            Err(ProcessUnavailable::NotFound | ProcessUnavailable::ReadFailed)
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_kernel_status_rejects_exited_but_preserves_stopped_identity() {
        let pid = std::process::id();
        let mut info =
            libproc::proc_pid::pidinfo::<libproc::bsd_info::BSDInfo>(pid as i32, 0).unwrap();
        info.pbi_status = 4;
        assert!(macos_sample(pid, &info).is_ok());
        info.pbi_status = 5;
        assert_eq!(macos_sample(pid, &info), Err(ProcessUnavailable::NotFound));
    }
}
