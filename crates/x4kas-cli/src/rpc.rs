//! `x4kas-cli rpc <method> [args…]`: run one of the read-only RPC methods from the RPC Cmds
//! tab and print its JSON response. Subcommands are generated from `RPC_METHODS`, so the
//! CLI and the GUI always expose the same methods with the same parameters and defaults.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use clap::{Arg, ArgAction, ArgMatches, Command, FromArgMatches, Subcommand};
use tokio::sync::RwLock;
use tokio::time::Instant;

use x4kas_core::app::App;
use x4kas_core::rpc::client::RpcManager;
use x4kas_core::rpc::methods::{self, ParamKind, RPC_METHODS, RpcMethod, RpcParam};

/// A parsed `rpc` subcommand: the method name and its arguments as entered (lists
/// joined with commas), ready for `RpcManager::rpc_json`.
#[derive(Debug, Clone, PartialEq)]
pub struct RpcCall {
    pub method: String,
    pub args: Vec<String>,
}

impl FromArgMatches for RpcCall {
    fn from_arg_matches(matches: &ArgMatches) -> Result<Self, clap::Error> {
        let (name, sub) = matches
            .subcommand()
            .ok_or_else(|| clap::Error::new(clap::error::ErrorKind::MissingSubcommand))?;
        let spec = methods::find(name).ok_or_else(|| {
            clap::Error::raw(
                clap::error::ErrorKind::InvalidSubcommand,
                format!("unknown RPC method '{name}'\n"),
            )
        })?;
        let args = spec
            .params
            .iter()
            .map(|p| {
                sub.get_many::<String>(p.name)
                    .map(|v| v.cloned().collect::<Vec<_>>().join(","))
                    .unwrap_or_default()
            })
            .collect();
        Ok(Self {
            method: name.to_string(),
            args,
        })
    }

    fn update_from_arg_matches(&mut self, matches: &ArgMatches) -> Result<(), clap::Error> {
        *self = Self::from_arg_matches(matches)?;
        Ok(())
    }
}

impl Subcommand for RpcCall {
    fn augment_subcommands(cmd: Command) -> Command {
        RPC_METHODS
            .iter()
            .fold(cmd, |cmd, m| cmd.subcommand(method_command(m)))
            .subcommand_required(true)
    }

    fn augment_subcommands_for_update(cmd: Command) -> Command {
        Self::augment_subcommands(cmd)
    }

    fn has_subcommand(name: &str) -> bool {
        methods::find(name).is_some()
    }
}

fn method_command(m: &'static RpcMethod) -> Command {
    let last = m.params.len().saturating_sub(1);
    m.params
        .iter()
        .enumerate()
        .fold(Command::new(m.name).about(m.description), |cmd, (i, p)| {
            cmd.arg(param_arg(p, i == last))
        })
}

/// A positional argument. A list parameter in the last position also takes its items as
/// separate words (`get_balances_by_addresses kaspa:a kaspa:b`).
fn param_arg(p: &'static RpcParam, is_last: bool) -> Arg {
    let is_list = matches!(p.kind, ParamKind::Addresses | ParamKind::Numbers);
    let help = match p.kind {
        ParamKind::Choice(opts) => format!("one of: {}", opts.join(", ")),
        ParamKind::Addresses | ParamKind::Numbers => format!("{} (comma-separated)", p.kind.hint()),
        kind => kind.hint().to_string(),
    };
    let mut arg = Arg::new(p.name)
        .help(help)
        .required(p.default.is_none())
        .action(ArgAction::Set);
    if is_list && is_last {
        arg = arg.num_args(1..).action(ArgAction::Append);
    }
    if let Some(d) = p.default.filter(|d| !d.is_empty()) {
        arg = arg.default_value(d);
    }
    arg
}

/// Each connection attempt through the resolver gets this long before trying another
/// node, since the resolver occasionally hands out one that never answers.
const RESOLVER_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(3);

/// Connect (to `url`, or through the public resolver), run the call and print the full
/// JSON response to stdout. Arguments are checked before connecting.
pub async fn run(url: Option<&str>, network: &str, timeout: Duration, call: RpcCall) -> Result<()> {
    let spec = methods::find(&call.method)
        .ok_or_else(|| anyhow!("unknown RPC method '{}'", call.method))?;
    let args = spec.validate_args(&call.args)?;
    let rpc = connect(url.map(str::trim), network, timeout).await?;
    let result = rpc.rpc_json(&call.method, &args).await;
    let _ = rpc.disconnect().await;
    println!("{}", result?);
    Ok(())
}

/// Connect within `timeout`: a single attempt to a URL, or repeated attempts through the
/// resolver (each picks a node anew).
async fn connect(url: Option<&str>, network: &str, timeout: Duration) -> Result<RpcManager> {
    let target = url.unwrap_or("the public resolver");
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let attempt = match url {
            Some(_) => remaining,
            None => remaining.min(RESOLVER_ATTEMPT_TIMEOUT),
        };
        let rpc = RpcManager::new(url, network, Arc::new(RwLock::new(App::default())))?;
        let err = match tokio::time::timeout(attempt, rpc.connect_once(attempt)).await {
            Ok(Ok(())) => return Ok(rpc),
            Ok(Err(e)) => anyhow!("failed to connect to {target}: {e}"),
            Err(_) => anyhow!("timed out connecting to {target}"),
        };
        if url.is_some() || Instant::now() >= deadline {
            return Err(err);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Args, Command as CliCommand};
    use clap::Parser;

    const HASH: &str = "0000000000000000000000000000000000000000000000000000000000000001";
    const ADDR: &str = "kaspa:qpauqsvk7yf9unexwmxsnmg547mhyga37csh0kj53q6xxgl24ydxjsgzthw5j";

    fn parse(argv: &[&str]) -> Result<RpcCall, clap::Error> {
        let args = Args::try_parse_from(std::iter::once("x4kas-cli").chain(argv.iter().copied()))?;
        match args.command {
            CliCommand::Rpc { call } => Ok(call),
        }
    }

    fn call(method: &str, args: &[&str]) -> RpcCall {
        RpcCall {
            method: method.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn every_gui_method_is_a_cli_subcommand() {
        let cmd = <Args as clap::CommandFactory>::command();
        let rpc = cmd.find_subcommand("rpc").unwrap();
        for m in RPC_METHODS {
            let sub = rpc
                .find_subcommand(m.name)
                .unwrap_or_else(|| panic!("{} missing from CLI", m.name));
            assert_eq!(sub.get_arguments().count(), m.params.len(), "{}", m.name);
        }
        assert_eq!(rpc.get_subcommands().count(), RPC_METHODS.len());
    }

    #[test]
    fn parses_no_arg_method() {
        assert_eq!(parse(&["rpc", "get_info"]).unwrap(), call("get_info", &[]));
    }

    #[test]
    fn fills_defaults() {
        assert_eq!(
            parse(&["rpc", "get_block", HASH]).unwrap(),
            call("get_block", &[HASH, "true"])
        );
        assert_eq!(
            parse(&["rpc", "get_virtual_chain_from_block_v2", HASH]).unwrap(),
            call("get_virtual_chain_from_block_v2", &[HASH, "none", ""])
        );
    }

    #[test]
    fn overrides_defaults_positionally() {
        assert_eq!(
            parse(&["rpc", "get_blocks", HASH, "true", "false"]).unwrap(),
            call("get_blocks", &[HASH, "true", "false"])
        );
    }

    #[test]
    fn trailing_list_takes_several_words() {
        assert_eq!(
            parse(&["rpc", "get_daa_score_timestamp_estimate", "1", "2", "3"]).unwrap(),
            call("get_daa_score_timestamp_estimate", &["1,2,3"])
        );
        assert_eq!(
            parse(&["rpc", "get_balances_by_addresses", ADDR, ADDR]).unwrap(),
            call("get_balances_by_addresses", &[&format!("{ADDR},{ADDR}")])
        );
    }

    #[test]
    fn rejects_missing_required_and_unknown_methods() {
        assert!(parse(&["rpc", "get_block"]).is_err());
        assert!(parse(&["rpc", "submit_transaction"]).is_err());
        assert!(parse(&["rpc"]).is_err());
        assert!(parse(&["rpc", "ping", "extra"]).is_err());
    }

    #[test]
    fn connection_flags_work_after_the_method() {
        let args = Args::try_parse_from([
            "x4kas-cli",
            "rpc",
            "get_info",
            "--url",
            "ws://127.0.0.1:17110",
            "-n",
            "testnet-10",
        ])
        .unwrap();
        assert_eq!(args.url.as_deref(), Some("ws://127.0.0.1:17110"));
        assert_eq!(args.network, "testnet-10");
    }
}
