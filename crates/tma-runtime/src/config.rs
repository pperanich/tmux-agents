//! The user config file (`~/.config/tma/config.toml`), in `tma-runtime` so tma-core stays a pure
//! detection library. Zero-config is unchanged: every field's serde default equals the value it
//! replaced. [`load`] resolves one path (`--config`, then `TMA_CONFIG`, then `$XDG_CONFIG_HOME/tma/`,
//! then `~/.config/tma/`): absent ⇒ defaults, malformed ⇒ a loud error naming the file and key
//! (`deny_unknown_fields`). One-shots read it per invocation; the daemon (SIGHUP) and picker
//! hot-reload the same path, keeping the last good file on error. `[[agent]]` is the v1
//! extensibility surface (enable/disable + `process_names`); a `[[hooks.map]]` grammar is a future
//! addition.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use tma_core::{AgentState, FoldConfig};

/// The whole `config.toml`. Every section is optional and every leaf defaults to the value it
/// replaced, so [`Config::default`] is byte-for-byte the pre-config behavior (`deny_unknown_fields`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub fold: FoldSection,
    #[serde(default)]
    pub status: StatusStyles,
    #[serde(default)]
    pub picker: PickerStyles,
    #[serde(default)]
    pub daemon: DaemonSection,
    #[serde(default)]
    pub notify: NotifySection,
    #[serde(default)]
    pub focus: FocusSection,
    #[serde(default)]
    pub install: InstallSection,
    #[serde(default)]
    pub telemetry: TelemetrySection,
    #[serde(default)]
    pub tmux: TmuxSection,
    /// `[[agent]]` entries: enable/disable + custom process-name maps.
    #[serde(default, rename = "agent")]
    pub agent_overrides: Vec<AgentConfig>,
    /// `[api.<name>]` per-agent API-channel config: the fallback server base URL for the
    /// action broker's API lane. Distinct from the `[[agent]]` array above (which keys off `name`).
    #[serde(default)]
    pub api: ApiSection,
    /// The pre-rename `[agents]` table, captured so [`load`] can name the rename instead of letting
    /// `deny_unknown_fields` print an unknown-key error the user has to decode.
    #[serde(default, rename = "agents")]
    legacy_agents: Option<toml::Value>,
}

// ---- [fold] ------------------------------------------------------------------------------

/// `[fold]` tuning (dwell, hook decay, stamp freshness). The bin constructs and injects a
/// [`FoldConfig`]; defaults read [`FoldConfig::default`] so zero-config and config cannot diverge.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FoldSection {
    #[serde(default = "default_dwell_secs")]
    pub dwell_secs: u64,
    #[serde(default = "default_hook_decay_secs")]
    pub hook_decay_secs: u64,
    #[serde(default = "default_blocked_decay_secs")]
    pub blocked_decay_secs: u64,
    #[serde(default = "default_freshness_secs")]
    pub freshness_secs: u64,
}

fn default_dwell_secs() -> u64 {
    FoldConfig::default().dwell_secs
}
fn default_hook_decay_secs() -> u64 {
    FoldConfig::default().hook_decay_secs
}
fn default_blocked_decay_secs() -> u64 {
    FoldConfig::default().blocked_decay_secs
}
fn default_freshness_secs() -> u64 {
    FoldConfig::default().freshness_secs
}

impl Default for FoldSection {
    fn default() -> Self {
        FoldSection {
            dwell_secs: default_dwell_secs(),
            hook_decay_secs: default_hook_decay_secs(),
            blocked_decay_secs: default_blocked_decay_secs(),
            freshness_secs: default_freshness_secs(),
        }
    }
}

impl FoldSection {
    /// The [`FoldConfig`] the surfaces and the daemon inject into the pure fold.
    pub fn to_fold_config(&self) -> FoldConfig {
        FoldConfig {
            dwell_secs: self.dwell_secs,
            hook_decay_secs: self.hook_decay_secs,
            blocked_decay_secs: self.blocked_decay_secs,
            freshness_secs: self.freshness_secs,
        }
    }
}

impl Config {
    /// Shortcut: the injected [`FoldConfig`].
    pub fn fold_config(&self) -> FoldConfig {
        self.fold.to_fold_config()
    }
}

// ---- [status] / [picker] glyphs + colors -------------------------------------------------

/// One state's glyph + color override; both optional, so a partial entry keeps the other default.
/// Colors are strings: `tma status` embeds them in `#[fg=...]`, the picker maps them to ratatui
/// colours in the UI crate (`tma_ui_core::palette`).
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StateStyle {
    pub glyph: Option<String>,
    pub color: Option<String>,
}

/// `[status]` glyphs + colors for the `tma status` one-liner. Defaults: `⚑` red, `●` yellow, `○`
/// green, `?` colour244, plus `done` (idle + `@agent_attention`) `✓` magenta.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusStyles {
    #[serde(default)]
    pub blocked: StateStyle,
    #[serde(default)]
    pub working: StateStyle,
    #[serde(default)]
    pub idle: StateStyle,
    #[serde(default)]
    pub unknown: StateStyle,
    /// "done" surface: an idle pane still carrying `@agent_attention`. Presentation only, the state
    /// token stays `idle`.
    #[serde(default)]
    pub done: StateStyle,
}

impl StatusStyles {
    /// The resolved `(glyph, color-string)` for a state (override, else default), inserted verbatim
    /// into `#[fg=...]`.
    pub fn resolved(&self, state: AgentState) -> (&str, &str) {
        match state {
            AgentState::Blocked => pick(&self.blocked, "⚑", "red"),
            AgentState::Working => pick(&self.working, "●", "yellow"),
            AgentState::Idle => pick(&self.idle, "○", "green"),
            AgentState::Unknown => pick(&self.unknown, "?", "colour244"),
        }
    }

    /// The resolved `(glyph, color-string)` for the "done" surface (idle + attention).
    pub fn resolved_done(&self) -> (&str, &str) {
        pick(&self.done, "✓", "magenta")
    }
}

/// `[picker]` glyphs + colors for the ratatui picker. Defaults match `tma status` except `unknown`,
/// whose default is `darkgray` (the pre-config picker value).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PickerStyles {
    #[serde(default)]
    pub blocked: StateStyle,
    #[serde(default)]
    pub working: StateStyle,
    #[serde(default)]
    pub idle: StateStyle,
    #[serde(default)]
    pub unknown: StateStyle,
    /// "done" surface: idle + `@agent_attention`. Default `✓` magenta (see [`StatusStyles`]).
    #[serde(default)]
    pub done: StateStyle,
}

impl PickerStyles {
    /// The resolved `(glyph, color-string)` for a state (override, else default). The UI crate maps
    /// the color string to a ratatui `Color` via `tma_ui_core::palette::RowPalette`; the picker's
    /// `unknown` default is `darkgray` (the pre-config picker value, not status's `colour244`).
    pub fn resolved_str(&self, state: AgentState) -> (&str, &str) {
        match state {
            AgentState::Blocked => pick(&self.blocked, "⚑", "red"),
            AgentState::Working => pick(&self.working, "●", "yellow"),
            AgentState::Idle => pick(&self.idle, "○", "green"),
            AgentState::Unknown => pick(&self.unknown, "?", "darkgray"),
        }
    }

    /// The resolved `(glyph, color-string)` for the "done" surface (idle + attention).
    pub fn resolved_done_str(&self) -> (&str, &str) {
        pick(&self.done, "✓", "magenta")
    }
}

/// Resolve a [`StateStyle`] against `(default_glyph, default_color)` string pair.
fn pick<'a>(s: &'a StateStyle, glyph: &'a str, color: &'a str) -> (&'a str, &'a str) {
    (
        s.glyph.as_deref().unwrap_or(glyph),
        s.color.as_deref().unwrap_or(color),
    )
}

// ---- [daemon] ----------------------------------------------------------------------------

/// `[daemon]` knobs (daemon-only); defaults read the same `const`s the daemon ships. Only the normal
/// sweep cadence is configurable; the degraded one stays derived.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonSection {
    /// Reconciliation-sweep cadence when control-mode push is available (DAEMON 30–60 s).
    #[serde(default = "default_sweep_secs")]
    pub sweep_secs: u64,
    /// Per-pane active→quiet threshold in milliseconds (the on-demand capture trigger).
    #[serde(default = "default_quiet_ms")]
    pub quiet_ms: u64,
    /// Liveness recheck cadence while the control pool is clientless (zero-member recovery).
    #[serde(default = "default_zero_member_recheck_secs")]
    pub zero_member_recheck_secs: u64,
    /// Hook-liveness demotion threshold: activity edges with no fresh hook claim after which a
    /// hook-capable pane's coverage goes suspect (default 5; see [`crate::capture`]).
    #[serde(default = "default_demote_edges")]
    pub demote_edges: u32,
    /// Opt-in auto-start: when `true`, the user-invoked surfaces run `tma daemon --ensure` before
    /// their own work so a daemon comes up on first use. Default `false`; consumed by the bin's
    /// dispatch, and a failed spawn never fails or delays the caller.
    #[serde(default)]
    pub autostart: bool,
}

fn default_sweep_secs() -> u64 {
    tma_tmux::control::SWEEP_NORMAL.as_secs()
}
fn default_quiet_ms() -> u64 {
    tma_tmux::control::QUIET_THRESHOLD.as_millis() as u64
}
fn default_zero_member_recheck_secs() -> u64 {
    tma_tmux::control::EMPTY_POOL_RECHECK.as_secs()
}
fn default_demote_edges() -> u32 {
    crate::capture::DEMOTE_EDGES
}

impl Default for DaemonSection {
    fn default() -> Self {
        DaemonSection {
            sweep_secs: default_sweep_secs(),
            quiet_ms: default_quiet_ms(),
            zero_member_recheck_secs: default_zero_member_recheck_secs(),
            demote_edges: default_demote_edges(),
            autostart: false,
        }
    }
}

impl DaemonSection {
    pub fn sweep(&self) -> Duration {
        Duration::from_secs(self.sweep_secs)
    }
    pub fn quiet_threshold(&self) -> Duration {
        Duration::from_millis(self.quiet_ms)
    }
    pub fn zero_member_recheck(&self) -> Duration {
        Duration::from_secs(self.zero_member_recheck_secs)
    }
}

// ---- [tmux] ------------------------------------------------------------------------------

/// `[tmux]`: which tmux-compatible binary tma spawns. Default (absent, or `bin` unset) is plain
/// `tmux` off `PATH`, so zero-config is unchanged. The env `TMA_TMUX_BIN` overrides this key.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TmuxSection {
    /// A `PATH` name (`tmate`) or a path (`/opt/homebrew/bin/tmux`); anything containing a `/` is
    /// used as-is. Point this at the client matching the server you talk to — a tmate server speaks
    /// its fork's protocol, and tma's own tmux would be refused with a version mismatch.
    #[serde(default)]
    pub bin: Option<String>,
}

// ---- [notify] ----------------------------------------------------------------------------

/// One `notify.on` trigger token: `blocked` (a pane entering `blocked`, the sole default) or `done`
/// (a working→idle completion). A serde enum, so any other string is a loud config error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NotifyTrigger {
    Blocked,
    Done,
}

impl NotifyTrigger {
    /// The trigger word in the notification payload (`blocked`/`done`). `done` reads as the
    /// transition, not the landing token (`idle`), so a hook can tell finished from blocked.
    pub fn word(self) -> &'static str {
        match self {
            NotifyTrigger::Blocked => "blocked",
            NotifyTrigger::Done => "done",
        }
    }
}

/// `[notify]`: `from_event` (daemonless direct-fire opt-in), `command` (the hook command run
/// alongside `display-message`), `on` (transitions that fire, default `["blocked"]`). Config is
/// canonical; `TMA_NOTIFY_FROM_EVENT`/`TMA_NOTIFY_CMD` are a test/CI override that wins. Each trigger
/// may route to its own command via its `[notify.<trigger>]` sub-table ([`NotifyCommands`]).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotifySection {
    #[serde(default)]
    pub from_event: bool,
    #[serde(default)]
    pub command: Option<String>,
    /// `[notify.blocked]`: per-trigger routing for the `blocked` fire. Absent ⇒ the global `command`.
    #[serde(default)]
    pub blocked: Option<TriggerSection>,
    /// `[notify.done]`: per-trigger routing for the working→idle completion fire.
    #[serde(default)]
    pub done: Option<TriggerSection>,
    /// Noteworthy transitions that fire. Default `["blocked"]`; add `"done"` for working→idle. The
    /// per-transition dedup is reused unchanged (`@agent_since` is write-once per state, so a
    /// blocked-then-done episode fires once each).
    #[serde(default = "default_notify_on")]
    pub on: Vec<NotifyTrigger>,
    /// Also ring the terminal bell when a notification fires (default `false`): a single BEL byte to
    /// the firing pane's tty, sharing the `on` trigger set and marker dedup.
    #[serde(default)]
    pub bell: bool,
    /// Also post an OSC 9 desktop notification to the firing pane's tty (default `false`, since
    /// emulator support varies). Like the bell it writes the tty, so it crosses ssh/mosh/tmate to the
    /// emulator you are actually sitting at.
    #[serde(default)]
    pub osc: bool,
    /// Append one JSON line per fired notification to this path (default unset). The daemonless
    /// answer to the daemon's in-memory transition ring: durable, and a record of what was sent.
    #[serde(default)]
    pub log: Option<String>,
    /// `context_high`: fire once when a pane's context utilization crosses `threshold`, on its
    /// own `@agent_context_notified_at` armed flag (never the state lane's marker). Absent ⇒ disabled;
    /// a present sub-table with a `threshold` percent enables it. Rearms below `threshold - 10`.
    #[serde(default)]
    pub context_high: Option<ContextHighSection>,
}

/// `[notify.<trigger>]`: one trigger's routing. Only `command` for now, and unknown keys stay a loud
/// error, so a mistyped override never silently falls back to the global command.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriggerSection {
    /// The command this trigger fires instead of the global `notify.command`. Unset ⇒ the global one.
    #[serde(default)]
    pub command: Option<String>,
}

/// `[notify.context_high]`: the context-utilization notify trigger. Distinct from `on`'s
/// state triggers — it rides its own armed flag and carries a percent `threshold`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextHighSection {
    /// Fire when a real observation lands at or above this percent while armed; rearm below
    /// `threshold - 10`. No default: naming the sub-table is the opt-in, so the threshold is required.
    pub threshold: u8,
    /// Per-trigger routing, like the state triggers' sub-tables. Unset ⇒ the global `notify.command`.
    #[serde(default)]
    pub command: Option<String>,
}

/// The `TMA_NOTIFY_CMD` test/CI seam, read in one place so every surface resolves it identically. An
/// empty value is treated as unset.
pub fn notify_cmd_env() -> Option<String> {
    std::env::var("TMA_NOTIFY_CMD")
        .ok()
        .filter(|c| !c.is_empty())
}

/// The tty-writing sinks a fire rides alongside `display-message` and the hook command. Carried as
/// one value so both fire paths enable exactly the same set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NotifySinks {
    pub bell: bool,
    pub osc: bool,
    /// `notify.log`: the JSONL audit file every fire appends to, `None` when unconfigured.
    pub log: Option<PathBuf>,
}

/// The resolved notify commands: the global `notify.command` plus each trigger's optional override.
/// Both fire paths carry this rather than a single string, so `fire` stays one choke point taking an
/// already-resolved command and the routing decision lives in exactly one place.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NotifyCommands {
    pub global: Option<String>,
    pub blocked: Option<String>,
    pub done: Option<String>,
    pub context_high: Option<String>,
}

impl NotifyCommands {
    /// The command a state trigger fires: its own override, else the global one.
    pub fn for_trigger(&self, trigger: NotifyTrigger) -> Option<&str> {
        let own = match trigger {
            NotifyTrigger::Blocked => &self.blocked,
            NotifyTrigger::Done => &self.done,
        };
        own.as_deref().or(self.global.as_deref())
    }

    /// The command the `context_high` trigger fires: its own override, else the global one.
    pub fn for_context_high(&self) -> Option<&str> {
        self.context_high.as_deref().or(self.global.as_deref())
    }

    /// Apply the `TMA_NOTIFY_CMD` test/CI seam: a set override replaces the command for EVERY
    /// trigger (the seam exists so one instrumented sink sees every fire), so a config's per-trigger
    /// routing applies only when the env var is unset.
    pub fn overridden_by(self, env_command: Option<String>) -> NotifyCommands {
        match env_command.filter(|c| !c.is_empty()) {
            Some(command) => NotifyCommands {
                global: Some(command),
                ..NotifyCommands::default()
            },
            None => self,
        }
    }
}

fn default_notify_on() -> Vec<NotifyTrigger> {
    vec![NotifyTrigger::Blocked]
}

impl Default for NotifySection {
    fn default() -> Self {
        NotifySection {
            from_event: false,
            command: None,
            blocked: None,
            done: None,
            on: default_notify_on(),
            bell: false,
            osc: false,
            log: None,
            context_high: None,
        }
    }
}

/// Whether an `on` set fires `trigger`. The `[notify] on` membership rule in one place: the daemon
/// dispatch and the daemonless `tma event` path both hold the resolved set as a bare slice rather
/// than a [`NotifySection`], so a method alone would leave them re-spelling the test.
pub fn trigger_enabled(on: &[NotifyTrigger], trigger: NotifyTrigger) -> bool {
    on.contains(&trigger)
}

impl NotifySection {
    /// Does the configured `on` set fire on `trigger`?
    pub fn fires_on(&self, trigger: NotifyTrigger) -> bool {
        trigger_enabled(&self.on, trigger)
    }

    /// The configured tty sinks (`bell`, `osc`).
    pub fn sinks(&self) -> NotifySinks {
        NotifySinks {
            bell: self.bell,
            osc: self.osc,
            log: self
                .log
                .as_ref()
                .filter(|p| !p.is_empty())
                .map(PathBuf::from),
        }
    }

    /// The configured routing: the global command plus each trigger's override.
    pub fn commands(&self) -> NotifyCommands {
        NotifyCommands {
            global: self.command.clone(),
            blocked: self.blocked.as_ref().and_then(|t| t.command.clone()),
            done: self.done.as_ref().and_then(|t| t.command.clone()),
            context_high: self.context_high.as_ref().and_then(|c| c.command.clone()),
        }
    }
}

// ---- [focus] -----------------------------------------------------------------------------

/// `[focus]` posture. The `after-select-pane`/`-window` attention-clear hooks are always installed;
/// `events = true` adds a `pane-focus-in` hook (fires only under tmux `focus-events on`). Default
/// off.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FocusSection {
    #[serde(default)]
    pub events: bool,
}

// ---- [install] ---------------------------------------------------------------------------

/// How an agent config should NAME the `tma-hook` wrapper it invokes.
///
/// Three of the six wiring mechanisms spawn the wrapper as argv with no shell (Codex's
/// `notify` array, the OpenCode plugin's and pi's `spawn`), so a `$HOME`-relative string is not an
/// option: it would be passed through literally. The choice is therefore between an absolute path
/// and a bare name resolved off `$PATH`, both of which every mechanism handles.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WrapperRef {
    /// The wrapper's absolute path. Found whatever `$PATH` the agent inherits, which is why it is
    /// the default; machine-specific, so a synced config points at another machine's home.
    #[default]
    Absolute,
    /// The bare name `tma-hook`, resolved off `$PATH` when the hook fires. One string on every
    /// machine, at the cost of needing the wrapper's directory on the `$PATH` each agent inherits
    /// (a GUI-launched agent often has a narrower one than your shell).
    Bare,
}

/// `[install]` posture for `install-hooks`. Config rather than an env var because `--check` and
/// `tma doctor` have to resolve the same reference install wrote: disagree, and every check reports
/// drift against a wiring that is in fact correct.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallSection {
    #[serde(default)]
    pub wrapper_ref: WrapperRef,
}

// ---- [telemetry] -------------------------------------------------------------------------

/// `[telemetry]` config: the recognized-model table. A metric-named posture matching the
/// manifest's `[telemetry.context]`, so a future metric's config is an additive sibling here too.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelemetrySection {
    /// `[telemetry.windows]`: the model names `tma doctor` recognizes.
    #[serde(default)]
    pub windows: WindowsSection,
}

/// The model names shipped as recognized. Only the `gemini-*` models were ever seeded, back when
/// the table was expected to size a gauge; they stay so dropping them cannot turn a model doctor
/// has always been quiet about into a reported one.
const SHIPPED_MODELS: &[&str] = &["gemini-2.5-pro", "gemini-2.5-flash", "gemini-1.5-pro"];

/// `[telemetry.windows]`: a set of model names, layered over [`SHIPPED_MODELS`].
///
/// The TOML is still `"<model>" = <tokens>` and the sizes still have to parse, so a config written
/// against the original shape keeps loading — but no size is ever read. Every context channel tma
/// ships (Claude, Codex, pi, Cursor) computes its percent from a window its own payload carries, and
/// a channel with no usable window stamps nothing rather than guessing one. What remains is name
/// recognition: `tma doctor` reports a stamped `@agent_model` no entry names.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(transparent)]
pub struct WindowsSection {
    entries: std::collections::BTreeMap<String, u64>,
}

impl WindowsSection {
    /// Whether the user's table or [`SHIPPED_MODELS`] names `model`.
    pub fn knows(&self, model: &str) -> bool {
        self.entries.contains_key(model) || SHIPPED_MODELS.contains(&model)
    }
}

// ---- [api.<name>] (API channel) -----------------------------------------------------------

/// `[api.<name>]`: per-agent API-channel config. v1 carries only `api_base`, the fallback
/// OpenCode server base URL the broker uses when the plugin did not stamp `@agent_api_endpoint`.
/// Transparent map so a new agent is one more table, keyed by agent name.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(transparent)]
pub struct ApiSection {
    entries: std::collections::BTreeMap<String, ApiEntry>,
}

/// One `[api.<name>]` entry. `deny_unknown_fields` so a mistyped key is a loud error.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiEntry {
    #[serde(default)]
    pub api_base: Option<String>,
}

impl ApiSection {
    /// The configured `api_base` for `agent`, `None` when unset (the broker then relies on the
    /// pane-stamped `@agent_api_endpoint`, or refuses `requires-unmet`).
    pub fn api_base(&self, agent: &str) -> Option<&str> {
        self.entries
            .get(agent)
            .and_then(|e| e.api_base.as_deref())
            .filter(|s| !s.is_empty())
    }
}

// ---- [[agent]] ---------------------------------------------------------------------------

/// One `[[agent]]` entry. `enabled = false` drops the named manifest from the loaded set;
/// `process_names` extends that manifest's identity match with extra launcher basenames.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub process_names: Vec<String>,
}

fn default_true() -> bool {
    true
}

// ---- load --------------------------------------------------------------------------------

/// Errors loading `config.toml`. Both name the file; [`ConfigError::Parse`] additionally
/// carries the toml span so the message points at the offending key.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config {file}: {source}")]
    // Exposing `toml::de::Error` publicly is deliberate: this crate feeds the tma binary, the toml
    // version is workspace-pinned, and an opaque wrapper would cost more than it protects.
    Parse {
        file: String,
        #[source]
        source: toml::de::Error,
    },
    #[error("cannot read config {file}: {source}")]
    Read {
        file: String,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "config {file}: the [agents] table was renamed to [api]: write `[api.<name>] api_base = …` \
         instead of `[agents.<name>]`. The `[[agent]]` override array is unchanged."
    )]
    Renamed { file: String },
}

/// Load the effective config (`explicit` = the `--config` flag). An absent file ⇒
/// [`Config::default`]; a malformed one is a hard error naming the file + key.
pub fn load(explicit: Option<&Path>) -> Result<Config, ConfigError> {
    let Some(path) = resolve_path(explicit) else {
        return Ok(Config::default());
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let config: Config = toml::from_str(&text).map_err(|source| ConfigError::Parse {
                file: path.display().to_string(),
                source,
            })?;
            match config.legacy_agents {
                Some(_) => Err(ConfigError::Renamed {
                    file: path.display().to_string(),
                }),
                None => Ok(config),
            }
        }
        // Absent file ⇒ zero-config defaults. This also applies to an explicit `--config`
        // pointing at a not-yet-created file: defaults, not an error.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(source) => Err(ConfigError::Read {
            file: path.display().to_string(),
            source,
        }),
    }
}

/// Hot-reload, all-or-nothing: re-read config + manifests and swap both only when both parse,
/// keeping the last good pair on a mid-save error. Shared by the long-lived poll surfaces
/// (`tma watch`, `tma wait`, `tma subscribe`) and the daemon's SIGHUP, so they cannot drift on what
/// "reload" means or on how a failure reads.
///
/// `Ok` carries the skipped user manifests, which are not a failed reload: what did load swapped in.
/// `Err` is the ready-to-print line naming the failing file, for the caller to put where its surface
/// can show it (a TUI defers it past the terminal guard; everything else writes stderr straight out).
/// Callers on a poll loop latch it, since a config left malformed would otherwise repeat every tick.
pub fn reload_pair(
    config: &mut Config,
    manifests: &mut Vec<crate::manifests::LoadedManifest>,
    config_path: Option<&Path>,
    manifest_dir: Option<&Path>,
) -> Result<Vec<crate::manifests::ManifestFailure>, String> {
    let new_config = load(config_path)
        .map_err(|err| format!("tma: reload failed (config): {err}; keeping the last good pair"))?;
    let new_set =
        crate::manifests::load(manifest_dir, &new_config.agent_overrides).map_err(|err| {
            format!("tma: reload failed (manifests): {err}; keeping the last good pair")
        })?;
    *config = new_config;
    *manifests = new_set.manifests;
    Ok(new_set.failures)
}

/// Latch a [`reload_pair`] outcome to the line worth printing: `Some` only when the failure differs
/// from the last one reported, `None` otherwise. The poll surfaces reload every tick, so an
/// unlatched failure would repeat until the config is fixed; a clean reload re-arms the latch, so a
/// re-broken file speaks up again. The skipped user manifests in the `Ok` are deliberately not
/// reported here: `tma doctor` and the surfaces' own load path are where a file gets named.
pub fn reload_notice(
    outcome: Result<Vec<crate::manifests::ManifestFailure>, String>,
    last: &mut Option<String>,
) -> Option<String> {
    match outcome {
        Ok(_) => {
            *last = None;
            None
        }
        Err(msg) if last.as_ref() == Some(&msg) => None,
        Err(msg) => {
            *last = Some(msg.clone());
            Some(msg)
        }
    }
}

/// Resolve the config path: `--config`, then `TMA_CONFIG`, then `$XDG_CONFIG_HOME/tma/`, then
/// `~/.config/tma/`. `None` only when none is set and there is no `HOME`. Mirrors the base-dir logic
/// in `install.rs` and `manifests.rs`.
fn resolve_path(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return Some(p.to_path_buf());
    }
    if let Some(p) = std::env::var_os("TMA_CONFIG") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg).join("tma/config.toml"));
        }
    }
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(|home| PathBuf::from(home).join(".config/tma/config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Zero-config: an empty document deserializes to exactly the documented defaults, so the
    /// no-file path and the pre-config hardcodes agree field-for-field.
    #[test]
    fn zero_config_equals_documented_defaults() {
        let c: Config = toml::from_str("").unwrap();
        // Fold parity is proven against the core's own default (the canonical zero-config path).
        assert_eq!(c.fold_config(), FoldConfig::default());
        // Status defaults (⚑ red / ● yellow / ○ green / ? colour244).
        assert_eq!(c.status.resolved(AgentState::Blocked), ("⚑", "red"));
        assert_eq!(c.status.resolved(AgentState::Working), ("●", "yellow"));
        assert_eq!(c.status.resolved(AgentState::Idle), ("○", "green"));
        assert_eq!(c.status.resolved(AgentState::Unknown), ("?", "colour244"));
        // "done" surface (idle + attention): ✓ magenta in both status and picker.
        assert_eq!(c.status.resolved_done(), ("✓", "magenta"));
        assert_eq!(c.picker.resolved_done_str(), ("✓", "magenta"));
        // Picker default strings (unknown is darkgray, not status's colour244); the ratatui-Color
        // mapping is proven in `tma_ui_core::palette`.
        assert_eq!(c.picker.resolved_str(AgentState::Blocked), ("⚑", "red"));
        assert_eq!(
            c.picker.resolved_str(AgentState::Unknown),
            ("?", "darkgray")
        );
        // Daemon knob defaults equal the shipped consts.
        assert_eq!(c.daemon.sweep(), tma_tmux::control::SWEEP_NORMAL);
        assert_eq!(
            c.daemon.quiet_threshold(),
            tma_tmux::control::QUIET_THRESHOLD
        );
        assert_eq!(c.daemon.demote_edges, crate::capture::DEMOTE_EDGES);
        assert_eq!(
            c.daemon.zero_member_recheck(),
            tma_tmux::control::EMPTY_POOL_RECHECK
        );
        // Daemon autostart is opt-in: off by default (the daemon stays strictly additive).
        assert!(!c.daemon.autostart);
        // Notify + focus defaults: off / none. `on` defaults to blocked-only.
        assert!(!c.notify.from_event);
        assert!(c.notify.command.is_none());
        assert_eq!(c.notify.on, vec![NotifyTrigger::Blocked]);
        assert!(c.notify.fires_on(NotifyTrigger::Blocked));
        assert!(!c.notify.fires_on(NotifyTrigger::Done));
        // Both tty sinks are opt-in: off by default (display-message-only behavior unchanged).
        assert!(!c.notify.bell);
        assert!(!c.notify.osc);
        assert!(c.notify.log.is_none());
        assert_eq!(c.notify.sinks(), NotifySinks::default());
        // context_high is opt-in: absent by default (no context-utilization notifications).
        assert!(c.notify.context_high.is_none());
        // No per-trigger routing by default: every trigger resolves to the (unset) global command.
        assert!(c.notify.blocked.is_none() && c.notify.done.is_none());
        assert!(!c.focus.events);
        // The tmux binary is unset by default: plain `tmux` off PATH.
        assert!(c.tmux.bin.is_none());
        assert!(c.agent_overrides.is_empty());
        // Per-agent API config is empty by default: the broker relies on the pane stamp.
        assert!(c.api.api_base("opencode").is_none());
        // Telemetry windows: zero-config recognizes the shipped names and nothing else.
        assert!(c.telemetry.windows.knows("gemini-1.5-pro"));
        assert!(!c.telemetry.windows.knows("some-unknown-model"));
    }

    /// `[telemetry.windows]` extends the recognized set. The sizes still have to parse (an existing
    /// config keeps loading) but nothing reads them, so an entry naming a shipped model is a no-op.
    #[test]
    fn telemetry_windows_extends_the_recognized_set() {
        let c: Config = toml::from_str(
            "[telemetry.windows]\n\"gemini-2.5-pro\" = 2000000\n\"gpt-5-codex\" = 272000\n",
        )
        .unwrap();
        assert!(c.telemetry.windows.knows("gpt-5-codex"));
        assert!(c.telemetry.windows.knows("gemini-2.5-pro"));
        // An untouched shipped name is still recognized.
        assert!(c.telemetry.windows.knows("gemini-1.5-pro"));
        // Still unknown outside the union.
        assert!(!c.telemetry.windows.knows("mystery-model"));
    }

    /// A partial section fills only the named field; the rest stay at their per-field defaults.
    #[test]
    fn partial_fold_keeps_other_fields_default() {
        let c: Config = toml::from_str("[fold]\ndwell_secs = 9\n").unwrap();
        let f = c.fold_config();
        assert_eq!(f.dwell_secs, 9);
        assert_eq!(f.hook_decay_secs, FoldConfig::default().hook_decay_secs);
        assert_eq!(
            f.blocked_decay_secs,
            FoldConfig::default().blocked_decay_secs
        );
        assert_eq!(f.freshness_secs, FoldConfig::default().freshness_secs);
    }

    /// The blocked window is configurable and independent of `hook_decay_secs`; its default sits
    /// well above it so a silent permission prompt outlives an ordinary hook claim.
    #[test]
    fn blocked_decay_is_separately_configurable_and_longer_by_default() {
        let d = FoldConfig::default();
        assert!(d.blocked_decay_secs > d.hook_decay_secs);
        let c: Config = toml::from_str("[fold]\nblocked_decay_secs = 900\n").unwrap();
        let f = c.fold_config();
        assert_eq!(f.blocked_decay_secs, 900);
        assert_eq!(f.hook_decay_secs, d.hook_decay_secs);
    }

    /// A partial glyph entry keeps the color at its default (per-field, not per-section).
    #[test]
    fn partial_status_style_keeps_default_color() {
        let c: Config = toml::from_str("[status]\nblocked = { glyph = \"!\" }\n").unwrap();
        assert_eq!(c.status.resolved(AgentState::Blocked), ("!", "red"));
    }

    /// An unknown key is a loud error (never silently ignored).
    #[test]
    fn unknown_key_is_rejected() {
        let err = toml::from_str::<Config>("[fold]\nnope = 1\n").unwrap_err();
        assert!(
            err.to_string().contains("nope"),
            "error names the key: {err}"
        );
    }

    /// `[api.opencode] api_base` parses into the per-agent API section and coexists with the
    /// `[[agent]]` override array.
    #[test]
    fn api_base_parses_and_coexists_with_agent_array() {
        let c: Config = toml::from_str(
            "[[agent]]\nname = \"opencode\"\n\n[api.opencode]\napi_base = \"http://127.0.0.1:4096\"\n",
        )
        .unwrap();
        assert_eq!(c.agent_overrides.len(), 1);
        assert_eq!(c.api.api_base("opencode"), Some("http://127.0.0.1:4096"));
        assert!(c.api.api_base("claude").is_none());
        // An unknown key inside the entry is a loud error.
        assert!(toml::from_str::<Config>("[api.opencode]\nurl = \"x\"\n").is_err());
    }

    /// The pre-rename `[agents]` table fails with the rename named, not a bare unknown-key error.
    #[test]
    fn legacy_agents_table_reports_the_rename() {
        let path = std::env::temp_dir().join(format!(
            "tma-config-legacy-agents-{}.toml",
            std::process::id()
        ));
        std::fs::write(
            &path,
            "[agents.opencode]\napi_base = \"http://127.0.0.1:4096\"\n",
        )
        .unwrap();
        let err = load(Some(&path)).unwrap_err();
        let _ = std::fs::remove_file(&path);
        let msg = err.to_string();
        assert!(
            msg.contains("the [agents] table was renamed to [api]") && msg.contains("[[agent]]"),
            "the error names the rename and clears the `[[agent]]` array: {msg}"
        );
    }

    #[test]
    fn agent_entry_defaults_enabled_true() {
        let c: Config = toml::from_str("[[agent]]\nname = \"claude\"\n").unwrap();
        assert_eq!(c.agent_overrides.len(), 1);
        assert!(c.agent_overrides[0].enabled);
        assert!(c.agent_overrides[0].process_names.is_empty());
    }

    /// `notify.on` parses the two trigger tokens, and `done` opts the working→idle completion in
    /// without disturbing the blocked default of an unset config.
    #[test]
    fn notify_on_parses_blocked_and_done() {
        let c: Config = toml::from_str("[notify]\non = [\"blocked\", \"done\"]\n").unwrap();
        assert_eq!(
            c.notify.on,
            vec![NotifyTrigger::Blocked, NotifyTrigger::Done]
        );
        assert!(c.notify.fires_on(NotifyTrigger::Blocked));
        assert!(c.notify.fires_on(NotifyTrigger::Done));

        // Done-only is expressible (drop the blocked default entirely).
        let done_only: Config = toml::from_str("[notify]\non = [\"done\"]\n").unwrap();
        assert!(!done_only.notify.fires_on(NotifyTrigger::Blocked));
        assert!(done_only.notify.fires_on(NotifyTrigger::Done));
    }

    /// An unknown `notify.on` value is a loud error naming the bad token (never silently dropped),
    /// matching the `deny_unknown_fields` posture elsewhere.
    #[test]
    fn notify_on_rejects_unknown_trigger() {
        let err = toml::from_str::<Config>("[notify]\non = [\"finished\"]\n").unwrap_err();
        assert!(
            err.to_string().contains("finished"),
            "error names the bad value: {err}"
        );
    }

    /// `notify.context_high` parses its `threshold` from the sub-table, and naming the table is the
    /// opt-in (absent stays disabled). A missing `threshold` is a loud error, not a silent default.
    #[test]
    fn notify_context_high_parses_threshold() {
        let c: Config = toml::from_str("[notify.context_high]\nthreshold = 75\n").unwrap();
        assert_eq!(c.notify.context_high.map(|c| c.threshold), Some(75));
        // Inline-table form parses identically.
        let inline: Config =
            toml::from_str("[notify]\ncontext_high = { threshold = 90 }\n").unwrap();
        assert_eq!(inline.notify.context_high.map(|c| c.threshold), Some(90));
        // The sub-table requires `threshold` (no silent default).
        assert!(toml::from_str::<Config>("[notify.context_high]\n").is_err());
    }

    /// Per-trigger routing: each `[notify.<trigger>]` command wins for its own trigger, and every
    /// unrouted trigger falls back to the global `notify.command`.
    #[test]
    fn notify_sub_tables_route_per_trigger() {
        let c: Config = toml::from_str(
            "[notify]\ncommand = \"global\"\n\
             [notify.blocked]\ncommand = \"ntfy\"\n\
             [notify.context_high]\nthreshold = 80\ncommand = \"log-it\"\n",
        )
        .unwrap();
        let cmds = c.notify.commands();
        assert_eq!(cmds.for_trigger(NotifyTrigger::Blocked), Some("ntfy"));
        assert_eq!(
            cmds.for_trigger(NotifyTrigger::Done),
            Some("global"),
            "an unrouted trigger falls back to the global command"
        );
        assert_eq!(cmds.for_context_high(), Some("log-it"));

        // No global command: an unrouted trigger simply has none (display-message only).
        let only_done: Config = toml::from_str("[notify.done]\ncommand = \"say done\"\n").unwrap();
        let cmds = only_done.notify.commands();
        assert_eq!(cmds.for_trigger(NotifyTrigger::Done), Some("say done"));
        assert_eq!(cmds.for_trigger(NotifyTrigger::Blocked), None);
        assert_eq!(cmds.for_context_high(), None);

        // Zero-config routes nothing at all.
        assert_eq!(
            Config::default().notify.commands(),
            NotifyCommands::default()
        );

        // An unknown key inside a sub-table stays a loud error.
        assert!(toml::from_str::<Config>("[notify.blocked]\ncmd = \"x\"\n").is_err());
    }

    /// The `TMA_NOTIFY_CMD` seam replaces every trigger's command, so an instrumented sink sees each
    /// fire regardless of the config's routing.
    #[test]
    fn env_override_replaces_every_trigger_command() {
        let c: Config = toml::from_str(
            "[notify]\ncommand = \"global\"\n[notify.blocked]\ncommand = \"ntfy\"\n",
        )
        .unwrap();
        let cmds = c.notify.commands().overridden_by(Some("sink".to_string()));
        assert_eq!(cmds.for_trigger(NotifyTrigger::Blocked), Some("sink"));
        assert_eq!(cmds.for_trigger(NotifyTrigger::Done), Some("sink"));
        assert_eq!(cmds.for_context_high(), Some("sink"));
        // An unset or empty override leaves the config's routing intact.
        let kept = c.notify.commands().overridden_by(Some(String::new()));
        assert_eq!(kept.for_trigger(NotifyTrigger::Blocked), Some("ntfy"));
        assert_eq!(c.notify.commands().overridden_by(None), kept);
    }

    /// `[tmux] bin` parses as an opaque name-or-path string, and an unknown key in the section is a
    /// loud error like everywhere else.
    #[test]
    fn tmux_bin_parses_a_name_or_a_path() {
        let by_name: Config = toml::from_str("[tmux]\nbin = \"tmate\"\n").unwrap();
        assert_eq!(by_name.tmux.bin.as_deref(), Some("tmate"));
        let by_path: Config = toml::from_str("[tmux]\nbin = \"/opt/homebrew/bin/tmux\"\n").unwrap();
        assert_eq!(by_path.tmux.bin.as_deref(), Some("/opt/homebrew/bin/tmux"));
        assert!(toml::from_str::<Config>("[tmux]\nbinary = \"tmate\"\n").is_err());
    }

    /// The two opt-in convenience keys parse to `true` from their documented tables (and stay
    /// independent — enabling one leaves the other's section default untouched).
    #[test]
    fn autostart_and_bell_parse_opt_in() {
        let c: Config = toml::from_str("[daemon]\nautostart = true\n").unwrap();
        assert!(c.daemon.autostart);
        assert!(
            !c.notify.bell,
            "bell stays default when only autostart is set"
        );

        let c: Config = toml::from_str("[notify]\nbell = true\n").unwrap();
        assert!(c.notify.bell);
        assert!(
            !c.daemon.autostart,
            "autostart stays default when only bell is set"
        );
    }

    // ---- config example ⇄ Config schema drift guard --------------------------------
    //
    // Both the README's landing-page snippet and the full `docs/reference/configuration.md` are
    // swept for ```toml fences; every fence must parse under the real `Config` serde, so a
    // documented key that drifts from the schema fails here under `deny_unknown_fields`.

    const README_MD: &str = include_str!("../../../README.md");
    const CONFIG_REFERENCE_MD: &str = include_str!("../../../docs/reference/configuration.md");

    /// One ```toml fence plus the heading it sits under (how a fence says what it is).
    struct DocFence {
        heading: String,
        body: String,
    }

    /// The fenced ```toml blocks in a Markdown doc. Both docs use TOML fences only for `config.toml`
    /// examples, so a whole-file sweep guards every one.
    fn readme_toml_blocks(md: &str) -> Vec<DocFence> {
        let mut blocks = Vec::new();
        let mut heading = String::new();
        let mut current: Option<String> = None;
        for line in md.lines() {
            match &mut current {
                None if line.trim_start() == "```toml" => current = Some(String::new()),
                None => {
                    if line.starts_with('#') {
                        heading = line.to_string();
                    }
                }
                Some(_) if line.trim_start() == "```" => blocks.push(DocFence {
                    heading: heading.clone(),
                    body: current.take().unwrap(),
                }),
                Some(buf) => {
                    buf.push_str(line);
                    buf.push('\n');
                }
            }
        }
        blocks
    }

    /// Assert every ```toml fence in `md` parses under the real `Config` serde, and that there is at
    /// least one to guard (a doc with no example fails loudly).
    fn assert_config_examples_parse(md: &str, doc: &str) {
        let blocks = readme_toml_blocks(md);
        assert!(
            !blocks.is_empty(),
            "{doc} has no ```toml config example to guard against schema drift"
        );
        for block in &blocks {
            let parsed = toml::from_str::<Config>(&block.body);
            assert!(
                parsed.is_ok(),
                "documented config.toml example in {} does not parse under Config:\n{}\nerror: {}",
                doc,
                block.body,
                parsed.unwrap_err()
            );
        }
    }

    /// Every documented `config.toml` example parses under the real `Config` serde, in both the
    /// README and the configuration reference: a `focus_events`-style doc bug fails the suite, not
    /// the user.
    #[test]
    fn readme_config_examples_parse() {
        assert_config_examples_parse(README_MD, "README.md");
        assert_config_examples_parse(CONFIG_REFERENCE_MD, "docs/reference/configuration.md");
    }

    // ---- documented default VALUES ⇄ Config::default() ------------------------------
    //
    // Parsing is not enough: `hook_decay_secs = 45` parses fine while the real default is 60. The
    // guard below reads the defaults out of the code and compares them against what the doc claims,
    // in the fence whose heading says it shows defaults and in the `| key | default |` tables.

    /// Every documented default as `dotted.path → value`, read from the live defaults rather than
    /// retyped here. Style entries carry the RESOLVED pair the doc shows (`{ glyph, color }`); the
    /// struct field behind them is `None`, which no doc would print.
    fn documented_defaults() -> Vec<(String, toml::Value)> {
        let c = Config::default();
        let f = c.fold_config();
        let d = &c.daemon;
        let secs = |v: u64| toml::Value::Integer(v as i64);
        let mut out = vec![
            ("fold.dwell_secs".to_string(), secs(f.dwell_secs)),
            ("fold.hook_decay_secs".to_string(), secs(f.hook_decay_secs)),
            (
                "fold.blocked_decay_secs".to_string(),
                secs(f.blocked_decay_secs),
            ),
            ("fold.freshness_secs".to_string(), secs(f.freshness_secs)),
            ("daemon.sweep_secs".to_string(), secs(d.sweep_secs)),
            ("daemon.quiet_ms".to_string(), secs(d.quiet_ms)),
            (
                "daemon.zero_member_recheck_secs".to_string(),
                secs(d.zero_member_recheck_secs),
            ),
            (
                "daemon.demote_edges".to_string(),
                toml::Value::Integer(d.demote_edges as i64),
            ),
            (
                "daemon.autostart".to_string(),
                toml::Value::Boolean(d.autostart),
            ),
            (
                "focus.events".to_string(),
                toml::Value::Boolean(c.focus.events),
            ),
            (
                "install.wrapper_ref".to_string(),
                toml::Value::String(
                    match c.install.wrapper_ref {
                        WrapperRef::Absolute => "absolute",
                        WrapperRef::Bare => "bare",
                    }
                    .to_string(),
                ),
            ),
            (
                "notify.from_event".to_string(),
                toml::Value::Boolean(c.notify.from_event),
            ),
            (
                "notify.bell".to_string(),
                toml::Value::Boolean(c.notify.bell),
            ),
            ("notify.osc".to_string(), toml::Value::Boolean(c.notify.osc)),
            (
                "notify.on".to_string(),
                toml::Value::Array(
                    c.notify
                        .on
                        .iter()
                        .map(|t| toml::Value::String(t.word().to_string()))
                        .collect(),
                ),
            ),
        ];
        let states = [
            (AgentState::Blocked, "blocked"),
            (AgentState::Working, "working"),
            (AgentState::Idle, "idle"),
            (AgentState::Unknown, "unknown"),
        ];
        let mut style = |section: &str, key: &str, (glyph, color): (&str, &str)| {
            out.push((
                format!("{section}.{key}.glyph"),
                toml::Value::String(glyph.to_string()),
            ));
            out.push((
                format!("{section}.{key}.color"),
                toml::Value::String(color.to_string()),
            ));
        };
        for (state, key) in states {
            style("status", key, c.status.resolved(state));
            style("picker", key, c.picker.resolved_str(state));
        }
        style("status", "done", c.status.resolved_done());
        style("picker", "done", c.picker.resolved_done_str());
        out
    }

    /// Whether a fence's heading claims it shows the built-in defaults. This is the marking rule:
    /// only a fence a heading calls "defaults" is held to `Config::default()`; every other fence
    /// (the README's override snippet) is an example and is only required to parse.
    fn claims_defaults(heading: &str) -> bool {
        heading.to_ascii_lowercase().contains("defaults")
    }

    /// Flatten a TOML table to `dotted.path → value` leaves. Arrays stay leaves, so an
    /// array-of-tables like `[[agent]]` lands under its own key and is skipped as an example below.
    fn flatten(table: &toml::Table, prefix: &str, out: &mut Vec<(String, toml::Value)>) {
        for (key, value) in table {
            let path = if prefix.is_empty() {
                key.clone()
            } else {
                format!("{prefix}.{key}")
            };
            match value {
                toml::Value::Table(t) => flatten(t, &path, out),
                other => out.push((path, other.clone())),
            }
        }
    }

    /// The defaults fence shows exactly `Config::default()`. Every leaf it sets must be known to
    /// [`documented_defaults`] and equal to it, so a newly documented key cannot slip past by being
    /// unrecognized. `[[agent]]` is the one exempt table: it documents the shape of an override, and
    /// the default is no entries at all.
    #[test]
    fn documented_defaults_fence_matches_config_default() {
        let doc = "docs/reference/configuration.md";
        let fences: Vec<_> = readme_toml_blocks(CONFIG_REFERENCE_MD)
            .into_iter()
            .filter(|f| claims_defaults(&f.heading))
            .collect();
        assert_eq!(
            fences.len(),
            1,
            "{doc} must carry exactly one fence whose heading names it as the defaults example \
             (found {}); the value guard keys off that heading",
            fences.len()
        );

        let table: toml::Table = fences[0]
            .body
            .parse()
            .expect("defaults fence parses as TOML");
        let mut leaves = Vec::new();
        flatten(&table, "", &mut leaves);
        let expected = documented_defaults();
        let mut checked = 0;
        for (path, value) in &leaves {
            if path.split('.').next() == Some("agent") {
                continue; // an override example, not a default
            }
            let want = expected
                .iter()
                .find(|(p, _)| p == path)
                .unwrap_or_else(|| panic!("{doc} documents `{path}` as a default, which this guard does not know; add it to documented_defaults"));
            assert_eq!(
                value, &want.1,
                "{doc}: documented default for `{path}` is not the real default"
            );
            checked += 1;
        }
        assert!(
            checked >= 10,
            "only {checked} documented defaults checked; the fence sweep is not seeing the example"
        );
    }

    /// The `| key | default | meaning |` tables carry the same defaults as the fence, and drifted
    /// independently of it before. Each row under a heading naming one `[section]` is compared when
    /// its value is a TOML literal and the key is a known default; prose cells (`unset`,
    /// `(required)`) carry no value to check.
    #[test]
    fn documented_default_tables_match_config_default() {
        let doc = "docs/reference/configuration.md";
        let expected = documented_defaults();
        let mut section = String::new();
        let mut checked = 0;
        for line in CONFIG_REFERENCE_MD.lines() {
            if line.starts_with('#') {
                // A heading naming exactly one `[section]` sets the scope; anything else (the
                // shared `[status]` and `[picker]` heading) leaves no scope, so its rows are skipped.
                let named: Vec<&str> = line.split('`').skip(1).step_by(2).collect();
                section = match named.as_slice() {
                    [one] => one.trim_matches(['[', ']']).to_string(),
                    _ => String::new(),
                };
                continue;
            }
            if section.is_empty() || !line.starts_with("| `") {
                continue;
            }
            let cells: Vec<&str> = line.split('|').map(str::trim).collect();
            let (Some(key), Some(cell)) = (cells.get(1), cells.get(2)) else {
                continue;
            };
            let path = format!("{}.{}", section, key.trim_matches('`'));
            let Some((_, want)) = expected.iter().find(|(p, _)| *p == path) else {
                continue; // not a key with a documentable default (`command`, `name`, …)
            };
            let literal = cell.trim_matches('`');
            let Ok(parsed) = format!("x = {literal}").parse::<toml::Table>() else {
                continue; // prose, not a value
            };
            assert_eq!(
                &parsed["x"], want,
                "{doc}: the `{path}` row documents a value that is not the real default"
            );
            checked += 1;
        }
        assert!(
            checked >= 12,
            "only {checked} documented default rows checked; the table sweep is not seeing the doc"
        );
    }

    /// The reload latch: one line per breakage on a loop that reloads every tick, and a clean
    /// reload re-arms it so a file broken again is reported again.
    #[test]
    fn reload_notice_reports_each_breakage_once() {
        const BAD: &str = "tma: reload failed (config): boom";
        let bad = || Err(BAD.to_string());
        let mut last = None;

        assert_eq!(reload_notice(bad(), &mut last).as_deref(), Some(BAD));
        assert_eq!(
            reload_notice(bad(), &mut last),
            None,
            "the same failure stays quiet"
        );

        // A different failure is its own line.
        let other = Err("tma: reload failed (manifests): boom".to_string());
        assert!(reload_notice(other, &mut last).is_some());

        // A clean reload re-arms; the skipped manifests it carries are not reported here.
        assert_eq!(reload_notice(Ok(Vec::new()), &mut last), None);
        assert!(last.is_none(), "the latch cleared");
        assert!(
            reload_notice(bad(), &mut last).is_some(),
            "a re-broken file speaks up again"
        );
    }

    /// Failability: an example carrying an unknown key must fail the guard (proving
    /// `deny_unknown_fields` is what makes the drift test bite).
    #[test]
    fn readme_config_guard_rejects_unknown_key() {
        // A plausible-but-wrong top-level key.
        assert!(toml::from_str::<Config>("focus_events = true\n").is_err());
    }
}
