use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::Path;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::app::TimeWindow;
use crate::tx_inspect::{OpcodeUsage, ScriptClass, TransactionProtocol};

// --- Metrics ---

/// Output counts by script class.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptClassCounts {
    pub pubkey: u64,
    pub pubkey_ecdsa: u64,
    pub script_hash: u64,
    pub nonstandard: u64,
}

impl ScriptClassCounts {
    pub fn record(&mut self, class: ScriptClass) {
        match class {
            ScriptClass::PubKey => self.pubkey += 1,
            ScriptClass::PubKeyEcdsa => self.pubkey_ecdsa += 1,
            ScriptClass::ScriptHash => self.script_hash += 1,
            ScriptClass::NonStandard => self.nonstandard += 1,
        }
    }

    fn merge(&mut self, other: &Self) {
        self.pubkey += other.pubkey;
        self.pubkey_ecdsa += other.pubkey_ecdsa;
        self.script_hash += other.script_hash;
        self.nonstandard += other.nonstandard;
    }
}

/// Covenant-era activity: transactions using covenant opcodes, and covenant-bound
/// outputs. The ZK tag counts overlap and are subsets of `zk_precompile_txs`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectionCounts {
    pub introspection_txs: u64,
    pub zk_precompile_txs: u64,
    pub zk_groth16_txs: u64,
    pub zk_r0succinct_txs: u64,
    pub chainblock_seqcommit_txs: u64,
    pub covenant_creating_txs: u64,
    pub covenant_outputs_created: u64,
    /// A lower bound: only inputs whose spent UTXO the node resolved count.
    pub covenant_outputs_spent: u64,
}

impl InspectionCounts {
    /// Record one transaction, given the opcodes found across its scripts and its
    /// covenant-bound outputs created and spent.
    pub fn record_tx(&mut self, usage: OpcodeUsage, created: u64, spent: u64) {
        self.introspection_txs += usage.introspection as u64;
        self.zk_precompile_txs += usage.zk_precompile() as u64;
        self.zk_groth16_txs += usage.zk_groth16 as u64;
        self.zk_r0succinct_txs += usage.zk_r0succinct as u64;
        self.chainblock_seqcommit_txs += usage.chainblock_seqcommit as u64;
        self.covenant_creating_txs += (created > 0) as u64;
        self.covenant_outputs_created += created;
        self.covenant_outputs_spent += spent;
    }

    fn merge(&mut self, other: &Self) {
        self.introspection_txs += other.introspection_txs;
        self.zk_precompile_txs += other.zk_precompile_txs;
        self.zk_groth16_txs += other.zk_groth16_txs;
        self.zk_r0succinct_txs += other.zk_r0succinct_txs;
        self.chainblock_seqcommit_txs += other.chainblock_seqcommit_txs;
        self.covenant_creating_txs += other.covenant_creating_txs;
        self.covenant_outputs_created += other.covenant_outputs_created;
        self.covenant_outputs_spent += other.covenant_outputs_spent;
    }
}

/// Everything counted for a chain block, a time bucket, or a whole window.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Metrics {
    pub chain_blocks: u64,
    /// Accepted non-coinbase transactions.
    pub tx_count: u64,
    /// Transactions whose fee is known (every input's UTXO resolved).
    pub fee_tx_count: u64,
    /// Sum of known fees, in sompi.
    pub total_fees: u64,
    pub script_classes: ScriptClassCounts,
    pub inspection: InspectionCounts,
    pub protocols: HashMap<TransactionProtocol, u64>,
    /// Chain-block coinbases by miner node version (`""` = not set).
    pub node_versions: HashMap<String, u64>,
    pub senders: HashMap<String, u64>,
    pub receivers: HashMap<String, u64>,
}

impl Metrics {
    pub fn record_fee(&mut self, fee: Option<u64>) {
        if let Some(fee) = fee {
            self.fee_tx_count += 1;
            self.total_fees += fee;
        }
    }

    fn merge(&mut self, other: &Self) {
        self.chain_blocks += other.chain_blocks;
        self.tx_count += other.tx_count;
        self.fee_tx_count += other.fee_tx_count;
        self.total_fees += other.total_fees;
        self.script_classes.merge(&other.script_classes);
        self.inspection.merge(&other.inspection);
        add_counts(&mut self.protocols, &other.protocols);
        add_counts(&mut self.node_versions, &other.node_versions);
        add_counts(&mut self.senders, &other.senders);
        add_counts(&mut self.receivers, &other.receivers);
    }
}

fn add_counts<K: Clone + Eq + std::hash::Hash>(into: &mut HashMap<K, u64>, from: &HashMap<K, u64>) {
    for (key, count) in from {
        *into.entry(key.clone()).or_insert(0) += count;
    }
}

// --- Data Structures ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockSummary {
    pub hash: String,
    pub timestamp_ms: u64,
    pub metrics: Metrics,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeBucket {
    pub bucket_start_ms: u64,
    pub metrics: Metrics,
}

impl TimeBucket {
    fn new(bucket_start_ms: u64) -> Self {
        Self {
            bucket_start_ms,
            metrics: Metrics::default(),
        }
    }

    fn merge_block(&mut self, block: &BlockSummary) {
        self.metrics.merge(&block.metrics);
        // Amortized cap: only sort when 2x over limit
        for map in [&mut self.metrics.senders, &mut self.metrics.receivers] {
            if map.len() > MAX_ADDRESSES_PER_BUCKET * 2 {
                cap_hashmap(map, MAX_ADDRESSES_PER_BUCKET);
            }
        }
    }
}

fn cap_hashmap(map: &mut HashMap<String, u64>, max_entries: usize) {
    if map.len() <= max_entries {
        return;
    }
    let mut entries: Vec<(String, u64)> = map.drain().collect();
    entries.sort_unstable_by_key(|a| std::cmp::Reverse(a.1));
    entries.truncate(max_entries);
    *map = entries.into_iter().collect();
}

// --- Analytics Engine ---

const ONE_MINUTE_MS: u64 = 60_000;
const ONE_HOUR_MS: u64 = 3_600_000;
const TEN_MINUTES_MS: u64 = 600_000;
const TWENTY_FOUR_HOURS_MS: u64 = 86_400_000;
const MAX_MINUTE_BUCKETS: usize = 60;
const MAX_TEN_MINUTE_BUCKETS: usize = 144;
const MAX_ADDRESSES_PER_BUCKET: usize = 100;
const TOP_ADDRESSES: usize = 20;
/// Written before the engine in the cache file; bump when the format changes so
/// an old cache is discarded instead of misread.
const CACHE_MAGIC: u64 = 0x7475_6934_6b61_7302;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalyticsEngine {
    pub recent_blocks: IndexMap<String, BlockSummary>,
    pub minute_buckets: VecDeque<TimeBucket>,
    pub ten_minute_buckets: VecDeque<TimeBucket>,
    pub total_blocks_processed: u64,
    pub total_transactions: u64,
    pub last_known_chain_block: Option<String>,
}

impl AnalyticsEngine {
    pub fn new() -> Self {
        Self {
            recent_blocks: IndexMap::new(),
            minute_buckets: VecDeque::new(),
            ten_minute_buckets: VecDeque::new(),
            total_blocks_processed: 0,
            total_transactions: 0,
            last_known_chain_block: None,
        }
    }

    pub fn add_block(&mut self, summary: BlockSummary) {
        self.total_blocks_processed += 1;
        self.total_transactions += summary.metrics.tx_count;
        self.last_known_chain_block = Some(summary.hash.clone());
        self.recent_blocks.insert(summary.hash.clone(), summary);
    }

    /// Remove a block from the recent cache (reorg handling).
    /// Returns `true` if the block was in the recent cache and removed.
    /// Returns `false` if the block was already finalized into time buckets (not in cache).
    /// When `false`, global counters are NOT decremented because the bucket data cannot
    /// be unwound — the caller should notify the user via analytics_reorg_notification.
    pub fn remove_block(&mut self, hash: &str) -> bool {
        if let Some(block) = self.recent_blocks.swap_remove(hash) {
            self.total_blocks_processed = self.total_blocks_processed.saturating_sub(1);
            self.total_transactions = self
                .total_transactions
                .saturating_sub(block.metrics.tx_count);
            true
        } else {
            false
        }
    }

    /// Move blocks older than 1 minute from recent cache into time buckets.
    pub fn finalize_old_blocks(&mut self, now_ms: u64) {
        let cutoff = now_ms.saturating_sub(ONE_MINUTE_MS);

        let to_finalize: Vec<String> = self
            .recent_blocks
            .iter()
            .filter(|(_, b)| b.timestamp_ms < cutoff)
            .map(|(hash, _)| hash.clone())
            .collect();

        for hash in to_finalize {
            if let Some(block) = self.recent_blocks.swap_remove(&hash) {
                add_to_bucket(&mut self.minute_buckets, ONE_MINUTE_MS, &block);
                add_to_bucket(&mut self.ten_minute_buckets, TEN_MINUTES_MS, &block);
            }
        }
    }

    /// Prune old buckets and cap address maps.
    pub fn prune_buckets(&mut self, now_ms: u64) {
        let hour_cutoff = now_ms.saturating_sub(ONE_HOUR_MS);
        self.minute_buckets
            .retain(|b| b.bucket_start_ms >= hour_cutoff);

        let day_cutoff = now_ms.saturating_sub(TWENTY_FOUR_HOURS_MS);
        self.ten_minute_buckets
            .retain(|b| b.bucket_start_ms >= day_cutoff);

        // Enforce max bucket counts
        while self.minute_buckets.len() > MAX_MINUTE_BUCKETS {
            self.minute_buckets.pop_front();
        }
        while self.ten_minute_buckets.len() > MAX_TEN_MINUTE_BUCKETS {
            self.ten_minute_buckets.pop_front();
        }
    }

    /// Views for every window, indexed like [`TimeWindow::ALL`].
    pub fn views(&self, now_ms: u64) -> [AggregatedView; 3] {
        TimeWindow::ALL.map(|w| self.get_view(w, now_ms))
    }

    /// Aggregate a window ending at `now_ms`. Blocks from the last minute are
    /// still in the recent cache, so every window includes them.
    pub fn get_view(&self, window: TimeWindow, now_ms: u64) -> AggregatedView {
        let start_ms = now_ms.saturating_sub(window.duration_ms());
        let buckets = match window {
            TimeWindow::OneMin => None,
            TimeWindow::OneHour => Some(&self.minute_buckets),
            TimeWindow::TwentyFourHour => Some(&self.ten_minute_buckets),
        };
        let items = buckets
            .into_iter()
            .flatten()
            .map(|b| (b.bucket_start_ms, &b.metrics))
            .chain(
                self.recent_blocks
                    .values()
                    .filter(|b| b.timestamp_ms >= start_ms)
                    .map(|b| (b.timestamp_ms, &b.metrics)),
            );
        build_aggregated_view(window, now_ms, items)
    }

    // --- Persistence ---

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let data = bincode::serialize(&(CACHE_MAGIC, self))?;
        std::fs::write(path, data)?;
        Ok(())
    }

    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let data = std::fs::read(path)?;
        let (magic, engine): (u64, Self) = bincode::deserialize(&data)?;
        anyhow::ensure!(magic == CACHE_MAGIC, "outdated analytics cache");
        Ok(engine)
    }
}

/// Merge a block into the bucket covering its timestamp. Chain blocks arrive
/// nearly in time order, so only the last few buckets are searched.
fn add_to_bucket(buckets: &mut VecDeque<TimeBucket>, width_ms: u64, block: &BlockSummary) {
    let start = block.timestamp_ms / width_ms * width_ms;
    if let Some(bucket) = buckets
        .iter_mut()
        .rev()
        .take(3)
        .find(|b| b.bucket_start_ms == start)
    {
        bucket.merge_block(block);
        return;
    }
    let mut bucket = TimeBucket::new(start);
    bucket.merge_block(block);
    // Keep buckets sorted so pruning from the front stays correct.
    let at = buckets.partition_point(|b| b.bucket_start_ms < start);
    buckets.insert(at, bucket);
}

fn build_aggregated_view<'a>(
    window: TimeWindow,
    now_ms: u64,
    items: impl Iterator<Item = (u64, &'a Metrics)>,
) -> AggregatedView {
    let bin_ms = window.series_bin_ms();
    let start_ms = now_ms.saturating_sub(window.duration_ms());
    // Zero-filled bins so gaps show as empty bars.
    let mut bins: BTreeMap<u64, u64> = (start_ms / bin_ms..=now_ms / bin_ms)
        .map(|i| (i * bin_ms, 0))
        .collect();

    let mut total = Metrics::default();
    let mut earliest_ms = None::<u64>;
    for (ts, metrics) in items {
        total.merge(metrics);
        earliest_ms = Some(earliest_ms.map_or(ts, |e| e.min(ts)));
        if let Some(bin) = bins.get_mut(&(ts / bin_ms * bin_ms)) {
            *bin += metrics.tx_count;
        }
    }

    let covered_ms = earliest_ms.map_or(0, |e| now_ms.saturating_sub(e));
    AggregatedView {
        window,
        covered_secs: covered_ms.min(window.duration_ms()) / 1000,
        top_senders: top_n_sorted(std::mem::take(&mut total.senders), TOP_ADDRESSES),
        top_receivers: top_n_sorted(std::mem::take(&mut total.receivers), TOP_ADDRESSES),
        node_versions: top_n_sorted(std::mem::take(&mut total.node_versions), usize::MAX),
        tx_series: bins.into_iter().collect(),
        series_bin_ms: bin_ms,
        totals: total,
    }
}

fn top_n_sorted(map: HashMap<String, u64>, n: usize) -> Vec<(String, u64)> {
    let mut entries: Vec<(String, u64)> = map.into_iter().collect();
    entries.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    entries.truncate(n);
    entries
}

// --- Aggregated View (for UI consumption) ---

#[derive(Debug, Clone, Default)]
pub struct AggregatedView {
    pub window: TimeWindow,
    /// Window totals. The address and node-version maps are moved into the
    /// sorted lists below and left empty.
    pub totals: Metrics,
    /// How much of the window has data, in seconds (less than the window right
    /// after a fresh start).
    pub covered_secs: u64,
    pub top_senders: Vec<(String, u64)>,
    pub top_receivers: Vec<(String, u64)>,
    /// Chain blocks by node version, most first.
    pub node_versions: Vec<(String, u64)>,
    /// Transactions per bin: `(bin_start_ms, count)`, oldest first.
    pub tx_series: Vec<(u64, u64)>,
    pub series_bin_ms: u64,
}

impl AggregatedView {
    /// Transactions per second over the covered part of the window.
    pub fn tps(&self) -> Option<f64> {
        (self.covered_secs > 0).then(|| self.totals.tx_count as f64 / self.covered_secs as f64)
    }

    /// Mean fee per transaction with a known fee, in sompi.
    pub fn avg_fee(&self) -> Option<f64> {
        let t = &self.totals;
        (t.fee_tx_count > 0).then(|| t.total_fees as f64 / t.fee_tx_count as f64)
    }

    pub fn protocol_count(&self, protocol: TransactionProtocol) -> u64 {
        self.totals.protocols.get(&protocol).copied().unwrap_or(0)
    }

    /// Blocks with a known node version entry, i.e. the share denominator.
    pub fn node_version_total(&self) -> u64 {
        self.node_versions.iter().map(|(_, c)| c).sum()
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    /// A chain block with `tx_count` transactions, all paying `fee` sompi.
    fn make_block(hash: &str, timestamp_ms: u64, tx_count: u64, fee: u64) -> BlockSummary {
        BlockSummary {
            hash: hash.to_string(),
            timestamp_ms,
            metrics: Metrics {
                chain_blocks: 1,
                tx_count,
                fee_tx_count: tx_count,
                total_fees: fee * tx_count,
                senders: HashMap::from([("sender1".to_string(), tx_count)]),
                receivers: HashMap::from([("receiver1".to_string(), tx_count)]),
                ..Default::default()
            },
        }
    }

    fn make_block_with(
        hash: &str,
        timestamp_ms: u64,
        edit: impl FnOnce(&mut Metrics),
    ) -> BlockSummary {
        let mut block = make_block(hash, timestamp_ms, 1, 100);
        edit(&mut block.metrics);
        block
    }

    #[test]
    fn add_and_remove_block() {
        let mut engine = AnalyticsEngine::new();
        engine.add_block(make_block("hash1", 1000, 5, 100));

        assert_eq!(engine.total_blocks_processed, 1);
        assert_eq!(engine.total_transactions, 5);
        assert_eq!(engine.recent_blocks.len(), 1);

        assert!(engine.remove_block("hash1"));
        assert_eq!(engine.total_blocks_processed, 0);
        assert_eq!(engine.total_transactions, 0);
        assert!(engine.recent_blocks.is_empty());
    }

    #[test]
    fn remove_nonexistent_block_returns_false() {
        let mut engine = AnalyticsEngine::new();
        assert!(!engine.remove_block("doesnt_exist"));
    }

    #[test]
    fn finalize_old_blocks_moves_to_buckets() {
        let mut engine = AnalyticsEngine::new();
        let now = 120_000u64; // 2 minutes
        engine.add_block(make_block("old", 10_000, 3, 50)); // very old
        engine.add_block(make_block("recent", now - 30_000, 2, 100)); // 30s ago

        engine.finalize_old_blocks(now);

        assert!(!engine.recent_blocks.contains_key("old"));
        assert!(engine.recent_blocks.contains_key("recent"));
        assert_eq!(engine.minute_buckets.len(), 1);
        assert_eq!(engine.ten_minute_buckets.len(), 1);
    }

    #[test]
    fn late_block_merges_into_its_bucket_in_order() {
        let mut engine = AnalyticsEngine::new();
        engine.add_block(make_block("b2", 130_000, 1, 1));
        engine.add_block(make_block("b3", 190_000, 1, 1));
        engine.finalize_old_blocks(1_000_000);
        // Arrives after a later bucket exists
        engine.add_block(make_block("b1", 70_000, 1, 1));
        engine.add_block(make_block("b2b", 150_000, 4, 1));
        engine.finalize_old_blocks(1_000_000);

        let starts: Vec<u64> = engine
            .minute_buckets
            .iter()
            .map(|b| b.bucket_start_ms)
            .collect();
        assert_eq!(starts, vec![60_000, 120_000, 180_000]);
        assert_eq!(engine.minute_buckets[1].metrics.tx_count, 5);
    }

    #[test]
    fn prune_buckets_removes_old() {
        let mut engine = AnalyticsEngine::new();
        let now = TWENTY_FOUR_HOURS_MS + ONE_HOUR_MS + 1000;

        engine
            .minute_buckets
            .push_back(TimeBucket::new(now - ONE_HOUR_MS - 1000));
        engine
            .minute_buckets
            .push_back(TimeBucket::new(now - 30_000));
        engine
            .ten_minute_buckets
            .push_back(TimeBucket::new(now - TWENTY_FOUR_HOURS_MS - 1000));
        engine
            .ten_minute_buckets
            .push_back(TimeBucket::new(now - 1000));

        engine.prune_buckets(now);

        assert_eq!(engine.minute_buckets.len(), 1);
        assert_eq!(engine.ten_minute_buckets.len(), 1);
    }

    #[test]
    fn one_min_view_uses_recent_blocks() {
        let mut engine = AnalyticsEngine::new();
        let now = 100_000;
        engine.add_block(make_block("b1", now - 1000, 5, 100));
        engine.add_block(make_block("b2", now - 2000, 3, 200));
        // Not finalized yet, but outside the window
        engine.add_block(make_block("stale", now - 61_000, 7, 1));

        let view = engine.get_view(TimeWindow::OneMin, now);
        assert_eq!(view.totals.tx_count, 8);
        assert_eq!(view.totals.total_fees, 5 * 100 + 3 * 200);
        assert_eq!(view.totals.chain_blocks, 2);
        assert_eq!(view.top_senders, vec![("sender1".to_string(), 8)]);
        assert!(view.totals.senders.is_empty());
    }

    #[test]
    fn hour_view_combines_buckets_and_recent_blocks() {
        let mut engine = AnalyticsEngine::new();
        let now = 10 * ONE_MINUTE_MS;
        let mut bucket = TimeBucket::new(5 * ONE_MINUTE_MS);
        bucket.metrics.tx_count = 10;
        engine.minute_buckets.push_back(bucket);
        engine.add_block(make_block("b1", now - 1000, 2, 1));

        let view = engine.get_view(TimeWindow::OneHour, now);
        assert_eq!(view.totals.tx_count, 12);
        // Data starts 5 minutes ago
        assert_eq!(view.covered_secs, 300);
        assert!((view.tps().unwrap() - 12.0 / 300.0).abs() < 1e-9);
    }

    #[test]
    fn tx_series_is_zero_filled_per_bin() {
        let mut engine = AnalyticsEngine::new();
        let now = 2 * ONE_HOUR_MS + 30 * ONE_MINUTE_MS;
        let mut bucket = TimeBucket::new(2 * ONE_HOUR_MS + 10 * ONE_MINUTE_MS);
        bucket.metrics.tx_count = 7;
        engine.ten_minute_buckets.push_back(bucket);
        let mut bucket = TimeBucket::new(2 * ONE_HOUR_MS + 20 * ONE_MINUTE_MS);
        bucket.metrics.tx_count = 3;
        engine.ten_minute_buckets.push_back(bucket);

        let view = engine.get_view(TimeWindow::TwentyFourHour, now);
        assert_eq!(view.series_bin_ms, ONE_HOUR_MS);
        // Hours 0, 1 and 2 (the partial current hour)
        assert_eq!(
            view.tx_series,
            vec![(0, 0), (ONE_HOUR_MS, 0), (2 * ONE_HOUR_MS, 10)]
        );
    }

    #[test]
    fn empty_view_has_no_rates() {
        let view = AnalyticsEngine::new().get_view(TimeWindow::OneHour, ONE_HOUR_MS);
        assert_eq!(view.tps(), None);
        assert_eq!(view.avg_fee(), None);
    }

    #[test]
    fn avg_fee_ignores_txs_without_fee() {
        let block = make_block_with("b1", 1000, |m| {
            m.tx_count = 4;
            m.fee_tx_count = 2;
            m.total_fees = 3000;
        });
        let mut engine = AnalyticsEngine::new();
        engine.add_block(block);
        let view = engine.get_view(TimeWindow::OneMin, 2000);
        assert_eq!(view.avg_fee(), Some(1500.0));
    }

    #[test]
    fn protocol_and_node_version_counts_in_view() {
        let mut engine = AnalyticsEngine::new();
        engine.add_block(make_block_with("b1", 1000, |m| {
            m.protocols.insert(TransactionProtocol::Krc, 2);
            m.node_versions.insert("1.0.1".into(), 1);
        }));
        engine.add_block(make_block_with("b2", 2000, |m| {
            m.protocols.insert(TransactionProtocol::Kns, 1);
            m.node_versions.insert("1.0.1".into(), 1);
        }));
        engine.add_block(make_block_with("b3", 3000, |m| {
            m.node_versions.insert("".into(), 1);
        }));

        let view = engine.get_view(TimeWindow::OneMin, 4000);
        assert_eq!(view.protocol_count(TransactionProtocol::Krc), 2);
        assert_eq!(view.protocol_count(TransactionProtocol::Kns), 1);
        assert_eq!(view.protocol_count(TransactionProtocol::Igra), 0);
        assert_eq!(
            view.node_versions,
            vec![("1.0.1".to_string(), 2), (String::new(), 1)]
        );
        assert_eq!(view.node_version_total(), 3);
    }

    #[test]
    fn inspection_counts_record_tx_flags() {
        let mut counts = InspectionCounts::default();
        let usage = OpcodeUsage {
            introspection: true,
            chainblock_seqcommit: true,
            zk_groth16: true,
            ..Default::default()
        };
        counts.record_tx(usage, 2, 1);
        counts.record_tx(OpcodeUsage::default(), 0, 3);

        assert_eq!(counts.introspection_txs, 1);
        assert_eq!(counts.chainblock_seqcommit_txs, 1);
        assert_eq!(counts.zk_precompile_txs, 1);
        assert_eq!(counts.zk_groth16_txs, 1);
        assert_eq!(counts.zk_r0succinct_txs, 0);
        assert_eq!(counts.covenant_creating_txs, 1);
        assert_eq!(counts.covenant_outputs_created, 2);
        assert_eq!(counts.covenant_outputs_spent, 4);
    }

    #[test]
    fn last_known_chain_block_tracks_last_added() {
        let mut engine = AnalyticsEngine::new();
        engine.add_block(make_block("first", 1000, 1, 10));
        assert_eq!(engine.last_known_chain_block, Some("first".to_string()));
        engine.add_block(make_block("second", 2000, 1, 10));
        assert_eq!(engine.last_known_chain_block, Some("second".to_string()));
    }

    #[test]
    fn persistence_round_trip() {
        let mut engine = AnalyticsEngine::new();
        engine.add_block(make_block("b1", 1000, 5, 100));
        engine.add_block(make_block_with("b2", 2000, |m| {
            m.protocols.insert(TransactionProtocol::Krc, 1);
        }));

        let dir = std::env::temp_dir().join("tui4kas_test_analytics");
        let path = dir.join("test_cache.bin");
        engine.save(&path).unwrap();

        let loaded = AnalyticsEngine::load(&path).unwrap();
        assert_eq!(loaded.total_blocks_processed, 2);
        assert_eq!(loaded.total_transactions, 6);
        assert_eq!(loaded.recent_blocks.len(), 2);
        assert_eq!(loaded.last_known_chain_block, Some("b2".to_string()));

        // A cache written in another format is rejected
        std::fs::write(&path, bincode::serialize(&(1u64, 2u64)).unwrap()).unwrap();
        assert!(AnalyticsEngine::load(&path).is_err());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cap_hashmap_limits_entries() {
        let mut map: HashMap<String, u64> = (0..200).map(|i| (format!("addr{i}"), i)).collect();
        cap_hashmap(&mut map, 10);
        assert_eq!(map.len(), 10);
        assert!(map.contains_key("addr199"));
        assert!(map.contains_key("addr190"));
    }

    #[test]
    fn time_bucket_merge_block() {
        let mut bucket = TimeBucket::new(0);
        bucket.merge_block(&make_block("b1", 100, 5, 50));
        bucket.merge_block(&make_block("b2", 200, 3, 100));

        let m = &bucket.metrics;
        assert_eq!(m.chain_blocks, 2);
        assert_eq!(m.tx_count, 8);
        assert_eq!(m.fee_tx_count, 8);
        assert_eq!(m.total_fees, 5 * 50 + 3 * 100);
        assert_eq!(m.senders["sender1"], 8);
    }
}
