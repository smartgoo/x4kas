use kaspa_rpc_core::{
    GetBlockDagInfoResponse, GetBlockRewardInfoResponse, GetCoinSupplyResponse,
    GetServerInfoResponse, RpcBlockColor, RpcFeeEstimate, RpcMempoolEntry,
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
    pub entries: Vec<MempoolEntryInfo>,
    pub total_fees: u64,
}

impl From<Vec<RpcMempoolEntry>> for MempoolState {
    fn from(r: Vec<RpcMempoolEntry>) -> Self {
        let entries: Vec<MempoolEntryInfo> = r.into_iter().map(|e| e.into()).collect();
        let total_fees = entries.iter().map(|e| e.fee).sum();
        Self {
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
    /// Fee rates in sompi/gram: priority, first normal and first low bucket.
    pub priority_feerate: f64,
    pub normal_feerate: Option<f64>,
    pub low_feerate: Option<f64>,
}

impl From<RpcFeeEstimate> for FeeEstimateInfo {
    fn from(r: RpcFeeEstimate) -> Self {
        Self {
            priority_feerate: r.priority_bucket.feerate,
            normal_feerate: r.normal_buckets.first().map(|b| b.feerate),
            low_feerate: r.low_buckets.first().map(|b| b.feerate),
        }
    }
}

/// A block's standing in the DAG, from `get_block_reward_info`: whether it was merged
/// blue or red, how deep it is, and what it earned.
#[derive(Debug, Clone, PartialEq)]
pub struct BlockRewardInfo {
    pub color: BlockColor,
    /// Chain blocks on top of the one that merged it.
    pub confirmations: Option<u64>,
    /// The chain block that merged it.
    pub merging_chain_block: Option<String>,
    /// Sompi paid to its miner (blue blocks only).
    pub reward: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockColor {
    /// Not merged by the selected chain yet.
    Unknown,
    Blue,
    Red,
}

impl BlockColor {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Unknown => "Not merged yet",
            Self::Blue => "Blue",
            Self::Red => "Red",
        }
    }
}

impl From<GetBlockRewardInfoResponse> for BlockRewardInfo {
    fn from(r: GetBlockRewardInfoResponse) -> Self {
        Self {
            color: match r.block_color {
                RpcBlockColor::Unknown => BlockColor::Unknown,
                RpcBlockColor::Blue => BlockColor::Blue,
                RpcBlockColor::Red => BlockColor::Red,
            },
            confirmations: r.confirmation_count,
            merging_chain_block: r.merging_chain_block_hash.map(|h| h.to_string()),
            reward: r.reward_amount,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct MarketData {
    pub price_usd: f64,
    pub price_btc: f64,
    pub market_cap: f64,
    pub volume_24h: f64,
    pub price_change_24h_pct: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaspa_rpc_core::{
        RpcFeerateBucket, RpcHash, RpcMempoolEntry, RpcNetworkId, RpcSubnetworkId, RpcTransaction,
        RpcTransactionVerboseData,
    };
    use std::str::FromStr;

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
        assert!(state.entries.is_empty());
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
        assert_eq!(state.entries.len(), 3);
        assert_eq!(state.total_fees, 400);
    }

    // --- From<RpcFeeEstimate> ---

    #[test]
    fn fee_estimate_takes_first_bucket_rates() {
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
        assert_eq!(info.priority_feerate, 100_000_000.0);
        assert_eq!(info.normal_feerate, Some(50_000_000.0));
        assert_eq!(info.low_feerate, None);
    }
}
