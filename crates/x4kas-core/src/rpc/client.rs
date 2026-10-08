use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use kaspa_rpc_core::api::rpc::RpcApi;
use kaspa_rpc_core::{
    GetVirtualChainFromBlockV2Response, RpcAddress, RpcDataVerbosityLevel, RpcHash, RpcHeader,
    UtxosChangedNotification,
};
use kaspa_wrpc_client::prelude::*;
use serde::Serialize;
use serde_json::json;
use std::str::FromStr;
use tokio::sync::RwLock;

use crate::app::{App, ConnectionStatus, DagBlock, Tab};
use crate::emission;
use crate::format::sompi_to_kas;
use crate::rpc::methods::{
    self, parse_address, parse_addresses, parse_bool, parse_hash, parse_opt_u64,
    parse_subnetwork_id, parse_u64, parse_u64_list, parse_verbosity,
};

pub struct RpcManager {
    client: KaspaRpcClient,
    app_state: Arc<RwLock<App>>,
}

impl RpcManager {
    /// `url: None` connects through the public node resolver.
    pub fn new(url: Option<&str>, network: &str, app_state: Arc<RwLock<App>>) -> Result<Self> {
        let client = KaspaRpcClient::new(
            WrpcEncoding::Borsh,
            url,
            url.is_none().then(Resolver::default),
            Some(NetworkId::from_str(network)?),
            None,
        )?;
        Ok(Self { client, app_state })
    }

    /// Connect, recording the outcome in `connection_status` (the controller has already
    /// set it to `Connecting`).
    pub async fn connect(&self) -> Result<()> {
        self.connect_with(None).await
    }

    /// Connect once, failing after `timeout` instead of retrying (for one-shot CLI calls).
    pub async fn connect_once(&self, timeout: Duration) -> Result<()> {
        self.connect_with(Some(ConnectOptions {
            strategy: ConnectStrategy::Fallback,
            connect_timeout: Some(timeout),
            ..Default::default()
        }))
        .await
    }

    async fn connect_with(&self, options: Option<ConnectOptions>) -> Result<()> {
        match self.client.connect(options).await {
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
    pub async fn poll_forever(&self, interval: Duration) {
        let mut ticker = tokio::time::interval(interval);
        loop {
            ticker.tick().await;
            if !self.app_state.read().await.paused {
                self.poll_once().await;
            }
        }
    }

    async fn poll_once(&self) {
        let client = &self.client;
        let start = std::time::Instant::now();

        let (server_info, dag_info, mempool, supply, fee_estimate, sink_blue_score) = tokio::join!(
            client.get_server_info(),
            client.get_block_dag_info(),
            client.get_mempool_entries(true, false),
            client.get_coin_supply(),
            client.get_fee_estimate(),
            client.get_sink_blue_score(),
        );
        // Headers only (no transactions) of the sink, for "seconds behind tip", and of the
        // pruning point, the address index's retention floor.
        let (sink_block, pruning_block) = match dag_info {
            Ok(ref info) => {
                let (sink, pruning) = tokio::join!(
                    client.get_block(info.sink, false),
                    client.get_block(info.pruning_point_hash, false),
                );
                (Some(sink), Some(pruning))
            }
            Err(_) => (None, None),
        };
        // Balance lookups need the node's UTXO index.
        let burn_address = server_info
            .as_ref()
            .ok()
            .filter(|info| info.has_utxo_index)
            .and_then(|info| emission::burn_address(&info.network_id.to_string()));
        let burn_balance = match burn_address {
            Some(address) => Some(client.get_balance_by_address(address).await),
            None => None,
        };

        let mut app = self.app_state.write().await;
        let mut errors: Vec<String> = Vec::new();

        match server_info {
            Ok(v) => app.node.server_info = Some(v.into()),
            Err(e) => errors.push(format!("server_info: {}", e)),
        }

        match dag_info {
            Ok(v) => {
                app.node.dag_info = Some(v.into());
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
        match burn_balance {
            Some(Ok(sompi)) => app.node.burn_balance = Some(sompi),
            Some(Err(e)) => errors.push(format!("burn_balance: {}", e)),
            None => {}
        }
        match sink_block {
            Some(Ok(block)) => app.node.sink_timestamp_ms = Some(block.header.timestamp),
            Some(Err(e)) => errors.push(format!("sink_block: {}", e)),
            None => {}
        }
        match pruning_block {
            Some(Ok(block)) => {
                app.node.pruning_point_timestamp_ms = Some(block.header.timestamp);
            }
            Some(Err(e)) => errors.push(format!("pruning_point_block: {}", e)),
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

    /// Feed the DAG visualizer from the node's `BlockAdded` notifications until the
    /// surrounding task is aborted. The node drops subscriptions with the connection (and
    /// the client rebuilds its notifier), so subscribe again on every connect.
    pub async fn stream_blocks(&self) {
        let ctl = self.client.rpc_ctl().multiplexer().channel();
        let (sender, receiver) = async_channel::unbounded();
        if self.client.is_connected() {
            self.subscribe_blocks(&sender).await;
        }
        loop {
            tokio::select! {
                state = ctl.receiver.recv() => match state {
                    Ok(RpcState::Connected) => self.subscribe_blocks(&sender).await,
                    Ok(RpcState::Disconnected) => {}
                    Err(_) => return,
                },
                notification = receiver.recv() => match notification {
                    Ok(Notification::BlockAdded(n)) => self.record_block(&n.block.header).await,
                    Ok(_) => {}
                    Err(_) => return,
                },
            }
        }
    }

    async fn subscribe_blocks(&self, sender: &async_channel::Sender<Notification>) {
        let id = self.client.register_new_listener(ChannelConnection::new(
            "x4kas-dag",
            sender.clone(),
            ChannelType::Persistent,
        ));
        if let Err(e) = self
            .client
            .start_notify(id, Scope::BlockAdded(BlockAddedScope {}))
            .await
        {
            self.app_state.write().await.node.last_error = Some(format!("block_added: {e}"));
        }
    }

    async fn record_block(&self, header: &RpcHeader) {
        let mut app = self.app_state.write().await;
        // Skip IBD's flood of old blocks, and keep the view frozen while paused.
        let synced = app.node.server_info.as_ref().is_some_and(|s| s.is_synced);
        if app.paused || !synced {
            return;
        }
        let block = DagBlock {
            hash: header.hash.to_string(),
            daa_score: header.daa_score,
            parents: header
                .direct_parents()
                .iter()
                .map(|h| h.to_string())
                .collect(),
        };
        // Only the Dashboard shows these, so don't wake the GUI ten times a second elsewhere.
        if app.node.dag_visualizer.add(block) && app.active_tab == Tab::Dashboard {
            app.mark_dirty();
        }
    }

    /// Forward the node's `UtxosChanged` notifications for `addresses` to `sender` until
    /// the surrounding task is aborted, resubscribing on every connect like
    /// [`Self::stream_blocks`]. Needs the node's UTXO index.
    pub async fn watch_utxos(
        &self,
        addresses: Vec<RpcAddress>,
        sender: tokio::sync::mpsc::Sender<UtxosChangedNotification>,
    ) {
        let ctl = self.client.rpc_ctl().multiplexer().channel();
        let (tx, rx) = async_channel::unbounded();
        let subscribe = |tx: &async_channel::Sender<Notification>| {
            let id = self.client.register_new_listener(ChannelConnection::new(
                "x4kas-watch",
                tx.clone(),
                ChannelType::Persistent,
            ));
            let scope = Scope::UtxosChanged(UtxosChangedScope::new(addresses.clone()));
            async move { self.client.start_notify(id, scope).await }
        };
        if self.client.is_connected()
            && let Err(e) = subscribe(&tx).await
        {
            self.app_state.write().await.watch.status.last_error = Some(format!("subscribe: {e}"));
        }
        loop {
            tokio::select! {
                state = ctl.receiver.recv() => match state {
                    Ok(RpcState::Connected) => {
                        if let Err(e) = subscribe(&tx).await {
                            self.app_state.write().await.watch.status.last_error =
                                Some(format!("subscribe: {e}"));
                        }
                    }
                    Ok(RpcState::Disconnected) => {}
                    Err(_) => return,
                },
                notification = rx.recv() => match notification {
                    Ok(Notification::UtxosChanged(n)) => {
                        if sender.send(n).await.is_err() {
                            return;
                        }
                    }
                    Ok(_) => {}
                    Err(_) => return,
                },
            }
        }
    }

    /// Balances of `addresses` in sompi (needs the node's UTXO index).
    pub async fn balances(&self, addresses: Vec<RpcAddress>) -> Result<Vec<(String, u64)>> {
        Ok(self
            .client
            .get_balances_by_addresses(addresses)
            .await?
            .into_iter()
            .map(|e| (e.address.to_string(), e.balance.unwrap_or(0)))
            .collect())
    }

    /// Unconfirmed activity of `addresses`: `(address, incoming, outgoing)` sompi in the
    /// mempool, from the transactions paying each address and spending from it.
    pub async fn pending_by_addresses(
        &self,
        addresses: Vec<RpcAddress>,
    ) -> Result<Vec<(String, u64, u64)>> {
        let entries = self
            .client
            .get_mempool_entries_by_addresses(addresses, false, false)
            .await?;
        Ok(entries
            .into_iter()
            .map(|e| {
                let address = e.address.to_string();
                let incoming: u64 = e
                    .receiving
                    .iter()
                    .flat_map(|m| &m.transaction.outputs)
                    .filter(|o| {
                        o.verbose_data
                            .as_ref()
                            .is_some_and(|v| v.script_public_key_address.to_string() == address)
                    })
                    .map(|o| o.value)
                    .sum();
                // Mempool inputs carry no amounts, so what leaves is what the spending
                // transactions pay to other addresses.
                let outgoing: u64 = e
                    .sending
                    .iter()
                    .flat_map(|m| &m.transaction.outputs)
                    .filter(|o| {
                        o.verbose_data
                            .as_ref()
                            .is_none_or(|v| v.script_public_key_address.to_string() != address)
                    })
                    .map(|o| o.value)
                    .sum();
                (address, incoming, outgoing)
            })
            .collect())
    }

    /// Balance of one address in sompi (needs the node's UTXO index).
    pub async fn balance(&self, address: RpcAddress) -> Result<u64> {
        Ok(self.client.get_balance_by_address(address).await?)
    }

    /// Run a read-only RPC method from the RPC Cmds tab, truncating long responses so
    /// the result viewer stays responsive.
    pub async fn execute_rpc_call(&self, method: &str, args: &[String]) -> Result<String> {
        Ok(truncate_response(self.rpc_json(method, args).await?))
    }

    /// Run a read-only RPC method and return its full pretty-printed JSON response.
    /// Missing or empty arguments take the defaults declared in `methods::RPC_METHODS`.
    pub async fn rpc_json(&self, method: &str, args: &[String]) -> Result<String> {
        let spec = methods::find(method)
            .ok_or_else(|| anyhow::anyhow!("Unknown RPC method: '{}'", method))?;
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
                to_json(&json!({ "networkHashesPerSecond": self.estimate_hashrate().await? }))?
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
        Ok(out)
    }

    /// Network hashrate (hashes per second) estimated over the last 1000 blocks.
    pub async fn estimate_hashrate(&self) -> Result<u64> {
        let dag = self.client.get_block_dag_info().await?;
        Ok(self
            .client
            .estimate_network_hashes_per_second(1000, Some(dag.sink))
            .await?)
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

    /// The next batch of selected chain block hashes after `start_hash` (VSPC v1 without
    /// transaction IDs: a cheap way to walk the chain).
    pub async fn fetch_chain_hashes(&self, start_hash: RpcHash) -> Result<Vec<RpcHash>> {
        let response = self
            .client
            .get_virtual_chain_from_block(start_hash, false, None)
            .await?;
        Ok(response.added_chain_block_hashes.to_vec())
    }

    /// A block's header timestamp in milliseconds.
    pub async fn get_block_timestamp(&self, hash: RpcHash) -> Result<u64> {
        Ok(self.client.get_block(hash, false).await?.header.timestamp)
    }

    /// Get the pruning point hash from block DAG info.
    pub async fn get_pruning_point_hash(&self) -> Result<RpcHash> {
        let dag = self.client.get_block_dag_info().await?;
        Ok(dag.pruning_point_hash)
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

    /// Every method in `RPC_METHODS` must have a match arm in `rpc_json`.
    /// Checked against the source so a missing arm fails without a live node.
    #[test]
    fn all_rpc_methods_have_handler() {
        let src = include_str!("client.rs");
        let start = src.find("pub async fn rpc_json").unwrap();
        let end = start + src[start..].find("No handler for RPC method").unwrap();
        let body = &src[start..end];
        for m in RPC_METHODS {
            assert!(
                body.contains(&format!("\"{}\" =>", m.name)),
                "RPC method '{}' has no handler in rpc_json",
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
