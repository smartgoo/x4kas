mod analytics;
mod analytics_streaming;
mod app;
mod cli;
mod config;
mod controller;
mod format;
mod gui;
mod polling;
mod rpc;
mod tx_inspect;

use std::time::Duration;

use anyhow::Result;
use clap::Parser;

use crate::cli::{CliArgs, Command};

fn main() -> Result<()> {
    let args = CliArgs::parse();

    // Built manually (not #[tokio::main]) so the GUI can own the main thread, which
    // must stay outside the runtime context for `RwLock::blocking_*` to work.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    match args.command.clone() {
        Some(Command::Rpc { timeout, call }) => rt.block_on(cli::rpc::run(
            args.url.as_deref(),
            &args.network,
            Duration::from_secs(timeout),
            call,
        )),
        None => gui::run(&rt, args),
    }
}
