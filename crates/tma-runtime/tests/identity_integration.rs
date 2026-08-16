//! Acceptance: the identity engine finds a nested agent by the process-tree walk when
//! `#{pane_current_command}` shows only the wrapping shell, on a scratch tmux server.
//!
//! The scratch `tmux -L tma_test_<unique>` socket (`-f /dev/null`) is killed on drop — the
//! default server is never touched.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use common::Scratch;
use tma_test_support as common;

/// `tma debug explain <pane> --json` through the real loader (bundled + user overrides): no
/// `--manifest-dir` (which would bypass the overlay), user config pinned to `xdg` via
/// `XDG_CONFIG_HOME`, so `manifests::load(None)` runs the shadow path against `<xdg>/tma/agents/`.
fn explain_xdg(s: &Scratch, pane: &str, xdg: &Path) -> String {
    // `TMA_CONFIG` pins the config file to the empty default, keeping the real config out; the
    // manifest overlay reads `<xdg>/tma/agents/` from `XDG_CONFIG_HOME`, unaffected by it.
    let out = Command::new(common::tma_bin())
        .args(["debug", "explain", pane, "--json"])
        .arg("--socket-name")
        .arg(&s.socket)
        .env("XDG_CONFIG_HOME", xdg)
        .env("TMA_CONFIG", common::empty_config_path())
        .output()
        .expect("spawn tma");
    assert!(
        out.status.success(),
        "explain failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn basename(s: &str) -> String {
    s.trim().rsplit('/').next().unwrap_or(s).trim().to_string()
}

/// The quoted `process_names` CSV for a session's pane, discovered at runtime (foreground command +
/// `ps` comm) so identity works regardless of how `sleep`/`tail` resolve on the host.
fn process_names_csv(s: &Scratch, session: &str) -> String {
    let pane_pid = s.display(session, "#{pane_pid}");
    let cc = basename(&s.display(session, "#{pane_current_command}"));
    let psc = basename(&String::from_utf8_lossy(
        &Command::new("ps")
            .args(["-o", "comm=", "-p", &pane_pid])
            .output()
            .expect("ps")
            .stdout,
    ));
    let mut names = vec![cc, psc];
    names.sort();
    names.dedup();
    names
        .iter()
        .map(|n| format!("\"{n}\""))
        .collect::<Vec<_>>()
        .join(", ")
}

fn manifest_toml(process_names_csv: &str) -> String {
    format!(
        "min_engine_version = \"0.1\"\n\
         [identity]\nprocess_names = [{process_names_csv}]\n\
         [capture]\nvisible = [\"working\", \"idle\", \"blocked\"]\n"
    )
}

/// The comm basename of a pid, for comparing a nested child against the pane's foreground command.
fn comm_of(pid: &str) -> String {
    let out = Command::new("ps")
        .args(["-o", "comm=", "-p", pid])
        .output()
        .expect("ps");
    basename(&String::from_utf8_lossy(&out.stdout))
}

/// First child pid of `parent`, via `ps -eo pid=,ppid=` — the same tool the production walk
/// uses, so no extra dependency on `pgrep` (absent in minimal environments like the nix sandbox).
fn first_child_of(parent: &str) -> Option<String> {
    let out = Command::new("ps")
        .args(["-eo", "pid=,ppid="])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.lines().find_map(|line| {
        let mut cols = line.split_whitespace();
        let pid = cols.next()?;
        (cols.next()? == parent).then(|| pid.to_string())
    })
}

#[test]
fn nested_agent_found_by_walk_when_command_shows_shell() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("nested");

    // A shell that keeps a long-lived child alive in the background: the pane's foreground
    // process is the shell (running `wait`), the "agent" is the nested child.
    let out = s.tmux(&[
        "new-session",
        "-d",
        "-x",
        "80",
        "-y",
        "24",
        "sleep 100000 & wait",
    ]);
    assert!(
        out.status.success(),
        "new-session failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let pane = s.display("", "#{pane_id}");
    let pane_pid = s.display(&pane, "#{pane_pid}");
    // Wait for the pair this test needs: a nested child under the pane shell whose name DIFFERS
    // from the pane's foreground command, so identity can only succeed via the process-tree walk.
    // Both reads are polled together because the first child seen under load can be a transient
    // shell fork, and tmux settles `#{pane_current_command}` on its own clock.
    let mut current_command = String::new();
    let mut child = None;
    let settled = common::wait_until(common::POLL_CEILING, || {
        current_command = basename(&s.display(&pane, "#{pane_current_command}"));
        child = first_child_of(&pane_pid).map(|pid| {
            let comm = comm_of(&pid);
            (pid, comm)
        });
        child
            .as_ref()
            .is_some_and(|(_, comm)| comm != &current_command)
    });
    let Some((_child_pid, child_comm)) = child else {
        eprintln!("skipping: no nested child found (shell inlined the child)");
        return;
    };
    assert!(
        settled,
        "test needs the agent nested under a differently-named shell \
         (foreground {current_command}, child {child_comm})"
    );

    // Author a manifest matching only the nested child, never the shell.
    std::fs::write(
        s.workdir.join("myagent.toml"),
        format!(
            "min_engine_version = \"0.1\"\n\
             [identity]\nprocess_names = [\"{child_comm}\"]\n\
             [capture]\nvisible = [\"working\"]\n"
        ),
    )
    .unwrap();

    let out = s.tma(&["debug", "explain", &pane, "--json"]);
    assert!(
        out.status.success(),
        "explain failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json = String::from_utf8_lossy(&out.stdout);

    assert!(
        json.contains("\"agent\":\"myagent\""),
        "walk must identify the nested agent though the command shows the shell:\n{json}"
    );
    assert!(
        json.contains("\"foreground_is_agent\":false"),
        "foreground is the shell, so the foreground cap applies:\n{json}"
    );
}

/// A hook-registered pane (`@agent_session` set) whose process the walk cannot see must be honored,
/// not force-deregistered: the registered half holds the stamp until a cycle sees the process again.
/// Removal is reserved for a genuine SessionEnd (which this test leaves unset).
#[test]
fn registered_pane_survives_a_walk_miss() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("registered");

    // A plain shell pane the walk never matches. The `READY` marker gates readiness host-agnostically
    // (its render implies the shell reached its `exec`), unlike a `#{pane_current_command}` check.
    let out = s.tmux(&[
        "new-session",
        "-d",
        "-x",
        "80",
        "-y",
        "24",
        "printf 'READY\\n'; exec sleep 100000",
    ]);
    assert!(
        out.status.success(),
        "new-session failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        common::wait_capture_contains(&s.socket, "", "READY", common::POLL_CEILING),
        "agent pane's chrome did not render"
    );
    let pane = s.display("", "#{pane_id}");

    // A manifest whose process_names match nothing on this host: the ps-walk finds no agent,
    // so identity rests entirely on the registration below.
    std::fs::write(
        s.workdir.join("myagent.toml"),
        "min_engine_version = \"0.1\"\n\
         [identity]\nprocess_names = [\"no-such-agent-xyz\"]\n\
         [capture]\nvisible = [\"working\", \"idle\", \"blocked\"]\n",
    )
    .unwrap();

    // Stamp the pane as a hook-registered, blocked agent (`@agent_session` + `@agent_name` are the
    // marker); the epochs are far in the past so the cycle takes the producer path, not the fast one.
    let old = "1000000000"; // 2001, well outside the 3s freshness window
    s.set_opt(&pane, "@agent_name", "myagent");
    s.set_opt(&pane, "@agent_session", "sess-1");
    s.set_opt(&pane, "@agent_state", "blocked");
    s.set_opt(&pane, "@agent_source", "hook");
    s.set_opt(&pane, "@agent_pid", "0");
    s.set_opt(&pane, "@agent_since", old);
    s.set_opt(&pane, "@agent_evidence_at", old);
    s.set_opt(&pane, "@agent_stamped_at", old);

    // Run a poll cycle (the same entry `tma status`/the picker use).
    let out = s.tma(&["ls", "--json"]);
    assert!(
        out.status.success(),
        "ls failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The live registration must be honored: options intact, state unchanged.
    assert_eq!(
        s.pane_option(&pane, "@agent_state"),
        "blocked",
        "registered pane's state must survive a walk miss (not removed, not capped to unknown)"
    );
    assert_eq!(
        s.pane_option(&pane, "@agent_session"),
        "sess-1",
        "the registration marker must remain until a genuine SessionEnd"
    );
    assert_eq!(
        s.pane_option(&pane, "@agent_name"),
        "myagent",
        "the agent identity must remain"
    );
}

/// A pane whose foreground is a nested multiplexer client is named as such, not left as an
/// unexplained non-agent. The inner server's processes are not in the outer pane's tree, so the walk
/// can never find the agent; what the outer pane *does* have is a composited screen that a screen
/// rule could match by coincidence, which is why the classification happens before the walk.
#[test]
fn a_nested_multiplexer_pane_is_explained_not_silently_skipped() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let mut s = Scratch::new("nested_mux");

    // An inner tmux server (its own `-L` socket, killed with the outer one on drop) attached from a
    // pane on the outer scratch server: the real nesting, not a `tmux` process standing in for one.
    let inner = s.nested_socket("nested_mux-inner");
    let cmd = format!("printf 'READY\\n'; exec tmux -L {inner} -f /dev/null new-session -A -s in");
    assert!(s
        .tmux(&["new-session", "-d", "-x", "80", "-y", "24", &cmd])
        .status
        .success());
    let pane = s.display("", "#{pane_id}");
    // Readiness IS the property under test: wait for the outer pane's foreground to become the
    // inner tmux client. A host where it never does has nothing to test, so skip rather than fail.
    let ready = common::wait_until(common::POLL_CEILING, || {
        basename(&s.display(&pane, "#{pane_current_command}")) == "tmux"
    });
    if !ready {
        eprintln!("skipping: the pane's foreground never became the nested tmux client");
        return;
    }

    let out = s.tma(&["debug", "explain", &pane, "--json"]);
    assert!(out.status.success());
    let json = String::from_utf8_lossy(&out.stdout);
    assert!(
        json.contains("\"out_of_scope_kind\":\"nested_multiplexer\""),
        "the pane is classified as a nested multiplexer: {json}"
    );
    assert!(
        json.contains("\"agent\":\"unknown\""),
        "and claims no agent of its own: {json}"
    );

    let text = String::from_utf8_lossy(&s.tma(&["debug", "explain", &pane]).stdout).to_string();
    assert!(
        text.contains("nested multiplexer") && text.contains("run tma there"),
        "the human form says where the state actually lives: {text}"
    );

    // The non-regression a registration must not weaken: with no `@agent_session` claiming this
    // pane, it stays invisible — no row — and a leftover stamp is removed rather than trusted.
    s.write_manifest("myagent.toml", BOUNDARY_AGENT);
    s.set_opt(&pane, "@agent_name", "myagent");
    s.set_opt(&pane, "@agent_state", "working");
    s.set_opt(&pane, "@agent_source", "hook");
    s.set_opt(&pane, "@agent_pid", "0");
    s.set_opt(&pane, "@agent_since", "1000000000");
    s.set_opt(&pane, "@agent_evidence_at", "1000000000");
    age_stamp(&s, &pane);
    let rows = s.ls_json();
    assert!(
        !rows.contains(&format!("\"pane\":\"{pane}\"")),
        "an unregistered nested-multiplexer pane must never produce a row: {rows}"
    );
    assert_eq!(
        s.pane_option(&pane, "@agent_state"),
        "",
        "and its leftover stamp is removed, as before"
    );
}

/// A hook-only manifest for the boundary tests: nothing on the host can ever match its
/// `process_names`, so identity rests entirely on the registration the `Boot` event writes.
const BOUNDARY_AGENT: &str = "min_engine_version = \"0.1\"\n\
     [identity]\nprocess_names = [\"tma-no-such-agent-proc\"]\n\
     [hooks]\ncovers = [\"working\", \"lifecycle\"]\n\
     [[hooks.map]]\nevent = \"Boot\"\nclaim = { lifecycle = \"start\" }\n\
     [[hooks.map]]\nevent = \"Run\"\nclaim = { state = \"working\" }\n\
     [[hooks.map]]\nevent = \"Bye\"\nclaim = { lifecycle = \"end\" }\n\
     [capture]\n";

const BOUNDARY_PAYLOAD: &str = r#"{"session_id":"sess-1","hook_event_name":"Boot"}"#;

/// A copy of the `tma` binary named `docker`, parked on a blocking read of the pane's tty. This is
/// the portable way to give a pane a `#{pane_current_command}` of `docker` on a host with no Docker:
/// a renamed coreutils binary dispatches on argv[0] and exits, and a copied system shell is killed
/// by macOS code signing, while a copy of our own binary runs anywhere the suite already runs.
fn fake_remote_client(s: &Scratch) -> PathBuf {
    let dst = s.workdir.join("docker");
    std::fs::copy(common::tma_bin(), &dst).expect("copy the tma binary as a fake docker client");
    dst
}

/// Backdate `@agent_stamped_at` so the next cycle takes the producer path (identity + fold) rather
/// than consuming the hook-fresh stamp, without sleeping out the freshness window.
fn age_stamp(s: &Scratch, pane: &str) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    s.set_opt(pane, "@agent_stamped_at", &(now - 60_000).to_string());
}

/// A pane whose foreground is a container client, carrying a live hook registration, is an agent
/// pane: the hook fired IN this pane, so the boundary hides the agent's process, not the agent.
/// Under the carve-out alone the poll cycle did not merely skip such a pane — it REMOVED the stamps
/// a container agent had just written, three seconds after every hook.
#[test]
fn a_registered_pane_behind_a_remote_shell_keeps_its_stamps() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("registered_remote");
    let client = fake_remote_client(&s);
    let cmd = format!("exec {} debug redact /dev/stdin", client.display());
    assert!(s
        .tmux(&["new-session", "-d", "-x", "80", "-y", "24", &cmd])
        .status
        .success());
    let pane = s.display("", "#{pane_id}");
    // The carve-out keys on the reported foreground, so a host where tmux never reports `docker`
    // has nothing to test.
    if !common::wait_until(common::POLL_CEILING, || {
        basename(&s.display(&pane, "#{pane_current_command}")) == "docker"
    }) {
        eprintln!("skipping: the pane's foreground never became the fake docker client");
        return;
    }

    s.write_manifest("myagent.toml", BOUNDARY_AGENT);
    assert!(s
        .event("myagent", "Boot", &pane, BOUNDARY_PAYLOAD)
        .status
        .success());
    assert!(s
        .event("myagent", "Run", &pane, BOUNDARY_PAYLOAD)
        .status
        .success());
    assert_eq!(s.pane_option(&pane, "@agent_state"), "working");

    // Three producer cycles, the stamp aged before each so the consumer fast path never stands in
    // for the identity decision under test.
    for i in 1..=3 {
        age_stamp(&s, &pane);
        let json = s.ls_json();
        assert_eq!(
            s.pane_option(&pane, "@agent_state"),
            "working",
            "cycle {i} wiped a container agent's stamp"
        );
        assert!(
            json.contains(&format!("\"pane\":\"{pane}\"")),
            "cycle {i}: the registered pane must still have a row: {json}"
        );
    }
    assert_eq!(
        s.pane_option(&pane, "@agent_session"),
        "sess-1",
        "the registration is what holds the pane; only a SessionEnd clears it"
    );

    // Deregistration is the way out, exactly as for any registered pane: the stamps go, and with
    // the registration gone the carve-out is back in force, so the pane leaves the listing.
    assert!(s
        .event("myagent", "Bye", &pane, BOUNDARY_PAYLOAD)
        .status
        .success());
    assert_eq!(s.pane_option(&pane, "@agent_state"), "");
    let json = s.ls_json();
    assert!(
        !json.contains(&format!("\"pane\":\"{pane}\"")),
        "a deregistered pane behind a remote shell is out of scope again: {json}"
    );
}

/// A user manifest in `<XDG>/tma/agents/` whose stem collides with a bundled one shadows it, while a
/// new stem loads alongside the corpus. Exercised through the real `manifests::load(None)` overlay
/// with the config pinned to a temp `XDG_CONFIG_HOME`, so the real `~/.config` is never touched.
#[test]
fn user_manifest_shadows_bundled_and_adds_new_stem() {
    if !tma_test_support::tmux_available() {
        eprintln!("skipping: tmux not installed");
        return;
    }
    let s = Scratch::new("override");

    // Two panes with distinctly-named foreground processes, so one user manifest matches each.
    let out = s.tmux(&[
        "new-session",
        "-d",
        "-s",
        "a",
        "-x",
        "80",
        "-y",
        "24",
        "printf 'READY\\n'; exec sleep 100000",
    ]);
    assert!(
        out.status.success(),
        "new-session a failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = s.tmux(&[
        "new-session",
        "-d",
        "-s",
        "b",
        "-x",
        "80",
        "-y",
        "24",
        "printf 'READY\\n'; exec tail -f /dev/null",
    ]);
    assert!(
        out.status.success(),
        "new-session b failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The `READY` marker gates readiness host-agnostically (its render implies the shell reached
    // its `exec`), unlike a `#{pane_current_command}` name check that a coreutils `sleep` breaks.
    assert!(
        common::wait_capture_contains(&s.socket, "a", "READY", common::POLL_CEILING)
            && common::wait_capture_contains(&s.socket, "b", "READY", common::POLL_CEILING),
        "the two agent panes did not render their READY markers"
    );

    let pane_a = s.display("a", "#{pane_id}");
    let pane_b = s.display("b", "#{pane_id}");
    let names_a = process_names_csv(&s, "a");
    let names_b = process_names_csv(&s, "b");
    // Distinct names are the whole point: shared names would let both manifests match one pane
    // and make identity ambiguous, hollowing out the assertions below.
    assert_ne!(
        names_a, names_b,
        "test needs two distinctly-named agent processes (got {names_a} / {names_b})"
    );

    // A user config dir: `claude.toml` shadows the bundled `claude` (which matches neither pane) with
    // one matching pane A; `sidekick.toml` is a brand-new stem matching pane B.
    let xdg = s.workdir.join("xdg");
    let agents = xdg.join("tma/agents");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::write(agents.join("claude.toml"), manifest_toml(&names_a)).unwrap();
    std::fs::write(agents.join("sidekick.toml"), manifest_toml(&names_b)).unwrap();

    // Control: with an empty user config only the bundled corpus loads, so pane A (a `sleep`)
    // resolves to no agent, proving the identities below come from the user overlay.
    let empty = s.workdir.join("xdg_empty");
    std::fs::create_dir_all(&empty).unwrap();
    let control = explain_xdg(&s, &pane_a, &empty);
    assert!(
        control.contains("\"agent\":\"unknown\""),
        "bundled-only load must not identify a sleep pane as an agent:\n{control}"
    );

    // Shadowing: the user `claude.toml` replaced the bundled one, so pane A is now `claude`.
    let ja = explain_xdg(&s, &pane_a, &xdg);
    assert!(
        ja.contains("\"agent\":\"claude\""),
        "user claude.toml must shadow the bundled manifest by stem:\n{ja}"
    );
    // New stem alongside: pane B resolves to the added `sidekick` manifest.
    let jb = explain_xdg(&s, &pane_b, &xdg);
    assert!(
        jb.contains("\"agent\":\"sidekick\""),
        "a new-stem user manifest must load alongside the bundled corpus:\n{jb}"
    );
}
