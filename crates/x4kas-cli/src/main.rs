//! `x4kas-cli`: headless access to a Kaspa node. Each command connects, prints its
//! result to stdout and exits, with a nonzero exit code on error.

mod address;
mod index;
mod labels;
mod logs;
mod query;
mod rpc;
mod watch;
mod watchlist;

use std::time::Duration;

use anyhow::Result;
use clap::{ArgAction, Parser, Subcommand};
use x4kas_core::logging::{self, CLI_PREFIX, LogConfig, LogFormat, LogLevel, LogSettings};

use crate::address::AddressCommand;
use crate::index::IndexCommand;
use crate::labels::LabelsCommand;
use crate::logs::LogsCommand;
use crate::query::QueryCommand;
use crate::rpc::RpcCall;
use crate::watch::WatchArgs;
use crate::watchlist::WatchlistCommand;

#[derive(Parser, Debug, Clone)]
#[command(
    name = "x4kas-cli",
    version,
    about = "Command-line access to Kaspa L1 nodes"
)]
pub struct Args {
    /// wRPC endpoint URL (e.g., ws://127.0.0.1:17110). If omitted, use the public resolver.
    #[arg(short, long, global = true)]
    pub url: Option<String>,

    /// Network: mainnet, testnet-10, testnet-11
    #[arg(short, long, default_value = "mainnet", global = true)]
    pub network: String,

    /// Connect timeout in seconds
    #[arg(short, long, default_value = "20", global = true)]
    pub timeout: u64,

    /// Log level for this run: error, warn, info, debug or trace (default: `logs
    /// settings`, info). Logs go to stderr, never stdout
    #[arg(long, global = true, value_name = "LEVEL")]
    pub log_level: Option<LogLevel>,

    /// Log more: -v one level up from the configured one, -vv two
    #[arg(short, long, action = ArgAction::Count, global = true, conflicts_with = "log_quiet")]
    pub verbose: u8,

    /// Log less: -q one level down, -qq two (short only: `query run --quiet` is about
    /// its progress line)
    #[arg(short = 'q', action = ArgAction::Count, global = true)]
    pub log_quiet: u8,

    /// Also log to ~/.x4kas/logs/x4kas-cli.<date>.log (kept as `logs settings` says)
    #[arg(long, global = true)]
    pub log_file: bool,

    /// Log line format for this run: text or json
    #[arg(long, global = true, value_name = "FORMAT")]
    pub log_format: Option<LogFormat>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug, Clone)]
pub enum Command {
    /// Run a read-only RPC method (the GUI's RPC Cmds tab) and print its JSON response
    Rpc {
        #[command(subcommand)]
        call: RpcCall,
    },
    /// Query the address index (built by the GUI or `index run`); needs no node
    Address {
        #[command(subcommand)]
        cmd: AddressCommand,
    },
    /// Build the address index headlessly, or show what it holds
    Index {
        #[command(subcommand)]
        cmd: IndexCommand,
    },
    /// Follow addresses live: one JSON line per balance change and alert, until Ctrl+C
    Watch(WatchArgs),
    /// The named watchlists: list, create, rename, delete, add, remove and move addresses
    Watchlist {
        #[command(subcommand)]
        cmd: WatchlistCommand,
    },
    /// Logging: the settings, where the files are, and the end of the newest
    Logs {
        #[command(subcommand)]
        cmd: LogsCommand,
    },
    /// Address labels: yours and the public api.kaspa.org list
    Labels {
        #[command(subcommand)]
        cmd: LabelsCommand,
    },
    /// Run, save and explain queries over the address index; needs no node
    #[command(after_help = query::AFTER_HELP)]
    Query {
        #[command(subcommand)]
        cmd: QueryCommand,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    // To stderr, never stdout (the command's output); kept until `main` returns.
    let _logs = init_logging(&args);
    let url = args.url.as_deref();
    let timeout = Duration::from_secs(args.timeout);
    match args.command {
        Command::Rpc { call } => rpc::run(url, &args.network, timeout, call).await,
        Command::Address { cmd } => address::run(&args.network, cmd),
        Command::Index { cmd } => index::run(url, &args.network, cmd).await,
        Command::Watch(watch_args) => watch::run(url, &args.network, watch_args).await,
        Command::Watchlist { cmd } => watchlist::run(&args.network, cmd),
        Command::Labels { cmd } => labels::run(cmd).await,
        Command::Logs { cmd } => logs::run(cmd).await,
        Command::Query { cmd } => query::run(url, &args.network, cmd).await,
    }
}

/// Log as `logs settings` says, with this run's flags over it.
fn init_logging(args: &Args) -> Option<logging::LogRuntime> {
    let (settings, warnings) = match LogSettings::load_with_warnings() {
        Ok(loaded) => loaded,
        Err(e) => (
            LogSettings::default(),
            vec![("", format!("{e:#}: using defaults"))],
        ),
    };
    let logs = match logging::init(&log_config(args, &settings)) {
        Ok(logs) => logs,
        Err(e) => {
            eprintln!("logging disabled: {e:#}");
            return None;
        }
    };
    tracing::debug!(version = env!("CARGO_PKG_VERSION"), filter = ?logs.source(), "x4kas-cli starting");
    // `logs settings` says itself which of them it fixes.
    if !matches!(
        args.command,
        Command::Logs {
            cmd: LogsCommand::Settings { .. }
        }
    ) {
        for (_, warning) in warnings {
            tracing::warn!("{}: {warning}", LogSettings::path().display());
        }
    }
    Some(logs)
}

/// The CLI's logging: the settings, then `--log-level`, stepped by `-v`/`-q`, and the
/// file and format flags.
fn log_config(args: &Args, settings: &LogSettings) -> LogConfig {
    let mut config = LogConfig::cli(settings);
    let level = args.log_level.unwrap_or(settings.level);
    config.level = level.step(i32::from(args.verbose) - i32::from(args.log_quiet));
    if let Some(format) = args.log_format {
        config.format = format;
    }
    if args.log_file {
        config.file = Some(settings.file_log(CLI_PREFIX));
    }
    config
}

#[cfg(test)]
mod tests {
    use super::*;
    use x4kas_core::config::Retention;

    #[test]
    fn log_flags_override_the_settings() {
        let settings = LogSettings {
            level: LogLevel::Warn,
            keep_days: 3,
            ..LogSettings::default()
        };
        let config = |argv: &[&str]| {
            let mut full = vec!["x4kas-cli"];
            full.extend_from_slice(argv);
            log_config(&Args::parse_from(full), &settings)
        };
        let c = config(&["rpc", "ping"]);
        assert_eq!((c.level, c.format), (LogLevel::Warn, LogFormat::Text));
        assert!(c.stderr && c.file.is_none());
        assert_eq!(config(&["-v", "rpc", "ping"]).level, LogLevel::Info);
        assert_eq!(config(&["rpc", "ping", "-vv"]).level, LogLevel::Debug);
        assert_eq!(config(&["-qqq", "rpc", "ping"]).level, LogLevel::Error);
        let c = config(&["--log-level", "trace", "-q", "rpc", "ping"]);
        assert_eq!(c.level, LogLevel::Debug);
        let c = config(&["rpc", "ping", "--log-file", "--log-format", "json"]);
        assert_eq!(c.format, LogFormat::Json);
        let file = c.file.unwrap();
        assert_eq!((file.prefix, file.keep_files), (CLI_PREFIX, 3));
        assert!(Args::try_parse_from(["x4kas-cli", "-v", "-q", "rpc", "ping"]).is_err());
        // `query run --quiet` (its progress) is not the global -q.
        let args = Args::parse_from(["x4kas-cli", "query", "run", "tx last 1h", "--quiet", "-q"]);
        assert_eq!(args.log_quiet, 1);
        assert!(Args::try_parse_from(["x4kas-cli", "--log-level", "loud", "rpc", "ping"]).is_err());
    }

    #[test]
    fn logs_args() {
        let args = Args::parse_from([
            "x4kas-cli",
            "logs",
            "settings",
            "--level",
            "debug",
            "--file",
            "off",
            "--keep-days",
            "14",
            "--filter",
            " fjall=info ",
        ]);
        let Command::Logs { cmd } = args.command else {
            panic!("not logs");
        };
        assert_eq!(
            cmd,
            LogsCommand::Settings {
                level: Some(LogLevel::Debug),
                filter: Some("fjall=info".into()),
                file: Some(false),
                format: None,
                keep_days: Some(14),
            }
        );
        for bad in [
            ["logs", "settings", "--keep-days", "0"],
            ["logs", "settings", "--filter", "x4kas=["],
            ["logs", "settings", "--format", "xml"],
        ] {
            let mut argv = vec!["x4kas-cli"];
            argv.extend(bad);
            assert!(Args::try_parse_from(argv).is_err(), "{bad:?}");
        }
        let args = Args::parse_from(["x4kas-cli", "logs", "tail", "-f", "--level", "warn"]);
        assert!(matches!(
            args.command,
            Command::Logs {
                cmd: LogsCommand::Tail {
                    lines: 50,
                    follow: true,
                    level: Some(LogLevel::Warn),
                    cli: false
                }
            }
        ));
    }

    #[test]
    fn args_defaults() {
        let args = Args::parse_from(["x4kas-cli", "rpc", "ping"]);
        assert_eq!(args.url, None);
        assert_eq!(args.network, "mainnet");
        assert_eq!(args.timeout, 20);
    }

    #[test]
    fn watch_and_watchlist_args() {
        let args = Args::parse_from(["x4kas-cli", "watch", "--list", "A", "--list", "B"]);
        let Command::Watch(watch) = args.command else {
            panic!("not watch");
        };
        assert_eq!(watch.lists, vec!["A", "B"]);
        assert!(watch.addresses.is_empty());
        let args = Args::parse_from([
            "x4kas-cli",
            "watchlist",
            "add",
            "Cold",
            "kaspa:a",
            "kaspa:b",
        ]);
        assert!(matches!(
            args.command,
            Command::Watchlist {
                cmd: WatchlistCommand::Add { ref list, ref addresses }
            } if list == "Cold" && addresses.len() == 2
        ));
        assert!(Args::try_parse_from(["x4kas-cli", "watchlist", "add", "Cold"]).is_err());
        let args = Args::parse_from(["x4kas-cli", "watchlist", "move", "kaspa:a", "A", "B"]);
        assert!(matches!(
            args.command,
            Command::Watchlist {
                cmd: WatchlistCommand::Move { .. }
            }
        ));
    }

    #[test]
    fn index_settings_args() {
        let args = Args::parse_from(["x4kas-cli", "index", "settings", "--slab-hours", "12"]);
        assert!(matches!(
            args.command,
            Command::Index {
                cmd: IndexCommand::Settings {
                    slab_hours: Some(12),
                    ..
                }
            }
        ));
        for bad in ["0", "5", "48", "six"] {
            assert!(
                Args::try_parse_from(["x4kas-cli", "index", "settings", "--slab-hours", bad])
                    .is_err()
            );
        }
        let args = Args::parse_from(["x4kas-cli", "index", "settings", "--retention", "7d"]);
        assert!(matches!(
            args.command,
            Command::Index {
                cmd: IndexCommand::Settings {
                    retention: Some(Retention::Days(7)),
                    ..
                }
            }
        ));
        for bad in ["0d", "week", "forever"] {
            assert!(
                Args::try_parse_from(["x4kas-cli", "index", "settings", "--retention", bad])
                    .is_err()
            );
        }
        let args = Args::parse_from(["x4kas-cli", "index", "run", "--resync"]);
        assert!(matches!(
            args.command,
            Command::Index {
                cmd: IndexCommand::Run {
                    backfill_hours: None,
                    ..
                }
            }
        ));
        assert!(matches!(
            args.command,
            Command::Index {
                cmd: IndexCommand::Run { resync: true, .. }
            }
        ));
    }

    #[test]
    fn requires_a_command() {
        assert!(Args::try_parse_from(["x4kas-cli"]).is_err());
    }

    #[test]
    fn args_definition_is_valid() {
        <Args as clap::CommandFactory>::command().debug_assert();
    }
}
