//! Acceptance: the OpenCode plugin asset, executed under `node` against a stub `tma-hook`.
//!
//! The rest of the suite only asserts on the plugin's TEXT (that install renders it, that its
//! `fire()` tokens match the manifest). This one runs it: a driver module imports the rendered
//! plugin, feeds it OpenCode-shaped events, and the stub wrapper records the `opencode <event>`
//! argv plus the JSON it received on stdin. No tmux and no `tma` binary are involved.
//!
//! Skipped when `node` is absent (it is not a build dependency of tma).

use std::path::{Path, PathBuf};
use std::process::Command;

use tma_test_support as common;

const PLUGIN_SRC: &str = include_str!("../assets/opencode-plugin.js");

/// One `tma-hook` fire the stub recorded: the two positionals and the stdin payload.
struct Fire {
    agent: String,
    event: String,
    payload: String,
}

fn node_available() -> bool {
    Command::new("node")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// A scratch dir holding the stub wrapper, the rendered plugin, and the fire log.
fn workdir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tma_oc_plugin_{}", common::unique_id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The stub `tma-hook`: appends `<agent> <event> <stdin>` as one line to `$TMA_TEST_LOG`. The
/// payload is read before the line is printed, so the concurrent fires cannot interleave mid-line.
fn stub_wrapper(dir: &Path) -> PathBuf {
    let path = dir.join("tma-hook");
    std::fs::write(
        &path,
        "#!/bin/sh\npayload=$(cat)\nprintf '%s %s %s\\n' \"$1\" \"$2\" \"$payload\" >> \"$TMA_TEST_LOG\"\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
    }
    path
}

/// Render the plugin with the stub wrapper baked in, as `.mjs` so node loads it as ESM (OpenCode
/// installs it as `tma.js` and loads it as a module either way).
fn render_plugin(dir: &Path, wrapper: &Path) -> PathBuf {
    let path = dir.join("plugin.mjs");
    let src = PLUGIN_SRC.replace("@@TMA_HOOK@@", &wrapper.display().to_string());
    assert!(!src.contains("@@TMA_HOOK@@"), "placeholder substituted");
    std::fs::write(&path, src).unwrap();
    path
}

/// Run `driver` under node with `TMUX_PANE` set unless `pane` is `None`, then wait for the
/// detached wrapper children to land `expected` lines in the log (they outlive node by design).
fn drive(dir: &Path, driver: &str, pane: Option<&str>, expected: usize) -> Vec<Fire> {
    let log = dir.join("fires.log");
    let path = dir.join("driver.mjs");
    std::fs::write(&path, driver).unwrap();

    let mut cmd = Command::new("node");
    cmd.arg(&path).env("TMA_TEST_LOG", &log);
    match pane {
        Some(p) => cmd.env("TMUX_PANE", p),
        None => cmd.env_remove("TMUX_PANE"),
    };
    let out = cmd.output().expect("spawn node");
    assert!(
        out.status.success(),
        "driver failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Expecting none: give a stray fire a beat to land rather than racing it to the log.
    if expected == 0 {
        std::thread::sleep(std::time::Duration::from_millis(300));
        return read_fires(&log);
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let fires = read_fires(&log);
        if fires.len() >= expected || std::time::Instant::now() > deadline {
            return fires;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

fn read_fires(log: &Path) -> Vec<Fire> {
    let Ok(text) = std::fs::read_to_string(log) else {
        return Vec::new();
    };
    text.lines()
        .filter(|l| !l.is_empty())
        .map(|line| {
            let mut parts = line.splitn(3, ' ');
            Fire {
                agent: parts.next().unwrap_or_default().to_string(),
                event: parts.next().unwrap_or_default().to_string(),
                payload: parts.next().unwrap_or_default().to_string(),
            }
        })
        .collect()
}

/// The payload of the single fire carrying `event`, or the first when several do.
fn payload_of<'a>(fires: &'a [Fire], event: &str) -> &'a str {
    let fire = fires
        .iter()
        .find(|f| f.event == event)
        .unwrap_or_else(|| panic!("no {event} fire in {:?}", tokens(fires)));
    &fire.payload
}

fn tokens(fires: &[Fire]) -> Vec<&str> {
    fires.iter().map(|f| f.event.as_str()).collect()
}

/// The full event lane: load, the bus edges, and the API-channel fields the intake reads
/// (`session_id`, `api_endpoint`, `request_id`).
#[test]
fn plugin_forwards_opencode_events_to_the_wrapper() {
    if !node_available() {
        eprintln!("skipping: node not installed");
        return;
    }
    let dir = workdir();
    let plugin = render_plugin(&dir, &stub_wrapper(&dir));
    let driver = format!(
        r#"
import {{ TmaBridge }} from "{plugin}";

const hooks = await TmaBridge({{ serverUrl: "http://127.0.0.1:4096" }});
const bus = (type, properties) => hooks.event({{ event: {{ type, properties }} }});
const ses = "ses_test01";

await bus("session.created", {{ sessionID: ses }});
await bus("session.status", {{ sessionID: ses, status: {{ type: "busy" }} }});
await bus("permission.asked", {{
  sessionID: ses,
  id: "per_test01",
  permission: "bash",
}});
await bus("permission.replied", {{ sessionID: ses, requestID: "per_test01", reply: "once" }});
await bus("session.idle", {{ sessionID: ses }});
"#,
        plugin = plugin.display()
    );

    // Six fires: the load-time registration plus one per bus edge.
    let fires = drive(&dir, &driver, Some("%9"), 6);
    let mut seen = tokens(&fires);
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(
        seen,
        [
            "permission-replied",
            "permission-required",
            "session-start",
            "stop",
            "user-prompt-submit"
        ],
        "wrapper tokens: {:?}",
        tokens(&fires)
    );
    assert!(
        fires.iter().all(|f| f.agent == "opencode"),
        "every fire names the opencode agent"
    );

    // Two registrations: the one at load and the one on `session.created`. The fires are separate
    // detached processes, so the log tells us nothing about their order.
    let starts: Vec<&str> = fires
        .iter()
        .filter(|f| f.event == "session-start")
        .map(|f| f.payload.as_str())
        .collect();
    assert_eq!(starts.len(), 2, "load plus session.created: {starts:?}");
    assert!(
        starts
            .iter()
            .all(|p| p.contains(r#""api_endpoint":"http://127.0.0.1:4096""#)),
        "both carry the serving base URL: {starts:?}"
    );
    // The load fire precedes any session event, so it names no session: an empty one would stamp
    // `@agent_session` blank rather than leave it for the `session.created` edge.
    assert!(
        starts.iter().any(|p| !p.contains("session_id")),
        "load registration carries no session id: {starts:?}"
    );
    assert!(
        starts
            .iter()
            .any(|p| p.contains(r#""session_id":"ses_test01""#)),
        "session.created records the session: {starts:?}"
    );

    let blocked = payload_of(&fires, "permission-required");
    assert!(
        blocked.contains(r#""session_id":"ses_test01""#)
            && blocked.contains(r#""request_id":"per_test01""#),
        "permission.asked: {blocked}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Outside tmux the plugin registers nothing: `tma event` binds to `$TMUX_PANE`, so a fire there
/// would spawn a process per event to no effect.
#[test]
fn plugin_is_inert_without_a_pane() {
    if !node_available() {
        eprintln!("skipping: node not installed");
        return;
    }
    let dir = workdir();
    let plugin = render_plugin(&dir, &stub_wrapper(&dir));
    let driver = format!(
        r#"
import {{ TmaBridge }} from "{plugin}";

const hooks = await TmaBridge({{ serverUrl: "http://127.0.0.1:4096" }});
if (hooks.event) throw new Error("hooks registered without a pane");
"#,
        plugin = plugin.display()
    );

    let fires = drive(&dir, &driver, None, 0);
    assert!(fires.is_empty(), "fires outside tmux: {:?}", tokens(&fires));

    let _ = std::fs::remove_dir_all(&dir);
}
