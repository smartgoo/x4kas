pub mod rpc;

use clap::{Parser, Subcommand};

use crate::cli::rpc::RpcCall;

#[derive(Parser, Debug, Clone)]
#[command(name = "x4kas", version, about = "Desktop monitor for Kaspa L1")]
pub struct CliArgs {
    /// wRPC endpoint URL (e.g., ws://127.0.0.1:17110).
    /// If omitted, the GUI lets you choose a connection and CLI commands use the public resolver.
    #[arg(short, long, global = true)]
    pub url: Option<String>,

    /// Network: mainnet, testnet-10, testnet-11
    #[arg(short, long, default_value = "mainnet", global = true)]
    pub network: String,

    /// Auto-refresh interval in milliseconds
    #[arg(short = 'r', long, default_value = "1000")]
    pub refresh_interval_ms: u64,

    /// Run a command and exit instead of opening the GUI.
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum Command {
    /// Run a read-only RPC method (the RPC Cmds tab) and print its JSON response
    Rpc {
        /// Connect timeout in seconds
        #[arg(short, long, default_value = "20", global = true)]
        timeout: u64,

        #[command(subcommand)]
        call: RpcCall,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_defaults() {
        let args = CliArgs::parse_from(["x4kas"]);
        assert_eq!(args.url, None);
        assert_eq!(args.network, "mainnet");
        assert_eq!(args.refresh_interval_ms, 1000);
        assert!(args.command.is_none());
    }

    #[test]
    fn cli_custom_values() {
        let args = CliArgs::parse_from([
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
    fn cli_short_flags() {
        let args = CliArgs::parse_from([
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
