//! The hook reply lane: a third transport beside keystrokes and HTTP, for answering a Claude Code
//! permission prompt with a structured decision instead of a keypress.
//!
//! Two halves meeting on two files under [`crate::ipc::runtime_dir`]. The **request** half runs
//! inside `tma event claude PermissionRequest`: it writes `requests/<id>.json` (0600) describing the
//! call the agent is asking about, then holds, polling `verdicts/<id>` for at most `hold_ms`. The
//! **reply** half is [`crate::broker`]'s `hook` arm: under the pane's held single-flight lock it
//! creates that verdict file, exclusively, and clears `@agent_permission_request`.
//!
//! Degradation is the guarantee. A hold that expires removes its own request record and prints
//! nothing, which leaves claude's native prompt exactly as it was; a broker fire that finds no
//! record falls through to the key sequence. Nothing here can make a pane behave worse than a pane
//! with the lane switched off.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tma_core::HookVerdict;

use crate::event::{json_object_field, json_string_field};
use crate::ipc::runtime_dir;
use crate::json::JsonWriter;

/// The hold loop's poll interval. Claude draws its own dialog immediately and waits six seconds
/// before its `Notification` fallback (research/27 §1), so a tenth of a second is far finer
/// than anything a person can perceive and still costs ~250 `stat` calls across a full 25 s hold.
pub const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// The message a denied call reports back to the model. Claude quotes it verbatim into the
/// transcript, so it has to say who refused rather than reading as the tool's own failure.
const DENY_MESSAGE: &str = "Denied via tma";

/// The directory of pending request records.
pub fn requests_dir() -> PathBuf {
    runtime_dir().join("requests")
}

/// The directory of written verdicts.
pub fn verdicts_dir() -> PathBuf {
    runtime_dir().join("verdicts")
}

/// Whether `id` is safe as a filename: non-empty ASCII alphanumerics plus `-`/`_`, capped. The same
/// charset the broker validates `@agent_permission_request` with at read, checked again here because
/// this value becomes a path and `..` must never be one of them.
pub fn valid_request_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// One parked permission request: what the agent asked for, as the hook received it.
///
/// `tool_input` is the payload's own object text, copied verbatim rather than re-serialized: it is
/// the whole reason the lane exists (a phone-width screen wraps a rendered label and the wrap is not
/// invertible), so nothing here may reshape it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestRecord {
    pub id: String,
    pub pane: String,
    pub session_id: String,
    pub prompt_id: String,
    pub tool_name: String,
    /// The payload's `tool_input` object, verbatim. `{}` when the payload carried none.
    pub tool_input: String,
    pub episode_ms: u64,
    pub stamped_at_ms: u64,
}

impl RequestRecord {
    /// Build a record from a `PermissionRequest` payload. Every field comes from the payload or the
    /// caller; nothing is scraped off the pane's screen.
    pub fn from_payload(
        id: &str,
        pane: &str,
        payload: &str,
        episode_ms: u64,
        stamped_at_ms: u64,
    ) -> RequestRecord {
        RequestRecord {
            id: id.to_string(),
            pane: pane.to_string(),
            session_id: json_string_field(payload, "session_id").unwrap_or_default(),
            prompt_id: json_string_field(payload, "prompt_id").unwrap_or_default(),
            tool_name: json_string_field(payload, "tool_name").unwrap_or_default(),
            tool_input: json_object_field(payload, "tool_input")
                .unwrap_or("{}")
                .to_string(),
            episode_ms,
            stamped_at_ms,
        }
    }

    /// The record as it lands on disk.
    pub fn render(&self) -> String {
        let mut j = JsonWriter::new();
        j.begin_object();
        j.string("id", &self.id);
        j.string("pane", &self.pane);
        j.string("session_id", &self.session_id);
        j.string("prompt_id", &self.prompt_id);
        j.string("tool_name", &self.tool_name);
        j.raw_value("tool_input", &self.tool_input);
        j.number("episode_ms", self.episode_ms as i64);
        j.number("stamped_at_ms", self.stamped_at_ms as i64);
        j.end_object();
        j.finish()
    }
}

/// Write the request record for `id`, replacing any record left by an earlier prompt with the same
/// minted id. `Err` means the hook cannot park the request, so the caller must not hold.
pub fn write_request(record: &RequestRecord) -> std::io::Result<PathBuf> {
    let dir = requests_dir();
    create_private_dir(&dir)?;
    let path = dir.join(format!("{}.json", record.id));
    write_private(&path, record.render().as_bytes())?;
    Ok(path)
}

/// Whether a request record is parked for `id`. The broker's test for "a hook is holding": a
/// necessary condition, not proof, which is why a spent verdict is refused at the file itself.
pub fn request_pending(id: &str) -> bool {
    valid_request_id(id) && requests_dir().join(format!("{id}.json")).exists()
}

/// The parked record for `id`, `None` when nothing is parked or the file is not one.
///
/// The read half of [`write_request`], for a card that has to say what the pane is asking about.
/// `tool_input` comes back through [`json_object_field`] rather than the parser, because it is the
/// payload's own object text and a re-serialization would reorder its keys.
pub fn read_request(id: &str) -> Option<RequestRecord> {
    if !valid_request_id(id) {
        return None;
    }
    let text = std::fs::read_to_string(requests_dir().join(format!("{id}.json"))).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let string = |key: &str| {
        value
            .get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let number = |key: &str| {
        value
            .get(key)
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default()
    };
    let record = RequestRecord {
        id: string("id"),
        pane: string("pane"),
        session_id: string("session_id"),
        prompt_id: string("prompt_id"),
        tool_name: string("tool_name"),
        tool_input: json_object_field(&text, "tool_input")
            .unwrap_or("{}")
            .to_string(),
        episode_ms: number("episode_ms"),
        stamped_at_ms: number("stamped_at_ms"),
    };
    // A record whose own id is not the one asked for names another prompt; answering with it would
    // put a different call's tool input on the card.
    (record.id == id).then_some(record)
}

/// Remove the request record for `id`, if any. Best-effort: a hold that expired has nothing left to
/// protect, and a failure here must not turn a clean fall-through into a hook error.
pub fn remove_request(id: &str) {
    let _ = std::fs::remove_file(requests_dir().join(format!("{id}.json")));
}

/// What a verdict write did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VerdictWrite {
    /// This call created the verdict.
    Written,
    /// A verdict for this request already existed; nothing was overwritten.
    Exists,
    Error(String),
}

/// Create the verdict for `id`, exclusively.
///
/// Temp file in the same directory, fsync, then `link(2)` onto the final name: `rename` would be
/// atomic but would also clobber, and the one thing this file must guarantee is that a second
/// dispatch against one request cannot answer it a second time. `link` fails `EEXIST` instead.
pub fn write_verdict(id: &str, verdict: HookVerdict) -> VerdictWrite {
    if !valid_request_id(id) {
        return VerdictWrite::Error(format!("refusing an unsafe request id {id:?}"));
    }
    let dir = verdicts_dir();
    if let Err(e) = create_private_dir(&dir) {
        return VerdictWrite::Error(format!("create {}: {e}", dir.display()));
    }
    let final_path = dir.join(id);
    let temp = dir.join(format!("{id}.{}.tmp", std::process::id()));
    let body = format!(
        "{{\"decision\":\"{}\",\"request\":\"{id}\"}}\n",
        verdict.token()
    );
    if let Err(e) = write_private_synced(&temp, body.as_bytes()) {
        let _ = std::fs::remove_file(&temp);
        return VerdictWrite::Error(format!("write {}: {e}", temp.display()));
    }
    let linked = std::fs::hard_link(&temp, &final_path);
    let _ = std::fs::remove_file(&temp);
    match linked {
        Ok(()) => VerdictWrite::Written,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => VerdictWrite::Exists,
        Err(e) => VerdictWrite::Error(format!("link {}: {e}", final_path.display())),
    }
}

/// Hold for a verdict on `id`, then clean up after itself.
///
/// `Some(json)` is the `PermissionRequest` decision object to print on stdout; both files are gone
/// by then. `None` is the hold expiring with nothing on disk: the request record is removed (so a
/// later `tma act` falls through to the keys arm) and the caller prints nothing, which is what hands
/// the prompt back to claude untouched.
pub fn hold_for_verdict(id: &str, hold: Duration) -> Option<String> {
    let path = verdicts_dir().join(id);
    let deadline = Instant::now() + hold;
    loop {
        if let Some(verdict) = read_verdict(&path) {
            let _ = std::fs::remove_file(&path);
            remove_request(id);
            return Some(decision_json(verdict));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            remove_request(id);
            return None;
        }
        std::thread::sleep(POLL_INTERVAL.min(remaining));
    }
}

/// The verdict a written file carries, `None` when it is absent or says neither word. A file whose
/// bytes we cannot read as a verdict is treated as absent: the hold keeps waiting and then expires
/// into claude's own prompt, which is the safe end of both mistakes.
fn read_verdict(path: &Path) -> Option<HookVerdict> {
    let body = std::fs::read_to_string(path).ok()?;
    if body.contains("\"decision\":\"allow\"") {
        Some(HookVerdict::Allow)
    } else if body.contains("\"decision\":\"deny\"") {
        Some(HookVerdict::Deny)
    } else {
        None
    }
}

/// The `PermissionRequest` decision object claude reads off the hook's stdout.
pub fn decision_json(verdict: HookVerdict) -> String {
    let mut j = JsonWriter::new();
    j.begin_object();
    j.key("hookSpecificOutput");
    j.begin_object();
    j.string("hookEventName", "PermissionRequest");
    j.key("decision");
    j.begin_object();
    j.string("behavior", verdict.token());
    if verdict == HookVerdict::Deny {
        j.string("message", DENY_MESSAGE);
    }
    j.end_object();
    j.end_object();
    j.end_object();
    j.finish()
}

/// A `0700` directory, created if absent. The parent runtime dir is created too, at the same mode
/// the daemon's socket dir uses.
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    if let Some(parent) = dir.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

/// Write `bytes` to `path` at mode 0600, truncating an existing file.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)
}

/// [`write_private`] plus an fsync, and `create_new` so a stale temp cannot be appended into.
fn write_private_synced(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let _ = std::fs::remove_file(path);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Point the runtime dir at a scratch directory for the duration of one test. Every path in
    /// this module derives from `$XDG_RUNTIME_DIR`, so setting it is the whole isolation.
    struct ScratchRuntime {
        dir: PathBuf,
    }

    impl ScratchRuntime {
        fn new(tag: &str) -> ScratchRuntime {
            let dir = std::env::temp_dir().join(format!(
                "tma_hook_lane_{tag}_{}_{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            std::env::set_var("XDG_RUNTIME_DIR", &dir);
            ScratchRuntime { dir }
        }
    }

    impl Drop for ScratchRuntime {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// The lane's tests mutate process-wide `$XDG_RUNTIME_DIR`, so they run under one mutex rather
    /// than in parallel. Poisoning is irrelevant here: a panicking test has already failed.
    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn record(id: &str) -> RequestRecord {
        RequestRecord {
            id: id.to_string(),
            pane: "%1".to_string(),
            session_id: "s".to_string(),
            prompt_id: "p".to_string(),
            tool_name: "Bash".to_string(),
            tool_input: "{\"command\":\"touch x\"}".to_string(),
            episode_ms: 7,
            stamped_at_ms: 9,
        }
    }

    /// A verdict landing mid-hold ends it with the decision object, and takes both files with it.
    #[test]
    fn a_verdict_ends_the_hold_and_removes_both_files() {
        let _g = env_guard();
        let _rt = ScratchRuntime::new("verdict");
        let rec = record("req1");
        write_request(&rec).unwrap();
        assert!(request_pending("req1"));

        let writer = std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(120));
            write_verdict("req1", HookVerdict::Allow)
        });
        let out = hold_for_verdict("req1", Duration::from_secs(10));
        assert_eq!(writer.join().unwrap(), VerdictWrite::Written);

        assert_eq!(
            out.as_deref(),
            Some(
                "{\"hookSpecificOutput\":{\"hookEventName\":\"PermissionRequest\",\
                 \"decision\":{\"behavior\":\"allow\"}}}"
            )
        );
        assert!(!verdicts_dir().join("req1").exists(), "verdict consumed");
        assert!(!request_pending("req1"), "request record consumed");
    }

    /// The deny decision carries the message claude reports back to the model.
    #[test]
    fn a_deny_verdict_carries_the_refusal_message() {
        let _g = env_guard();
        let _rt = ScratchRuntime::new("deny");
        write_request(&record("req2")).unwrap();
        assert_eq!(
            write_verdict("req2", HookVerdict::Deny),
            VerdictWrite::Written
        );
        assert_eq!(
            hold_for_verdict("req2", Duration::from_secs(5)).as_deref(),
            Some(
                "{\"hookSpecificOutput\":{\"hookEventName\":\"PermissionRequest\",\
                 \"decision\":{\"behavior\":\"deny\",\"message\":\"Denied via tma\"}}}"
            )
        );
    }

    /// The whole degradation guarantee in one assertion: nothing answered, so nothing is printed
    /// and the record is gone, which leaves a later `tma act` on the keys arm.
    #[test]
    fn an_expired_hold_prints_nothing_and_removes_the_record() {
        let _g = env_guard();
        let _rt = ScratchRuntime::new("expire");
        write_request(&record("req3")).unwrap();
        let started = Instant::now();
        assert_eq!(hold_for_verdict("req3", Duration::from_millis(250)), None);
        assert!(started.elapsed() >= Duration::from_millis(200), "it held");
        assert!(!request_pending("req3"), "the record is cleaned up");
    }

    /// A second write against one request is refused at the file, not merely by the pane stamp.
    #[test]
    fn a_second_verdict_write_is_refused() {
        let _g = env_guard();
        let _rt = ScratchRuntime::new("twice");
        assert_eq!(
            write_verdict("req4", HookVerdict::Allow),
            VerdictWrite::Written
        );
        assert_eq!(
            write_verdict("req4", HookVerdict::Deny),
            VerdictWrite::Exists
        );
        assert_eq!(
            std::fs::read_to_string(verdicts_dir().join("req4")).unwrap(),
            "{\"decision\":\"allow\",\"request\":\"req4\"}\n",
            "the first verdict stands"
        );
    }

    /// The record and verdict paths are built from the id, so an id that could escape the directory
    /// is refused before it becomes one.
    #[test]
    fn an_unsafe_request_id_is_refused() {
        let _g = env_guard();
        let _rt = ScratchRuntime::new("unsafe");
        assert!(!valid_request_id("../etc/passwd"));
        assert!(!valid_request_id(""));
        assert!(valid_request_id("d41d8cd98f00b204"));
        assert!(matches!(
            write_verdict("../escape", HookVerdict::Allow),
            VerdictWrite::Error(_)
        ));
        assert!(!request_pending("../escape"));
    }

    /// The live hook experiment's `PermissionRequest` payloads, redacted, as Claude Code 2.1.261 delivers them
    /// (research/27 §1, leg 1). Note what is NOT here: no `tool_use_id`, which is why the id is
    /// minted, and no rendered label, which is why the record copies `tool_input` instead.
    const E4_BASH: &str = r#"{
  "session_id": "7c74ac42-d330-4c0f-a7b1-ba9a918ba98e",
  "transcript_path": "<TRANSCRIPT>",
  "cwd": "<PROJECT>",
  "prompt_id": "20edf0d8-a22b-43de-94ab-4ecce0c78d11",
  "permission_mode": "default",
  "hook_event_name": "PermissionRequest",
  "tool_name": "Bash",
  "tool_input": {
    "command": "touch <SCRATCH>/marker-leg1 && echo leg1-done",
    "description": "Create marker file and print confirmation"
  },
  "permission_suggestions": [
    { "type": "setMode", "mode": "acceptEdits", "destination": "session" }
  ]
}"#;

    const E4_WRITE: &str = r#"{
  "session_id": "7c74ac42-d330-4c0f-a7b1-ba9a918ba98e",
  "transcript_path": "<TRANSCRIPT>",
  "cwd": "<PROJECT>",
  "prompt_id": "81ee2c39-0097-4cb2-bdee-3913af01ef5e",
  "permission_mode": "default",
  "hook_event_name": "PermissionRequest",
  "tool_name": "Write",
  "tool_input": {
    "file_path": "<PROJECT>/notes.txt",
    "content": "hello-e4\n"
  }
}"#;

    /// The host half: the record carries the tool name and the tool input the payload
    /// delivered, verbatim, and never a label anyone rendered off a screen.
    #[test]
    fn the_record_carries_the_payloads_own_tool_and_input() {
        let bash = RequestRecord::from_payload("id1", "%3", E4_BASH, 111, 222);
        assert_eq!(bash.tool_name, "Bash");
        assert!(
            bash.tool_input
                .contains("\"command\": \"touch <SCRATCH>/marker-leg1 && echo leg1-done\"")
                && bash.tool_input.contains("\"description\""),
            "the whole tool_input object survives: {}",
            bash.tool_input
        );
        assert_eq!(bash.session_id, "7c74ac42-d330-4c0f-a7b1-ba9a918ba98e");
        assert_eq!(bash.prompt_id, "20edf0d8-a22b-43de-94ab-4ecce0c78d11");
        assert_eq!(
            (bash.pane.as_str(), bash.episode_ms, bash.stamped_at_ms),
            ("%3", 111, 222)
        );

        let write = RequestRecord::from_payload("id2", "%3", E4_WRITE, 0, 0);
        assert_eq!(write.tool_name, "Write");
        assert!(
            write
                .tool_input
                .contains("\"file_path\": \"<PROJECT>/notes.txt\"")
                && write.tool_input.contains("hello-e4"),
            "a file tool's input survives too: {}",
            write.tool_input
        );

        // The two calls differ, so a record written for one can never be read as the other. The
        // `permission_suggestions` the dialog renders its extra options from are deliberately not
        // carried: v1 offers allow-once and deny-once and nothing else.
        assert_ne!(bash.tool_input, write.tool_input);
        for record in [&bash, &write] {
            let json = record.render();
            assert!(!json.contains("permission_suggestions"), "{json}");
            assert!(
                json.contains("\"tool_input\":{"),
                "embedded as JSON: {json}"
            );
        }
    }

    /// The record round-trips: what the hook parked is what a card reads back, `tool_input` object
    /// and all. The mismatched-id case is the one that matters, because the id is what the card's
    /// two options quote and a record from another prompt would answer the wrong call.
    #[test]
    fn a_parked_record_reads_back_and_a_foreign_one_does_not() {
        let _g = env_guard();
        let _rt = ScratchRuntime::new("read");
        let written = RequestRecord::from_payload("req6", "%3", E4_BASH, 111, 222);
        write_request(&written).unwrap();
        assert_eq!(read_request("req6").as_ref(), Some(&written));

        assert_eq!(read_request("nothing-parked"), None);
        assert_eq!(read_request("../etc/passwd"), None);

        // A file whose `id` names another prompt is refused rather than served under this one.
        std::fs::write(
            requests_dir().join("req7.json"),
            RequestRecord::from_payload("req6", "%3", E4_WRITE, 0, 0).render(),
        )
        .unwrap();
        assert_eq!(read_request("req7"), None);
    }

    /// Files carry the private modes the record's contents earn: 0600 on the record, 0700 on the
    /// directory holding it.
    #[test]
    fn the_record_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let _g = env_guard();
        let _rt = ScratchRuntime::new("modes");
        let path = write_request(&record("req5")).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&requests_dir()), 0o700);
    }
}
