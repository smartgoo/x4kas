use kaspa_rpc_core::{
    GetBlockDagInfoResponse, GetCoinSupplyResponse, GetServerInfoResponse, RpcFeeEstimate,
    RpcMempoolEntry,
};

#[derive(Debug, Clone)]
pub struct ServerInfo {
    pub server_version: String,
    pub network_id: String,
    pub is_synced: bool,
    pub virtual_daa_score: u64,
    pub has_utxo_index: bool,
}

impl From<GetServerInfoResponse> for ServerInfo {
    fn from(r: GetServerInfoResponse) -> Self {
        Self {
            server_version: r.server_version,
            network_id: r.network_id.to_string(),
            is_synced: r.is_synced,
            virtual_daa_score: r.virtual_daa_score,
            has_utxo_index: r.has_utxo_index,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DagInfo {
    pub network: String,
    pub block_count: u64,
    pub header_count: u64,
    pub tip_hashes: Vec<String>,
    pub difficulty: f64,
    pub past_median_time: u64,
    pub virtual_parent_hashes: Vec<String>,
    pub pruning_point_hash: String,
    pub virtual_daa_score: u64,
    pub sink: String,
}

impl From<GetBlockDagInfoResponse> for DagInfo {
    fn from(r: GetBlockDagInfoResponse) -> Self {
        Self {
            network: r.network.to_string(),
            block_count: r.block_count,
            header_count: r.header_count,
            tip_hashes: r.tip_hashes.iter().map(|h| h.to_string()).collect(),
            difficulty: r.difficulty,
            past_median_time: r.past_median_time,
            virtual_parent_hashes: r
                .virtual_parent_hashes
                .iter()
                .map(|h| h.to_string())
                .collect(),
            pruning_point_hash: r.pruning_point_hash.to_string(),
            virtual_daa_score: r.virtual_daa_score,
            sink: r.sink.to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct MempoolEntryInfo {
    pub transaction_id: String,
    pub fee: u64,
    pub is_orphan: bool,
}

impl From<RpcMempoolEntry> for MempoolEntryInfo {
    fn from(e: RpcMempoolEntry) -> Self {
        let transaction_id = e
            .transaction
            .verbose_data
            .as_ref()
            .map(|v| v.transaction_id.to_string())
            .unwrap_or_else(|| format!("mass:{}", e.transaction.storage_mass));
        Self {
            transaction_id,
            fee: e.fee,
            is_orphan: e.is_orphan,
        }
    }
}

#[derive(Debug, Clone)]
pub struct MempoolState {
    pub entry_count: usize,
    pub entries: Vec<MempoolEntryInfo>,
    pub total_fees: u64,
}

impl From<Vec<RpcMempoolEntry>> for MempoolState {
    fn from(r: Vec<RpcMempoolEntry>) -> Self {
        let entries: Vec<MempoolEntryInfo> = r.into_iter().map(|e| e.into()).collect();
        let total_fees = entries.iter().map(|e| e.fee).sum();
        let entry_count = entries.len();
        Self {
            entry_count,
            entries,
            total_fees,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CoinSupplyInfo {
    pub max_sompi: u64,
    pub circulating_sompi: u64,
}

impl From<GetCoinSupplyResponse> for CoinSupplyInfo {
    fn from(r: GetCoinSupplyResponse) -> Self {
        Self {
            max_sompi: r.max_sompi,
            circulating_sompi: r.circulating_sompi,
        }
    }
}

#[derive(Debug, Clone)]
pub struct FeeEstimateInfo {
    pub priority_bucket: String,
    pub normal_buckets: Vec<String>,
    pub low_buckets: Vec<String>,
}

impl From<RpcFeeEstimate> for FeeEstimateInfo {
    fn from(r: RpcFeeEstimate) -> Self {
        Self {
            priority_bucket: format!("{:.8} KAS/gram", r.priority_bucket.feerate / 1e8),
            normal_buckets: r
                .normal_buckets
                .iter()
                .map(|b| format!("{:.8} KAS/gram", b.feerate / 1e8))
                .collect(),
            low_buckets: r
                .low_buckets
                .iter()
                .map(|b| format!("{:.8} KAS/gram", b.feerate / 1e8))
                .collect(),
        }
    }
}

pub fn sompi_to_kas(sompi: u64) -> f64 {
    sompi as f64 / 1e8
}

pub fn format_number(n: u64) -> String {
    let s = n.to_string();
    let mut result = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result.chars().rev().collect()
}

pub fn shorten_address(addr: &str, prefix_len: usize, suffix_len: usize) -> String {
    let char_count = addr.chars().count();
    if char_count > prefix_len + suffix_len + 3 {
        let prefix: String = addr.chars().take(prefix_len).collect();
        let suffix: String = addr.chars().skip(char_count - suffix_len).collect();
        format!("{}...{}", prefix, suffix)
    } else {
        addr.to_string()
    }
}

#[derive(Debug, Clone, Default)]
pub struct MiningInfo {
    pub hashrate: f64,
    pub unique_miners: usize,
    pub top_miners: Vec<(String, usize)>,
    pub blocks_analyzed: usize,
}

#[derive(Debug, Clone, Default)]
pub struct MarketData {
    pub price_usd: f64,
    pub price_btc: f64,
    pub market_cap: f64,
    pub volume_24h: f64,
    pub price_change_24h_pct: f64,
}

/// Single source of truth for all RPC methods available in the explorer and command line.
/// Each entry is (method_name, description).
pub const RPC_METHODS: &[(&str, &str)] = &[
    ("get_server_info", "Get server info"),
    ("get_block_dag_info", "Get block DAG info"),
    ("get_block_count", "Get block count"),
    ("get_mempool_entries", "Get mempool entries"),
    ("get_coin_supply", "Get coin supply"),
    ("get_fee_estimate", "Get fee estimate"),
    (
        "get_fee_estimate_experimental",
        "Get experimental fee estimate (verbose)",
    ),
    ("get_connected_peer_info", "Get connected peer info"),
    ("get_peer_addresses", "Get known peer addresses"),
    ("get_current_network", "Get current network type"),
    ("get_sink", "Get sink (virtual selected parent) hash"),
    ("get_sink_blue_score", "Get sink blue score"),
    ("get_info", "Get general node info"),
    ("get_sync_status", "Get sync status"),
    ("get_virtual_chain", "Get virtual selected parent chain"),
    ("get_headers", "Get header count"),
    (
        "estimate_network_hashes_per_second",
        "Estimate network hashrate",
    ),
    ("ping", "Ping the node"),
];

#[cfg(test)]
mod tests {
    use super::*;
    use kaspa_rpc_core::{
        RpcFeerateBucket, RpcHash, RpcMempoolEntry, RpcNetworkId, RpcSubnetworkId, RpcTransaction,
        RpcTransactionVerboseData,
    };
    use std::str::FromStr;

    // --- sompi_to_kas ---

    #[test]
    fn sompi_to_kas_zero() {
        assert_eq!(sompi_to_kas(0), 0.0);
    }

    #[test]
    fn sompi_to_kas_one_kas() {
        assert_eq!(sompi_to_kas(100_000_000), 1.0);
    }

    #[test]
    fn sompi_to_kas_fractional() {
        let result = sompi_to_kas(50_000_000);
        assert!((result - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn sompi_to_kas_large_value() {
        let result = sompi_to_kas(2_900_000_000_000_000_000);
        assert!((result - 29_000_000_000.0).abs() < 1.0);
    }

    // --- From<GetServerInfoResponse> ---

    #[test]
    fn server_info_from_response() {
        let network_id = RpcNetworkId::from_str("mainnet").unwrap();
        let response = GetServerInfoResponse {
            rpc_api_version: 1,
            rpc_api_revision: 0,
            server_version: "0.14.0".to_string(),
            network_id,
            has_utxo_index: true,
            is_synced: true,
            virtual_daa_score: 12345,
        };
        let info: ServerInfo = response.into();
        assert_eq!(info.server_version, "0.14.0");
        assert!(info.is_synced);
        assert!(info.has_utxo_index);
        assert_eq!(info.virtual_daa_score, 12345);
        assert!(info.network_id.contains("mainnet"));
    }

    // --- From<GetBlockDagInfoResponse> ---

    #[test]
    fn dag_info_from_response() {
        let hash1 = RpcHash::from_bytes([1u8; 32]);
        let hash2 = RpcHash::from_bytes([2u8; 32]);
        let network_id = RpcNetworkId::from_str("mainnet").unwrap();
        let response = GetBlockDagInfoResponse::new(
            network_id,
            1000,
            2000,
            vec![hash1, hash2],
            1234.56,
            9999999,
            vec![hash1],
            hash2,
            5000,
            hash1,
        );
        let info: DagInfo = response.into();
        assert_eq!(info.block_count, 1000);
        assert_eq!(info.header_count, 2000);
        assert_eq!(info.tip_hashes.len(), 2);
        assert!((info.difficulty - 1234.56).abs() < f64::EPSILON);
        assert_eq!(info.past_median_time, 9999999);
        assert_eq!(info.virtual_parent_hashes.len(), 1);
        assert_eq!(info.virtual_daa_score, 5000);
    }

    // --- From<GetCoinSupplyResponse> ---

    #[test]
    fn coin_supply_from_response() {
        let response =
            GetCoinSupplyResponse::new(2_900_000_000_000_000_000, 1_000_000_000_000_000_000);
        let info: CoinSupplyInfo = response.into();
        assert_eq!(info.max_sompi, 2_900_000_000_000_000_000);
        assert_eq!(info.circulating_sompi, 1_000_000_000_000_000_000);
    }

    // --- From<RpcMempoolEntry> ---

    fn make_test_transaction(verbose_data: Option<RpcTransactionVerboseData>) -> RpcTransaction {
        RpcTransaction {
            version: 0,
            inputs: vec![],
            outputs: vec![],
            lock_time: 0,
            subnetwork_id: RpcSubnetworkId::from_byte(0),
            gas: 0,
            payload: vec![],
            storage_mass: 42,
            verbose_data,
        }
    }

    #[test]
    fn mempool_entry_without_verbose_data() {
        let entry = RpcMempoolEntry::new(500, make_test_transaction(None), false);
        let info: MempoolEntryInfo = entry.into();
        assert_eq!(info.transaction_id, "mass:42");
        assert_eq!(info.fee, 500);
        assert!(!info.is_orphan);
    }

    #[test]
    fn mempool_entry_with_verbose_data() {
        let tx_id = RpcHash::from_bytes([0xAB; 32]);
        let verbose = RpcTransactionVerboseData {
            transaction_id: tx_id,
            hash: RpcHash::from_bytes([0; 32]),
            compute_mass: 100,
            block_hash: RpcHash::from_bytes([0; 32]),
            block_time: 0,
        };
        let entry = RpcMempoolEntry::new(1000, make_test_transaction(Some(verbose)), true);
        let info: MempoolEntryInfo = entry.into();
        assert!(info.transaction_id.contains("abab"));
        assert_eq!(info.fee, 1000);
        assert!(info.is_orphan);
    }

    // --- From<Vec<RpcMempoolEntry>> ---

    #[test]
    fn mempool_state_empty() {
        let entries: Vec<RpcMempoolEntry> = vec![];
        let state: MempoolState = entries.into();
        assert_eq!(state.entry_count, 0);
        assert_eq!(state.total_fees, 0);
        assert!(state.entries.is_empty());
    }

    #[test]
    fn mempool_state_fee_summation() {
        let entries = vec![
            RpcMempoolEntry::new(100, make_test_transaction(None), false),
            RpcMempoolEntry::new(250, make_test_transaction(None), false),
            RpcMempoolEntry::new(50, make_test_transaction(None), true),
        ];
        let state: MempoolState = entries.into();
        assert_eq!(state.entry_count, 3);
        assert_eq!(state.total_fees, 400);
    }

    // --- From<RpcFeeEstimate> ---

    #[test]
    fn fee_estimate_formatting() {
        let estimate = RpcFeeEstimate {
            priority_bucket: RpcFeerateBucket {
                feerate: 100_000_000.0,
                estimated_seconds: 0.5,
            },
            normal_buckets: vec![RpcFeerateBucket {
                feerate: 50_000_000.0,
                estimated_seconds: 30.0,
            }],
            low_buckets: vec![],
        };
        let info: FeeEstimateInfo = estimate.into();
        assert_eq!(info.priority_bucket, "1.00000000 KAS/gram");
        assert_eq!(info.normal_buckets.len(), 1);
        assert_eq!(info.normal_buckets[0], "0.50000000 KAS/gram");
        assert!(info.low_buckets.is_empty());
    }

    // --- format_number ---

    #[test]
    fn format_number_zero() {
        assert_eq!(format_number(0), "0");
    }

    #[test]
    fn format_number_small() {
        assert_eq!(format_number(999), "999");
    }

    #[test]
    fn format_number_thousands() {
        assert_eq!(format_number(1_000), "1,000");
        assert_eq!(format_number(12_345), "12,345");
    }

    #[test]
    fn format_number_millions() {
        assert_eq!(format_number(1_000_000), "1,000,000");
    }

    // --- shorten_address ---

    #[test]
    fn shorten_address_short() {
        assert_eq!(shorten_address("abcdef", 10, 6), "abcdef");
    }

    #[test]
    fn shorten_address_long() {
        let addr = "kaspa:abcdefghijklmnopqrstuvwxyz0123456789";
        let result = shorten_address(addr, 10, 6);
        assert!(result.contains("..."));
        assert!(result.starts_with("kaspa:abcd"));
        assert!(result.ends_with("456789"));
    }
}
