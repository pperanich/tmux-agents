//! Bounded authenticated-free activation observation. Claude remains on fallback
//! unless native reports and independently declared coverage are actually verified.
use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tma_core::program_status::{ProgramCommand, ProgramEvent};
use tma_test_support::{self as common, DaemonGuard, Scratch};
use tma_tmux::control::{self, ControlPool};
use tma_tmux::tmux::Tmux;

#[test]
fn native_claude_bare_activation_probe_reports_separate_fallback_verdict() {
    if !common::have_agent("claude") || !common::tmux_available() {
        return;
    }
    let _gate = common::DaemonTestGuard::acquire();
    let scratch = Scratch::new_daemon("native_claude");
    scratch.write_config("[program_status]\nmode = 'prefer'\n[daemon]\nquiet_ms = 10000\nsweep_secs = 60\n[notify]\non = []\n");
    let home = scratch.workdir.join("claude-home");
    let project = scratch.workdir.join("empty-project");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    let path = std::env::var("PATH").unwrap();
    let mut env = common::live_agent_env("claude", &home);
    env.extend([
        (
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".into(),
            "1".into(),
        ),
        (
            "CLAUDE_CODE_DISABLE_OFFICIAL_MARKETPLACE_AUTOINSTALL".into(),
            "1".into(),
        ),
    ]);
    let mut help = Command::new("claude");
    let help = help
        .env_clear()
        .env("PATH", &path)
        .envs(env.iter().cloned())
        .arg("--help")
        .output()
        .unwrap();
    assert!(help.status.success());
    let help = String::from_utf8_lossy(&help.stdout);
    assert!(
        help.contains("--bare") && help.contains("keychain are never read"),
        "installed --bare isolation contract unavailable"
    );
    let mut version = Command::new("claude");
    let version = version
        .env_clear()
        .env("PATH", &path)
        .envs(env.iter().cloned())
        .arg("--version")
        .output()
        .unwrap();
    assert!(version.status.success());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    assert!(scratch
        .tmux(&[
            "new-session",
            "-d",
            "-s",
            "claude",
            "-x",
            "100",
            "-y",
            "32",
            "-c",
            project.to_str().unwrap(),
            "/bin/sh"
        ])
        .status
        .success());
    let pane = scratch.get("claude", "#{pane_id}");
    let mut daemon = DaemonGuard::new(
        scratch
            .command()
            .args([
                "--socket-name",
                &scratch.socket,
                "--config",
                scratch.config_path().to_str().unwrap(),
                "daemon",
                "--status-file",
                scratch.status_path().to_str().unwrap(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(scratch.daemon_log_stdio())
            .spawn()
            .unwrap(),
    );
    common::poll_until("production collector precedes Claude", || {
        assert!(matches!(daemon.child_status(), Ok(None)));
        scratch.status_u64("clients") == 1 && scratch.status_u64("pending_seeds") == 0
    });
    let tmux = Tmux::new(Some(scratch.socket.clone()));
    // Supplemental observation uses the same production decoder, never a reply or
    // negotiation input. It distinguishes native emission from alias eligibility.
    let mut observer = ControlPool::default();
    observer.set_program_collection(true);
    observer.reconcile(&tmux).unwrap();
    common::poll_until("supplemental observer attached before Claude", || {
        pump(&mut observer, &tmux);
        !observer.pending_seed()
    });
    let mut args: Vec<String> = if cfg!(target_os = "macos") {
        vec!["/usr/bin/sandbox-exec".into(), "-p".into(), "(version 1)(allow default)(deny network-outbound)(allow network-outbound (remote ip \"localhost:*\"))".into()]
    } else {
        Vec::new()
    };
    args.extend(["env".to_owned(), "-i".into(), format!("PATH={path}")]);
    args.extend(env.into_iter().map(|(key, value)| format!("{key}={value}")));
    args.extend([
        "ANTHROPIC_API_KEY=local-only-fake-key".into(),
        format!("ANTHROPIC_BASE_URL=http://{address}"),
        "claude".into(),
        "--bare".into(),
        "--setting-sources".into(),
        "".into(),
        "--strict-mcp-config".into(),
        "--mcp-config".into(),
        "{\"mcpServers\":{}}".into(),
        "--tools".into(),
        "".into(),
        "Say hello for this isolated activation probe.".into(),
    ]);
    let launch = format!(
        "{}; printf '\\nCLAUDE_PROCESS_DEPARTED\\n'; exec /bin/sh",
        args.iter()
            .map(|arg| format!("'{}'", arg.replace('\'', "'\\''")))
            .collect::<Vec<_>>()
            .join(" ")
    );
    assert!(scratch
        .tmux(&[
            "respawn-pane",
            "-k",
            "-t",
            &pane,
            "-c",
            project.to_str().unwrap(),
            &launch
        ])
        .status
        .success());
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut requests = 0;
    let mut queries = 0;
    let mut reports = Vec::new();
    while Instant::now() < deadline {
        assert!(matches!(daemon.child_status(), Ok(None)));
        pump(&mut observer, &tmux);
        for event in observer.drain_program().observations {
            match event.event {
                ProgramEvent::Command(ProgramCommand::Query) => queries += 1,
                ProgramEvent::Command(command) => reports.push(format!("{command:?}")),
                _ => {}
            }
        }
        match listener.accept() {
            Ok((mut stream, peer)) => {
                assert!(peer.ip().is_loopback());
                requests += 1;
                assert!(requests <= 8, "bounded local activation requests");
                stream
                    .set_read_timeout(Some(Duration::from_millis(100)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_millis(100)))
                    .unwrap();
                let mut bytes = [0; 4096];
                let _ = stream.read(&mut bytes);
                let body = r#"{"type":"error","error":{"type":"invalid_request_error","message":"Local activation probe has no model"}}"#;
                let _ = write!(stream, "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => panic!("local Claude activation listener: {error}"),
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let read = tma_tmux::persistence::read_pane(&tmux, &pane).unwrap();
    assert!(
        !read.program.program_origin,
        "Claude has no verified bundled OSC alias"
    );
    let capture = scratch.tmux(&["capture-pane", "-p", "-t", &pane]);
    eprintln!("Claude activation: version={}; startup_prompt_attempts=1; window_ms=10000; loopback_requests={requests}; native_queries={queries}; native_reports={reports:?}; daemon_status={:?}; verdict=unverified,fallback; screen={}", String::from_utf8_lossy(&version.stdout).trim(), scratch.status(), String::from_utf8_lossy(&capture.stdout));
    assert!(
        !String::from_utf8_lossy(&capture.stdout).contains("Error: --"),
        "probe CLI invocation failed: {}",
        String::from_utf8_lossy(&capture.stdout)
    );
    // Explicit scratch-pane teardown terminates the agent before the local listener drops.
    assert!(scratch
        .tmux(&["respawn-pane", "-k", "-t", &pane, "exec /bin/sh"])
        .status
        .success());
    assert!(Command::new("kill")
        .args(["-TERM", &daemon.pid().to_string()])
        .status()
        .unwrap()
        .success());
    assert!(daemon.wait_exit(Duration::from_secs(10)));
}
fn pump(pool: &mut ControlPool, tmux: &Tmux) {
    let ready = control::pollfds(pool)
        .into_iter()
        .map(|(session, _)| session)
        .collect();
    if control::dispatch_ready(pool, &ready, Instant::now()).need_reconcile {
        pool.reconcile(tmux).unwrap();
    }
    control::seed_attached(pool, tmux, Instant::now());
}
