use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use futures::stream::{self, StreamExt};
use kaspa_rpc_core::api::rpc::RpcApi;
use kaspa_rpc_core::{
    GetVirtualChainFromBlockV2Response, RpcDataVerbosityLevel, RpcHash, RpcOptionalTransaction,
};
use kaspa_wrpc_client::prelude::*;
use serde::Serialize;
use serde_json::json;
use std::str::FromStr;
use tokio::sync::RwLock;

use crate::analytics::{BlockSummary, Metrics};
use crate::app::{App, ConnectionStatus};
use crate::rpc::methods::{
    self, parse_address, parse_addresses, parse_bool, parse_hash, parse_opt_u64,
    parse_subnetwork_id, parse_u64, parse_u64_list, parse_verbosity,
};
use crate::rpc::types::{shorten_address, sompi_to_kas};
use crate::tx_inspect::{
    OpcodeUsage, ScriptClass, coinbase_node_version, detect_protocol, output_script_opcodes,
    redeem_script_opcodes, script_class,
};

pub struct RpcManager {
    client: Arc<KaspaRpcClient>,
    app_state: Arc<RwLock<App>>,
    poll_handle: Option<tokio::task::JoinHandle<()>>,
}

impl RpcManager {
    pub async fn new(
        url: Option<String>,
        network: &str,
        app_state: Arc<RwLock<App>>,
    ) -> Result<Self> {
        let network_id = NetworkId::from_str(network)?;

        let client = if let Some(ref url) = url {
            KaspaRpcClient::new(
                WrpcEncoding::Borsh,
                Some(url.as_str()),
                None,
                Some(network_id),
                None,
            )?
        } else {
            let resolver = Resolver::default();
            KaspaRpcClient::new(
                WrpcEncoding::Borsh,
                None,
                Some(resolver),
                Some(network_id),
                None,
            )?
        };

        Ok(Self {
            client: Arc::new(client),
            app_state,
            poll_handle: None,
        })
    }

    pub async fn connect(&self) -> Result<()> {
        {
            let mut app = self.app_state.write().await;
            app.node.connection_status = ConnectionStatus::Connecting;
        }

        match self.client.connect(None).await {
            Ok(_) => {
                let mut app = self.app_state.write().await;
                app.node.connection_status = ConnectionStatus::Connected;
                Ok(())
            }
            Err(e) => {
                let mut app = self.app_state.write().await;
                app.node.connection_status = ConnectionStatus::Error(e.to_string());
                Err(e.into())
            }
        }
    }

    pub async fn disconnect(&self) -> Result<()> {
        self.client.disconnect().await?;
        let mut app = self.app_state.write().await;
        app.node.connection_status = ConnectionStatus::Disconnected;
        Ok(())
    }

    /// Poll the node every `interval` until the surrounding task is aborted.
    pub async fn poll_forever(&self, interval: Duration, app_state: Arc<RwLock<App>>) {
        let mut ticker = tokio::time::interval(interval);
        loop {
            ticker.tick().await;
            if !app_state.read().await.paused {
                Self::poll_once(&self.client, &app_state).await;
            }
        }
    }

    async fn poll_once(client: &KaspaRpcClient, state: &Arc<RwLock<App>>) {
        let start = std::time::Instant::now();

        let (server_info, dag_info, mempool, supply, fee_estimate, sink_blue_score) = tokio::join!(
            client.get_server_info(),
            client.get_block_dag_info(),
            client.get_mempool_entries(true, false),
            client.get_coin_supply(),
            client.get_fee_estimate(),
            client.get_sink_blue_score(),
        );
        // Header only (no transactions) of the sink, for "seconds behind tip".
        let sink_block = match dag_info {
            Ok(ref info) => Some(client.get_block(info.sink, false).await),
            Err(_) => None,
        };

        let mut app = state.write().await;
        let mut errors: Vec<String> = Vec::new();

        match server_info {
            Ok(v) => app.node.server_info = Some(v.into()),
            Err(e) => errors.push(format!("server_info: {}", e)),
        }

        match dag_info {
            Ok(v) => {
                let info: crate::rpc::types::DagInfo = v.into();
                app.node
                    .dag_visualizer
                    .update(&info.tip_hashes, &info.virtual_parent_hashes);
                app.node.dag_info = Some(info);
            }
            Err(e) => errors.push(format!("dag_info: {}", e)),
        }
        match mempool {
            Ok(v) => app.node.mempool_state = Some(v.into()),
            Err(e) => errors.push(format!("mempool: {}", e)),
        }
        match supply {
            Ok(v) => app.node.coin_supply = Some(v.into()),
            Err(e) => errors.push(format!("coin_supply: {}", e)),
        }
        match fee_estimate {
            Ok(v) => app.node.fee_estimate = Some(v.into()),
            Err(e) => errors.push(format!("fee_estimate: {}", e)),
        }
        match sink_blue_score {
            Ok(v) => app.node.sink_blue_score = Some(v),
            Err(e) => errors.push(format!("sink_blue_score: {}", e)),
        }
        match sink_block {
            Some(Ok(block)) => app.node.sink_timestamp_ms = Some(block.header.timestamp),
            Some(Err(e)) => errors.push(format!("sink_block: {}", e)),
            None => {}
        }

        if let Some(dag) = app.node.dag_info.clone() {
            let blue = app.node.sink_blue_score;
            app.node.dag_stats.update(&dag, blue);
        }

        app.node.node_url = client.url();
        if let Some(desc) = client.node_descriptor() {
            app.node.node_uid = Some(desc.uid.clone());
        }

        let poll_duration_ms = start.elapsed().as_secs_f64() * 1000.0;
        app.node.last_refresh = Some(std::time::Instant::now());
        app.node.last_poll_duration_ms = Some(poll_duration_ms);
        app.node.last_error = if errors.is_empty() {
            None
        } else {
            Some(errors.join("; "))
        };
        app.mark_dirty();
    }

    /// Run a read-only RPC method from the RPC Cmds tab or command palette. Missing or
    /// empty arguments take the defaults declared in `methods::RPC_METHODS`.
    pub async fn execute_rpc_call(&self, method: &str, args: &[String]) -> Result<String> {
        let spec = methods::find(method).ok_or_else(|| {
            anyhow::anyhow!(
                "Unknown command: '{}'. Type 'help' for available commands.",
                method
            )
        })?;
        let args = spec.resolve_args(args)?;
        let arg = |i: usize| args[i].as_str();
        let c = &self.client;

        let out = match method {
            "ping" => {
                let start = std::time::Instant::now();
                c.ping().await?;
                to_json(&json!({ "latencyMs": start.elapsed().as_secs_f64() * 1000.0 }))?
            }
            "get_info" => to_json(&c.get_info().await?)?,
            "get_server_info" => to_json(&c.get_server_info().await?)?,
            "get_system_info" => to_json(&c.get_system_info().await?)?,
            "get_metrics" => to_json(&c.get_metrics(true, true, true, true, true, true).await?)?,
            "get_connections" => to_json(&c.get_connections(true).await?)?,
            "get_sync_status" => to_json(&json!({ "isSynced": c.get_sync_status().await? }))?,
            "get_current_network" => to_json(&c.get_current_network().await?)?,
            "get_connected_peer_info" => to_json(&c.get_connected_peer_info().await?)?,
            "get_peer_addresses" => to_json(&c.get_peer_addresses().await?)?,
            "get_block_dag_info" => to_json(&c.get_block_dag_info().await?)?,
            "get_block_count" => to_json(&c.get_block_count().await?)?,
            "get_sink" => to_json(&c.get_sink().await?)?,
            "get_sink_blue_score" => {
                to_json(&json!({ "blueScore": c.get_sink_blue_score().await? }))?
            }
            "get_coin_supply" => to_json(&c.get_coin_supply().await?)?,
            "estimate_network_hashes_per_second" => {
                let dag = c.get_block_dag_info().await?;
                let r = c
                    .estimate_network_hashes_per_second(1000, Some(dag.sink))
                    .await?;
                to_json(&json!({ "networkHashesPerSecond": r }))?
            }
            "get_virtual_chain" => {
                let dag = c.get_block_dag_info().await?;
                let r = c
                    .get_virtual_chain_from_block(dag.pruning_point_hash, false, None)
                    .await?;
                to_json(&json!({
                    "removedChainBlockCount": r.removed_chain_block_hashes.len(),
                    "addedChainBlockCount": r.added_chain_block_hashes.len(),
                    "acceptedTransactionIdCount": r.accepted_transaction_ids.len(),
                }))?
            }
            "get_block" => to_json(
                &c.get_block(parse_hash(arg(0))?, parse_bool(arg(1))?)
                    .await?,
            )?,
            "get_blocks" => to_json(
                &c.get_blocks(
                    Some(parse_hash(arg(0))?),
                    parse_bool(arg(1))?,
                    parse_bool(arg(2))?,
                )
                .await?,
            )?,
            "get_headers" => to_json(
                &c.get_headers(parse_hash(arg(0))?, parse_u64(arg(1))?, parse_bool(arg(2))?)
                    .await?,
            )?,
            "get_current_block_color" => {
                to_json(&c.get_current_block_color(parse_hash(arg(0))?).await?)?
            }
            "get_block_reward_info" => {
                to_json(&c.get_block_reward_info(parse_hash(arg(0))?).await?)?
            }
            "get_seq_commit_lane_proof" => to_json(
                &c.get_seq_commit_lane_proof(parse_hash(arg(0))?, parse_hash(arg(1))?)
                    .await?,
            )?,
            "get_virtual_chain_from_block_v2" => to_json(
                &c.get_virtual_chain_from_block_v2(
                    parse_hash(arg(0))?,
                    Some(parse_verbosity(arg(1))?),
                    parse_opt_u64(arg(2))?,
                )
                .await?,
            )?,
            "get_daa_score_timestamp_estimate" => {
                let scores = parse_u64_list(arg(0))?;
                let timestamps = c.get_daa_score_timestamp_estimate(scores.clone()).await?;
                let estimates: Vec<_> = scores
                    .iter()
                    .zip(timestamps)
                    .map(|(score, ts)| json!({ "daaScore": score, "timestamp": ts }))
                    .collect();
                to_json(&estimates)?
            }
            "get_subnetwork" => to_json(&c.get_subnetwork(parse_subnetwork_id(arg(0))?).await?)?,
            "get_block_template" => to_json(
                &c.get_block_template(parse_address(arg(0))?, arg(1).as_bytes().to_vec())
                    .await?,
            )?,
            "get_mempool_entries" => to_json(&c.get_mempool_entries(true, false).await?)?,
            "get_mempool_entry" => to_json(
                &c.get_mempool_entry(
                    parse_hash(arg(0))?,
                    parse_bool(arg(1))?,
                    parse_bool(arg(2))?,
                )
                .await?,
            )?,
            "get_mempool_entries_by_addresses" => to_json(
                &c.get_mempool_entries_by_addresses(
                    parse_addresses(arg(0))?,
                    parse_bool(arg(1))?,
                    parse_bool(arg(2))?,
                )
                .await?,
            )?,
            "get_fee_estimate" => to_json(&c.get_fee_estimate().await?)?,
            "get_fee_estimate_experimental" => {
                to_json(&c.get_fee_estimate_experimental(true).await?)?
            }
            "get_balance_by_address" => {
                let address = parse_address(arg(0))?;
                let sompi = c.get_balance_by_address(address.clone()).await?;
                to_json(&json!({
                    "address": address,
                    "balance": sompi,
                    "balanceKas": sompi_to_kas(sompi),
                }))?
            }
            "get_balances_by_addresses" => to_json(
                &c.get_balances_by_addresses(parse_addresses(arg(0))?)
                    .await?,
            )?,
            "get_utxos_by_addresses" => {
                to_json(&c.get_utxos_by_addresses(parse_addresses(arg(0))?).await?)?
            }
            "get_utxo_return_address" => to_json(&json!({
                "returnAddress": c
                    .get_utxo_return_address(parse_hash(arg(0))?, parse_u64(arg(1))?)
                    .await?
            }))?,
            _ => anyhow::bail!("No handler for RPC method '{}'", method),
        };
        Ok(truncate_response(out))
    }

    pub async fn fetch_mining_info(&self) -> Result<crate::rpc::types::MiningInfo> {
        use std::collections::HashMap;

        let dag = self.client.get_block_dag_info().await?;

        // Estimate hashrate
        let hashrate = self
            .client
            .estimate_network_hashes_per_second(1000, Some(dag.sink))
            .await
            .unwrap_or(0) as f64;

        // Get virtual chain to find recent blocks
        let vspc = self
            .client
            .get_virtual_chain_from_block(dag.pruning_point_hash, false, None)
            .await?;

        // Sample the last N blocks from the chain
        let sample_size = 100.min(vspc.added_chain_block_hashes.len());
        let start = vspc
            .added_chain_block_hashes
            .len()
            .saturating_sub(sample_size);
        let sample_hashes = &vspc.added_chain_block_hashes[start..];

        let mut miner_counts: HashMap<String, usize> = HashMap::new();

        // Fetch blocks in parallel (10 concurrent)
        let hashes: Vec<_> = sample_hashes.to_vec();
        let client = self.client.clone();
        let results: Vec<_> = stream::iter(hashes)
            .map(|hash| {
                let client = client.clone();
                async move { client.get_block(hash, true).await }
            })
            .buffer_unordered(10)
            .collect()
            .await;

        for block in results.into_iter().flatten() {
            // The first transaction in a block is the coinbase
            if let Some(coinbase) = block.transactions.first() {
                // Miner address is typically in the first output
                if let Some(output) = coinbase.outputs.first()
                    && let Some(ref verbose) = output.verbose_data
                {
                    let addr = verbose.script_public_key_address.to_string();
                    let short_addr = crate::rpc::types::shorten_address(&addr, 10, 6);
                    *miner_counts.entry(short_addr).or_insert(0) += 1;
                }
            }
        }

        let unique_miners = miner_counts.len();
        let mut top_miners: Vec<(String, usize)> = miner_counts.into_iter().collect();
        top_miners.sort_by_key(|a| std::cmp::Reverse(a.1));
        top_miners.truncate(5);

        Ok(crate::rpc::types::MiningInfo {
            hashrate,
            unique_miners,
            top_miners,
            blocks_analyzed: sample_size,
        })
    }

    /// Fetch the virtual selected parent chain v2 from a given start hash.
    /// Uses High verbosity for fee/address/payload data, and min_confirmation_count=10.
    pub async fn fetch_vspc_v2(
        &self,
        start_hash: RpcHash,
    ) -> Result<GetVirtualChainFromBlockV2Response> {
        let response = self
            .client
            .get_virtual_chain_from_block_v2(
                start_hash,
                Some(RpcDataVerbosityLevel::High),
                Some(10),
            )
            .await?;
        Ok(response)
    }

    /// Get the pruning point hash from block DAG info.
    pub async fn get_pruning_point_hash(&self) -> Result<RpcHash> {
        let dag = self.client.get_block_dag_info().await?;
        Ok(dag.pruning_point_hash)
    }

    /// Extract BlockSummary entries and removed hashes from a VSPC V2 response.
    pub fn extract_block_summaries(
        response: &GetVirtualChainFromBlockV2Response,
    ) -> (Vec<BlockSummary>, Vec<String>) {
        let removed: Vec<String> = response
            .removed_chain_block_hashes
            .iter()
            .map(|h| h.to_string())
            .collect();

        let summaries = response
            .chain_block_accepted_transactions
            .iter()
            .map(|chain_block| {
                let header = &chain_block.chain_block_header;
                let mut metrics = Metrics {
                    chain_blocks: 1,
                    ..Default::default()
                };
                for tx in chain_block.accepted_transactions.iter() {
                    record_transaction(&mut metrics, tx);
                }
                BlockSummary {
                    hash: header.hash.map(|h| h.to_string()).unwrap_or_default(),
                    timestamp_ms: header.timestamp.unwrap_or(0),
                    metrics,
                }
            })
            .collect();

        (summaries, removed)
    }

    pub async fn get_block_by_hash(&self, hash_str: &str) -> Result<String> {
        let hash = kaspa_rpc_core::RpcHash::from_str(hash_str)
            .map_err(|e| anyhow::anyhow!("Invalid hash: {}", e))?;
        let block = self.client.get_block(hash, true).await?;
        Ok(format!("{:#?}", block))
    }
}

/// Count one accepted transaction into a chain block's metrics.
fn record_transaction(metrics: &mut Metrics, tx: &RpcOptionalTransaction) {
    let payload = tx.payload.as_deref().unwrap_or(&[]);

    // Coinbase: no inputs. Its payload carries the miner's node version.
    if tx.inputs.is_empty() {
        if let Some(version) = coinbase_node_version(payload) {
            *metrics.node_versions.entry(version).or_insert(0) += 1;
        }
        return;
    }
    metrics.tx_count += 1;

    let mut usage = OpcodeUsage::default();
    let mut covenant_created = 0;
    let mut covenant_spent = 0;
    // Fee = inputs - outputs, known only if every spent UTXO's amount is.
    let mut input_sum = Some(0u64);

    for input in &tx.inputs {
        let utxo = input
            .verbose_data
            .as_ref()
            .and_then(|vd| vd.utxo_entry.as_ref());
        input_sum = input_sum
            .zip(utxo.and_then(|u| u.amount))
            .map(|(a, b)| a + b);
        let Some(utxo) = utxo else { continue };

        if let Some(addr) = utxo
            .verbose_data
            .as_ref()
            .and_then(|uvd| uvd.script_public_key_address.as_ref())
        {
            let short = shorten_address(&addr.to_string(), 10, 6);
            *metrics.senders.entry(short).or_insert(0) += 1;
        }
        if utxo.covenant_id.is_some() {
            covenant_spent += 1;
        }
        // Covenant opcodes live in the redeem script a P2SH spend reveals.
        if let Some(spk) = &utxo.script_public_key
            && script_class(spk.script()) == ScriptClass::ScriptHash
            && let Some(sig) = &input.signature_script
        {
            usage |= redeem_script_opcodes(sig);
        }
    }

    let mut output_sum = 0u64;
    for output in &tx.outputs {
        output_sum += output.value.unwrap_or(0);
        if let Some(spk) = &output.script_public_key {
            metrics.script_classes.record(script_class(spk.script()));
            usage |= output_script_opcodes(spk.script());
        }
        if output.covenant.as_ref().is_some_and(|c| c.0.is_some()) {
            covenant_created += 1;
        }
        if let Some(addr) = output
            .verbose_data
            .as_ref()
            .and_then(|vd| vd.script_public_key_address.as_ref())
        {
            let short = shorten_address(&addr.to_string(), 10, 6);
            *metrics.receivers.entry(short).or_insert(0) += 1;
        }
    }

    metrics.record_fee(input_sum.map(|i| i.saturating_sub(output_sum)));
    metrics
        .inspection
        .record_tx(usage, covenant_created, covenant_spent);

    let input_scripts: Vec<&[u8]> = tx
        .inputs
        .iter()
        .filter_map(|inp| inp.signature_script.as_deref())
        .collect();
    if let Some(proto) = detect_protocol(payload, &input_scripts) {
        *metrics.protocols.entry(proto).or_insert(0) += 1;
    }
}

impl Drop for RpcManager {
    fn drop(&mut self) {
        if let Some(handle) = self.poll_handle.take() {
            handle.abort();
        }
    }
}

/// Pretty-printed JSON for the result viewer.
fn to_json<T: Serialize + ?Sized>(value: &T) -> Result<String> {
    Ok(serde_json::to_string_pretty(value)?)
}

/// Responses beyond this many bytes are cut so the result viewer stays responsive.
const MAX_RESPONSE_CHARS: usize = 1_000_000;

fn truncate_response(mut s: String) -> String {
    if s.len() <= MAX_RESPONSE_CHARS {
        return s;
    }
    let total = s.len();
    let mut cut = MAX_RESPONSE_CHARS;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
    s.push_str(&format!(
        "\n\n… (response truncated: {total} bytes, showing first {cut})"
    ));
    s
}

#[cfg(test)]
mod tests {
    use super::{MAX_RESPONSE_CHARS, truncate_response};
    use crate::rpc::methods::RPC_METHODS;

    /// Every method in `RPC_METHODS` must have a match arm in `execute_rpc_call`.
    /// Checked against the source so a missing arm fails without a live node.
    #[test]
    fn all_rpc_methods_have_handler() {
        let src = include_str!("client.rs");
        let body = &src[src.find("pub async fn execute_rpc_call").unwrap()
            ..src.find("pub async fn fetch_mining_info").unwrap()];
        for m in RPC_METHODS {
            assert!(
                body.contains(&format!("\"{}\" =>", m.name)),
                "RPC method '{}' has no handler in execute_rpc_call",
                m.name
            );
        }
    }

    #[test]
    fn truncate_response_caps_long_output() {
        assert_eq!(truncate_response("short".into()), "short");
        let long = "é".repeat(MAX_RESPONSE_CHARS);
        let out = truncate_response(long);
        assert!(out.len() < MAX_RESPONSE_CHARS + 200);
        assert!(out.contains("response truncated"));
    }
}
