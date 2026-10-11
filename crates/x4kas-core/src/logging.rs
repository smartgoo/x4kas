//! Logging for both frontends: `tracing` events (and the `log` crate's, bridged in by
//! `tracing-subscriber`) go through one reloadable filter to stderr and/or a daily
//! rolling file under `~/.x4kas/logs/`, never to stdout, which is the CLI's output.
//! The Kaspa client logs through `workflow-log`, which would print to stdout: its
//! messages are piped into the `log` crate instead (`WorkflowSink`), so the same filter
//! applies to them.
//!
//! The filter is built from a [`LogLevel`] for the app's own crates (dependencies stay
//! at `warn`) plus extra directives, unless `X4KAS_LOG` or `RUST_LOG` is set, which
//! then replaces it whole ([`FilterSource::Env`]).

use std::fmt;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, Result};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::{Layered, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, Registry, reload};

use crate::config::SettingsWarning;

/// The folder of the data directory the GUI's log files go into; a reset keeps it.
pub const LOGS_DIR: &str = "logs";

/// The environment variables that replace the configured filter, first one set wins.
pub const ENV_VARS: [&str; 2] = ["X4KAS_LOG", "RUST_LOG"];

/// The filter target that covers every x4kas crate (`x4kas`, `x4kas_core`,
/// `x4kas_cli`): `EnvFilter` matches targets by prefix.
const APP_TARGET: &str = "x4kas";

/// The target of a `workflow-log` message that names none.
const WORKFLOW_TARGET: &str = "workflow";

/// How much the app's own crates log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum LogLevel {
    Error,
    Warn,
    #[default]
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    pub const ALL: [LogLevel; 5] = [
        LogLevel::Error,
        LogLevel::Warn,
        LogLevel::Info,
        LogLevel::Debug,
        LogLevel::Trace,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            LogLevel::Error => "error",
            LogLevel::Warn => "warn",
            LogLevel::Info => "info",
            LogLevel::Debug => "debug",
            LogLevel::Trace => "trace",
        }
    }

    /// The level `steps` more verbose (negative: quieter), clamped to the ends.
    pub fn step(self, steps: i32) -> LogLevel {
        let i = Self::ALL.iter().position(|l| *l == self).unwrap_or(2) as i32;
        Self::ALL[(i + steps).clamp(0, Self::ALL.len() as i32 - 1) as usize]
    }
}

impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for LogLevel {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|l| l.as_str().eq_ignore_ascii_case(s.trim()))
            .ok_or_else(|| format!("unknown log level `{s}` (error, warn, info, debug or trace)"))
    }
}

/// How each line is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogFormat {
    /// Time, level, target and spans, then the message.
    #[default]
    Text,
    /// One JSON object per line.
    Json,
}

impl LogFormat {
    pub const ALL: [LogFormat; 2] = [LogFormat::Text, LogFormat::Json];

    pub fn as_str(self) -> &'static str {
        match self {
            LogFormat::Text => "text",
            LogFormat::Json => "json",
        }
    }
}

impl fmt::Display for LogFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for LogFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|f| f.as_str().eq_ignore_ascii_case(s.trim()))
            .ok_or_else(|| format!("unknown log format `{s}` (text or json)"))
    }
}

/// A daily rolling log file: `<dir>/<prefix>.YYYY-MM-DD.log`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileLog {
    pub dir: PathBuf,
    pub prefix: &'static str,
    /// How many daily files are kept; older ones are deleted as the file rolls.
    pub keep_files: usize,
}

/// What [`init`] sets up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogConfig {
    pub level: LogLevel,
    /// Extra `EnvFilter` directives after the level's, e.g. `fjall=info`.
    pub filter: String,
    pub format: LogFormat,
    pub stderr: bool,
    pub file: Option<FileLog>,
    /// Keep the latest records in memory for a viewer ([`LogRuntime::buffer`]).
    pub buffer: bool,
}

/// The file prefix of the GUI's logs: `x4kas.YYYY-MM-DD.log`.
pub const GUI_PREFIX: &str = "x4kas";
/// The file prefix of the CLI's logs (`--log-file`): `x4kas-cli.YYYY-MM-DD.log`.
pub const CLI_PREFIX: &str = "x4kas-cli";

impl LogConfig {
    /// The GUI's: a rolling file in `~/.x4kas/logs/` unless `settings.file` is off, and
    /// stderr in debug builds.
    pub fn gui(settings: &LogSettings) -> Self {
        Self {
            stderr: cfg!(debug_assertions),
            file: settings.file.then(|| settings.file_log(GUI_PREFIX)),
            buffer: true,
            ..Self::from_settings(settings)
        }
    }

    /// The CLI's: stderr only, never stdout (the command's output); its flags may add a
    /// file (`LogSettings::file_log(CLI_PREFIX)`).
    pub fn cli(settings: &LogSettings) -> Self {
        Self {
            stderr: true,
            file: None,
            ..Self::from_settings(settings)
        }
    }

    fn from_settings(settings: &LogSettings) -> Self {
        Self {
            level: settings.level,
            filter: settings.filter.clone(),
            format: settings.format,
            stderr: false,
            file: None,
            buffer: false,
        }
    }

    /// The filter directives the level and extra filter make.
    pub fn directives(&self) -> String {
        directives(self.level, &self.filter)
    }
}

/// How many daily files are kept by default.
pub const DEFAULT_KEEP_DAYS: u32 = 7;
/// The most daily files `keep_days` may keep.
pub const MAX_KEEP_DAYS: u32 = 90;
/// The `keep_days` the GUI offers.
pub const KEEP_DAYS_CHOICES: [u32; 6] = [1, 3, 7, 14, 30, 90];

/// The logging settings both frontends read at launch (`~/.x4kas/logging.toml`): one
/// level for the GUI and the CLI, which the CLI's flags override for a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogSettings {
    pub level: LogLevel,
    /// Extra filter directives, e.g. `x4kas_core::index=debug,fjall=info`.
    pub filter: String,
    /// Whether the GUI writes its log file (the CLI's is `--log-file`).
    pub file: bool,
    pub format: LogFormat,
    /// How many daily log files are kept, 1 to [`MAX_KEEP_DAYS`].
    pub keep_days: u32,
}

impl Default for LogSettings {
    fn default() -> Self {
        Self {
            level: LogLevel::Info,
            filter: String::new(),
            file: true,
            format: LogFormat::Text,
            keep_days: DEFAULT_KEEP_DAYS,
        }
    }
}

/// One change to the logging settings, applied to the file as it is
/// ([`LogSettings::update_file`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogSettingsEdit {
    Level(LogLevel),
    /// Extra directives, checked with [`validate_filter`] first; empty clears them.
    Filter(String),
    File(bool),
    Format(LogFormat),
    KeepDays(u32),
}

impl LogSettingsEdit {
    /// The key in `logging.toml` the edit sets.
    pub fn key(&self) -> &'static str {
        match self {
            Self::Level(_) => "level",
            Self::Filter(_) => "filter",
            Self::File(_) => "file",
            Self::Format(_) => "format",
            Self::KeepDays(_) => "keep_days",
        }
    }
}

/// Whether `filter` parses as extra directives; the error says why not.
pub fn validate_filter(filter: &str) -> Result<(), String> {
    EnvFilter::try_new(directives(LogLevel::Info, filter))
        .map(|_| ())
        .map_err(|e| format!("invalid log filter `{}`: {e}", filter.trim()))
}

/// Whether `days` is a `keep_days` the settings take.
pub fn validate_keep_days(days: u32) -> Result<(), String> {
    if (1..=MAX_KEEP_DAYS).contains(&days) {
        Ok(())
    } else {
        Err(format!(
            "keep_days must be 1 to {MAX_KEEP_DAYS}, not {days}"
        ))
    }
}

impl LogSettings {
    pub const KEYS: [&'static str; 5] = ["level", "filter", "file", "format", "keep_days"];

    pub fn path() -> PathBuf {
        crate::config::data_dir().join("logging.toml")
    }

    /// The settings on disk, and a warning for each key whose value didn't load (a hand
    /// edit), which then has its default: the rest of the file still counts.
    pub fn load_with_warnings() -> Result<(Self, Vec<SettingsWarning>)> {
        let path = Self::path();
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e.into()),
        };
        Self::parse_keyed(&text).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
    }

    /// Load the file, apply `edits` and save it, under its lock, keeping what another
    /// process changed, keys this version doesn't know and the text of a key that
    /// didn't load unless an edit sets it (as `IndexSettings::update_file`). Returns the
    /// settings and the load warnings the edits didn't settle.
    pub fn update_file(edits: &[LogSettingsEdit]) -> Result<(Self, Vec<SettingsWarning>)> {
        crate::config::update_toml_file(&Self::path(), |text| {
            let (settings, warnings, out) = Self::edit_text(text, edits)?;
            Ok(((settings, warnings), out))
        })
    }

    fn edit_text(
        text: &str,
        edits: &[LogSettingsEdit],
    ) -> Result<(Self, Vec<SettingsWarning>, String)> {
        let (mut settings, mut warnings) = Self::parse_keyed(text)?;
        for edit in edits {
            settings.apply(edit.clone());
        }
        warnings.retain(|(key, _)| !edits.iter().any(|e| e.key() == *key));
        let mut out: toml::Table = toml::from_str(text)?;
        for (key, value) in settings.to_table() {
            if !warnings.iter().any(|(k, _)| *k == key) {
                out.insert(key, value);
            }
        }
        Ok((settings, warnings, toml::to_string_pretty(&out)?))
    }

    pub fn apply(&mut self, edit: LogSettingsEdit) {
        match edit {
            LogSettingsEdit::Level(level) => self.level = level,
            LogSettingsEdit::Filter(filter) => self.filter = filter.trim().to_string(),
            LogSettingsEdit::File(on) => self.file = on,
            LogSettingsEdit::Format(format) => self.format = format,
            LogSettingsEdit::KeepDays(days) => self.keep_days = days,
        }
    }

    /// Each key on its own, so one bad value falls back alone. Fails only on a file that
    /// isn't TOML.
    fn parse_keyed(text: &str) -> Result<(Self, Vec<SettingsWarning>)> {
        let table: toml::Table = toml::from_str(text)?;
        let mut settings = Self::default();
        let mut warnings = Vec::new();
        let mut warn = |key: &'static str, why: String, fallback: &dyn fmt::Display| {
            warnings.push((key, format!("{key}: {why}: using {fallback}")));
        };
        if let Some(v) = table.get("level") {
            match v.as_str().map(str::parse::<LogLevel>) {
                Some(Ok(level)) => settings.level = level,
                Some(Err(e)) => warn("level", e, &settings.level),
                None => warn(
                    "level",
                    format!("expected a string, not {v}"),
                    &settings.level,
                ),
            }
        }
        if let Some(v) = table.get("filter") {
            match v.as_str().map(|f| validate_filter(f).map(|()| f)) {
                Some(Ok(filter)) => settings.filter = filter.trim().to_string(),
                Some(Err(e)) => warn("filter", e, &"no filter"),
                None => warn(
                    "filter",
                    format!("expected a string, not {v}"),
                    &"no filter",
                ),
            }
        }
        if let Some(v) = table.get("file") {
            match v.as_bool() {
                Some(on) => settings.file = on,
                None => warn("file", format!("expected true or false, not {v}"), &true),
            }
        }
        if let Some(v) = table.get("format") {
            match v.as_str().map(str::parse::<LogFormat>) {
                Some(Ok(format)) => settings.format = format,
                Some(Err(e)) => warn("format", e, &settings.format),
                None => warn(
                    "format",
                    format!("expected a string, not {v}"),
                    &settings.format,
                ),
            }
        }
        if let Some(v) = table.get("keep_days") {
            let days = v
                .as_integer()
                .ok_or_else(|| format!("expected a number, not {v}"))
                .and_then(|d| u32::try_from(d).map_err(|_| format!("{d} is out of range")))
                .and_then(|d| validate_keep_days(d).map(|()| d));
            match days {
                Ok(days) => settings.keep_days = days,
                Err(e) => warn("keep_days", e, &DEFAULT_KEEP_DAYS),
            }
        }
        Ok((settings, warnings))
    }

    fn to_table(&self) -> toml::Table {
        let mut t = toml::Table::new();
        t.insert("level".into(), self.level.as_str().into());
        t.insert("filter".into(), self.filter.clone().into());
        t.insert("file".into(), self.file.into());
        t.insert("format".into(), self.format.as_str().into());
        t.insert("keep_days".into(), i64::from(self.keep_days).into());
        t
    }

    /// The filter directives the level and extra filter make.
    pub fn directives(&self) -> String {
        directives(self.level, &self.filter)
    }

    /// A daily rolling file in `~/.x4kas/logs/` named by `prefix`, kept `keep_days`.
    pub fn file_log(&self, prefix: &'static str) -> FileLog {
        FileLog {
            dir: logs_dir(),
            prefix,
            keep_files: self.keep_days as usize,
        }
    }
}

/// `~/.x4kas/logs`.
pub fn logs_dir() -> PathBuf {
    crate::config::data_dir().join(LOGS_DIR)
}

/// Dependencies at `warn` (`error` at the error level), the app's crates at `level`,
/// then `extra`.
pub fn directives(level: LogLevel, extra: &str) -> String {
    let base = if level == LogLevel::Error {
        "error"
    } else {
        "warn"
    };
    let mut out = format!("{base},{APP_TARGET}={level}");
    let extra = extra.trim().trim_matches(',');
    if !extra.is_empty() {
        out.push(',');
        out.push_str(extra);
    }
    out
}

/// Where the filter in force came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterSource {
    /// The configuration: the level and the extra filter.
    Config,
    /// An environment variable (one of [`ENV_VARS`]) that replaces the configuration.
    Env(&'static str),
}

/// The first of [`ENV_VARS`] set to something, and its value.
fn env_override(get: impl Fn(&str) -> Option<String>) -> Option<(&'static str, String)> {
    ENV_VARS
        .into_iter()
        .find_map(|var| get(var).filter(|v| !v.trim().is_empty()).map(|v| (var, v)))
}

type Base = Layered<reload::Layer<EnvFilter, Registry>, Registry>;
type BoxedLayer = Box<dyn Layer<Base> + Send + Sync>;

/// Changes the filter of the running subscriber.
#[derive(Clone)]
pub struct LogHandle(reload::Handle<EnvFilter, Registry>);

impl LogHandle {
    /// Replace the filter with `directives`, refusing ones that don't parse.
    pub fn set_directives(&self, directives: &str) -> Result<()> {
        let filter = EnvFilter::try_new(directives)
            .with_context(|| format!("invalid log filter `{directives}`"))?;
        self.0.reload(filter)?;
        Ok(())
    }
}

/// The installed logging: keep it until the process exits, since dropping it stops the
/// file writer (flushing what it has, [`flush`]).
pub struct LogRuntime {
    handle: LogHandle,
    source: FilterSource,
    file: Option<FileLog>,
    buffer: Option<Arc<LogBuffer>>,
}

/// The log file's writer: dropping it writes out what it holds and stops it. Kept here
/// rather than in `LogRuntime` so [`flush`] can reach it from an exit path that never
/// gets back to `main` (macOS's Quit).
static FILE_GUARD: std::sync::Mutex<Option<tracing_appender::non_blocking::WorkerGuard>> =
    std::sync::Mutex::new(None);

/// Write out what the log file's writer still holds, and stop it: for a process about
/// to exit without returning from `main` (dropping the `LogRuntime` does it otherwise).
/// Later events no longer reach the file.
pub fn flush() {
    let guard = FILE_GUARD.lock().ok().and_then(|mut g| g.take());
    drop(guard);
}

impl Drop for LogRuntime {
    fn drop(&mut self) {
        flush();
    }
}

impl LogRuntime {
    pub fn handle(&self) -> &LogHandle {
        &self.handle
    }

    /// Where the filter in force came from: the configuration can't change it while an
    /// environment variable set it.
    pub fn source(&self) -> &FilterSource {
        &self.source
    }

    /// The rolling file being written, if any (`None` also when it couldn't be opened).
    pub fn file(&self) -> Option<&FileLog> {
        self.file.as_ref()
    }

    /// The latest records, when the configuration asked for them.
    pub fn buffer(&self) -> Option<Arc<LogBuffer>> {
        self.buffer.clone()
    }

    /// What a frontend needs to change the filter while running.
    pub fn control(&self) -> LogControl {
        LogControl {
            handle: self.handle.clone(),
            source: self.source.clone(),
        }
    }
}

/// Changes the running filter: what the controller gets to apply the level and extra
/// filter set in the GUI at once.
#[derive(Clone)]
pub struct LogControl {
    pub handle: LogHandle,
    /// While an environment variable set the filter, the settings don't change it.
    pub source: FilterSource,
}

impl LogControl {
    /// Apply `settings`' level and extra filter, unless an environment variable decides.
    pub fn apply(&self, settings: &LogSettings) -> Result<()> {
        match self.source {
            FilterSource::Env(_) => Ok(()),
            FilterSource::Config => self.handle.set_directives(&settings.directives()),
        }
    }
}

impl fmt::Debug for LogRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LogRuntime")
            .field("source", &self.source)
            .field("file", &self.file)
            .finish_non_exhaustive()
    }
}

/// Install the global subscriber, the `log` and `workflow-log` bridges and a panic hook
/// that logs panics. Call once, first thing in `main`. A log file that can't be opened
/// or a bad environment filter doesn't fail it: logging goes on without it and says so.
pub fn init(config: &LogConfig) -> Result<LogRuntime> {
    let mut warnings = Vec::new();
    let (source, filter) = match env_override(|v| std::env::var(v).ok()) {
        Some((var, value)) => match EnvFilter::try_new(&value) {
            Ok(filter) => (FilterSource::Env(var), filter),
            Err(e) => {
                warnings.push(format!("ignoring {var}={value}: {e}"));
                (FilterSource::Config, config_filter(config, &mut warnings))
            }
        },
        None => (FilterSource::Config, config_filter(config, &mut warnings)),
    };
    let (filter, reload) = reload::Layer::new(filter);

    let mut layers: Vec<BoxedLayer> = Vec::new();
    if config.stderr {
        let ansi = std::io::stderr().is_terminal();
        layers.push(fmt_layer(std::io::stderr, config.format, ansi));
    }
    let buffer = config.buffer.then(|| Arc::new(LogBuffer::default()));
    if let Some(buffer) = &buffer {
        layers.push(BufferLayer(buffer.clone()).boxed());
    }
    let mut guard = None;
    let mut file = None;
    if let Some(file_log) = &config.file {
        match open_file(file_log) {
            Ok(appender) => {
                let (writer, g) = tracing_appender::non_blocking(appender);
                layers.push(fmt_layer(writer, config.format, false));
                guard = Some(g);
                file = Some(file_log.clone());
            }
            Err(e) => warnings.push(format!("not logging to {}: {e:#}", file_log.dir.display())),
        }
    }

    tracing_subscriber::registry()
        .with(filter)
        .with(layers)
        .try_init()
        .context("a logger is already installed")?;
    if let Ok(mut slot) = FILE_GUARD.lock() {
        *slot = guard;
    }

    // Let every workflow-log message through to the sink; the filter decides.
    workflow_log::set_log_level(log::LevelFilter::Trace);
    workflow_log::pipe(Some(Arc::new(WorkflowSink)));
    install_panic_hook();

    for warning in warnings {
        tracing::warn!("{warning}");
    }
    Ok(LogRuntime {
        handle: LogHandle(reload),
        source,
        file,
        buffer,
    })
}

/// The configuration's filter, or the default level's if its extra directives don't
/// parse.
fn config_filter(config: &LogConfig, warnings: &mut Vec<String>) -> EnvFilter {
    EnvFilter::try_new(config.directives()).unwrap_or_else(|e| {
        warnings.push(format!("ignoring log filter `{}`: {e}", config.filter));
        EnvFilter::new(directives(config.level, ""))
    })
}

fn open_file(file: &FileLog) -> Result<PruningAppender> {
    std::fs::create_dir_all(&file.dir)?;
    // No `max_log_files`: tracing-appender prunes every file whose name starts with the
    // prefix, so the GUI's `x4kas` would take the CLI's `x4kas-cli.…` files with it.
    let inner = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix(file.prefix)
        .filename_suffix("log")
        .build(&file.dir)?;
    let mut appender = PruningAppender {
        inner,
        dir: file.dir.clone(),
        prefix: file.prefix,
        keep: file.keep_files.max(1),
        day: utc_day(),
    };
    appender.prune();
    Ok(appender)
}

/// The UTC day number, the daily rotation's.
fn utc_day() -> u64 {
    crate::format::now_ms() / 86_400_000
}

/// The rolling appender, keeping the newest `keep` daily files of its own prefix only
/// ([`log_files`]): pruned when opened and on the first write of each day.
struct PruningAppender {
    inner: tracing_appender::rolling::RollingFileAppender,
    dir: PathBuf,
    prefix: &'static str,
    keep: usize,
    day: u64,
}

impl PruningAppender {
    fn prune(&mut self) {
        prune_log_files(&self.dir, self.prefix, self.keep);
    }
}

/// Delete all but the newest `keep` daily files of `prefix` in `dir`.
fn prune_log_files(dir: &Path, prefix: &str, keep: usize) {
    let Ok(files) = log_files(dir, prefix) else {
        return;
    };
    let excess = files.len().saturating_sub(keep);
    for path in &files[..excess] {
        // Not logged: this runs inside the file writer.
        let _ = std::fs::remove_file(path);
    }
}

impl std::io::Write for PruningAppender {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buf)?;
        let day = utc_day();
        if day != self.day {
            // After the write, so the new day's file exists and counts.
            self.day = day;
            self.prune();
        }
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn fmt_layer<W>(writer: W, format: LogFormat, ansi: bool) -> BoxedLayer
where
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    let layer = tracing_subscriber::fmt::layer()
        .with_writer(writer)
        .with_ansi(ansi)
        .with_target(true);
    match format {
        LogFormat::Text => layer.boxed(),
        LogFormat::Json => layer
            .json()
            .flatten_event(true)
            .with_current_span(true)
            .with_span_list(false)
            .boxed(),
    }
}

/// How a failure that repeats on a timer (a poll, a write per batch) changed since it
/// was last seen: logged when it starts or its text changes, and once when it clears,
/// never on every repeat ([`log_error_change!`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorChange<'a> {
    /// A new failure, or a different one than before.
    Failed(&'a str),
    /// It failed before and doesn't now.
    Recovered,
    /// As before: the same failure again, or still fine.
    Same,
}

/// What going from the error `before` to `now` (each `None` while fine) amounts to.
pub fn error_change<'a>(before: Option<&str>, now: Option<&'a str>) -> ErrorChange<'a> {
    match (before, now) {
        (before, Some(e)) if before != Some(e) => ErrorChange::Failed(e),
        (Some(_), None) => ErrorChange::Recovered,
        _ => ErrorChange::Same,
    }
}

/// The last outcome of a repeating request, for [`error_change`] when no other state
/// holds it (or that state is shared with other errors).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ErrorLog(Option<String>);

impl ErrorLog {
    /// Whether the last outcome was a failure.
    pub fn failing(&self) -> bool {
        self.0.is_some()
    }

    /// Record this run's error (`None`: it succeeded) and say how it changed.
    pub fn update<'a>(&mut self, now: Option<&'a str>) -> ErrorChange<'a> {
        let change = error_change(self.0.as_deref(), now);
        if change != ErrorChange::Same {
            self.0 = now.map(str::to_string);
        }
        change
    }
}

/// Log an [`ErrorChange`]: a failure at `$level` as "<what> failed: <error>", a
/// recovery at info as "<what> recovered", nothing for the same again. A macro so the
/// event's target is the caller's module. Fields may follow `$what`, as in `tracing`'s
/// macros: `log_error_change!(Level::WARN, change, "watched query", query = %name)`.
macro_rules! log_error_change {
    ($level:expr, $change:expr, $what:literal $(, $($fields:tt)+)?) => {
        match $change {
            $crate::logging::ErrorChange::Failed(e) => {
                tracing::event!($level, $($($fields)+,)? "{} failed: {}", $what, e)
            }
            $crate::logging::ErrorChange::Recovered => {
                tracing::info!($($($fields)+,)? "{} recovered", $what)
            }
            $crate::logging::ErrorChange::Same => {}
        }
    };
}
pub(crate) use log_error_change;

/// The daily files of `prefix` in `dir`, oldest first (their dates sort by name). A
/// missing directory has none.
pub fn log_files(dir: &Path, prefix: &str) -> std::io::Result<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut files = Vec::new();
    for entry in entries {
        let path = entry?.path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        // `x4kas.` doesn't match `x4kas-cli.…`.
        if name
            .strip_prefix(prefix)
            .and_then(|rest| rest.strip_prefix('.'))
            .is_some_and(|rest| rest.ends_with(".log"))
        {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

/// The level a log line starts a record with, in either format; `None` for a line that
/// continues the record before it (a multi-line message such as a panic's backtrace).
pub fn record_level(line: &str) -> Option<LogLevel> {
    record_level_bytes(line.as_bytes())
}

/// [`record_level`] of a line as it is in the file: no UTF-8 check and no JSON parse,
/// for indexing files of millions of lines.
pub fn record_level_bytes(line: &[u8]) -> Option<LogLevel> {
    if line.first() == Some(&b'{') {
        const KEY: &[u8] = b"\"level\":\"";
        let start = line.windows(KEY.len()).position(|w| w == KEY)? + KEY.len();
        let len = line[start..].iter().position(|b| *b == b'"')?;
        return level_from_bytes(&line[start..start + len]);
    }
    // `2026-10-11T19:01:02.123456Z  INFO target: message`
    let mut words = line
        .split(|b| b.is_ascii_whitespace())
        .filter(|w| !w.is_empty());
    let time = words.next()?;
    if !(time.first()?.is_ascii_digit() && time.last() == Some(&b'Z')) {
        return None;
    }
    level_from_bytes(words.next()?)
}

fn level_from_bytes(word: &[u8]) -> Option<LogLevel> {
    LogLevel::ALL
        .into_iter()
        .find(|l| l.as_str().as_bytes().eq_ignore_ascii_case(word))
}

/// Picks the lines of records at `min` or more severe (all with `None`), keeping a
/// continuation line with the record it belongs to.
#[derive(Debug, Clone)]
pub struct RecordFilter {
    min: Option<LogLevel>,
    in_record: bool,
}

impl RecordFilter {
    pub fn new(min: Option<LogLevel>) -> Self {
        Self {
            min,
            in_record: true,
        }
    }

    pub fn accept(&mut self, line: &str) -> bool {
        let Some(min) = self.min else {
            return true;
        };
        if let Some(level) = record_level(line) {
            // Error is the lowest variant: more severe means smaller.
            self.in_record = level <= min;
        }
        self.in_record
    }
}

/// The last `n` lines of `text` that `filter` keeps.
pub fn last_lines(text: &str, n: usize, min: Option<LogLevel>) -> Vec<&str> {
    let mut filter = RecordFilter::new(min);
    let kept: Vec<&str> = text.lines().filter(|l| filter.accept(l)).collect();
    kept[kept.len().saturating_sub(n)..].to_vec()
}

/// [`last_lines`] of the file at `path`, read back from its end a block at a time
/// rather than whole (a day's log at trace level can run to gigabytes), and the offset
/// just past its last whole line, to follow it from.
pub fn tail_file(
    path: &Path,
    n: usize,
    min: Option<LogLevel>,
) -> std::io::Result<(Vec<String>, u64)> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let mut block = TAIL_BLOCK;
    loop {
        let start = len.saturating_sub(block);
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = Vec::new();
        (&mut file).take(len - start).read_to_end(&mut bytes)?;
        // From the first whole line in the block, unless it starts the file.
        let from = match (start, bytes.iter().position(|b| *b == b'\n')) {
            (0, _) => Some(0),
            (_, newline) => newline.map(|i| i + 1),
        };
        if let Some(from) = from {
            let text = String::from_utf8_lossy(&bytes[from..]);
            let lines = last_lines(&text, n, min);
            // Lines continuing a record from before the block are dropped, but they come
            // before the `n` kept, so the answer is whole.
            if lines.len() >= n || start == 0 {
                let end = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
                let lines = lines.into_iter().map(str::to_string).collect();
                return Ok((lines, start + end as u64));
            }
        }
        block = block.saturating_mul(2);
    }
}

/// The first block [`tail_file`] reads, doubled until it holds the lines asked for.
const TAIL_BLOCK: u64 = 64 * 1024;

/// How many records the in-memory buffer keeps.
pub const BUFFER_RECORDS: usize = 5_000;

/// One logged event as a viewer shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRecord {
    /// Its place in the buffer's sequence, counting every record since launch.
    pub seq: u64,
    pub time_ms: u64,
    pub level: LogLevel,
    pub target: String,
    /// The spans it was logged in, outermost first, `:`-separated (`node:…`).
    pub spans: String,
    /// The message, then its other fields as `name=value`.
    pub message: String,
}

impl LogRecord {
    /// The record as one text line, as the file has it (but for the time's precision).
    pub fn line(&self) -> String {
        let spans = if self.spans.is_empty() {
            String::new()
        } else {
            format!("{}: ", self.spans)
        };
        format!(
            "{} {:>5} {spans}{}: {}",
            crate::format::format_utc(self.time_ms),
            self.level.as_str().to_uppercase(),
            self.target,
            self.message
        )
    }
}

/// The latest [`BUFFER_RECORDS`] records, filled by the subscriber (only with what its
/// filter lets through) and read by a viewer, which asks for what it hasn't seen.
#[derive(Debug, Default)]
pub struct LogBuffer {
    inner: std::sync::Mutex<BufferInner>,
}

#[derive(Debug, Default)]
struct BufferInner {
    records: std::collections::VecDeque<LogRecord>,
    next_seq: u64,
    /// Warnings and errors logged since the last [`LogBuffer::mark_seen`].
    unseen: Unseen,
}

/// The warnings and errors logged since someone last looked ([`LogBuffer::mark_seen`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Unseen {
    pub warnings: u64,
    pub errors: u64,
    /// The latest of them.
    pub latest: Option<LogRecord>,
}

impl LogBuffer {
    pub fn push(&self, mut record: LogRecord) {
        let mut inner = self.inner.lock().expect("log buffer poisoned");
        record.seq = inner.next_seq;
        inner.next_seq += 1;
        match record.level {
            LogLevel::Error => inner.unseen.errors += 1,
            LogLevel::Warn => inner.unseen.warnings += 1,
            _ => {}
        }
        if record.level <= LogLevel::Warn {
            inner.unseen.latest = Some(record.clone());
        }
        inner.records.push_back(record);
        if inner.records.len() > BUFFER_RECORDS {
            inner.records.pop_front();
        }
    }

    /// The warnings and errors logged since the last [`LogBuffer::mark_seen`].
    pub fn unseen(&self) -> Unseen {
        self.inner
            .lock()
            .expect("log buffer poisoned")
            .unseen
            .clone()
    }

    /// Someone looked at the logs: what was logged so far is no longer new.
    pub fn mark_seen(&self) {
        self.inner.lock().expect("log buffer poisoned").unseen = Unseen::default();
    }

    /// The records kept from sequence number `from` on (older ones are gone), oldest
    /// first. Ask next from the last one's `seq + 1`.
    pub fn since(&self, from: u64) -> Vec<LogRecord> {
        let inner = self.inner.lock().expect("log buffer poisoned");
        let skip = inner
            .records
            .front()
            .map_or(0, |first| from.saturating_sub(first.seq) as usize);
        inner.records.iter().skip(skip).cloned().collect()
    }
}

impl From<&tracing::Level> for LogLevel {
    fn from(level: &tracing::Level) -> Self {
        match *level {
            tracing::Level::ERROR => LogLevel::Error,
            tracing::Level::WARN => LogLevel::Warn,
            tracing::Level::INFO => LogLevel::Info,
            tracing::Level::DEBUG => LogLevel::Debug,
            tracing::Level::TRACE => LogLevel::Trace,
        }
    }
}

/// Copies every event the filter lets through into a [`LogBuffer`].
struct BufferLayer(Arc<LogBuffer>);

impl<S> Layer<S> for BufferLayer
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_event(&self, event: &tracing::Event<'_>, ctx: tracing_subscriber::layer::Context<'_, S>) {
        use tracing_log::NormalizeEvent;
        // An event from the `log` crate carries its real target in fields.
        let normalized = event.normalized_metadata();
        let metadata = normalized.as_ref().unwrap_or_else(|| event.metadata());
        let mut fields = FieldText::default();
        event.record(&mut fields);
        let spans = ctx
            .event_scope(event)
            .map(|scope| {
                scope
                    .from_root()
                    .map(|s| s.name())
                    .collect::<Vec<_>>()
                    .join(":")
            })
            .unwrap_or_default();
        self.0.push(LogRecord {
            seq: 0,
            time_ms: crate::format::now_ms(),
            level: metadata.level().into(),
            target: metadata.target().to_string(),
            spans,
            message: fields.text(),
        });
    }
}

/// An event's fields as text: the message, then `name=value` for the others.
#[derive(Default)]
struct FieldText {
    message: String,
    others: Vec<String>,
}

impl FieldText {
    fn text(self) -> String {
        let mut text = self.message;
        for field in self.others {
            if !text.is_empty() {
                text.push(' ');
            }
            text.push_str(&field);
        }
        text
    }
}

impl tracing::field::Visit for FieldText {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        match field.name() {
            "message" => self.message = value.to_string(),
            name if name.starts_with("log.") => {}
            name => self.others.push(format!("{name}={value}")),
        }
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
        match field.name() {
            "message" => self.message = format!("{value:?}"),
            // The `log` bridge's bookkeeping (target, module, file, line).
            name if name.starts_with("log.") => {}
            name => self.others.push(format!("{name}={value:?}")),
        }
    }
}

/// How many log files of either frontend are in `dir`, and their size.
pub fn disk_usage(dir: &Path) -> std::io::Result<(usize, u64)> {
    let mut files = 0;
    let mut bytes = 0;
    for prefix in [GUI_PREFIX, CLI_PREFIX] {
        for path in log_files(dir, prefix)? {
            files += 1;
            bytes += std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        }
    }
    Ok((files, bytes))
}

/// Hands `workflow-log` messages (the Kaspa client's) to the `log` crate, whose events
/// the subscriber picks up, instead of letting workflow-log print them to stdout.
struct WorkflowSink;

impl workflow_log::Sink for WorkflowSink {
    fn write(&self, target: Option<&str>, level: log::Level, args: &fmt::Arguments<'_>) -> bool {
        let logger = log::logger();
        let metadata = log::Metadata::builder()
            .level(level)
            .target(target.unwrap_or(WORKFLOW_TARGET))
            .build();
        if logger.enabled(&metadata) {
            logger.log(
                &log::Record::builder()
                    .metadata(metadata)
                    .args(*args)
                    .build(),
            );
        }
        // Handled: workflow-log prints nothing itself.
        true
    }
}

/// Log every panic (message, thread, location, backtrace) before the default hook runs:
/// a Windows release GUI has no console to print it on.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let message = info.payload_as_str().unwrap_or("(no message)");
        let location = info.location().map(|l| l.to_string()).unwrap_or_default();
        let backtrace = std::backtrace::Backtrace::force_capture();
        tracing::error!(
            target: "x4kas::panic",
            thread = thread.name().unwrap_or("unnamed"),
            location,
            "panic: {message}\n{backtrace}"
        );
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_hands_out_what_is_new_and_keeps_the_latest() {
        let record = |message: &str| LogRecord {
            seq: 0,
            time_ms: 0,
            level: LogLevel::Info,
            target: "x4kas".into(),
            spans: String::new(),
            message: message.into(),
        };
        let buffer = LogBuffer::default();
        assert!(buffer.since(0).is_empty());
        buffer.push(record("a"));
        buffer.push(record("b"));
        let all = buffer.since(0);
        assert_eq!(
            all.iter()
                .map(|r| (r.seq, r.message.as_str()))
                .collect::<Vec<_>>(),
            [(0, "a"), (1, "b")]
        );
        assert!(buffer.since(2).is_empty());
        buffer.push(record("c"));
        assert_eq!(buffer.since(2)[0].message, "c");
        // Past the capacity the oldest go; asking from before them gets what is left.
        for i in 0..BUFFER_RECORDS {
            buffer.push(record(&i.to_string()));
        }
        let kept = buffer.since(0);
        assert_eq!(kept.len(), BUFFER_RECORDS);
        assert_eq!(kept[0].seq, 3);
        assert_eq!(buffer.since(BUFFER_RECORDS as u64 + 2).len(), 1);
    }

    #[test]
    fn buffer_counts_problems_until_seen() {
        let record = |level| LogRecord {
            seq: 0,
            time_ms: 0,
            level,
            target: "x4kas".into(),
            spans: String::new(),
            message: format!("{level}"),
        };
        let buffer = LogBuffer::default();
        buffer.push(record(LogLevel::Info));
        assert_eq!(buffer.unseen(), Unseen::default());
        buffer.push(record(LogLevel::Warn));
        buffer.push(record(LogLevel::Error));
        buffer.push(record(LogLevel::Debug));
        let unseen = buffer.unseen();
        assert_eq!((unseen.warnings, unseen.errors), (1, 1));
        assert_eq!(unseen.latest.map(|r| r.level), Some(LogLevel::Error));
        buffer.mark_seen();
        assert_eq!(buffer.unseen(), Unseen::default());
    }

    #[test]
    fn record_line_reads_like_the_file() {
        let record = LogRecord {
            seq: 7,
            time_ms: 1_791_746_462_345,
            level: LogLevel::Warn,
            target: "x4kas_core::rpc::client".into(),
            spans: "node".into(),
            message: "lost node=x".into(),
        };
        assert!(
            record
                .line()
                .ends_with(" WARN node: x4kas_core::rpc::client: lost node=x")
        );
    }

    #[test]
    fn error_changes_log_once_per_change() {
        use ErrorChange::*;
        assert_eq!(error_change(None, None), Same);
        assert_eq!(error_change(None, Some("a")), Failed("a"));
        assert_eq!(error_change(Some("a"), Some("a")), Same);
        assert_eq!(error_change(Some("a"), Some("b")), Failed("b"));
        assert_eq!(error_change(Some("a"), None), Recovered);

        let mut log = ErrorLog::default();
        let outcomes = [None, Some("x"), Some("x"), Some("y"), None, None, Some("x")];
        let changes: Vec<_> = outcomes.iter().map(|o| log.update(*o)).collect();
        assert_eq!(
            changes,
            [
                Same,
                Failed("x"),
                Same,
                Failed("y"),
                Recovered,
                Same,
                Failed("x")
            ]
        );
    }

    #[test]
    fn levels_parse_print_and_step() {
        for level in LogLevel::ALL {
            assert_eq!(level.to_string().parse::<LogLevel>(), Ok(level));
        }
        assert_eq!(" DEBUG ".parse::<LogLevel>(), Ok(LogLevel::Debug));
        assert!("verbose".parse::<LogLevel>().is_err());
        assert_eq!(LogLevel::Info.step(1), LogLevel::Debug);
        assert_eq!(LogLevel::Info.step(5), LogLevel::Trace);
        assert_eq!(LogLevel::Warn.step(-1), LogLevel::Error);
        assert_eq!(LogLevel::Warn.step(-9), LogLevel::Error);
    }

    #[test]
    fn directives_keep_dependencies_quiet() {
        assert_eq!(directives(LogLevel::Info, ""), "warn,x4kas=info");
        assert_eq!(directives(LogLevel::Trace, ""), "warn,x4kas=trace");
        assert_eq!(directives(LogLevel::Error, ""), "error,x4kas=error");
        assert_eq!(
            directives(LogLevel::Debug, " fjall=info, "),
            "warn,x4kas=debug,fjall=info"
        );
        for level in LogLevel::ALL {
            assert!(EnvFilter::try_new(directives(level, "workflow=info")).is_ok());
        }
    }

    #[test]
    fn env_overrides_in_order() {
        let none = |_: &str| None;
        assert_eq!(env_override(none), None);
        let both = |v: &str| Some(format!("{v}-value"));
        assert_eq!(
            env_override(both),
            Some(("X4KAS_LOG", "X4KAS_LOG-value".to_string()))
        );
        let rust_only = |v: &str| match v {
            "X4KAS_LOG" => Some("  ".to_string()),
            _ => Some("debug".to_string()),
        };
        assert_eq!(
            env_override(rust_only),
            Some(("RUST_LOG", "debug".to_string()))
        );
    }

    #[test]
    fn workflow_sink_swallows_messages() {
        use workflow_log::Sink;
        // Without the sink claiming it, workflow-log would print the message to stdout.
        assert!(WorkflowSink.write(None, log::Level::Info, &format_args!("hello")));
        assert!(WorkflowSink.write(Some("t"), log::Level::Trace, &format_args!("x")));
    }

    #[test]
    fn defaults_per_frontend() {
        let settings = LogSettings::default();
        let gui = LogConfig::gui(&settings);
        assert_eq!(gui.level, LogLevel::Info);
        assert!(gui.file.as_ref().is_some_and(|f| f.dir.ends_with(LOGS_DIR)
            && f.prefix == GUI_PREFIX
            && f.keep_files == DEFAULT_KEEP_DAYS as usize));
        let off = LogConfig::gui(&LogSettings {
            file: false,
            ..settings.clone()
        });
        assert!(off.file.is_none());
        let cli = LogConfig::cli(&settings);
        assert!(cli.stderr && cli.file.is_none());
        assert_eq!(cli.directives(), "warn,x4kas=info");
    }

    #[test]
    fn settings_load_each_key_alone() {
        let (s, w) = LogSettings::parse_keyed("").unwrap();
        assert_eq!(s, LogSettings::default());
        assert!(w.is_empty());
        let (s, w) = LogSettings::parse_keyed(
            "level = \"debug\"\nfilter = \"fjall=info\"\nfile = false\n\
             format = \"json\"\nkeep_days = 30\n",
        )
        .unwrap();
        assert_eq!(
            s,
            LogSettings {
                level: LogLevel::Debug,
                filter: "fjall=info".into(),
                file: false,
                format: LogFormat::Json,
                keep_days: 30,
            }
        );
        assert!(w.is_empty());
        let (s, w) = LogSettings::parse_keyed(
            "level = \"loud\"\nfilter = \"x4kas=[\"\nfile = \"yes\"\n\
             format = 3\nkeep_days = 500\n",
        )
        .unwrap();
        assert_eq!(s, LogSettings::default());
        let keys: Vec<_> = w.iter().map(|(k, _)| *k).collect();
        assert_eq!(keys, LogSettings::KEYS);
        assert!(LogSettings::parse_keyed("level = ").is_err());
    }

    #[test]
    fn edits_keep_other_keys_and_bad_text() {
        let text = "level = \"loud\"\nkeep_days = 3\nfuture_key = 1\n";
        let (s, w, out) =
            LogSettings::edit_text(text, &[LogSettingsEdit::Format(LogFormat::Json)]).unwrap();
        assert_eq!(
            (s.format, s.keep_days, s.level),
            (LogFormat::Json, 3, LogLevel::Info)
        );
        assert_eq!(w.len(), 1);
        let saved: toml::Table = toml::from_str(&out).unwrap();
        assert_eq!(saved["level"].as_str(), Some("loud"), "bad text kept");
        assert_eq!(saved["future_key"].as_integer(), Some(1));
        assert_eq!(saved["format"].as_str(), Some("json"));
        // An edit of the key settles its warning and replaces the text.
        let (s, w, out) =
            LogSettings::edit_text(&out, &[LogSettingsEdit::Level(LogLevel::Trace)]).unwrap();
        assert_eq!(s.level, LogLevel::Trace);
        assert!(w.is_empty());
        assert_eq!(LogSettings::parse_keyed(&out).unwrap(), (s, Vec::new()));
        // Round trip from empty.
        let (s, _, out) = LogSettings::edit_text(
            "",
            &[
                LogSettingsEdit::Filter(" fjall=info ".into()),
                LogSettingsEdit::KeepDays(14),
                LogSettingsEdit::File(false),
            ],
        )
        .unwrap();
        assert_eq!(s.filter, "fjall=info");
        assert_eq!(LogSettings::parse_keyed(&out).unwrap().0, s);
    }

    #[test]
    fn validators() {
        assert!(validate_filter("").is_ok());
        assert!(validate_filter("fjall=info,x4kas_core::index=trace").is_ok());
        assert!(validate_filter("x4kas=[").is_err());
        assert!(validate_keep_days(1).is_ok() && validate_keep_days(MAX_KEEP_DAYS).is_ok());
        assert!(validate_keep_days(0).is_err() && validate_keep_days(MAX_KEEP_DAYS + 1).is_err());
        assert_eq!("JSON".parse::<LogFormat>(), Ok(LogFormat::Json));
        assert!("xml".parse::<LogFormat>().is_err());
    }

    #[test]
    fn files_by_prefix_oldest_first() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "x4kas.2026-10-11.log",
            "x4kas.2026-10-09.log",
            "x4kas-cli.2026-10-11.log",
            "x4kas.toml",
            "other.2026-10-11.log",
        ] {
            std::fs::write(dir.path().join(name), "").unwrap();
        }
        let names = |prefix| {
            log_files(dir.path(), prefix)
                .unwrap()
                .iter()
                .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names(GUI_PREFIX),
            ["x4kas.2026-10-09.log", "x4kas.2026-10-11.log"]
        );
        assert_eq!(names(CLI_PREFIX), ["x4kas-cli.2026-10-11.log"]);
        assert!(
            log_files(&dir.path().join("missing"), GUI_PREFIX)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn pruning_keeps_the_other_prefix() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "x4kas.2026-10-09.log",
            "x4kas.2026-10-10.log",
            "x4kas.2026-10-11.log",
            "x4kas-cli.2026-10-01.log",
            "x4kas-cli.2026-10-02.log",
        ] {
            std::fs::write(dir.path().join(name), "").unwrap();
        }
        prune_log_files(dir.path(), GUI_PREFIX, 1);
        let mut left: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                "x4kas-cli.2026-10-01.log",
                "x4kas-cli.2026-10-02.log",
                "x4kas.2026-10-11.log"
            ]
        );
    }

    #[test]
    fn records_filter_by_level_with_their_continuations() {
        let text = "\
2026-10-11T19:01:02.123456Z  INFO x4kas::a: started
2026-10-11T19:01:03.000000Z ERROR x4kas::panic: panic: boom
   0: std::backtrace
   1: main
2026-10-11T19:01:04.000000Z DEBUG x4kas::b: detail
{\"timestamp\":\"2026-10-11T19:01:05Z\",\"level\":\"WARN\",\"message\":\"json\"}
";
        assert_eq!(record_level("   0: std::backtrace"), None);
        assert_eq!(
            record_level("2026-10-11T19:01:04.000000Z DEBUG x4kas::b: detail"),
            Some(LogLevel::Debug)
        );
        assert_eq!(last_lines(text, 100, None).len(), 6);
        assert_eq!(
            last_lines(text, 100, Some(LogLevel::Warn)),
            [
                "2026-10-11T19:01:03.000000Z ERROR x4kas::panic: panic: boom",
                "   0: std::backtrace",
                "   1: main",
                "{\"timestamp\":\"2026-10-11T19:01:05Z\",\"level\":\"WARN\",\"message\":\"json\"}",
            ]
        );
        assert_eq!(last_lines(text, 1, Some(LogLevel::Error)), ["   1: main"]);
    }

    #[test]
    fn tail_file_reads_back_from_the_end() {
        // Several blocks of records, some with continuation lines, and a half-written
        // last line.
        let mut text = String::new();
        for i in 0..20_000 {
            let level = ["INFO", "DEBUG", "WARN", "ERROR"][i % 4];
            text.push_str(&format!(
                "2026-10-11T19:01:02Z {level:>5} x4kas::a: line {i}\n"
            ));
            if i % 7 == 0 {
                text.push_str(&format!("   continued {i}\n"));
            }
        }
        let whole = text.len() as u64;
        text.push_str("2026-10-11T19:01:03Z  INFO x4kas::a: half");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x4kas.2026-10-11.log");
        std::fs::write(&path, &text).unwrap();
        for (n, min) in [
            (0, None),
            (10, None),
            (5_000, Some(LogLevel::Error)),
            (100_000, Some(LogLevel::Warn)),
        ] {
            let (lines, end) = tail_file(&path, n, min).unwrap();
            assert_eq!(lines, last_lines(&text, n, min), "n={n} min={min:?}");
            assert_eq!(end, whole);
        }
    }
}
