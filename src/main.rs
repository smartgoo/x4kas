mod analytics;
mod analytics_streaming;
mod app;
mod cli;
mod config;
mod controller;
mod daemon;
mod daemon_lifecycle;
mod format;
mod gui;
mod rpc;
mod tx_inspect;

use anyhow::Result;
use clap::Parser;

use crate::cli::CliArgs;
use crate::config::DaemonConfig;

fn main() -> Result<()> {
    let args = CliArgs::parse();

    // Load daemon config (always, for tab state initialization)
    let daemon_config = DaemonConfig::load().unwrap_or_default();

    // Built manually (not #[tokio::main]) so the GUI can own the main thread, which
    // must stay outside the runtime context for `RwLock::blocking_*` to work.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    gui::run(&rt, args, daemon_config)
}
