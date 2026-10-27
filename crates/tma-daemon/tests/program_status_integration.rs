//! Production-binary OSC collection. Synthetic cat emission proves the daemon
//! path; native Pi activation remains a separate live gate.
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tma_core::persistence::{self as model, CollectorLease};
use tma_runtime::ipc;
use tma_test_support::{self as common, DaemonGuard, Scratch};
use tma_tmux::persistence as store;
use tma_tmux::tmux::{Server, Tmux};

struct Fixture {
    scratch: Scratch,
    daemon: DaemonGuard,
    pane: String,
    status: PathBuf,
    tmux: Tmux,
}
impl Fixture {
    fn command(scratch: &Scratch) -> Command {
        let mut command = scratch.command();
        command.args(["--socket-name", &scratch.socket]);
        command.args(["--config", scratch.config_path().to_str().unwrap()]);
        command.args(["--manifest-dir", scratch.manifest_dir().to_str().unwrap()]);
        command
    }
    fn new(mode: &str) -> Self {
        Self::new_with(mode, |_| None)
    }
    fn new_with(mode: &str, setup: impl FnOnce(&Scratch) -> Option<PathBuf>) -> Self {
        let scratch = Scratch::new_daemon("osc_daemon");
        scratch.write_manifest(
            "reporter.toml",
            r#"min_engine_version = "0.1"
[identity]
process_names = ["cat"]
[capture]
visible = ["working"]
[program_status]
apps = ["pi"]
[hooks]
covers = ["working", "lifecycle"]
[[hooks.map]]
event = "working"
claim = { state = "working" }
"#,
        );
        scratch.write_config(&Self::config(mode));
        assert!(scratch
            .tmux(&[
                "new-session",
                "-d",
                "-s",
                "reporter",
                "stty -echo; /bin/cat; exec sleep 100000"
            ])
            .status
            .success());
        let pane = scratch.get("reporter", "#{pane_id}");
        let status = scratch.workdir.join("program-status");
        let binary = setup(&scratch);
        let mut daemon_command = Self::command(&scratch);
        if let Some(binary) = binary {
            daemon_command.env("TMA_TMUX_BIN", binary);
        }
        let daemon = DaemonGuard::new(
            daemon_command
                .args(["daemon", "--status-file", status.to_str().unwrap()])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(scratch.daemon_log_stdio())
                .spawn()
                .unwrap(),
        );
        let tmux = Tmux::connect(&Server::named(Some(scratch.socket.clone())));
        let mut f = Self {
            scratch,
            daemon,
            pane,
            status,
            tmux,
        };
        common::poll_until("daemon attaches and independently binds reporter", || {
            let child = f.daemon.child_status();
            assert!(
                matches!(child, Ok(None)),
                "daemon left during initial binding: {child:?}; status={:?}; log={}",
                f.stats(),
                std::fs::read_to_string(f.scratch.daemon_log_path()).unwrap_or_default()
            );
            let s = f.stats();
            if mode == "off" {
                s.get("program_mode").is_some_and(|v| v == mode)
                    && s.get("pending_seeds").is_some_and(|v| v == "0")
            } else {
                s.get("program_mode").is_some_and(|v| v == mode)
                    && s.get(&format!("program_pane_{}", f.pane))
                        .is_some_and(|v| v.contains("BindingBoundary") && v.contains("owner=Some"))
            }
        });
        // First ST proves framing only. The following report is post-binding.
        f.emit("sync\x1b\\");
        f
    }
    fn config(mode: &str) -> String {
        format!("[program_status]\nmode = {mode:?}\n[daemon]\nquiet_ms = 10000\nsweep_secs = 60\n[notify]\non = [\"blocked\", \"done\"]\nbell = false\nosc = false\ncommand = \"printf '%s\\n' \\\"$TMA_STATE\\\" >> {}\"\n", "/dev/null")
    }
    fn stats(&self) -> BTreeMap<String, String> {
        std::fs::read_to_string(&self.status)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.split_once('=').map(|(k, v)| (k.into(), v.into())))
            .collect()
    }
    fn count(&self, key: &str) -> u64 {
        self.stats()
            .get(key)
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    }
    fn emit(&self, bytes: &str) {
        assert!(self
            .scratch
            .tmux(&["send-keys", "-t", &self.pane, "-l", bytes])
            .status
            .success());
        assert!(self
            .scratch
            .tmux(&["send-keys", "-t", &self.pane, "Enter"])
            .status
            .success());
    }
    fn report(&self, state: &str) {
        self.emit(&format!("\x1b]7501;state={state}:app=pi\x1b\\"));
    }
    fn read(&self) -> store::PaneRead {
        store::read_pane(&self.tmux, &self.pane).unwrap()
    }
    fn live(&self, state: &str) -> bool {
        matches!(self.read().projection(tma_runtime::now_ms()), Ok(model::ProjectionRead::Live { stamp, .. }) if stamp.state.token() == state)
    }
    fn wait_live(&self, state: &str) {
        common::poll_until(&format!("program live {state}"), || self.live(state));
    }
    fn reload(&self, mode: &str) {
        let before = std::fs::read_to_string(self.scratch.daemon_log_path())
            .unwrap_or_default()
            .matches("reloaded config + manifests")
            .count();
        self.scratch.write_config(&Self::config(mode));
        assert!(Self::command(&self.scratch)
            .arg("reload")
            .output()
            .unwrap()
            .status
            .success());
        common::poll_until("reloaded mode", || {
            std::fs::read_to_string(self.scratch.daemon_log_path())
                .unwrap_or_default()
                .matches("reloaded config + manifests")
                .count()
                > before
                && self.stats().get("program_mode").is_some_and(|v| v == mode)
        });
    }
    fn new_reporter(&mut self, name: &str) -> String {
        assert!(self
            .scratch
            .tmux(&[
                "new-window",
                "-t",
                "reporter",
                "-n",
                name,
                "stty -echo; exec /bin/cat"
            ])
            .status
            .success());
        let pane = self.scratch.get(&format!("reporter:{name}"), "#{pane_id}");
        common::poll_until("peer process bound", || {
            self.stats()
                .get(&format!("program_pane_{pane}"))
                .is_some_and(|value| value.contains("owner=Some") && value.contains("Recovering"))
        });
        let previous = std::mem::replace(&mut self.pane, pane.clone());
        self.emit("sync\x1b\\");
        self.report("working");
        self.wait_live("working");
        common::poll_until("peer working publication completed", || {
            self.stats()
                .get(&format!("program_pane_{pane}"))
                .is_some_and(|value| value.contains("selection=Some(Working"))
        });
        self.pane = previous;
        pane
    }
    fn marker(&self, marker: &str) {
        self.emit(marker);
        common::poll_until(
            "synthetic output reached tmux before next staged input",
            || {
                let output = self.scratch.tmux(&["capture-pane", "-p", "-t", &self.pane]);
                output.status.success() && String::from_utf8_lossy(&output.stdout).contains(marker)
            },
        );
    }
    fn hook_peer(&self, pane: &str) -> UnixStream {
        let socket = std::fs::read_dir(self.scratch.workdir.join("tma"))
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| path.extension().is_some_and(|ext| ext == "sock"))
            .unwrap();
        let mut peer = UnixStream::connect(socket).unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        peer.write_all(&ipc::encode_frame(
            pane,
            "reporter",
            "working",
            r#"{"session_id":"budget-hook"}"#,
        ))
        .unwrap();
        peer
    }
    fn signal(&self, signal: &str) {
        assert!(Command::new("kill")
            .args([signal, &self.daemon.pid().to_string()])
            .status()
            .unwrap()
            .success());
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!(
                "actual daemon child before teardown: {:?}",
                self.daemon.child_status()
            );
            for field in ["status", "stdout", "stderr"] {
                let path = self.scratch.workdir.join(format!("startup-socket-{field}"));
                if path.exists() {
                    eprintln!(
                        "startup socket {field}: {:?}",
                        std::fs::read_to_string(path)
                    );
                }
            }
            let read = self.read();
            eprintln!("typed persisted truth: owner={:?}, commit={:?}, result={:?}, ack={:?}, projection={:?}", read.program.owner, read.program.commit, read.program.result, read.program.acknowledged, read.projection(tma_runtime::now_ms()));
            eprintln!(
                "daemon status: {:?}\ndaemon log: {}",
                self.stats(),
                std::fs::read_to_string(self.scratch.daemon_log_path()).unwrap_or_default()
            );
        }
    }
}
fn available() -> bool {
    common::tmux_available()
}

fn gated_owner_read(scratch: &Scratch, ordinal: usize) -> PathBuf {
    let wrapper = scratch.workdir.join("review-owner-read-gate");
    std::fs::write(
        &wrapper,
        format!(
            r#"#!/bin/sh
tma_review_read=0
for arg do
case "$arg" in display-message) tma_review_read=1 ;; esac
done
if [ -e '{armed}' ] && [ ! -e '{ready}' ] && [ "$tma_review_read" = 1 ]; then
for arg do
case "$arg" in
*'pane_id'*'@tma_program_origin'*'after-set-option'*)
printf x >> '{count}'
if [ "$(wc -c < '{count}')" -eq {ordinal} ]; then
tmux "$@" > '{output}'; tma_review_result=$?
printf '%s\n' "$tma_review_result" > '{done}'
[ "$tma_review_result" = 0 ] || exit "$tma_review_result"
touch '{ready}'; i=0
while [ ! -e '{release}' ] && [ $i -lt 400 ]; do sleep 0.005; i=$((i+1)); done
if [ ! -e '{release}' ]; then touch '{expired}'; exit 124; fi
touch '{released}'; cat '{output}'; exit 0
fi
break ;;
esac
done
fi
if [ -e '{ready}' ]; then
for arg do
case "$arg" in
*'set-option'*'@tma_program_origin'*'@agent_state'*) printf 'publication\n' >> '{mutations}'; break ;;
*'set-option'*'@tma_program_collector'*) printf 'collector\n' >> '{mutations}'; break ;;
*'set-option'*'@tma_program_commit'*) printf 'pane\n' >> '{mutations}'; break ;;
esac
done
fi
exec tmux "$@"
"#,
            armed = scratch.workdir.join("review-read-armed").display(),
            ready = scratch.workdir.join("review-read-ready").display(),
            count = scratch.workdir.join("review-read-count").display(),
            output = scratch.workdir.join("review-read-output").display(),
            done = scratch.workdir.join("review-read-done").display(),
            release = scratch.workdir.join("review-read-release").display(),
            expired = scratch.workdir.join("review-read-expired").display(),
            released = scratch.workdir.join("review-read-released").display(),
            mutations = scratch.workdir.join("review-read-mutations").display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    wrapper
}

fn release_owner_read(f: &Fixture) {
    assert_eq!(
        std::fs::read_to_string(f.scratch.workdir.join("review-read-done"))
            .unwrap()
            .trim(),
        "0"
    );
    std::fs::write(f.scratch.workdir.join("review-read-release"), "release").unwrap();
}

#[test]
fn default_mode_collects_native_reports() {
    let _gate = common::DaemonTestGuard::acquire();
    if !available() {
        return;
    }
    let f = Fixture::new_with("prefer", |scratch| {
        scratch.write_config("[notify]\non = []\n");
        None
    });
    f.report("working");
    f.wait_live("working");
}

#[test]
fn term_during_maintenance_read_retires_lease_before_another_pane_mutation() {
    let _gate = common::DaemonTestGuard::acquire();
    if !available() {
        return;
    }
    let mut f = Fixture::new_with("prefer", |scratch| Some(gated_owner_read(scratch, 1)));
    f.report("working");
    f.wait_live("working");
    common::poll_until("working publication completed", || {
        f.count("program_publications") >= 1
    });
    let token = f.read().program.lease.unwrap().token;
    f.signal("-STOP");
    let session = f.scratch.get(&f.pane, "#{session_id}");
    assert!(f
        .scratch
        .tmux(&[
            "set-hook",
            "-t",
            &session,
            "after-set-option",
            "run-shell true"
        ])
        .status
        .success());
    std::fs::write(f.scratch.workdir.join("review-read-armed"), "armed").unwrap();
    std::thread::sleep(Duration::from_millis(1100));
    f.signal("-CONT");
    common::poll_until("maintenance read completed inside its gate", || {
        f.scratch.workdir.join("review-read-ready").exists()
    });
    f.signal("-TERM");
    release_owner_read(&f);
    assert!(f.daemon.wait_exit(Duration::from_secs(10)));
    assert!(!f.scratch.workdir.join("review-read-expired").exists());
    assert!(f.scratch.workdir.join("review-read-released").exists());
    let mutations =
        std::fs::read_to_string(f.scratch.workdir.join("review-read-mutations")).unwrap();
    assert_eq!(
        mutations.lines().next(),
        Some("collector"),
        "retire before new pane IO: {mutations}"
    );
    let read = f.read();
    assert_eq!(read.program.lease.as_ref().unwrap().token, token);
    assert_eq!(read.program.lease.as_ref().unwrap().expires_at, 0);
    assert!(!matches!(
        read.projection(tma_runtime::now_ms()),
        Ok(model::ProjectionRead::Live { .. })
    ));
}

#[test]
fn silent_ack_after_a_slow_read_rechecks_departed_agent_before_publication() {
    let _gate = common::DaemonTestGuard::acquire();
    if !available() {
        return;
    }
    let f = Fixture::new_with("prefer", |scratch| Some(gated_owner_read(scratch, 2)));
    f.report("error");
    f.wait_live("idle");
    common::poll_until("error fully established", || {
        f.count("program_publications") >= 1
            && f.stats()
                .get(&format!("program_pane_{}", f.pane))
                .is_some_and(|value| value.contains("selection=Some(Error)"))
    });
    f.signal("-STOP");
    assert!(Fixture::command(&f.scratch)
        .args(["clear-attention", &f.pane])
        .output()
        .unwrap()
        .status
        .success());
    let read = f.read();
    assert!(read.program.acknowledged.is_some());
    assert_eq!(read.program.acknowledged, read.program.result);
    std::fs::write(f.scratch.workdir.join("review-read-armed"), "armed").unwrap();
    std::thread::sleep(Duration::from_millis(1100));
    f.signal("-CONT");
    common::poll_until("ACK read completed inside its gate", || {
        f.scratch.workdir.join("review-read-ready").exists()
    });
    assert!(f
        .scratch
        .tmux(&["send-keys", "-t", &f.pane, "C-d"])
        .status
        .success());
    common::poll_until("agent exited with shell process still alive", || {
        f.scratch.get(&f.pane, "#{pane_current_command}") != "cat"
    });
    std::thread::sleep(Duration::from_millis(1100));
    release_owner_read(&f);
    common::poll_until("departed owner revoked", || {
        f.read().program.commit.is_none()
    });
    assert!(!f.scratch.workdir.join("review-read-expired").exists());
    assert!(f.scratch.workdir.join("review-read-released").exists());
    let mutations =
        std::fs::read_to_string(f.scratch.workdir.join("review-read-mutations")).unwrap();
    assert!(
        !mutations.lines().any(|line| line == "publication"),
        "departed process must not get an ACK projection: {mutations}"
    );
}

#[test]
fn closed_owned_pane_is_pruned_before_maintenance_and_preserves_healthy_peer() {
    let _gate = common::DaemonTestGuard::acquire();
    if !available() {
        return;
    }
    let mut f = Fixture::new_with("prefer", |scratch| {
        let wrapper = scratch.workdir.join("record-departed-target");
        std::fs::write(
            &wrapper,
            format!(
                r#"#!/bin/sh
if [ -e '{armed}' ] && [ ! -e '{ready}' ]; then
for arg do
case "$arg" in list-panes)
touch '{ready}'; i=0
while [ ! -e '{release}' ] && [ $i -lt 400 ]; do sleep 0.005; i=$((i+1)); done
if [ ! -e '{release}' ]; then touch '{expired}'; exit 124; fi
touch '{released}'
tmux "$@" > '{output}'; tma_fleet_result=$?
printf '%s\n' "$tma_fleet_result" > '{done}'
cat '{output}'; exit "$tma_fleet_result" ;;
esac
done
fi
if [ -e '{departed}' ]; then
tma_departed_target=$(cat '{departed}')
for arg do
if [ "$arg" = "$tma_departed_target" ]; then printf x >> '{calls}'; break; fi
done
fi
exec tmux "$@"
"#,
                departed = scratch.workdir.join("departed-target").display(),
                calls = scratch.workdir.join("departed-target-calls").display(),
                armed = scratch.workdir.join("fleet-armed").display(),
                ready = scratch.workdir.join("fleet-ready").display(),
                release = scratch.workdir.join("fleet-release").display(),
                released = scratch.workdir.join("fleet-released").display(),
                expired = scratch.workdir.join("fleet-expired").display(),
                output = scratch.workdir.join("fleet-output").display(),
                done = scratch.workdir.join("fleet-done").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
        Some(wrapper)
    });
    f.report("working");
    f.wait_live("working");
    let departed = f.pane.clone();
    f.pane = f.new_reporter("surviving-peer");
    common::poll_until("both publications confirmed before pane close", || {
        f.count("program_publications") >= 2 && f.count("program_stores") == 2
    });
    assert!(matches!(
        store::read_pane(&f.tmux, &departed)
            .unwrap()
            .projection(tma_runtime::now_ms()),
        Ok(model::ProjectionRead::Live { .. })
    ));
    let before = f.read();
    let lease = before.program.lease.as_ref().unwrap();
    let retirements = f.count("program_lease_retirements");
    let session = f.scratch.get(&departed, "#{session_id}");
    std::fs::write(f.scratch.workdir.join("fleet-armed"), "armed").unwrap();
    common::poll_until("next actual fleet read waits before execution", || {
        f.scratch.workdir.join("fleet-ready").exists()
    });
    assert!(f
        .scratch
        .tmux(&["kill-pane", "-t", &departed])
        .status
        .success());
    std::fs::write(
        f.scratch.workdir.join("departed-target"),
        format!("{session}:.{departed}"),
    )
    .unwrap();
    std::fs::write(f.scratch.workdir.join("fleet-release"), "release").unwrap();
    common::poll_until("actual fresh fleet read completed", || {
        f.scratch.workdir.join("fleet-done").exists()
    });
    assert_eq!(
        std::fs::read_to_string(f.scratch.workdir.join("fleet-done"))
            .unwrap()
            .trim(),
        "0"
    );
    assert!(!f.scratch.workdir.join("fleet-expired").exists());
    assert!(f.scratch.workdir.join("fleet-released").exists());
    common::poll_until("closed pane absent from service bookkeeping", || {
        f.count("program_stores") == 1
    });
    let after = f.read();
    assert_eq!(
        after.owner(),
        before.owner(),
        "ordinary pane close cannot revoke a healthy peer"
    );
    assert_eq!(after.program.lease.as_ref().unwrap().token, lease.token);
    assert_eq!(f.count("program_lease_retirements"), retirements);
    assert!(f.live("working"));
    common::poll_until("healthy peer renews through its own target", || {
        f.read().program.lease.is_some_and(|current| {
            current.token == lease.token && current.expires_at > lease.expires_at
        })
    });
    f.signal("-TERM");
    assert!(f.daemon.wait_exit(Duration::from_secs(10)));
    let retired = f.read().program.lease.unwrap();
    assert_eq!(retired.token, lease.token);
    assert_eq!(retired.expires_at, 0);
    assert!(
        !f.scratch.workdir.join("departed-target-calls").exists(),
        "fresh fleet absence must prevent later IO to the departed target"
    );
}

#[test]
fn off_and_observe_never_publish_protocol_effects_or_lease() {
    let _gate = common::DaemonTestGuard::acquire();
    if !available() {
        return;
    }
    for mode in ["off", "observe"] {
        let f = Fixture::new(mode);
        f.emit("\x1b]7501;?\x1b\\");
        f.report("done");
        if mode == "observe" {
            common::poll_until("observed complete native report", || {
                f.stats()
                    .get(&format!("program_pane_{}", f.pane))
                    .is_some_and(|v| v.contains("Active") && v.contains("last_report=Some"))
            });
            assert_eq!(f.count("program_publications"), 0);
            assert_eq!(f.count("program_notify_passes"), 0);
        } else {
            std::thread::sleep(Duration::from_millis(200));
            assert_eq!(f.count("program_stores"), 0);
        }
        let read = f.read();
        assert!(read.owner().is_none());
        assert!(read.program.lease.is_none());
        assert!(!read.options.contains_key("@agent_attention"));
        assert!(!read.options.contains_key("@agent_turn_at"));
        assert!(!read.options.contains_key(model::opt::NOTIFIED));
    }
}

#[test]
fn prefer_is_immediate_orders_done_working_clear_and_suppresses_duplicates() {
    let _gate = common::DaemonTestGuard::acquire();
    if !available() {
        return;
    }
    let f = Fixture::new("prefer");
    let started = Instant::now();
    f.report("blocked");
    f.wait_live("blocked");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "must precede configured quiet capture"
    );
    assert_eq!(f.count("on_demand_captures"), 0);
    common::poll_until("initial publication status flushed", || {
        f.count("program_publications") >= 1
    });
    let first = f.read();
    let commit = first.program.commit.clone();
    let publications = f.count("program_publications");
    let notify_passes = f.count("program_notify_passes");
    let reconciliations = f.count("notify_reconciliations");
    let duplicates = f.count("program_duplicates");
    f.emit(&"\x1b]7501;state=blocked:app=pi\x1b\\".repeat(20));
    common::poll_until("duplicate reports drained", || {
        f.count("program_duplicates") >= duplicates + 20
    });
    assert_eq!(f.count("program_publications"), publications);
    assert_eq!(f.count("program_notify_passes"), notify_passes);
    assert_eq!(f.count("notify_reconciliations"), reconciliations);
    assert_eq!(f.read().program.commit, commit);
    f.emit("\x1b]7501;state=done:app=pi\x1b\\\x1b]7501;state=working:app=pi\x1b\\");
    f.wait_live("working");
    let read = f.read();
    assert!(read.options.contains_key("@agent_turn_at"));
    common::poll_until("done notified before next working reduction", || {
        f.count("notify_fires") >= 2
    });
    f.emit("\x1b]7501;state=clear\x1b\\");
    common::poll_until("clear releases exact ownership", || {
        f.read().program.commit.is_none()
    });
    f.report("working");
    f.wait_live("working");
}

#[test]
fn observe_prefer_reload_has_fresh_boundary_and_invalid_reload_keeps_owner() {
    let _gate = common::DaemonTestGuard::acquire();
    if !available() {
        return;
    }
    let f = Fixture::new("observe");
    f.report("done");
    common::poll_until("observe retains report", || {
        f.stats()
            .get(&format!("program_pane_{}", f.pane))
            .is_some_and(|v| v.contains("Active"))
    });
    f.reload("prefer");
    common::poll_until("prefer fresh boundary", || {
        f.stats()
            .get(&format!("program_pane_{}", f.pane))
            .is_some_and(|v| v.contains("BindingBoundary"))
    });
    assert!(f.read().program.commit.is_none());
    assert!(!f.read().options.contains_key("@agent_turn_at"));
    f.emit("sync\x1b\\");
    f.report("working");
    f.wait_live("working");
    let owner = f.read().owner();
    let lease = f.read().program.lease.unwrap().token;
    f.scratch
        .write_config("[program_status]\nmode = \"invalid\"\n");
    f.signal("-HUP");
    common::poll_until("invalid reload logged", || {
        std::fs::read_to_string(f.scratch.daemon_log_path())
            .unwrap_or_default()
            .contains("reload failed")
    });
    assert_eq!(f.read().owner(), owner);
    f.reload("prefer");
    assert_eq!(
        f.read().owner(),
        owner,
        "unrelated reload retains healthy coverage"
    );
    assert_eq!(f.read().program.lease.unwrap().token, lease);
    f.reload("off");
    common::poll_until("disable releases commit and lease", || {
        let read = f.read();
        read.program.commit.is_none()
            && read
                .program
                .lease
                .is_none_or(|lease| !lease.live_at(tma_runtime::now_ms()))
    });
}

#[test]
fn silent_agent_departure_with_shell_alive_revokes_process_authority() {
    let _gate = common::DaemonTestGuard::acquire();
    if !available() {
        return;
    }
    let mut f = Fixture::new("prefer");
    f.report("working");
    f.wait_live("working");
    let peer = f.new_reporter("silent-peer");
    let peer_before = store::read_pane(&f.tmux, &peer).unwrap();
    assert!(f
        .scratch
        .tmux(&["send-keys", "-t", &f.pane, "C-d"])
        .status
        .success());
    common::poll_until("cat departed but pane survives", || {
        f.scratch.get(&f.pane, "#{pane_current_command}") != "cat"
    });
    common::poll_until("periodic ancestry check revokes silence", || {
        f.read().program.commit.is_none()
    });
    assert!(f
        .scratch
        .tmux(&["display-message", "-p", "-t", &f.pane, "#{pane_pid}"])
        .status
        .success());
    assert_eq!(f.count("on_demand_captures"), 0);
    let peer_after = store::read_pane(&f.tmux, &peer).unwrap();
    assert_eq!(peer_after.owner(), peer_before.owner());
    assert_eq!(
        peer_after.program.lease.as_ref().map(|lease| &lease.token),
        peer_before.program.lease.as_ref().map(|lease| &lease.token)
    );
    assert!(matches!(
        peer_after.projection(tma_runtime::now_ms()),
        Ok(model::ProjectionRead::Live { .. })
    ));
}

#[test]
fn shutdown_and_stolen_lease_revoke_exact_coverage_and_require_fresh_report() {
    let _gate = common::DaemonTestGuard::acquire();
    if !available() {
        return;
    }
    let mut f = Fixture::new("prefer");
    f.report("working");
    f.wait_live("working");
    let old = f.read().program.lease.unwrap();
    let replacement = CollectorLease {
        token: store::fresh_token().unwrap(),
        expires_at: tma_runtime::now_ms() + 15_000,
    };
    assert!(f
        .scratch
        .tmux(&[
            "set-option",
            "-s",
            model::opt::COLLECTOR,
            &replacement.encode()
        ])
        .status
        .success());
    common::poll_until("stolen persisted lease revokes current commit", || {
        f.read().program.commit.is_none()
    });
    assert_eq!(f.read().program.lease.unwrap().token, replacement.token);
    assert!(f
        .scratch
        .tmux(&[
            "set-option",
            "-s",
            model::opt::COLLECTOR,
            &CollectorLease {
                token: replacement.token,
                expires_at: 0
            }
            .encode()
        ])
        .status
        .success());
    common::poll_until("fresh collector token", || {
        f.read()
            .program
            .lease
            .is_some_and(|lease| lease.token != old.token && lease.live_at(tma_runtime::now_ms()))
    });
    assert!(f.read().program.commit.is_none());
    f.emit("sync\x1b\\");
    f.report("blocked");
    f.wait_live("blocked");
    let stopping_token = f.read().program.lease.unwrap().token;
    f.signal("-TERM");
    let exited = f.daemon.wait_exit(Duration::from_secs(10));
    if !exited {
        let processes = Command::new("ps")
            .args(["-axo", "pid=,ppid=,state=,etime=,command="])
            .output()
            .unwrap();
        let owner = f.daemon.pid().to_string();
        for line in String::from_utf8_lossy(&processes.stdout).lines() {
            let fields: Vec<_> = line.split_whitespace().take(2).collect();
            if fields.iter().any(|field| *field == owner) {
                eprintln!("owned shutdown process: {line}");
            }
        }
    }
    assert!(exited);
    let read = f.read();
    // A write applied before confirmed-cache admission may retain private markers.
    assert!(!matches!(
        read.projection(tma_runtime::now_ms()),
        Ok(model::ProjectionRead::Live { .. })
    ));
    assert!(store::AttentionObservation::from_read(read.clone(), tma_runtime::now_ms()).is_none());
    let lease = read.program.lease.unwrap();
    assert_eq!(lease.token, stopping_token);
    assert_eq!(lease.expires_at, 0);
}

#[test]
fn same_wake_hook_conflict_preserves_program_state_and_hook_metadata() {
    let _gate = common::DaemonTestGuard::acquire();
    if !available() {
        return;
    }
    let f = Fixture::new("prefer");
    f.report("working");
    f.wait_live("working");
    // Stage both inputs while the actual foreground daemon is stopped.
    f.signal("-STOP");
    f.report("blocked");
    let socket = std::fs::read_dir(f.scratch.workdir.join("tma"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "sock"))
        .unwrap();
    let mut peer = UnixStream::connect(socket).unwrap();
    let frame = ipc::Frame {
        agent: "reporter".into(),
        pane: f.pane.clone(),
        kind: "working".into(),
        payload: r#"{"session_id":"hook-session"}"#.into(),
    };
    peer.write_all(&ipc::encode_frame(
        &frame.pane,
        &frame.agent,
        &frame.kind,
        &frame.payload,
    ))
    .unwrap();
    f.signal("-CONT");
    f.wait_live("blocked");
    peer.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut ack = [0u8; 1];
    peer.read_exact(&mut ack).unwrap();
    assert_eq!(ack, [ipc::ACK]);
    common::poll_until("one queued hook frame applied", || {
        f.scratch.get(&f.pane, "#{@agent_session}") == "hook-session"
    });
    assert!(f.live("blocked"));
}

#[test]
fn hot_output_does_not_sample_per_byte_or_starve_peer_and_shutdown() {
    let _gate = common::DaemonTestGuard::acquire();
    if !available() {
        return;
    }
    let mut f = Fixture::new("prefer");
    f.report("working");
    f.wait_live("working");
    assert!(f
        .scratch
        .tmux(&["new-session", "-d", "-s", "hot", "exec yes hot-output"])
        .status
        .success());
    let samples = f.count("program_process_samples");
    let publications = f.count("program_publications");
    std::thread::sleep(Duration::from_secs(2));
    let started = Instant::now();
    f.report("blocked");
    f.wait_live("blocked");
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(
        f.count("program_process_samples") - samples < 30,
        "fleet sampling is per batch/cadence, not output chunk"
    );
    assert!(f.count("program_publications") <= publications + 1);
    let stopped = Instant::now();
    f.signal("-TERM");
    assert!(f.daemon.wait_exit(Duration::from_secs(10)));
    assert!(stopped.elapsed() < Duration::from_secs(10));
    assert!(f.read().program.commit.is_none());
}

#[test]
fn idle_to_idle_results_acknowledge_without_more_output_and_failure_clears_detail() {
    let _gate = common::DaemonTestGuard::acquire();
    if !available() {
        return;
    }
    let f = Fixture::new("prefer");
    f.report("done");
    f.wait_live("idle");
    common::poll_until("first result fully published", || {
        f.count("program_publications") >= 1
    });
    let turn = f.scratch.get(&f.pane, "#{@agent_turn_at}");
    let publications = f.count("program_publications");
    assert!(Fixture::command(&f.scratch)
        .args(["clear-attention", &f.pane])
        .output()
        .unwrap()
        .status
        .success());
    common::poll_until("durable exact result ACK", || {
        f.read().program.acknowledged.is_some()
    });
    // No OSC after ACK: the periodic sampler must update the reducer silently.
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(
        f.count("program_publications"),
        publications,
        "ordinary ACK does not rewrite the tuple"
    );
    f.report("done");
    common::poll_until("ACKed duplicate reduced", || {
        f.count("program_duplicates") >= 1
    });
    assert!(!f.read().options.contains_key("@agent_attention"));
    assert_eq!(f.scratch.get(&f.pane, "#{@agent_turn_at}"), turn);
    f.report("idle");
    f.wait_live("idle");
    let before_error = f.count("program_publications");
    f.report("error");
    common::poll_until("idle-to-idle failure presents", || {
        f.scratch.get(&f.pane, "#{@agent_detail}") == "error"
            && f.read().options.contains_key("@agent_attention")
    });
    common::poll_until("failure publication and notification completed", || {
        f.count("program_publications") > before_error
            && f.stats()
                .get(&format!("program_pane_{}", f.pane))
                .is_some_and(|value| value.contains("selection=Some(Error)"))
    });
    assert!(Fixture::command(&f.scratch)
        .args(["clear-attention", &f.pane])
        .output()
        .unwrap()
        .status
        .success());
    common::poll_until("failure durable ACK won", || {
        let read = f.read();
        read.program.acknowledged.is_some() && read.program.acknowledged == read.program.result
    });
    common::poll_until(
        "silent ACK clears selected failure detail through guarded publication",
        || f.scratch.get(&f.pane, "#{@agent_detail}").is_empty(),
    );
    assert!(!f.read().options.contains_key("@agent_attention"));
}

#[test]
fn dropped_control_owner_releases_until_new_post_reconnect_report() {
    let _gate = common::DaemonTestGuard::acquire();
    if !available() {
        return;
    }
    let f = Fixture::new("prefer");
    f.report("working");
    f.wait_live("working");
    let before = f.read().owner().unwrap();
    let clients = f.scratch.tmux(&["list-clients", "-F", "#{client_pid}"]);
    assert!(clients.status.success());
    let pid = String::from_utf8(clients.stdout)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .to_string();
    assert!(Command::new("kill")
        .args(["-TERM", &pid])
        .status()
        .unwrap()
        .success());
    common::poll_until("control loss releases exact commit", || {
        f.read().program.commit.is_none()
    });
    common::poll_until("new collector boundary", || {
        f.stats()
            .get(&format!("program_pane_{}", f.pane))
            .is_some_and(|v| v.contains("Recovering") && v.contains("owner=Some"))
    });
    assert!(f.read().program.commit.is_none());
    f.emit("sync\x1b\\");
    f.report("blocked");
    f.wait_live("blocked");
    assert_ne!(f.read().owner().unwrap().stream_epoch, before.stream_epoch);
}

#[test]
fn target_capability_loss_revokes_only_affected_owner_and_healthy_peer_renews() {
    let _gate = common::DaemonTestGuard::acquire();
    if !available() {
        return;
    }
    let mut f = Fixture::new("prefer");
    f.report("working");
    f.wait_live("working");
    let original = f.pane.clone();
    let original_session = f.scratch.get(&original, "#{session_id}");
    assert!(f
        .scratch
        .tmux(&[
            "new-session",
            "-d",
            "-s",
            "peer",
            "stty -echo; exec /bin/cat"
        ])
        .status
        .success());
    f.pane = f.scratch.get("peer", "#{pane_id}");
    common::poll_until("peer bound", || {
        f.stats()
            .get(&format!("program_pane_{}", f.pane))
            .is_some_and(|v| v.contains("BindingBoundary"))
    });
    f.emit("sync\x1b\\");
    f.report("working");
    f.wait_live("working");
    let peer = f.pane.clone();
    let peer_owner = f.read().owner();
    let token = f.read().program.lease.unwrap().token;
    assert!(f
        .scratch
        .tmux(&[
            "set-hook",
            "-t",
            &original_session,
            "after-set-option",
            "run-shell true"
        ])
        .status
        .success());
    f.pane = original;
    common::poll_until("affected target loses capability", || {
        f.read().program.commit.is_none()
    });
    f.pane = peer;
    assert_eq!(f.read().owner(), peer_owner);
    assert!(f.live("working"));
    let expiry = f.read().program.lease.unwrap().expires_at;
    common::poll_until("healthy target renews same collector token", || {
        f.read()
            .program
            .lease
            .is_some_and(|lease| lease.token == token && lease.expires_at > expiry)
    });
    assert_eq!(f.read().owner(), peer_owner);
}

#[test]
fn off_and_observe_restart_do_not_claim_prior_live_program_result() {
    let _gate = common::DaemonTestGuard::acquire();
    if !available() {
        return;
    }
    for mode in ["off", "observe"] {
        let mut f = Fixture::new("prefer");
        f.scratch.write_config(
            &Fixture::config("prefer").replace("on = [\"blocked\", \"done\"]", "on = []"),
        );
        assert!(Fixture::command(&f.scratch)
            .arg("reload")
            .output()
            .unwrap()
            .status
            .success());
        common::poll_until("notification disable reload applied", || {
            std::fs::read_to_string(f.scratch.daemon_log_path())
                .unwrap_or_default()
                .contains("reloaded config")
        });
        f.report("done");
        f.wait_live("idle");
        common::poll_until("unclaimed result committed", || {
            f.count("program_publications") >= 1
        });
        assert!(f.read().program.notified.is_none());
        f.signal("-KILL");
        assert!(f.daemon.wait_exit(Duration::from_secs(5)));
        f.scratch.write_config(&Fixture::config(mode));
        f.daemon = DaemonGuard::new(
            Fixture::command(&f.scratch)
                .args(["daemon", "--status-file", f.status.to_str().unwrap()])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(f.scratch.daemon_log_stdio())
                .spawn()
                .unwrap(),
        );
        common::poll_until("new mode cold-start pass completed", || {
            f.stats()
                .get("program_mode")
                .is_some_and(|value| value == mode)
        });
        assert!(f
            .read()
            .program
            .lease
            .is_some_and(|lease| lease.live_at(tma_runtime::now_ms())));
        assert!(
            f.read().program.notified.is_none(),
            "denied old collector must not spend a notification claim"
        );
        assert!(f.read().options.contains_key("@agent_attention"));
        assert_eq!(f.count("notify_fires"), 0);
    }
}

#[test]
fn lost_silent_failure_ack_publication_releases_and_recovers_without_more_osc() {
    let _gate = common::DaemonTestGuard::acquire();
    if !available() {
        return;
    }
    let f = Fixture::new_with("prefer", |scratch| {
        let wrapper = scratch.workdir.join("reject-one-publication");
        let trigger = scratch.workdir.join("reject-armed");
        let spent = scratch.workdir.join("reject-spent");
        std::fs::write(&wrapper, format!("#!/bin/sh\nfor arg do\nif [ \"$arg\" = '#{{socket_path}}' ]; then\ntmux \"$@\" > '{root}/startup-socket-stdout' 2> '{root}/startup-socket-stderr'\nstatus=$?\nprintf '%s' \"$status\" > '{root}/startup-socket-status'\ncat '{root}/startup-socket-stdout'\ncat '{root}/startup-socket-stderr' >&2\nexit \"$status\"\nfi\ndone\nif [ -e '{trigger}' ] && [ ! -e '{spent}' ]; then\nfor arg do\ncase \"$arg\" in\n*'@tma_program_origin'*'@agent_detail'*) touch '{spent}'; exit 1 ;;\nesac\ndone\nfi\nexec tmux \"$@\"\n", root=scratch.workdir.display(), trigger=trigger.display(), spent=spent.display())).unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
        Some(wrapper)
    });
    f.report("error");
    f.wait_live("idle");
    common::poll_until("failure fully published", || {
        f.count("program_publications") >= 1
    });
    std::fs::write(f.scratch.workdir.join("reject-armed"), "armed").unwrap();
    assert!(Fixture::command(&f.scratch)
        .args(["clear-attention", &f.pane])
        .output()
        .unwrap()
        .status
        .success());
    common::poll_until("delayed publication was actually rejected", || {
        f.scratch.workdir.join("reject-spent").exists()
    });
    common::poll_until(
        "failed post-ACK publication releases exact authority",
        || f.read().program.commit.is_none(),
    );
    assert!(!f.live("idle"));
    common::poll_until("failed publication diagnostics completed", || {
        f.count("program_errors") >= 1
    });
    // No new report is necessary to release the mismatched projection. Recovery
    // of program authority itself requires a fresh post-loss report.
    common::poll_until("fresh recovery boundary", || {
        f.stats()
            .get(&format!("program_pane_{}", f.pane))
            .is_some_and(|v| v.contains("owner=Some") && v.contains("Recovering"))
    });
    f.report("working");
    f.wait_live("working");
}

#[test]
fn missed_lease_deadline_discards_queued_completion_before_fresh_epoch() {
    let _gate = common::DaemonTestGuard::acquire();
    if !available() {
        return;
    }
    let f = Fixture::new("prefer");
    f.report("working");
    f.wait_live("working");
    common::poll_until("working publication complete", || {
        f.count("program_publications") >= 1
    });
    let old = f.read().program.lease.unwrap().token;
    f.signal("-STOP");
    f.report("done");
    std::thread::sleep(Duration::from_secs(16));
    f.signal("-CONT");
    common::poll_until("expired collector replaced", || {
        f.read()
            .program
            .lease
            .is_some_and(|lease| lease.token != old && lease.live_at(tma_runtime::now_ms()))
    });
    assert!(f.read().program.commit.is_none());
    assert!(!f.read().options.contains_key("@agent_attention"));
    assert!(!f.read().options.contains_key("@agent_turn_at"));
    assert_eq!(f.count("notify_fires"), 0);
    common::poll_until("fresh process/stream boundary", || {
        f.stats()
            .get(&format!("program_pane_{}", f.pane))
            .is_some_and(|v| v.contains("owner=Some") && v.contains("Recovering"))
    });
    f.report("done");
    f.wait_live("idle");
    assert!(f.read().options.contains_key("@agent_attention"));
}

#[test]
fn aliases_and_ignore_changes_revoke_without_reusing_old_reports() {
    let _gate = common::DaemonTestGuard::acquire();
    if !available() {
        return;
    }
    let f = Fixture::new("prefer");
    f.report("working");
    f.wait_live("working");
    common::poll_until("working committed", || f.count("program_publications") >= 1);
    let manifest = f.scratch.manifest_dir().join("reporter.toml");
    let original = std::fs::read_to_string(&manifest).unwrap();
    std::fs::write(
        &manifest,
        original.replace("apps = [\"pi\"]", "apps = [\"another\"]"),
    )
    .unwrap();
    f.reload("prefer");
    common::poll_until("changed aliases revoke old authority", || {
        f.read().program.commit.is_none()
    });
    f.emit("sync\x1b\\");
    f.report("working");
    common::poll_until("ineligible report observed", || {
        f.stats()
            .get(&format!("program_pane_{}", f.pane))
            .is_some_and(|v| v.contains("records=1"))
    });
    assert!(f.read().program.commit.is_none());
    std::fs::write(&manifest, original).unwrap();
    f.reload("prefer");
    common::poll_until("restored alias starts fresh boundary", || {
        f.stats()
            .get(&format!("program_pane_{}", f.pane))
            .is_some_and(|v| v.contains("owner=Some") && v.contains("Recovering"))
    });
    f.emit("sync\x1b\\");
    f.report("working");
    f.wait_live("working");
    assert!(f
        .scratch
        .tmux(&["set-option", "-p", "-t", &f.pane, "@agent_ignore", "1"])
        .status
        .success());
    common::poll_until("ignore revokes independently of output", || {
        f.read().program.commit.is_none()
    });
    assert!(f
        .scratch
        .tmux(&["set-option", "-pu", "-t", &f.pane, "@agent_ignore"])
        .status
        .success());
    common::poll_until("unignore binds a fresh interval", || {
        f.stats()
            .get(&format!("program_pane_{}", f.pane))
            .is_some_and(|v| v.contains("owner=Some") && v.contains("Recovering"))
    });
    assert!(f.read().program.commit.is_none());
    f.report("blocked");
    f.wait_live("blocked");
}

#[test]
fn semantic_and_release_flood_is_pane_local_and_keeps_peer_hooks_and_term_responsive() {
    let _gate = common::DaemonTestGuard::acquire();
    if !available() {
        return;
    }
    let mut f = Fixture::new("prefer");
    f.report("working");
    f.wait_live("working");
    let hot = f.pane.clone();
    let peer = f.new_reporter("budget-peer");
    let initial = f.count("program_publications");
    let token = f.read().program.lease.unwrap().token;
    f.signal("-STOP");
    let pair = "\x1b]7501;state=done:app=pi\x1b\\\x1b]7501;state=working:app=pi\x1b\\";
    for _ in 0..3 {
        f.emit(&pair.repeat(8));
    }
    f.emit(&"\x1bc\x1b]7501;state=clear\x1b\\".repeat(12));
    f.marker("hot-semantic-flood-complete");
    f.pane = peer.clone();
    f.report("blocked");
    f.marker("healthy-semantic-report-complete");
    let mut hook = f.hook_peer(&peer);
    let started = Instant::now();
    f.signal("-CONT");
    let mut ack = [0u8; 1];
    hook.read_exact(&mut ack).unwrap();
    assert_eq!(ack, [ipc::ACK]);
    f.wait_live("blocked");
    common::poll_until("pane work loss visible", || {
        f.count("program_work_pane_losses") >= 1
    });
    assert_eq!(
        f.count("program_work_batch_losses"),
        0,
        "healthy peer must survive local work exhaustion"
    );
    assert!(store::read_pane(&f.tmux, &hot)
        .unwrap()
        .program
        .commit
        .is_none());
    assert_eq!(f.read().program.lease.unwrap().token, token);
    assert!(
        f.count("program_publications") - initial <= 9,
        "flood performs only bounded semantic publications"
    );
    let stats = f.stats();
    eprintln!("semantic peer elapsed_ms={}, publications={}, max_operations={}, max_batch_ms={}, delayed_samples={}, pane_losses={}",started.elapsed().as_millis(),f.count("program_publications")-initial,stats["program_work_max_operations"],stats["program_work_max_batch_ms"],stats["program_delayed_process_samples"],stats["program_work_pane_losses"]);
    let stopped = Instant::now();
    f.signal("-TERM");
    assert!(f.daemon.wait_exit(Duration::from_secs(10)));
    eprintln!(
        "semantic shutdown elapsed_ms={}",
        stopped.elapsed().as_millis()
    );
    for pane in [hot, peer] {
        assert!(!matches!(
            store::read_pane(&f.tmux, &pane)
                .unwrap()
                .projection(tma_runtime::now_ms()),
            Ok(model::ProjectionRead::Live { .. })
        ));
    }
}

#[test]
fn global_elapsed_work_loss_retires_lease_once_and_finishes_bounded_cleanup() {
    let _gate = common::DaemonTestGuard::acquire();
    if !available() {
        return;
    }
    let mut f = Fixture::new_with("prefer", |scratch| {
        let wrapper = scratch.workdir.join("delay-owned-publication");
        std::fs::write(&wrapper,format!("#!/bin/sh\nif [ -e '{}' ]; then\nfor arg do\ncase \"$arg\" in\n*'set-option'*'@tma_program_origin'*'@agent_state'*) printf x >> '{}'; sleep 2; break ;;\nesac\ndone\nfi\nexec tmux \"$@\"\n",scratch.workdir.join("delay-armed").display(),scratch.workdir.join("delay-count").display())).unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
        Some(wrapper)
    });
    f.report("working");
    f.wait_live("working");
    let mut panes = vec![f.pane.clone()];
    for name in ["global-a", "global-b", "global-c", "global-d"] {
        panes.push(f.new_reporter(name));
    }
    let old = f.read().program.lease.unwrap().token;
    let retirements = f.count("program_lease_retirements");
    f.signal("-STOP");
    std::fs::write(f.scratch.workdir.join("delay-armed"), "armed").unwrap();
    for (index, pane) in panes.iter().enumerate() {
        f.pane = pane.clone();
        f.report("blocked");
        f.marker(&format!("global-report-{index}"));
    }
    let started = Instant::now();
    f.signal("-CONT");
    common::poll_until("global elapsed loss latches", || {
        f.count("program_work_batch_losses") >= 1
    });
    assert!(
        std::fs::read(f.scratch.workdir.join("delay-count"))
            .unwrap()
            .len()
            >= 2,
        "native gate must delay actual publication commands"
    );
    assert_eq!(f.count("program_lease_retirements"), retirements + 1);
    assert_eq!(f.count("program_lease_retirement_uncertain"), 0);
    assert!(f.count("program_work_max_operations") <= 32);
    assert!(f.count("program_work_max_batch_ms") >= 5000);
    common::poll_until("bounded retired owner cleanup progresses", || {
        f.count("program_cleanup_remaining") == 0
    });
    for pane in &panes {
        assert!(!matches!(
            store::read_pane(&f.tmux, pane)
                .unwrap()
                .projection(tma_runtime::now_ms()),
            Ok(model::ProjectionRead::Live { .. })
        ));
    }
    eprintln!("global budget elapsed_ms={}, max_operations={}, max_batch_ms={}, delayed_samples={}, cleanup_attempts={}, lease_retirements={}",started.elapsed().as_millis(),f.count("program_work_max_operations"),f.count("program_work_max_batch_ms"),f.count("program_delayed_process_samples"),f.count("program_cleanup_attempts"),f.count("program_lease_retirements")-retirements);
    std::fs::remove_file(f.scratch.workdir.join("delay-armed")).unwrap();
    common::poll_until("fresh collector after cleanup", || {
        f.read()
            .program
            .lease
            .is_some_and(|lease| lease.token != old && lease.live_at(tma_runtime::now_ms()))
    });
    common::poll_until("fresh identity after global reset", || {
        f.stats()
            .get(&format!("program_pane_{}", f.pane))
            .is_some_and(|value| value.contains("owner=Some") && value.contains("Recovering"))
    });
    f.report("working");
    f.wait_live("working");
}

#[test]
fn term_during_semantic_io_stops_before_release_storm_and_revokes_authority() {
    let _gate = common::DaemonTestGuard::acquire();
    if !available() {
        return;
    }
    let mut f = Fixture::new_with("prefer", |scratch| {
        let wrapper = scratch.workdir.join("gate-semantic-publication");
        std::fs::write(
            &wrapper,
            format!(
                r#"#!/bin/sh
tma_semantic_gated=0
if [ -e '{armed}' ] && [ ! -e '{ready}' ]; then
for arg do
case "$arg" in
*'set-option'*'@tma_program_origin'*'@agent_state'*)
touch '{ready}'; i=0
while [ ! -e '{release}' ] && [ $i -lt 400 ]; do sleep 0.005; i=$((i+1)); done
if [ ! -e '{release}' ]; then touch '{expired}'; exit 124; fi
touch '{released}'; tma_semantic_gated=1; break ;;
esac
done
fi
if [ "$tma_semantic_gated" = 1 ]; then
tmux "$@"; tma_semantic_result=$?
printf '%s\n' "$tma_semantic_result" > '{done}'
exit "$tma_semantic_result"
fi
exec tmux "$@"
"#,
                armed = scratch.workdir.join("term-armed").display(),
                ready = scratch.workdir.join("term-ready").display(),
                release = scratch.workdir.join("term-release").display(),
                expired = scratch.workdir.join("term-expired").display(),
                released = scratch.workdir.join("term-gate-released").display(),
                done = scratch.workdir.join("term-command-done").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
        Some(wrapper)
    });
    f.report("working");
    f.wait_live("working");
    let hot = f.pane.clone();
    let peer = f.new_reporter("term-peer");
    std::fs::write(f.scratch.workdir.join("term-armed"), "armed").unwrap();
    f.report("blocked");
    f.emit(&"\x1bc\x1b]7501;state=clear\x1b\\".repeat(12));
    let _queued = f.hook_peer(&peer);
    common::poll_until("actual semantic IO reached bounded gate", || {
        f.scratch.workdir.join("term-ready").exists()
    });
    let started = Instant::now();
    f.signal("-TERM");
    std::fs::write(f.scratch.workdir.join("term-release"), "release").unwrap();
    assert!(f.daemon.wait_exit(Duration::from_secs(10)));
    assert!(!f.scratch.workdir.join("term-expired").exists());
    assert!(f.scratch.workdir.join("term-gate-released").exists());
    assert_eq!(
        std::fs::read_to_string(f.scratch.workdir.join("term-command-done"))
            .unwrap()
            .trim(),
        "0"
    );
    eprintln!(
        "TERM during semantic IO elapsed_ms={}",
        started.elapsed().as_millis()
    );
    for pane in [hot, peer] {
        assert!(!matches!(
            store::read_pane(&f.tmux, &pane)
                .unwrap()
                .projection(tma_runtime::now_ms()),
            Ok(model::ProjectionRead::Live { .. })
        ));
    }
}

#[test]
fn denied_exact_revocation_retires_the_token_before_retrying_private_cleanup() {
    let _gate = common::DaemonTestGuard::acquire();
    if !available() {
        return;
    }
    let mut f = Fixture::new_with("prefer", |scratch| {
        let wrapper = scratch.workdir.join("deny-exact-revocation");
        std::fs::write(
            &wrapper,
            format!(
                r#"#!/bin/sh
if [ -e '{armed}' ]; then
for arg do
case "$arg" in
*'@tma_program_origin'*|*'@agent_state'*) ;;
*'set-option'*'@tma_program_commit'*)
if [ ! -e '{spent}' ]; then touch '{spent}'; exit 1; fi
if [ ! -e '{release}' ]; then
touch '{ready}'; i=0
while [ ! -e '{release}' ] && [ $i -lt 400 ]; do sleep 0.005; i=$((i+1)); done
if [ ! -e '{release}' ]; then touch '{expired}'; exit 124; fi
touch '{released}'
fi
break ;;
esac
done
fi
exec tmux "$@"
"#,
                armed = scratch.workdir.join("deny-revoke-armed").display(),
                spent = scratch.workdir.join("deny-revoke-spent").display(),
                release = scratch.workdir.join("deny-revoke-release").display(),
                ready = scratch.workdir.join("deny-revoke-cleanup-ready").display(),
                expired = scratch.workdir.join("deny-revoke-expired").display(),
                released = scratch.workdir.join("deny-revoke-gate-released").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
        Some(wrapper)
    });
    f.report("working");
    f.wait_live("working");
    let hot = f.pane.clone();
    let peer = f.new_reporter("revoke-peer");
    let old = f.read().program.lease.unwrap().token;
    let retirements = f.count("program_lease_retirements");
    std::fs::write(f.scratch.workdir.join("deny-revoke-armed"), "armed").unwrap();
    f.signal("-STOP");
    let pair = "\x1b]7501;state=done:app=pi\x1b\\\x1b]7501;state=working:app=pi\x1b\\";
    for _ in 0..3 {
        f.emit(&pair.repeat(8));
    }
    f.marker("denied-revoke-flood-written");
    f.signal("-CONT");
    common::poll_until("actual exact COMMIT revocation rejected once", || {
        f.scratch.workdir.join("deny-revoke-spent").exists()
    });
    common::poll_until("later private cleanup waits after retirement", || {
        f.scratch.workdir.join("deny-revoke-cleanup-ready").exists()
    });
    // The later successful cleanup has not run yet. Only the server lease can
    // explain why both the uncertain hot tuple and its healthy peer lost authority.
    let retired = f.read().program.lease.unwrap();
    assert_eq!(retired.token, old);
    assert_eq!(retired.expires_at, 0);
    assert!(!retired.live_at(tma_runtime::now_ms()));
    for pane in [&hot, &peer] {
        assert!(!matches!(
            store::read_pane(&f.tmux, pane)
                .unwrap()
                .projection(tma_runtime::now_ms()),
            Ok(model::ProjectionRead::Live { .. })
        ));
    }
    std::fs::write(f.scratch.workdir.join("deny-revoke-release"), "release").unwrap();
    common::poll_until("retirement recorded without renewal uncertainty", || {
        f.count("program_lease_retirements") == retirements + 1
            && f.count("program_lease_release_wins") >= 1
            && f.count("program_cleanup_remaining") == 0
    });
    assert!(!f.scratch.workdir.join("deny-revoke-expired").exists());
    assert!(f.scratch.workdir.join("deny-revoke-gate-released").exists());
    assert!(f.count("program_work_pane_losses") >= 1);
    assert_eq!(f.count("program_work_batch_losses"), 0);
    assert!(f.stats()["program_last_loss"].contains("PersistenceLost"));
    assert_eq!(f.count("program_lease_retirement_uncertain"), 0);
    common::poll_until("fresh token after uncertain old owner retirement", || {
        f.read()
            .program
            .lease
            .is_some_and(|lease| lease.token != old && lease.live_at(tma_runtime::now_ms()))
    });
    let current = f.read().program.lease.unwrap().token;
    std::thread::sleep(Duration::from_millis(5100));
    assert_eq!(f.read().program.lease.unwrap().token, current);
    assert_ne!(
        current, old,
        "failed pane revocation cannot keep the old token renewing"
    );
    f.pane = peer;
    f.report("working");
    f.wait_live("working");
}

#[test]
fn capability_reacquisition_requires_a_fresh_report_and_query_cannot_restore_old_done() {
    let _gate = common::DaemonTestGuard::acquire();
    if !available() {
        return;
    }
    let f = Fixture::new_with("prefer", |scratch| {
        assert!(scratch
            .tmux(&["set-hook", "-g", "after-set-option", "run-shell true"])
            .status
            .success());
        None
    });
    f.report("done");
    common::poll_until("report observed without write capability", || {
        f.stats()
            .get(&format!("program_pane_{}", f.pane))
            .is_some_and(|value| value.contains("Active"))
    });
    assert!(f.read().program.lease.is_none());
    assert!(f.read().program.commit.is_none());
    assert!(f
        .scratch
        .tmux(&["set-hook", "-gu", "after-set-option"])
        .status
        .success());
    common::poll_until("new collector starts a fresh eligible boundary", || {
        f.read()
            .program
            .lease
            .is_some_and(|lease| lease.live_at(tma_runtime::now_ms()))
            && f.stats()
                .get(&format!("program_pane_{}", f.pane))
                .is_some_and(|value| value.contains("owner=Some") && value.contains("Recovering"))
    });
    let queries = f.count("program_queries");
    f.emit("\x1b]7501;?\x1b\\");
    common::poll_until("query observed after reacquisition", || {
        f.count("program_queries") > queries
    });
    assert!(f.read().program.commit.is_none());
    assert!(!f.read().options.contains_key("@agent_turn_at"));
    f.report("working");
    f.wait_live("working");
}

#[test]
fn term_after_an_applied_unconfirmed_publication_retires_coverage_before_cache_admission() {
    let _gate = common::DaemonTestGuard::acquire();
    if !available() {
        return;
    }
    let mut f = Fixture::new_with("prefer", |scratch| {
        let wrapper = scratch.workdir.join("gate-applied-unconfirmed-publication");
        std::fs::write(
            &wrapper,
            format!(
                r#"#!/bin/sh
if [ -e '{armed}' ] && [ ! -e '{ready}' ]; then
for arg do
case "$arg" in
*'set-option'*'@tma_program_origin'*'@agent_state'*)
tmux "$@"; tma_applied_result=$?
printf '%s\n' "$tma_applied_result" > '{done}'
[ "$tma_applied_result" = 0 ] || exit "$tma_applied_result"
touch '{ready}'; i=0
while [ ! -e '{release}' ] && [ $i -lt 400 ]; do sleep 0.005; i=$((i+1)); done
if [ ! -e '{release}' ]; then touch '{expired}'; exit 124; fi
touch '{released}'
exit 1 ;;
esac
done
fi
exec tmux "$@"
"#,
                armed = scratch.workdir.join("unconfirmed-armed").display(),
                ready = scratch.workdir.join("unconfirmed-ready").display(),
                done = scratch.workdir.join("unconfirmed-command-done").display(),
                release = scratch.workdir.join("unconfirmed-release").display(),
                expired = scratch.workdir.join("unconfirmed-expired").display(),
                released = scratch.workdir.join("unconfirmed-gate-released").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
        Some(wrapper)
    });
    std::fs::write(f.scratch.workdir.join("unconfirmed-armed"), "armed").unwrap();
    f.report("blocked");
    common::poll_until(
        "actual publication applied before its deliberate error return",
        || f.scratch.workdir.join("unconfirmed-ready").exists(),
    );
    assert_eq!(
        std::fs::read_to_string(f.scratch.workdir.join("unconfirmed-command-done"))
            .unwrap()
            .trim(),
        "0"
    );
    let applied = f.read();
    assert!(matches!(
        applied.projection(tma_runtime::now_ms()),
        Ok(model::ProjectionRead::Live { .. })
    ));
    let token = applied.program.lease.as_ref().unwrap().token.clone();
    let started = Instant::now();
    f.signal("-TERM");
    std::fs::write(f.scratch.workdir.join("unconfirmed-release"), "release").unwrap();
    assert!(f.daemon.wait_exit(Duration::from_secs(10)));
    assert!(!f.scratch.workdir.join("unconfirmed-expired").exists());
    assert!(f.scratch.workdir.join("unconfirmed-gate-released").exists());
    let retired = f.read();
    assert_eq!(
        retired.program.commit, applied.program.commit,
        "unconfirmed writes are deliberately absent from the confirmed cleanup cache"
    );
    assert_eq!(retired.program.lease.as_ref().unwrap().token, token);
    assert_eq!(retired.program.lease.as_ref().unwrap().expires_at, 0);
    assert!(!matches!(
        retired.projection(tma_runtime::now_ms()),
        Ok(model::ProjectionRead::Live { .. })
    ));
    assert!(!retired.options.contains_key(model::opt::EPISODE_NOTIFIED));
    assert_eq!(f.count("notify_fires"), 0);
    eprintln!("unconfirmed applied-write shutdown elapsed_ms={}, retained_private_commit={}, lease_expires_at=0, live=false",started.elapsed().as_millis(),retired.program.commit.is_some());
}

#[test]
fn production_linked_session_transfer_requires_fresh_report_without_duplicate_completion() {
    let _gate = common::DaemonTestGuard::acquire();
    if !available() {
        return;
    }
    let f = Fixture::new("prefer");
    f.report("working");
    f.wait_live("working");
    let owner = f.read().owner().unwrap();
    assert!(f
        .scratch
        .tmux(&["new-session", "-d", "-s", "linked", "-t", "reporter"])
        .status
        .success());
    common::poll_until("both linked session control streams attached", || {
        f.count("clients") == 2 && f.count("pending_seeds") == 0
    });
    let publications = f.count("program_publications");
    let reports = f.count("program_reports");
    f.report("done");
    f.wait_live("idle");
    common::poll_until("one linked logical result notified", || {
        f.read().program.notified.is_some()
    });
    common::poll_until("linked result completed daemon publication pass", || {
        f.count("program_reports") > reports && f.count("program_publications") > publications
    });
    assert_eq!(f.count("program_reports"), reports + 1);
    assert_eq!(f.count("program_publications"), publications + 1);
    let result = f.read().program.result.unwrap();
    assert!(Fixture::command(&f.scratch)
        .args(["clear-attention", &f.pane])
        .output()
        .unwrap()
        .status
        .success());
    common::poll_until("linked result durably acknowledged", || {
        f.read().program.acknowledged.as_ref() == Some(&result)
    });
    let fires = f.count("notify_fires");
    assert!(f
        .scratch
        .tmux(&["kill-session", "-t", "reporter"])
        .status
        .success());
    common::poll_until("transfer revokes former stream and enters recovery", || {
        f.read().program.commit.is_none()
            && f.stats()
                .get(&format!("program_pane_{}", f.pane))
                .is_some_and(|v| v.contains("Recovering") && v.contains("owner=Some"))
    });
    let transfer_publications = f.count("program_publications");
    let transfer_passes = f.count("program_notify_passes");
    f.emit("sync\x1b\\");
    f.report("done");
    f.wait_live("idle");
    common::poll_until("transferred stream publication confirmed", || {
        f.read()
            .owner()
            .is_some_and(|current| current.stream_epoch != owner.stream_epoch)
    });
    common::poll_until(
        "transferred result completed publication and notification pass",
        || {
            f.count("program_publications") > transfer_publications
                && f.count("program_notify_passes") > transfer_passes
        },
    );
    assert!(!f.read().options.contains_key("@agent_attention"));
    assert_eq!(
        f.count("notify_fires"),
        fires,
        "fresh same result preserves spent effect across linked owner transfer"
    );
    eprintln!("linked transfer: initial_owner={owner:?}, transferred_owner={:?}, logical_reports=1, duplicate_completion_effects=0", f.read().owner());
}

#[test]
fn production_real_reporter_process_replacement_drops_prior_birth_bound_authority() {
    let _gate = common::DaemonTestGuard::acquire();
    if !available() {
        return;
    }
    let f = Fixture::new("prefer");
    f.report("done");
    f.wait_live("idle");
    let old = f.read().owner().unwrap();
    assert!(f
        .scratch
        .tmux(&[
            "respawn-pane",
            "-k",
            "-t",
            &f.pane,
            "stty -echo; /bin/cat; exec sleep 100000"
        ])
        .status
        .success());
    common::poll_until("new actual process binding has no former commit", || {
        f.read().program.commit.is_none()
            && f.stats()
                .get(&format!("program_pane_{}", f.pane))
                .is_some_and(|v| v.contains("Recovering") && v.contains("owner=Some"))
    });
    f.emit("sync\x1b\\");
    f.report("working");
    f.wait_live("working");
    let new = f.read().owner().unwrap();
    assert_ne!(new.processes, old.processes);
    assert_ne!(new.episode, old.episode);
    let fresh = tma_tmux::tmux::program_processes().unwrap();
    for identity in new.processes.as_slice() {
        assert_eq!(fresh.identity(identity.pid).unwrap(), *identity);
    }
    assert!(!f.read().options.contains_key("@agent_attention"));
    eprintln!("process replacement: old={old:?}, new={new:?}, live_kernel_births_rechecked=true");
}

fn resource_sample(pid: u32) -> String {
    let ps = if cfg!(target_os = "macos") {
        "/bin/ps"
    } else {
        "ps"
    };
    let out = Command::new(ps)
        .args(["-o", "pid=,%cpu=,rss=,time=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "native resource ps: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

#[test]
fn production_measured_silence_duplicates_and_ordinary_output_keep_publication_and_storage_bounded()
{
    let _gate = common::DaemonTestGuard::acquire();
    if !available() {
        return;
    }
    let mut f = Fixture::new_with("prefer", |scratch| {
        let wrapper = scratch.workdir.join("count-tmux-processes");
        let count = scratch.workdir.join("tmux-process-count");
        std::fs::write(&count, "").unwrap();
        std::fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nprintf x >> '{}'\nexec tmux \"$@\"\n",
                count.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
        Some(wrapper)
    });
    f.report("working");
    f.wait_live("working");
    common::poll_until("initial publication counter flushed", || {
        f.count("program_publications") == 1
    });
    let count_path = f.scratch.workdir.join("tmux-process-count");
    let subprocesses = || std::fs::metadata(&count_path).unwrap().len();
    let initial_commit = f.read().program.commit;
    let initial_publications = f.count("program_publications");
    let initial_samples = f.count("program_process_samples");
    let before_processes = subprocesses();
    let before_cpu = resource_sample(f.daemon.pid());
    let silent_start = Instant::now();
    std::thread::sleep(Duration::from_secs(2));
    let silent_ms = silent_start.elapsed().as_millis();
    let silent_processes = subprocesses() - before_processes;
    let silent_cpu = resource_sample(f.daemon.pid());
    assert_eq!(f.read().program.commit, initial_commit);
    assert_eq!(f.count("program_publications"), initial_publications);
    let duplicates = f.count("program_duplicates");
    let duplicate_start = Instant::now();
    for batch in 1..=10 {
        f.emit(&"\x1b]7501;state=working:app=pi\x1b\\".repeat(20));
        common::poll_until("duplicate burst fully reduced", || {
            f.count("program_duplicates") >= duplicates + batch * 20
        });
    }
    let duplicate_ms = duplicate_start.elapsed().as_millis();
    assert_eq!(f.count("program_publications"), initial_publications);
    assert_eq!(f.read().program.commit, initial_commit);
    let duplicate_cpu = resource_sample(f.daemon.pid());
    let duplicate_processes = subprocesses() - before_processes - silent_processes;
    assert!(f
        .scratch
        .tmux(&[
            "new-session",
            "-d",
            "-s",
            "ordinary-hot",
            "exec yes ordinary-output-heavy"
        ])
        .status
        .success());
    std::thread::sleep(Duration::from_secs(2));
    let blocked_start = Instant::now();
    f.report("blocked");
    f.wait_live("blocked");
    let blocked_ms = blocked_start.elapsed().as_millis();
    assert!(
        blocked_ms < 10_000,
        "blocked precedes configured quiet edge"
    );
    common::poll_until("measured blocked completed publication pass", || {
        f.count("program_publications") == initial_publications + 1
            && f.stats()
                .get(&format!("program_pane_{}", f.pane))
                .is_some_and(|value| value.contains("selection=Some(Blocked"))
    });
    let stats = f.stats();
    assert!(f.count("program_queued_events") <= tma_tmux::control::MAX_PROGRAM_EVENTS as u64);
    assert!(f.count("program_queued_bytes") <= tma_tmux::control::MAX_PROGRAM_PAYLOAD_BYTES as u64);
    assert!(
        f.count("program_retained_bytes") <= tma_runtime::program_status::MAX_PROGRAM_BYTES as u64
    );
    assert_eq!(f.count("on_demand_captures"), 0);
    eprintln!("resource workload: silence_ms={silent_ms}, silent_publications=0, silent_tmux_processes={silent_processes}, duplicate_reports=200, duplicate_ms={duplicate_ms}, duplicate_publications=0, duplicate_tmux_processes={duplicate_processes}, ordinary_hot_ms=2000, blocked_ms={blocked_ms}, samples={}, daemon_tmux_processes={}, per_daemon_cpu_rss_time_before={before_cpu:?}, per_daemon_cpu_rss_time_silent={silent_cpu:?}, per_daemon_cpu_rss_time_duplicates={duplicate_cpu:?}, per_daemon_cpu_rss_time_hot={:?}, status={stats:?}", f.count("program_process_samples") - initial_samples, subprocesses(), resource_sample(f.daemon.pid()));
    let shutdown = Instant::now();
    f.signal("-TERM");
    assert!(f.daemon.wait_exit(Duration::from_secs(10)));
    eprintln!("resource daemon TERM={}ms", shutdown.elapsed().as_millis());
}

#[test]
fn production_query_overflow_revokes_until_a_new_valid_report() {
    let _gate = common::DaemonTestGuard::acquire();
    if !available() {
        return;
    }
    let f = Fixture::new("prefer");
    f.report("working");
    f.wait_live("working");
    let before = f.read().owner().unwrap();
    let renewed = f.count("program_lease_renewals");
    common::poll_until(
        "fresh lease before bounded stopped collector workload",
        || f.count("program_lease_renewals") > renewed,
    );
    f.signal("-STOP");
    let staged = Instant::now();
    for _ in 0..60 {
        f.emit(&"\x1b]7501;?\x1b\\".repeat(100));
    }
    f.marker("query-overflow-fully-staged");
    f.signal("-CONT");
    assert!(
        staged.elapsed() < Duration::from_secs(10),
        "staging must precede lease expiration"
    );
    common::poll_until("actual production event queue overflow", || {
        f.count("program_overflows") > 0
    });
    common::poll_until("overflow releases former live projection", || {
        f.read().program.commit.is_none()
    });
    assert!(!f.live("working"));
    assert!(f.count("program_queued_events") <= tma_tmux::control::MAX_PROGRAM_EVENTS as u64);
    assert!(f.count("program_queued_bytes") <= tma_tmux::control::MAX_PROGRAM_PAYLOAD_BYTES as u64);
    assert_eq!(
        f.count("program_lease_losses"),
        0,
        "trace must exercise overflow, not missed lease"
    );
    f.emit("sync\x1b\\");
    f.report("blocked");
    f.wait_live("blocked");
    assert_ne!(f.read().owner().unwrap().stream_epoch, before.stream_epoch);
    eprintln!("production overflow: staged_queries=6000, staged_ms={}, actual_overflows={}, queued_events={}, queued_bytes={}, fresh_blocked_recovered=true", staged.elapsed().as_millis(), f.count("program_overflows"), f.count("program_queued_events"), f.count("program_queued_bytes"));
}

#[test]
fn production_protocol_text_cannot_create_request_but_bound_hook_metadata_can() {
    let _gate = common::DaemonTestGuard::acquire();
    if !available() {
        return;
    }
    let f = Fixture::new_with("prefer", |scratch| {
        let path = scratch.manifest_dir().join("reporter.toml");
        let mut manifest = std::fs::read_to_string(&path).unwrap();
        manifest.push_str("\n[[hooks.map]]\nevent = 'blocked'\nclaim = { state = 'blocked', detail = 'permission' }\n");
        std::fs::write(path, manifest).unwrap();
        None
    });
    // Base64 text claims a request/tool; it is deliberately only protocol text.
    f.emit("\x1b]7501;state=blocked:kind=permission:app=pi:title=YXBwcm92ZV9mcm9tX3Byb3RvY29sX3RleHQ=:msg=cmVxdWVzdF9pZD1zcG9vZiB0b29sX25hbWU9QmFzaA==\x1b\\");
    f.wait_live("blocked");
    let owner = f.read().owner().unwrap();
    for key in [
        tma_core::stamp::opt::PERMISSION_REQUEST,
        tma_core::stamp::opt::PENDING_CALL,
        tma_core::stamp::opt::PENDING_TOOL,
    ] {
        assert!(!f.read().options.contains_key(key));
    }
    let config = tma_core::FoldConfig::default();
    let api = tma_runtime::config::ApiSection::default();
    let manifests = tma_runtime::manifests::load(Some(&f.scratch.manifest_dir()), &[]).unwrap();
    let action = tma_core::ActionManifest::parse("min_engine_version = '0.1'\nname = 'approve'\nlabel = 'Approve'\nkind = 'keys'\nwhen = { state = ['blocked'], detail = ['permission'] }\n[keys]\nreporter = ['1']\n", "approve", "owned-test.toml").unwrap();
    let io = tma_runtime::broker::TmuxBroker {
        tmux: &f.tmux,
        manifests: &manifests.manifests,
        cfg: &config,
        api_bases: &api,
        server: Server::named(Some(f.scratch.socket.clone())),
        notify_command: None,
    };
    let refusal = tma_runtime::broker::act(
        &io,
        &action,
        &f.pane,
        tma_runtime::broker::FireArgs {
            expect_permission_request: Some("spoof"),
            ..Default::default()
        },
    );
    assert_eq!(refusal.reason(), Some("request-gone"));
    let payload = f.scratch.workdir.join("genuine-hook-payload.json");
    std::fs::write(&payload, r#"{"session_id":"actual-hook","request_id":"genuine_request","tool_name":"Bash","tool_use_id":"genuine_call","tool_input":{"command":"echo harmless"}}"#).unwrap();
    let event = Fixture::command(&f.scratch)
        .args([
            "event",
            "--agent",
            "reporter",
            "--kind",
            "blocked",
            "--pane",
            &f.pane,
            "--payload",
            payload.to_str().unwrap(),
        ])
        .env("TMUX_PANE", &f.pane)
        .output()
        .unwrap();
    assert!(
        event.status.success(),
        "{}",
        String::from_utf8_lossy(&event.stderr)
    );
    common::poll_until(
        "daemon preserves independent request metadata under OSC ownership",
        || {
            let read = f.read();
            read.options
                .get(tma_core::stamp::opt::PERMISSION_REQUEST)
                .map(String::as_str)
                == Some("genuine_request")
                && read
                    .options
                    .get(tma_core::stamp::opt::PENDING_CALL)
                    .map(String::as_str)
                    == Some("genuine_call")
        },
    );
    assert_eq!(f.read().owner().as_ref(), Some(&owner));
    let sent = tma_runtime::broker::act(
        &io,
        &action,
        &f.pane,
        tma_runtime::broker::FireArgs {
            expect_permission_request: Some("genuine_request"),
            ..Default::default()
        },
    );
    assert_eq!(sent.outcome, tma_runtime::broker::Outcome::Sent);
    assert!(f.live("blocked"));
    eprintln!("production action safeguards: spoof=request-gone; real_same_process_hook_request=sent; full_program_owner_preserved=true");
}
