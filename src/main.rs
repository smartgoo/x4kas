mod analytics;
mod analytics_streaming;
mod app;
mod cli;
mod config;
mod controller;
mod daemon;
mod daemon_lifecycle;
mod event;
mod format;
mod gui;
mod keys;
mod rpc;
mod tui;
mod ui;

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

    if args.tui {
        return rt.block_on(tui::run(args, daemon_config));
    }

    gui::run(&rt, args, daemon_config)
}
