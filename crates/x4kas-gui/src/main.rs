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
}

fn main() -> Result<()> {
    let args = Args::parse();

    // Built manually (not #[tokio::main]) so the GUI can own the main thread, which
    // must stay outside the runtime context for `RwLock::blocking_*` to work.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    gui::run(&rt, args)
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
