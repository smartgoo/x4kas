//! Catalog of RPC methods exposed in the RPC Cmds tab, with their
//! parameters, plus parsing helpers for the string arguments entered in the UI.

use anyhow::{Result, anyhow, bail};
use kaspa_rpc_core::{RpcAddress, RpcDataVerbosityLevel, RpcHash, RpcSubnetworkId};
use std::str::FromStr;

/// How a parameter is entered and validated.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ParamKind {
    /// 64-character hex block hash or transaction id.
    Hash,
    Address,
    /// Addresses separated by commas or whitespace.
    Addresses,
    Number,
    /// Numbers separated by commas or whitespace.
    Numbers,
    Bool,
    Text,
    /// One of a fixed set of values.
    Choice(&'static [&'static str]),
}

impl ParamKind {
    pub fn hint(&self) -> &'static str {
        match self {
            ParamKind::Hash => "64-char hex hash",
            ParamKind::Address => "kaspa:…",
            ParamKind::Addresses => "kaspa:…, kaspa:…",
            ParamKind::Number => "number",
            ParamKind::Numbers => "1, 2, 3",
            ParamKind::Bool => "true / false",
            ParamKind::Text => "text",
            ParamKind::Choice(_) => "choice",
        }
    }
}

#[derive(Debug)]
pub struct RpcParam {
    pub name: &'static str,
    pub kind: ParamKind,
    /// Used when the argument is left empty. `None` means the argument is required;
    /// `Some("")` means it is optional with no value.
    pub default: Option<&'static str>,
}

#[derive(Debug)]
pub struct RpcMethod {
    pub name: &'static str,
    pub description: &'static str,
    pub params: &'static [RpcParam],
}

impl RpcMethod {
    /// e.g. `get_block <hash> [include_transactions=true]`
    pub fn usage(&self) -> String {
        let mut s = self.name.to_string();
        for p in self.params {
            match p.default {
                None => s.push_str(&format!(" <{}>", p.name)),
                Some("") => s.push_str(&format!(" [{}]", p.name)),
                Some(d) => s.push_str(&format!(" [{}={}]", p.name, d)),
            }
        }
        s
    }

    /// Fill in defaults for missing or empty arguments. Errors on a missing required
    /// argument or too many arguments. If the last parameter is a list, extra arguments
    /// are folded into it (so `cmd 1 2 3` works).
    pub fn resolve_args(&self, args: &[String]) -> Result<Vec<String>> {
        let last_is_list = self
            .params
            .last()
            .is_some_and(|p| matches!(p.kind, ParamKind::Addresses | ParamKind::Numbers));
        if last_is_list && args.len() > self.params.len() {
            let (head, tail) = args.split_at(self.params.len() - 1);
            let mut merged = head.to_vec();
            merged.push(tail.join(","));
            return self.resolve_args(&merged);
        }
        if args.len() > self.params.len() {
            bail!(
                "too many arguments ({} given, {} expected)\nusage: {}",
                args.len(),
                self.params.len(),
                self.usage()
            );
        }
        self.params
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let given = args.get(i).map(|a| a.trim()).unwrap_or_default();
                match (given, p.default) {
                    ("", Some(d)) => Ok(d.to_string()),
                    ("", None) => Err(anyhow!(
                        "missing argument <{}>\nusage: {}",
                        p.name,
                        self.usage()
                    )),
                    (v, _) => Ok(v.to_string()),
                }
            })
            .collect()
    }
}

const fn req(name: &'static str, kind: ParamKind) -> RpcParam {
    RpcParam {
        name,
        kind,
        default: None,
    }
}

const fn opt(name: &'static str, kind: ParamKind, default: &'static str) -> RpcParam {
    RpcParam {
        name,
        kind,
        default: Some(default),
    }
}

const fn method(
    name: &'static str,
    description: &'static str,
    params: &'static [RpcParam],
) -> RpcMethod {
    RpcMethod {
        name,
        description,
        params,
    }
}

const VERBOSITY: ParamKind = ParamKind::Choice(&["none", "low", "high", "full"]);

/// Read-only RPC methods. State-changing calls (submit_*, add_peer, ban/unban,
/// resolve_finality_conflict, shutdown) are intentionally not exposed.
pub const RPC_METHODS: &[RpcMethod] = &[
    // Node
    method("ping", "Ping the node", &[]),
    method("get_info", "Get general node info", &[]),
    method("get_server_info", "Get server info", &[]),
    method(
        "get_system_info",
        "Get node system info (version, CPU, memory)",
        &[],
    ),
    method(
        "get_metrics",
        "Get process, connection, bandwidth, consensus and storage metrics",
        &[],
    ),
    method("get_connections", "Get wRPC client connection counts", &[]),
    method("get_sync_status", "Get sync status", &[]),
    method("get_current_network", "Get current network type", &[]),
    method("get_connected_peer_info", "Get connected peer info", &[]),
    method("get_peer_addresses", "Get known peer addresses", &[]),
    // DAG
    method("get_block_dag_info", "Get block DAG info", &[]),
    method("get_block_count", "Get block and header counts", &[]),
    method("get_sink", "Get sink (virtual selected parent) hash", &[]),
    method("get_sink_blue_score", "Get sink blue score", &[]),
    method("get_coin_supply", "Get coin supply", &[]),
    method(
        "estimate_network_hashes_per_second",
        "Estimate network hashrate",
        &[],
    ),
    method(
        "get_virtual_chain",
        "Get virtual selected parent chain summary from the pruning point",
        &[],
    ),
    method(
        "get_block",
        "Get a block by hash",
        &[
            req("hash", ParamKind::Hash),
            opt("include_transactions", ParamKind::Bool, "true"),
        ],
    ),
    method(
        "get_blocks",
        "Get blocks from low_hash up to the virtual",
        &[
            req("low_hash", ParamKind::Hash),
            opt("include_blocks", ParamKind::Bool, "false"),
            opt("include_transactions", ParamKind::Bool, "false"),
        ],
    ),
    method(
        "get_headers",
        "Get headers from start_hash (not implemented by kaspad v2.1.0)",
        &[
            req("start_hash", ParamKind::Hash),
            opt("limit", ParamKind::Number, "10"),
            opt("is_ascending", ParamKind::Bool, "true"),
        ],
    ),
    method(
        "get_current_block_color",
        "Get whether a block is currently blue or red",
        &[req("hash", ParamKind::Hash)],
    ),
    method(
        "get_block_reward_info",
        "Get reward info for a chain block",
        &[req("hash", ParamKind::Hash)],
    ),
    method(
        "get_seq_commit_lane_proof",
        "Get a seq-commit lane proof for a chain block",
        &[
            req("block_hash", ParamKind::Hash),
            req("lane_key", ParamKind::Hash),
        ],
    ),
    method(
        "get_virtual_chain_from_block_v2",
        "Get the virtual chain from start_hash (V2)",
        &[
            req("start_hash", ParamKind::Hash),
            opt("verbosity", VERBOSITY, "none"),
            opt("min_confirmation_count", ParamKind::Number, ""),
        ],
    ),
    method(
        "get_daa_score_timestamp_estimate",
        "Estimate timestamps for DAA scores",
        &[req("daa_scores", ParamKind::Numbers)],
    ),
    method(
        "get_subnetwork",
        "Get subnetwork info (not implemented by kaspad v2.1.0)",
        &[req("subnetwork_id", ParamKind::Text)],
    ),
    method(
        "get_block_template",
        "Get a block template for a pay address",
        &[
            req("pay_address", ParamKind::Address),
            opt("extra_data", ParamKind::Text, ""),
        ],
    ),
    // Mempool & fees
    method("get_mempool_entries", "Get mempool entries", &[]),
    method(
        "get_mempool_entry",
        "Get a mempool transaction by id",
        &[
            req("transaction_id", ParamKind::Hash),
            opt("include_orphan_pool", ParamKind::Bool, "true"),
            opt("filter_transaction_pool", ParamKind::Bool, "false"),
        ],
    ),
    method(
        "get_mempool_entries_by_addresses",
        "Get mempool entries for addresses",
        &[
            req("addresses", ParamKind::Addresses),
            opt("include_orphan_pool", ParamKind::Bool, "true"),
            opt("filter_transaction_pool", ParamKind::Bool, "false"),
        ],
    ),
    method("get_fee_estimate", "Get fee estimate", &[]),
    method(
        "get_fee_estimate_experimental",
        "Get experimental fee estimate (verbose)",
        &[],
    ),
    // UTXO index (node must run with --utxoindex)
    method(
        "get_balance_by_address",
        "Get an address balance (needs --utxoindex)",
        &[req("address", ParamKind::Address)],
    ),
    method(
        "get_balances_by_addresses",
        "Get balances for addresses (needs --utxoindex)",
        &[req("addresses", ParamKind::Addresses)],
    ),
    method(
        "get_utxos_by_addresses",
        "Get UTXOs for addresses (needs --utxoindex)",
        &[req("addresses", ParamKind::Addresses)],
    ),
    method(
        "get_utxo_return_address",
        "Get the return address of a transaction (needs --utxoindex)",
        &[
            req("txid", ParamKind::Hash),
            req("accepting_block_daa_score", ParamKind::Number),
        ],
    ),
];

pub fn find(name: &str) -> Option<&'static RpcMethod> {
    RPC_METHODS.iter().find(|m| m.name == name)
}

// --- argument parsers ---

pub fn parse_hash(s: &str) -> Result<RpcHash> {
    RpcHash::from_str(s.trim()).map_err(|e| anyhow!("invalid hash '{}': {}", s, e))
}

pub fn parse_address(s: &str) -> Result<RpcAddress> {
    RpcAddress::try_from(s.trim()).map_err(|e| anyhow!("invalid address '{}': {}", s, e))
}

pub fn parse_addresses(s: &str) -> Result<Vec<RpcAddress>> {
    let list: Vec<_> = split_list(s).map(parse_address).collect::<Result<_>>()?;
    if list.is_empty() {
        bail!("at least one address is required");
    }
    Ok(list)
}

pub fn parse_u64(s: &str) -> Result<u64> {
    s.trim()
        .replace('_', "")
        .parse()
        .map_err(|_| anyhow!("invalid number '{}'", s))
}

pub fn parse_opt_u64(s: &str) -> Result<Option<u64>> {
    if s.trim().is_empty() {
        Ok(None)
    } else {
        parse_u64(s).map(Some)
    }
}

pub fn parse_u64_list(s: &str) -> Result<Vec<u64>> {
    let list: Vec<_> = split_list(s).map(parse_u64).collect::<Result<_>>()?;
    if list.is_empty() {
        bail!("at least one number is required");
    }
    Ok(list)
}

pub fn parse_bool(s: &str) -> Result<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "true" | "t" | "yes" | "y" | "1" => Ok(true),
        "false" | "f" | "no" | "n" | "0" => Ok(false),
        _ => Err(anyhow!("invalid boolean '{}' (use true/false)", s)),
    }
}

pub fn parse_verbosity(s: &str) -> Result<RpcDataVerbosityLevel> {
    match s.trim().to_ascii_lowercase().as_str() {
        "none" | "0" => Ok(RpcDataVerbosityLevel::None),
        "low" | "1" => Ok(RpcDataVerbosityLevel::Low),
        "high" | "2" => Ok(RpcDataVerbosityLevel::High),
        "full" | "3" => Ok(RpcDataVerbosityLevel::Full),
        _ => Err(anyhow!("invalid verbosity '{}' (none/low/high/full)", s)),
    }
}

pub fn parse_subnetwork_id(s: &str) -> Result<RpcSubnetworkId> {
    RpcSubnetworkId::from_str(s.trim()).map_err(|e| anyhow!("invalid subnetwork id '{}': {}", s, e))
}

fn split_list(s: &str) -> impl Iterator<Item = &str> {
    s.split(|c: char| c == ',' || c.is_whitespace())
        .filter(|p| !p.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    const HASH: &str = "0000000000000000000000000000000000000000000000000000000000000001";
    const ADDR: &str = "kaspa:qpauqsvk7yf9unexwmxsnmg547mhyga37csh0kj53q6xxgl24ydxjsgzthw5j";

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn method_names_are_unique() {
        let names: HashSet<_> = RPC_METHODS.iter().map(|m| m.name).collect();
        assert_eq!(names.len(), RPC_METHODS.len());
    }

    #[test]
    fn required_params_come_before_optional() {
        for m in RPC_METHODS {
            let first_opt = m.params.iter().position(|p| p.default.is_some());
            if let Some(i) = first_opt {
                assert!(
                    m.params[i..].iter().all(|p| p.default.is_some()),
                    "{}: required param after optional",
                    m.name
                );
            }
        }
    }

    #[test]
    fn choice_defaults_are_valid() {
        for m in RPC_METHODS {
            for p in m.params {
                if let (ParamKind::Choice(opts), Some(d)) = (p.kind, p.default) {
                    assert!(opts.contains(&d), "{}.{}", m.name, p.name);
                }
            }
        }
    }

    #[test]
    fn find_returns_method() {
        assert_eq!(find("get_block").unwrap().params.len(), 2);
        assert!(find("submit_transaction").is_none());
    }

    #[test]
    fn usage_shows_required_and_defaults() {
        assert_eq!(
            find("get_block").unwrap().usage(),
            "get_block <hash> [include_transactions=true]"
        );
        assert_eq!(
            find("get_virtual_chain_from_block_v2").unwrap().usage(),
            "get_virtual_chain_from_block_v2 <start_hash> [verbosity=none] [min_confirmation_count]"
        );
        assert_eq!(find("ping").unwrap().usage(), "ping");
    }

    #[test]
    fn resolve_args_fills_defaults() {
        let m = find("get_block").unwrap();
        assert_eq!(
            m.resolve_args(&args(&[HASH])).unwrap(),
            args(&[HASH, "true"])
        );
        assert_eq!(
            m.resolve_args(&args(&[HASH, ""])).unwrap(),
            args(&[HASH, "true"])
        );
        assert_eq!(
            m.resolve_args(&args(&[HASH, "false"])).unwrap(),
            args(&[HASH, "false"])
        );
    }

    #[test]
    fn resolve_args_rejects_missing_and_extra() {
        let m = find("get_block").unwrap();
        assert!(
            m.resolve_args(&[])
                .unwrap_err()
                .to_string()
                .contains("<hash>")
        );
        assert!(m.resolve_args(&args(&["  "])).is_err());
        assert!(m.resolve_args(&args(&[HASH, "true", "x"])).is_err());
        assert!(find("ping").unwrap().resolve_args(&args(&["x"])).is_err());
    }

    #[test]
    fn resolve_args_folds_extra_words_into_trailing_list() {
        let m = find("get_daa_score_timestamp_estimate").unwrap();
        assert_eq!(
            m.resolve_args(&args(&["1", "2", "3"])).unwrap(),
            args(&["1,2,3"])
        );
        // Not folded when the list is followed by other params.
        let m = find("get_mempool_entries_by_addresses").unwrap();
        assert!(
            m.resolve_args(&args(&[ADDR, ADDR, "true", "false"]))
                .is_err()
        );
    }

    #[test]
    fn parses_hashes() {
        assert!(parse_hash(HASH).is_ok());
        assert!(parse_hash("xyz").is_err());
        assert!(parse_hash("").is_err());
    }

    #[test]
    fn parses_addresses() {
        assert!(parse_address(ADDR).is_ok());
        assert!(parse_address("kaspa:nope").is_err());
        assert_eq!(
            parse_addresses(&format!("{ADDR}, {ADDR}")).unwrap().len(),
            2
        );
        assert_eq!(parse_addresses(&format!("{ADDR} {ADDR}")).unwrap().len(), 2);
        assert!(parse_addresses(" , ").is_err());
    }

    #[test]
    fn parses_numbers() {
        assert_eq!(parse_u64("1_000").unwrap(), 1000);
        assert!(parse_u64("-1").is_err());
        assert_eq!(parse_opt_u64("").unwrap(), None);
        assert_eq!(parse_opt_u64("5").unwrap(), Some(5));
        assert_eq!(parse_u64_list("1, 2 3").unwrap(), vec![1, 2, 3]);
        assert!(parse_u64_list("").is_err());
    }

    #[test]
    fn parses_bools_and_verbosity() {
        assert!(parse_bool("Yes").unwrap());
        assert!(!parse_bool("0").unwrap());
        assert!(parse_bool("maybe").is_err());
        assert!(matches!(
            parse_verbosity("HIGH").unwrap(),
            RpcDataVerbosityLevel::High
        ));
        assert!(parse_verbosity("max").is_err());
    }

    #[test]
    fn parses_subnetwork_id() {
        assert!(parse_subnetwork_id("0000000000000000000000000000000000000000").is_ok());
        assert!(parse_subnetwork_id("zz").is_err());
    }
}
