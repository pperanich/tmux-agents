//! Live gate for Pi's own OSC 7501 reports through the production daemon.
//! No tma state/metadata extension, account, credential, external model or context files.
//! The sole extension calls Pi's UI confirm; the loopback model only supplies HTTP/SSE.
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::Value;
use tma_core::persistence::ProjectionRead;
use tma_core::program_status::ResultKind;
use tma_core::stamp::opt;
use tma_test_support::{self as common, DaemonGuard, Scratch};
use tma_tmux::persistence as store;
use tma_tmux::tmux::Tmux;

struct ModelState {
    requests: AtomicUsize,
    released: Mutex<usize>,
    changed: Condvar,
    stop: AtomicBool,
}
struct LocalModel {
    port: u16,
    state: Arc<ModelState>,
    thread: Option<JoinHandle<()>>,
}
impl LocalModel {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let state = Arc::new(ModelState {
            requests: AtomicUsize::new(0),
            released: Mutex::new(0),
            changed: Condvar::new(),
            stop: AtomicBool::new(false),
        });
        let shared = Arc::clone(&state);
        let thread = std::thread::spawn(move || {
            let mut children = Vec::new();
            while !shared.stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, peer)) => {
                        assert!(peer.ip().is_loopback());
                        assert!(children.len() < 8, "bounded model connections");
                        let state = Arc::clone(&shared);
                        children.push(std::thread::spawn(move || serve_model(stream, &state)));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("local model accept: {error}"),
                }
            }
            for child in children {
                child.join().unwrap();
            }
        });
        Self {
            port,
            state,
            thread: Some(thread),
        }
    }
    fn release(&self, request: usize) {
        *self.state.released.lock().unwrap() = request;
        self.state.changed.notify_all();
    }
    fn await_request(&self, request: usize) {
        common::poll_until("Pi reached local model", || {
            self.state.requests.load(Ordering::SeqCst) >= request
        });
    }
}
impl Drop for LocalModel {
    fn drop(&mut self) {
        self.state.stop.store(true, Ordering::SeqCst);
        self.state.changed.notify_all();
        self.thread.take().unwrap().join().unwrap();
    }
}
fn serve_model(mut stream: TcpStream, state: &ModelState) {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut buf = [0; 4096];
    let (header_end, length) =
        loop {
            let Ok(n) = stream.read(&mut buf) else { return };
            if n == 0 {
                return;
            }
            bytes.extend_from_slice(&buf[..n]);
            assert!(bytes.len() <= 128 * 1024, "local request bound");
            if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                let header = std::str::from_utf8(&bytes[..end]).unwrap();
                assert!(header.starts_with("POST /v1/chat/completions HTTP/1.1\r\n"));
                assert!(header
                    .lines()
                    .any(|line| line
                        .eq_ignore_ascii_case("authorization: Bearer local-only-fake-key")));
                let length = header
                    .lines()
                    .find_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        key.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap();
                assert!(length <= 128 * 1024);
                break (end + 4, length);
            }
        };
    while bytes.len() - header_end < length {
        let Ok(n) = stream.read(&mut buf) else { return };
        if n == 0 {
            return;
        }
        bytes.extend_from_slice(&buf[..n]);
        assert!(bytes.len() <= 128 * 1024);
    }
    let body: Value = serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap();
    let request = state.requests.fetch_add(1, Ordering::SeqCst) + 1;
    let user = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|message| message["role"] == "user")
        .unwrap()["content"]
        .to_string();
    if user.contains("fail deliberately") {
        let body = r#"{"error":{"message":"Deliberate local model failure","type":"invalid_request_error"}}"#;
        let _ = write!(stream, "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        return;
    }
    if stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"id\":\"local\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"local\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"},\"finish_reason\":null}]}\n\n").is_err() { return; }
    let released = state.released.lock().unwrap();
    let (released, timeout) = state
        .changed
        .wait_timeout_while(released, Duration::from_secs(45), |released| {
            *released < request && !state.stop.load(Ordering::SeqCst)
        })
        .unwrap();
    if timeout.timed_out() || state.stop.load(Ordering::SeqCst) || *released < request {
        return;
    }
    let _ = stream.write_all(b"data: {\"id\":\"local\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"local\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Local model completed.\"},\"finish_reason\":null}]}\n\ndata: {\"id\":\"local\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"local\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n");
}

struct Pi {
    scratch: Scratch,
    daemon: DaemonGuard,
    tmux: Tmux,
    pane: String,
    model: LocalModel,
    notifications: PathBuf,
}
impl Pi {
    fn command(scratch: &Scratch) -> Command {
        let mut cmd = scratch.command();
        cmd.args([
            "--socket-name",
            &scratch.socket,
            "--config",
            scratch.config_path().to_str().unwrap(),
        ]);
        cmd
    }
    fn new() -> Self {
        let scratch = Scratch::new_daemon("native_pi");
        let model = LocalModel::new();
        let notifications = scratch.workdir.join("notifications");
        scratch.write_config(&format!("[program_status]\nmode = 'prefer'\n[daemon]\nquiet_ms = 10000\nsweep_secs = 60\n[notify]\non = ['blocked', 'done']\nbell = false\nosc = false\ncommand = \"cat >> {}\"\n", notifications.display()));
        let home = scratch.workdir.join("pi-home");
        let repo = scratch.workdir.join("empty-project");
        let config = home.join(".pi/agent");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(config.join("models.json"), format!(r#"{{"providers":{{"local":{{"baseUrl":"http://127.0.0.1:{}/v1","api":"openai-completions","apiKey":"local-only-fake-key","models":[{{"id":"local","name":"Local deterministic model","contextWindow":32000,"maxTokens":1000,"reasoning":false,"input":["text"],"cost":{{"input":0,"output":0,"cacheRead":0,"cacheWrite":0}}}}]}}}}}}"#, model.port)).unwrap();
        std::fs::write(
            config.join("settings.json"),
            r#"{"quietStartup":true,"retry":{"enabled":false}}"#,
        )
        .unwrap();
        let extension = scratch.workdir.join("dialog.mjs");
        std::fs::write(&extension, "export default function(pi) { pi.registerCommand('osc-dialog', {description: 'Local test dialog', handler: async (_, ctx) => { await ctx.ui.confirm('Native Pi permission', 'Continue local test?'); }}); }\n").unwrap();
        let path = std::env::var("PATH").unwrap();
        let env = common::live_agent_env("pi", &home);
        let mut version = Command::new("pi");
        version
            .env_clear()
            .env("PATH", &path)
            .envs(env.iter().cloned())
            .env("PI_TELEMETRY", "0");
        let out = version.arg("--version").output().unwrap();
        assert!(out.status.success());
        eprintln!(
            "native Pi version={} tmux={} host={}",
            String::from_utf8_lossy(&out.stdout).trim(),
            String::from_utf8_lossy(&scratch.tmux(&["-V"]).stdout).trim(),
            std::env::consts::OS
        );
        let flags = [
            "--offline",
            "--no-session",
            "--no-extensions",
            "--no-mcp",
            "--no-skills",
            "--no-prompt-templates",
            "--no-themes",
            "--no-context-files",
            "--no-approve",
            "--no-tools",
        ];
        let mut help = Command::new("pi");
        let help = help
            .env_clear()
            .env("PATH", &path)
            .envs(env.iter().cloned())
            .env("PI_TELEMETRY", "0")
            .arg("--help")
            .output()
            .unwrap();
        assert!(help.status.success());
        for flag in flags {
            assert!(
                String::from_utf8_lossy(&help.stdout).contains(flag),
                "Pi lacks {flag}"
            );
        }
        assert!(scratch
            .tmux(&[
                "new-session",
                "-d",
                "-s",
                "pi",
                "-x",
                "100",
                "-y",
                "32",
                "-c",
                repo.to_str().unwrap(),
                "/bin/sh"
            ])
            .status
            .success());
        let pane = scratch.get("pi", "#{pane_id}");
        let daemon = DaemonGuard::new(
            Self::command(&scratch)
                .args([
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
        let tmux = Tmux::new(Some(scratch.socket.clone()));
        let mut f = Self {
            scratch,
            daemon,
            tmux,
            pane,
            model,
            notifications,
        };
        f.until("production collector attaches before Pi starts", |f| {
            f.count("clients") == 1 && f.count("pending_seeds") == 0
        });
        let mut args = vec!["env".to_string(), "-i".into(), format!("PATH={path}")];
        args.extend(env.into_iter().map(|(key, value)| format!("{key}={value}")));
        args.extend([
            "PI_PROGRAM_STATUS=1".into(),
            "PI_TELEMETRY=0".into(),
            "pi".into(),
        ]);
        args.extend(flags.into_iter().map(str::to_owned));
        args.extend([
            "--extension".into(),
            extension.display().to_string(),
            "--provider".into(),
            "local".into(),
            "--model".into(),
            "local".into(),
            "--thinking".into(),
            "off".into(),
        ]);
        let launch = format!(
            "{}; printf '\\nPI_PROCESS_DEPARTED\\n'; exec /bin/sh",
            args.iter()
                .map(|s| shell_quote(s))
                .collect::<Vec<_>>()
                .join(" ")
        );
        assert!(f
            .scratch
            .tmux(&[
                "respawn-pane",
                "-k",
                "-t",
                &f.pane,
                "-c",
                repo.to_str().unwrap(),
                &launch
            ])
            .status
            .success());
        f.until("Pi independently recognized and birth-bound", |f| {
            f.scratch
                .status()
                .get(&format!("program_pane_{}", f.pane))
                .is_some_and(|s| s.contains("owner=Some"))
        });
        f.until("native Pi composer rendered", |f| {
            f.capture().contains("local")
        });
        f
    }
    fn until(&mut self, what: &str, mut cond: impl FnMut(&Self) -> bool) {
        let okay = common::wait_until(common::POLL_CEILING, || {
            assert!(
                matches!(self.daemon.child_status(), Ok(None)),
                "daemon exited before {what}"
            );
            cond(self)
        });
        assert!(
            okay,
            "{what}: child={:?}{}",
            self.daemon.child_status(),
            self.scratch.forensics(&[&self.pane])
        );
    }
    fn count(&self, key: &str) -> u64 {
        self.scratch.status_u64(key)
    }
    fn capture(&self) -> String {
        String::from_utf8_lossy(
            &self
                .scratch
                .tmux(&["capture-pane", "-p", "-t", &self.pane])
                .stdout,
        )
        .into_owned()
    }
    fn read(&self) -> store::PaneRead {
        store::read_pane(&self.tmux, &self.pane).unwrap()
    }
    fn live(&self, state: &str) -> bool {
        matches!(self.read().projection(tma_runtime::now_ms()), Ok(ProjectionRead::Live { stamp, .. }) if stamp.state.token() == state)
    }
    fn submit(&self, text: &str) {
        self.tmux
            .send_text(&self.pane, &[], text, &["Enter".into()])
            .unwrap();
    }
    fn key(&self, key: &str) {
        self.tmux.send_keys(&self.pane, &[key.into()]).unwrap();
    }
    fn clear_attention(&mut self) {
        let out = Self::command(&self.scratch)
            .args(["clear-attention", &self.pane])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        self.until("exact attention acknowledgement", |f| {
            !f.read().options.contains_key("@agent_attention")
        });
    }
    fn notification_events(&self) -> Vec<Value> {
        serde_json::Deserializer::from_str(&self.notifications())
            .into_iter::<Value>()
            .filter_map(|event| match event {
                Ok(event) => Some(event),
                Err(error) if error.is_eof() => None,
                Err(error) => panic!("structured notification: {error}"),
            })
            .collect()
    }
    fn notification_count(&self, state: &str) -> usize {
        self.notification_events()
            .iter()
            .filter(|event| event["pane"] == self.pane && event["state"] == state)
            .count()
    }
    fn exact_effect_count(&self, state: &str, episode: u64) -> usize {
        self.notification_events()
            .iter()
            .filter(|event| {
                event["pane"] == self.pane
                    && event["state"] == state
                    && event["episode_ms"] == episode
            })
            .count()
    }
    fn episode(&self) -> u64 {
        match self.read().projection(tma_runtime::now_ms()).unwrap() {
            ProjectionRead::Live { stamp, .. } => stamp.episode_at(),
            other => panic!("expected live episode: {other:?}"),
        }
    }
    fn wait_effect(&mut self, state: &str, episode: u64, prior_pass: u64) {
        self.until(
            "exact pane/episode effect and completed notification pass",
            |f| {
                f.exact_effect_count(state, episode) == 1
                    && f.count("program_notify_passes") > prior_pass
            },
        );
    }
    fn notifications(&self) -> String {
        std::fs::read_to_string(&self.notifications).unwrap_or_default()
    }
    fn finished(&mut self) {
        self.until("native done result and exact notification claim", |f| {
            let read = f.read();
            f.live("idle")
                && read
                    .program
                    .result
                    .as_ref()
                    .is_some_and(|r| r.kind == ResultKind::Completion)
                && read
                    .program
                    .notified
                    .as_ref()
                    .is_some_and(|claim| Some(&claim.result) == read.program.result.as_ref())
        });
    }
}
impl Drop for Pi {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!(
                "native Pi failure: child={:?}{}",
                self.daemon.child_status(),
                self.scratch.forensics(&[&self.pane])
            );
        }
    }
}
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[test]
fn native_pi_osc7501_through_production_daemon_without_state_hooks() {
    if !common::have_agent("pi") || !common::tmux_available() {
        return;
    }
    let _gate = common::DaemonTestGuard::acquire();
    let mut f = Pi::new();
    // Native startup may only synchronize framing, and binding drops pre-binding reports.
    // A real UI dialog supplies another native ST without any injected protocol bytes.
    f.submit("/osc-dialog");
    f.until("warmup native dialog", |f| {
        f.capture().contains("Continue local test?")
    });
    f.key("Escape");
    f.until("warmup dialog closed", |f| {
        !f.capture().contains("Continue local test?")
    });
    let initial_done = f.notification_count("done");
    f.submit("first local turn");
    f.model.await_request(1);
    f.until("native working", |f| f.live("working"));
    let read = f.read();
    assert!(read.program.program_origin);
    let owner = read.program.owner.unwrap();
    assert_eq!(read.program.commit.unwrap().owner, owner);
    assert!(!owner.processes.as_slice().is_empty());
    assert!(
        !read.options.contains_key(opt::SESSION),
        "no state/metadata hooks installed"
    );
    let done_pass = f.count("program_notify_passes");
    f.model.release(1);
    f.finished();
    f.wait_effect("done", f.episode(), done_pass);
    f.until("first exact done effect", |f| {
        f.notification_count("done") == initial_done + 1
    });
    let result = f.read().program.result.unwrap();
    let turn = f.scratch.get(&f.pane, "#{@agent_turn_at}");
    f.clear_attention();
    assert_eq!(f.read().program.acknowledged.as_ref(), Some(&result));
    let done_count = f.notification_count("done");
    let blocked_count = f.notification_count("blocked");
    let started = Instant::now();
    let blocked_pass = f.count("program_notify_passes");
    f.submit("/osc-dialog");
    f.until(
        "real native permission dialog before quiet-edge capture",
        |f| f.live("blocked") && f.scratch.get(&f.pane, "#{@agent_detail}") == "permission",
    );
    let blocked_ms = started.elapsed().as_millis();
    assert!(
        blocked_ms < 10_000,
        "native blocked arrived after quiet edge: {blocked_ms}ms"
    );
    let blocked_episode = f.episode();
    f.wait_effect("blocked", blocked_episode, blocked_pass);
    let blocked = f.read();
    for key in [
        opt::PENDING_CALL,
        opt::PENDING_TOOL,
        opt::PENDING_SUMMARY,
        opt::PERMISSION_REQUEST,
        opt::QUESTION_REQUEST,
    ] {
        assert!(
            !blocked.options.contains_key(key),
            "protocol dialog created action metadata {key}"
        );
    }
    let approve = Pi::command(&f.scratch)
        .args([
            "act",
            "approve",
            "--pane",
            &f.pane,
            "--expect-permission-request",
            "native-protocol-text-has-no-request",
            "--json",
        ])
        .output()
        .unwrap();
    let refusal: Value =
        serde_json::from_slice(&approve.stdout).expect("structured broker refusal");
    assert_eq!(approve.status.code(), Some(4), "approve status: {refusal}");
    assert_eq!(refusal["outcome"], "refused");
    assert_eq!(
        refusal["reason"], "wrong-agent",
        "Pi has no shipped approval action mapping"
    );
    f.until("exact blocked notification effect", |f| {
        f.notification_count("blocked") == blocked_count + 1
    });
    assert_eq!(f.notification_count("done"), done_count);
    assert!(f.live("blocked"));
    f.clear_attention();
    let restore_pass = f.count("program_notify_passes");
    f.key("Escape");
    f.until("native dialog restores spent done", |f| f.live("idle"));
    f.until("restore notification pass completed", |f| {
        f.count("program_notify_passes") > restore_pass
            && f.scratch
                .status()
                .get(&format!("program_pane_{}", f.pane))
                .is_some_and(|value| value.contains("selection=Some(Done)"))
    });
    assert_eq!(f.read().program.result.as_ref(), Some(&result));
    assert_eq!(f.scratch.get(&f.pane, "#{@agent_turn_at}"), turn);
    assert!(!f.read().options.contains_key("@agent_attention"));
    assert_eq!(
        f.notification_count("done"),
        done_count,
        "restore duplicated done"
    );
    assert_eq!(f.notification_count("blocked"), blocked_count + 1);
    eprintln!("native dialog: blocked_ms={blocked_ms}, done_effects={done_count}, blocked_effects={}, restored_exact_spent_result=true", f.notification_count("blocked"));
    // Two consecutive model turns retain separate native work/result generations.
    for request in 2..=3 {
        let old = f.read().program.result;
        let done_count = f.notification_count("done");
        f.submit(&format!("rapid local turn {request}"));
        f.model.await_request(request);
        f.until("rapid native working", |f| f.live("working"));
        let done_pass = f.count("program_notify_passes");
        f.model.release(request);
        f.finished();
        f.wait_effect("done", f.episode(), done_pass);
        f.until("one rapid done effect", |f| {
            f.notification_count("done") == done_count + 1
        });
        assert_ne!(f.read().program.result, old);
        f.clear_attention();
    }
    let since = tma_runtime::now_ms();
    let failure_pass = f.count("program_notify_passes");
    f.submit("fail deliberately");
    f.model.await_request(4);
    f.until("real Pi HTTP failure projects idle/error", |f| {
        f.live("idle")
            && f.scratch.get(&f.pane, "#{@agent_detail}") == "error"
            && f.read()
                .program
                .result
                .as_ref()
                .is_some_and(|r| r.kind == ResultKind::Failure)
    });
    let failed_episode = f.episode();
    f.wait_effect("done", failed_episode, failure_pass);
    let failure_effects = f.notification_events();
    assert!(failure_effects.iter().any(|event| event["pane"] == f.pane
        && event["episode_ms"] == failed_episode
        && event["detail"] == "error"));
    let rows = Pi::command(&f.scratch)
        .args(["ls", "--json"])
        .output()
        .unwrap();
    assert!(rows.status.success());
    let rows: Value = serde_json::from_slice(&rows.stdout).unwrap();
    let row = rows["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["pane"] == f.pane)
        .unwrap();
    assert_eq!(row["state"], "idle");
    assert_eq!(row["detail"], "error");
    assert!(row["permission_request"].is_null());
    assert_eq!(
        f.read().options.get(opt::SOURCE).map(String::as_str),
        Some("hook")
    );
    let waited = Pi::command(&f.scratch)
        .args([
            "wait",
            "--pane",
            &f.pane,
            "--since",
            &since.to_string(),
            "--timeout",
            "2",
            "--until",
            "done",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        waited.status.success(),
        "failure wait: stdout={} stderr={}",
        String::from_utf8_lossy(&waited.stdout),
        String::from_utf8_lossy(&waited.stderr)
    );
    let waited: Value = serde_json::from_slice(&waited.stdout).unwrap();
    assert_eq!(waited["detail"], "error");
    assert!(waited["episode_ms"].as_u64().unwrap() > since);
    assert!(f
        .read()
        .program
        .result
        .as_ref()
        .is_some_and(|result| result.kind == ResultKind::Failure));
    f.clear_attention();
    f.submit("cancel this local turn");
    f.model.await_request(5);
    f.until("native cancellable working", |f| f.live("working"));
    let cancel_reports = f.count("program_reports");
    let cancel_pass = f.count("program_notify_passes");
    let done_before_cancel = f.notification_count("done");
    f.key("Escape");
    f.until(
        "native cancellation reports idle without a completion",
        |f| {
            f.live("idle")
                && f.count("program_reports") > cancel_reports
                && f.count("program_notify_passes") > cancel_pass
                && f.scratch
                    .status()
                    .get(&format!("program_pane_{}", f.pane))
                    .is_some_and(|value| value.contains("selection=Some(Idle)"))
        },
    );
    assert!(
        f.read().program.result.is_none(),
        "Pi cancellation is native idle, not a terminal result"
    );
    assert!(!f.read().options.contains_key(opt::ATTENTION));
    assert_eq!(f.notification_count("done"), done_before_cancel);
    assert!(
        *f.model.state.released.lock().unwrap() < 5,
        "model still gated when native cancellation settles"
    );
    f.model.release(5);
    eprintln!("native Pi cancellation: fresh_idle_report=true; result=None; attention=false; completion_effects=0; model_response_still_gated=true");
    // Normal exit leaves the pane shell alive; process departure must revoke authority.
    let reports = f.count("program_reports");
    f.submit("/quit");
    f.until("Pi process departed into owned shell", |f| {
        f.capture().contains("PI_PROCESS_DEPARTED")
    });
    f.until("departure releases full live authority", |f| {
        !matches!(
            f.read().projection(tma_runtime::now_ms()),
            Ok(ProjectionRead::Live { .. })
        )
    });
    assert_eq!(f.scratch.get(&f.pane, "#{pane_dead}"), "0");
    eprintln!("native Pi ledger: blocked_ms={blocked_ms}; requests={}; reports_before_exit={reports}; reports_after_exit={}; status={:?}; notifications={}", f.model.state.requests.load(Ordering::SeqCst), f.count("program_reports"), f.scratch.status(), f.notifications());
    let shutdown = Instant::now();
    assert!(Command::new("kill")
        .args(["-TERM", &f.daemon.pid().to_string()])
        .status()
        .unwrap()
        .success());
    assert!(f.daemon.wait_exit(Duration::from_secs(10)));
    eprintln!("native Pi daemon TERM={}ms", shutdown.elapsed().as_millis());
    assert!(f
        .read()
        .program
        .lease
        .as_ref()
        .is_none_or(|lease| !lease.live_at(tma_runtime::now_ms())));
}
