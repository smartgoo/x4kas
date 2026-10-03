//! `x4kas-cli`: headless access to a Kaspa node. Each command connects, prints its
//! result to stdout and exits, with a nonzero exit code on error.

mod rpc;

use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::rpc::RpcCall;

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
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let url = args.url.as_deref();
    let timeout = Duration::from_secs(args.timeout);
    match args.command {
        Command::Rpc { call } => rpc::run(url, &args.network, timeout, call).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_defaults() {
        let args = Args::parse_from(["x4kas-cli", "rpc", "ping"]);
        assert_eq!(args.url, None);
        assert_eq!(args.network, "mainnet");
        assert_eq!(args.timeout, 20);
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
