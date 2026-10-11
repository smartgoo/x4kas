//! `x4kas-cli logs …`: the logging settings both frontends read at launch
//! (`~/.x4kas/logging.toml`), where the log files are, and the end of the newest one.

use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, bail};
use clap::Subcommand;
use x4kas_core::logging::{
    self, CLI_PREFIX, ENV_VARS, GUI_PREFIX, LogFormat, LogLevel, LogSettings, LogSettingsEdit,
    RecordFilter,
};

use crate::index::parse_on_off;

#[derive(Subcommand, Debug, Clone, PartialEq)]
pub enum LogsCommand {
    /// Show or change the logging settings. They apply to the CLI from its next command
    /// and to the GUI from its next launch; the CLI's --log-level, -v and -q override the
    /// level for one run, and X4KAS_LOG or RUST_LOG (filter directives) override them all
    Settings {
        /// How much x4kas logs: error, warn, info (the default), debug or trace.
        /// Dependencies log warnings and errors only, unless --filter says otherwise
        #[arg(long)]
        level: Option<LogLevel>,
        /// Extra filter directives after the level's, e.g. `x4kas_core::index=debug` or
        /// `fjall=info` (https://docs.rs/tracing-subscriber, EnvFilter); "" clears them
        #[arg(long, value_parser = parse_filter)]
        filter: Option<String>,
        /// Whether the GUI writes log files: on|off (the CLI's are --log-file)
        #[arg(long, value_parser = parse_on_off)]
        file: Option<bool>,
        /// How each line is written: text or json (one object per line)
        #[arg(long)]
        format: Option<LogFormat>,
        /// How many daily log files are kept (1 to 90; default 7)
        #[arg(long, value_parser = parse_keep_days)]
        keep_days: Option<u32>,
    },
    /// Print the directory the log files are in
    Path,
    /// Print the end of the newest log file (the GUI's unless --cli)
    Tail {
        /// How many lines
        #[arg(long, default_value = "50")]
        lines: usize,
        /// Keep printing lines as they are written, until Ctrl+C
        #[arg(short, long)]
        follow: bool,
        /// Only records at this level or more severe
        #[arg(long)]
        level: Option<LogLevel>,
        /// The CLI's log files (--log-file) instead of the GUI's
        #[arg(long)]
        cli: bool,
    },
}

fn parse_filter(s: &str) -> Result<String, String> {
    logging::validate_filter(s).map(|()| s.trim().to_string())
}

fn parse_keep_days(s: &str) -> Result<u32, String> {
    let days = s
        .parse::<u32>()
        .map_err(|_| format!("expected a number of days, not \"{s}\""))?;
    logging::validate_keep_days(days).map(|()| days)
}

pub async fn run(cmd: LogsCommand) -> Result<()> {
    match cmd {
        LogsCommand::Settings {
            level,
            filter,
            file,
            format,
            keep_days,
        } => {
            let edits: Vec<LogSettingsEdit> = [
                level.map(LogSettingsEdit::Level),
                filter.map(LogSettingsEdit::Filter),
                file.map(LogSettingsEdit::File),
                format.map(LogSettingsEdit::Format),
                keep_days.map(LogSettingsEdit::KeepDays),
            ]
            .into_iter()
            .flatten()
            .collect();
            settings(&edits)
        }
        LogsCommand::Path => {
            println!("{}", logging::logs_dir().display());
            Ok(())
        }
        LogsCommand::Tail {
            lines,
            follow,
            level,
            cli,
        } => {
            tail(
                if cli { CLI_PREFIX } else { GUI_PREFIX },
                lines,
                follow,
                level,
            )
            .await
        }
    }
}

/// Show the logging settings, after applying `edits`.
fn settings(edits: &[LogSettingsEdit]) -> Result<()> {
    let path = LogSettings::path();
    let (before, warnings) = LogSettings::load_with_warnings()?;
    // A key an edit sets no longer has the value warned about (`main` leaves the
    // warnings of this command to it).
    let settles = |key: &str| edits.iter().any(|e| e.key() == key);
    for (_, w) in warnings.iter().filter(|(key, _)| !settles(key)) {
        eprintln!("{}: {w}", path.display());
    }
    let mut s = before.clone();
    for edit in edits {
        s.apply(edit.clone());
    }
    // Also save when a key set had a value that didn't load, so the file stops warning.
    let save = s != before || warnings.iter().any(|(key, _)| settles(key));
    if save {
        // Applied to the file as it is now, under its lock, so a GUI saving at the same
        // moment loses nothing.
        s = LogSettings::update_file(edits)?.0;
    }
    let out = serde_json::json!({
        "level": s.level.as_str(),
        "filter": s.filter,
        "file": s.file,
        "format": s.format.as_str(),
        "keep_days": s.keep_days,
        "directives": s.directives(),
        "dir": logging::logs_dir(),
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    if save {
        eprintln!(
            "saved {}: the GUI applies it from its next launch",
            path.display()
        );
    }
    if let Some(var) = ENV_VARS
        .into_iter()
        .find(|v| std::env::var(v).is_ok_and(|v| !v.trim().is_empty()))
    {
        eprintln!("{var} is set: it replaces these settings' level and filter");
    }
    Ok(())
}

/// How often `tail --follow` looks for new lines.
const FOLLOW_POLL: Duration = Duration::from_millis(500);

/// Print the last `n` lines of the newest `prefix` file, then, with `follow`, what is
/// written after them, moving on to the next day's file when it appears.
async fn tail(prefix: &'static str, n: usize, follow: bool, min: Option<LogLevel>) -> Result<()> {
    let dir = logging::logs_dir();
    let newest = || -> Result<Option<PathBuf>> { Ok(logging::log_files(&dir, prefix)?.pop()) };
    let Some(path) = newest()? else {
        if !follow {
            bail!("no {prefix} log files in {}", dir.display());
        }
        eprintln!("waiting for {prefix} log files in {}…", dir.display());
        loop {
            tokio::time::sleep(FOLLOW_POLL).await;
            if let Some(path) = newest()? {
                return follow_from(path, 0, min, newest).await;
            }
        }
    };
    let (lines, read) = logging::tail_file(&path, n, min)?;
    for line in lines {
        println!("{line}");
    }
    if follow {
        // From the end of the last whole line, so a half-written one is printed whole.
        follow_from(path, read, min, newest).await?;
    }
    Ok(())
}

/// Print the whole lines written to `path` after byte `pos`, and to the newer files that
/// appear, until interrupted.
async fn follow_from(
    mut path: PathBuf,
    mut pos: u64,
    min: Option<LogLevel>,
    newest: impl Fn() -> Result<Option<PathBuf>>,
) -> Result<()> {
    let mut filter = RecordFilter::new(min);
    loop {
        let mut file = std::fs::File::open(&path)?;
        if file.metadata()?.len() < pos {
            pos = 0; // truncated or replaced
        }
        file.seek(SeekFrom::Start(pos))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let end = bytes.iter().rposition(|b| *b == b'\n');
        if let Some(end) = end {
            for line in String::from_utf8_lossy(&bytes[..end]).lines() {
                if filter.accept(line) {
                    println!("{line}");
                }
            }
            pos += end as u64 + 1;
        }
        match newest()? {
            // Move on to the next day's file once this one has nothing more to print.
            Some(next) if next != path && end.is_none() => {
                path = next;
                pos = 0;
            }
            _ => tokio::time::sleep(FOLLOW_POLL).await,
        }
    }
}
