// Release builds on Windows open no console window next to the GUI.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod gui;

use anyhow::Result;
use clap::Parser;

#[derive(Parser, Debug, Clone)]
#[command(name = "x4kas", version, about = "Desktop monitor for Kaspa L1")]
pub struct Args {
    /// wRPC endpoint URL (e.g., ws://127.0.0.1:17110).
    /// If omitted, choose a connection (URL or public resolver) in the app.
    #[arg(short, long)]
    pub url: Option<String>,

    /// Network: mainnet, testnet-10, testnet-11
    #[arg(short, long, default_value = "mainnet")]
    pub network: String,

    /// Auto-refresh interval in milliseconds
    #[arg(short = 'r', long, default_value = "1000")]
    pub refresh_interval_ms: u64,

    /// Set by a reset from Settings when it relaunches the app: what went wrong, shown
    /// on the Settings page's Reset section.
    #[arg(long, hide = true)]
    pub reset_report: Option<String>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    // Kept to the end of `main`: dropping it flushes and stops the log file's writer.
    let _logs = init_logging();

    // Built manually (not #[tokio::main]) so the GUI can own the main thread, which
    // must stay outside the runtime context for `RwLock::blocking_*` to work.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    match gui::run(&rt, args.clone(), _logs.as_ref())? {
        gui::Exit::Quit => Ok(()),
        gui::Exit::Reset => {
            // Stop what still runs in the background (the label and market refreshes)
            // so nothing writes into the data directory while it is deleted. The index
            // writer and query runs are already done: a reset only gets here after the
            // controller's shutdown finished.
            rt.shutdown_timeout(std::time::Duration::from_secs(2));
            // Relaunch whatever happened: a failure is reported by the new window (a
            // Windows release build has no console to print it on).
            let report = x4kas_core::config::reset_data_dir().err();
            match &report {
                None => tracing::info!("reset the data directory, relaunching"),
                Some(e) => tracing::error!("reset failed, relaunching: {e}"),
            }
            relaunch(&args, report)
        }
    }
}

/// Log as `~/.x4kas/logging.toml` says: to `~/.x4kas/logs/` (and stderr in debug
/// builds). A failure only means no logs: the app runs anyway.
fn init_logging() -> Option<x4kas_core::logging::LogRuntime> {
    use x4kas_core::logging::{self, LogConfig, LogSettings};
    let (settings, warnings) = match LogSettings::load_with_warnings() {
        Ok(loaded) => loaded,
        Err(e) => (
            LogSettings::default(),
            vec![("", format!("{e:#}: using defaults"))],
        ),
    };
    match logging::init(&LogConfig::gui(&settings)) {
        Ok(logs) => {
            tracing::info!(
                version = env!("CARGO_PKG_VERSION"),
                os = std::env::consts::OS,
                arch = std::env::consts::ARCH,
                filter = ?logs.source(),
                "x4kas starting"
            );
            for (_, warning) in warnings {
                tracing::warn!("{}: {warning}", LogSettings::path().display());
            }
            Some(logs)
        }
        Err(e) => {
            eprintln!("logging disabled: {e:#}");
            None
        }
    }
}

/// Start the app again as a new process, with the arguments it was given (but an
/// earlier reset's report) and `report`, what went wrong with this reset.
fn relaunch(args: &Args, report: Option<String>) -> Result<()> {
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    if let Some(url) = &args.url {
        cmd.args(["--url", url]);
    }
    cmd.args(["--network", &args.network]);
    cmd.args([
        "--refresh-interval-ms".to_string(),
        args.refresh_interval_ms.to_string(),
    ]);
    if let Some(report) = report {
        cmd.args(["--reset-report", &report]);
    }
    cmd.spawn()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_defaults() {
        let args = Args::parse_from(["x4kas"]);
        assert_eq!(args.url, None);
        assert_eq!(args.network, "mainnet");
        assert_eq!(args.refresh_interval_ms, 1000);
        assert_eq!(args.reset_report, None);
    }

    #[test]
    fn args_custom_values() {
        let args = Args::parse_from([
            "x4kas",
            "--url",
            "ws://127.0.0.1:17110",
            "--network",
            "testnet-10",
            "--refresh-interval-ms",
            "500",
        ]);
        assert_eq!(args.url, Some("ws://127.0.0.1:17110".to_string()));
        assert_eq!(args.network, "testnet-10");
        assert_eq!(args.refresh_interval_ms, 500);
    }

    #[test]
    fn args_short_flags() {
        let args = Args::parse_from([
            "x4kas",
            "-u",
            "ws://localhost:17110",
            "-n",
            "testnet-11",
            "-r",
            "2000",
        ]);
        assert_eq!(args.url, Some("ws://localhost:17110".to_string()));
        assert_eq!(args.network, "testnet-11");
        assert_eq!(args.refresh_interval_ms, 2000);
    }
}
