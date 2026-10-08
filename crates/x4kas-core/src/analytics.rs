use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::hash::Hash;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use kaspa_rpc_core::{GetVirtualChainFromBlockV2Response, RpcOptionalTransaction};
use serde::{Deserialize, Serialize};

use crate::app::TimeWindow;
use crate::tx_inspect::{
    OpcodeUsage, ScriptClass, TransactionProtocol, coinbase_miner_tag, coinbase_node_version,
    detect_protocol, output_script_opcodes, redeem_script_opcodes, script_class,
};

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
    /// Accepted coinbase transactions, one per mined block.
    pub mined_blocks: u64,
    /// Transactions whose fee is known (every input's UTXO resolved).
    pub fee_tx_count: u64,
    /// Sum of known fees, in sompi.
    pub total_fees: u64,
    pub script_classes: ScriptClassCounts,
    pub inspection: InspectionCounts,
    pub protocols: HashMap<TransactionProtocol, u64>,
    /// Accepted coinbases (one per mined block) by miner node version (`""` = not set).
    pub node_versions: HashMap<String, u64>,
    /// Accepted coinbases by miner address (the coinbase's first output).
    pub miners: HashMap<String, u64>,
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
        self.mined_blocks += other.mined_blocks;
        self.fee_tx_count += other.fee_tx_count;
        self.total_fees += other.total_fees;
        self.script_classes.merge(&other.script_classes);
        self.inspection.merge(&other.inspection);
        add_counts(&mut self.protocols, &other.protocols);
        add_counts(&mut self.node_versions, &other.node_versions);
        add_counts(&mut self.miners, &other.miners);
        add_counts(&mut self.senders, &other.senders);
        add_counts(&mut self.receivers, &other.receivers);
    }
}

fn add_counts<K: Clone + Eq + Hash>(into: &mut HashMap<K, u64>, from: &HashMap<K, u64>) {
    for (key, count) in from {
        *into.entry(key.clone()).or_insert(0) += count;
    }
}

fn bump<K: Eq + Hash>(counts: &mut HashMap<K, u64>, key: K) {
    *counts.entry(key).or_insert(0) += 1;
}

// --- Ingestion ---

/// The chain blocks a VSPC v2 response adds, summarized, and the hashes it removes.
pub fn summarize_chain_blocks(
    response: &GetVirtualChainFromBlockV2Response,
) -> (Vec<BlockSummary>, Vec<String>) {
    let removed = response
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
            for tx in &chain_block.accepted_transactions {
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

/// The coinbases a VSPC v2 response adds: each miner's payout address (the coinbase's
/// first output) and the miner tag from its payload. Feeds `labels::MinerTally`.
pub fn coinbase_miners(
    response: &GetVirtualChainFromBlockV2Response,
) -> Vec<(String, Option<String>)> {
    response
        .chain_block_accepted_transactions
        .iter()
        .flat_map(|chain_block| chain_block.accepted_transactions.iter())
        .filter(|tx| tx.inputs.is_empty())
        .filter_map(|tx| {
            let miner = tx
                .outputs
                .first()?
                .verbose_data
                .as_ref()?
                .script_public_key_address
                .as_ref()?
                .to_string();
            let tag = tx.payload.as_deref().and_then(coinbase_miner_tag);
            Some((miner, tag))
        })
        .collect()
}

/// Count one accepted transaction into a chain block's metrics.
fn record_transaction(metrics: &mut Metrics, tx: &RpcOptionalTransaction) {
    let payload = tx.payload.as_deref().unwrap_or(&[]);
    let output_address = |i: usize| {
        tx.outputs
            .get(i)?
            .verbose_data
            .as_ref()?
            .script_public_key_address
            .as_ref()
            .map(|a| a.to_string())
    };

    // Coinbase: no inputs. Its payload carries the miner's node version, its first
    // output pays the miner.
    if tx.inputs.is_empty() {
        metrics.mined_blocks += 1;
        if let Some(version) = coinbase_node_version(payload) {
            bump(&mut metrics.node_versions, version);
        }
        if let Some(miner) = output_address(0) {
            bump(&mut metrics.miners, miner);
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
            bump(&mut metrics.senders, addr.to_string());
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
    for (i, output) in tx.outputs.iter().enumerate() {
        output_sum += output.value.unwrap_or(0);
        if let Some(spk) = &output.script_public_key {
            metrics.script_classes.record(script_class(spk.script()));
            usage |= output_script_opcodes(spk.script());
        }
        if output.covenant.as_ref().is_some_and(|c| c.0.is_some()) {
            covenant_created += 1;
        }
        if let Some(addr) = output_address(i) {
            bump(&mut metrics.receivers, addr);
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
        bump(&mut metrics.protocols, proto);
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
        for map in [
            &mut self.metrics.senders,
            &mut self.metrics.receivers,
            &mut self.metrics.miners,
        ] {
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

/// Blocks newer than this stay in `recent_blocks`; older ones go into buckets.
const ONE_MINUTE_MS: u64 = 60_000;
const TEN_MINUTES_MS: u64 = 600_000;
/// Bucket count caps, on top of pruning by age, so buckets dated in the future (a
/// skewed clock) can't pile up.
const MAX_MINUTE_BUCKETS: usize = 60;
const MAX_TEN_MINUTE_BUCKETS: usize = 144;
const MAX_ADDRESSES_PER_BUCKET: usize = 100;
const TOP_ADDRESSES: usize = 20;

/// The two bucket resolutions: one-minute buckets back the 1h window, ten-minute
/// buckets the 24h window (and the transaction histogram).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BucketWidth {
    Minute,
    TenMinute,
}

impl BucketWidth {
    pub const ALL: [BucketWidth; 2] = [BucketWidth::Minute, BucketWidth::TenMinute];

    pub fn ms(self) -> u64 {
        match self {
            BucketWidth::Minute => ONE_MINUTE_MS,
            BucketWidth::TenMinute => TEN_MINUTES_MS,
        }
    }

    fn window(self) -> TimeWindow {
        match self {
            BucketWidth::Minute => TimeWindow::OneHour,
            BucketWidth::TenMinute => TimeWindow::TwentyFourHour,
        }
    }

    fn max_buckets(self) -> usize {
        match self {
            BucketWidth::Minute => MAX_MINUTE_BUCKETS,
            BucketWidth::TenMinute => MAX_TEN_MINUTE_BUCKETS,
        }
    }
}

/// A bucket's identity: its width and start time.
pub type BucketKey = (BucketWidth, u64);

/// What [`AnalyticsEngine::ingest`] changed, so a store can persist exactly that: the
/// recent blocks always change, buckets only when blocks are finalized or pruned.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ingest {
    /// Whether the recent blocks changed (blocks added, removed or finalized).
    pub recent_changed: bool,
    /// Buckets that gained blocks (new or updated).
    pub touched: BTreeSet<BucketKey>,
    /// Buckets pruned out of their window.
    pub removed: BTreeSet<BucketKey>,
    /// Removed chain blocks that were already finalized into buckets, which can't be
    /// unwound; the user should be told (`AnalyticsState::reorg_notification`).
    pub unresolved_reorgs: Vec<String>,
}

/// Rolling chain metrics: the last minute of blocks, then 1-minute and 10-minute
/// buckets for the 1h and 24h windows. Kept by the index writer, which persists it in
/// the index store (`index::analytics`) with every batch.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AnalyticsEngine {
    pub recent_blocks: IndexMap<String, BlockSummary>,
    pub minute_buckets: VecDeque<TimeBucket>,
    pub ten_minute_buckets: VecDeque<TimeBucket>,
}

impl AnalyticsEngine {
    pub fn add_block(&mut self, summary: BlockSummary) {
        self.recent_blocks.insert(summary.hash.clone(), summary);
    }

    /// Remove a block from the recent cache (reorg handling). Returns `false` if the
    /// block was already finalized into time buckets, which can't be unwound.
    pub fn remove_block(&mut self, hash: &str) -> bool {
        self.recent_blocks.swap_remove(hash).is_some()
    }

    /// Fold one VSPC v2 response in: undo its removed chain blocks, add the rest
    /// (except `skip`, chain blocks already seen), then finalize and prune by `now_ms`.
    pub fn ingest(
        &mut self,
        response: &GetVirtualChainFromBlockV2Response,
        skip: &HashSet<String>,
        now_ms: u64,
    ) -> Ingest {
        let (summaries, removed) = summarize_chain_blocks(response);
        let mut ingest = Ingest::default();
        for hash in removed {
            if self.remove_block(&hash) {
                ingest.recent_changed = true;
            } else {
                ingest.unresolved_reorgs.push(hash);
            }
        }
        for summary in summaries {
            if !skip.contains(&summary.hash) {
                self.add_block(summary);
                ingest.recent_changed = true;
            }
        }
        ingest.touched = self.finalize_old_blocks(now_ms);
        ingest.recent_changed |= !ingest.touched.is_empty();
        ingest.removed = self.prune_buckets(now_ms);
        ingest
    }

    /// Move blocks older than 1 minute from the recent cache into time buckets.
    /// Returns the buckets that changed.
    pub fn finalize_old_blocks(&mut self, now_ms: u64) -> BTreeSet<BucketKey> {
        let cutoff = now_ms.saturating_sub(ONE_MINUTE_MS);

        let to_finalize: Vec<String> = self
            .recent_blocks
            .iter()
            .filter(|(_, b)| b.timestamp_ms < cutoff)
            .map(|(hash, _)| hash.clone())
            .collect();

        let mut touched = BTreeSet::new();
        for hash in to_finalize {
            if let Some(block) = self.recent_blocks.swap_remove(&hash) {
                for width in BucketWidth::ALL {
                    let start = add_to_bucket(self.buckets_mut(width), width.ms(), &block);
                    touched.insert((width, start));
                }
            }
        }
        touched
    }

    /// Prune buckets older than their window (and beyond the count caps). Returns the
    /// buckets dropped.
    pub fn prune_buckets(&mut self, now_ms: u64) -> BTreeSet<BucketKey> {
        let mut removed = BTreeSet::new();
        for width in BucketWidth::ALL {
            let cutoff = now_ms.saturating_sub(width.window().duration_ms());
            let buckets = self.buckets_mut(width);
            let keep_from = buckets.partition_point(|b| b.bucket_start_ms < cutoff);
            let over_cap = buckets.len().saturating_sub(width.max_buckets());
            for bucket in buckets.drain(..keep_from.max(over_cap)) {
                removed.insert((width, bucket.bucket_start_ms));
            }
        }
        removed
    }

    pub fn buckets(&self, width: BucketWidth) -> &VecDeque<TimeBucket> {
        match width {
            BucketWidth::Minute => &self.minute_buckets,
            BucketWidth::TenMinute => &self.ten_minute_buckets,
        }
    }

    fn buckets_mut(&mut self, width: BucketWidth) -> &mut VecDeque<TimeBucket> {
        match width {
            BucketWidth::Minute => &mut self.minute_buckets,
            BucketWidth::TenMinute => &mut self.ten_minute_buckets,
        }
    }

    /// The bucket of `width` starting at `start_ms`, if any.
    pub fn bucket(&self, width: BucketWidth, start_ms: u64) -> Option<&TimeBucket> {
        let buckets = self.buckets(width);
        let at = buckets.partition_point(|b| b.bucket_start_ms < start_ms);
        buckets.get(at).filter(|b| b.bucket_start_ms == start_ms)
    }

    /// Put a stored bucket back, keeping the deque sorted (for `index::analytics::load`).
    pub fn insert_bucket(&mut self, width: BucketWidth, bucket: TimeBucket) {
        let buckets = self.buckets_mut(width);
        let at = buckets.partition_point(|b| b.bucket_start_ms < bucket.bucket_start_ms);
        buckets.insert(at, bucket);
    }

    /// Transactions per 10-minute interval over the last 24h, for the Dashboard's bar
    /// chart. Includes the blocks of the last minute, still in the recent cache.
    pub fn tx_histogram(&self, now_ms: u64) -> TxHistogram {
        let width = TEN_MINUTES_MS;
        let slots = MAX_TEN_MINUTE_BUCKETS;
        let start_ms = (now_ms / width).saturating_sub(slots as u64 - 1) * width;
        let mut counts = vec![None; slots];
        let finalized = self
            .ten_minute_buckets
            .iter()
            .map(|b| (b.bucket_start_ms, b.metrics.tx_count));
        let recent = self
            .recent_blocks
            .values()
            .map(|b| (b.timestamp_ms, b.metrics.tx_count));
        for (ts, tx_count) in finalized.chain(recent) {
            let Some(offset) = ts.checked_sub(start_ms) else {
                continue;
            };
            if let Some(count) = counts.get_mut((offset / width) as usize) {
                *count = Some(count.unwrap_or(0) + tx_count);
            }
        }
        // Once the data begins, an interval without a bucket had no transactions.
        if let Some(first) = counts.iter().position(Option::is_some) {
            for count in &mut counts[first..] {
                count.get_or_insert(0);
            }
        }
        TxHistogram {
            start_ms,
            interval_ms: width,
            counts,
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
}

// --- Legacy cache ---

/// Where earlier versions saved the engine between runs; now imported into the index
/// store on first launch (`index::analytics::import_legacy_cache`) and removed.
pub fn legacy_cache_path() -> PathBuf {
    crate::config::data_dir().join("analytics_cache.bin")
}

/// The magic written before the engine in the legacy cache file.
const LEGACY_CACHE_MAGIC: u64 = 0x7475_6934_6b61_7304;

/// The legacy cache file's layout.
#[derive(Serialize, Deserialize)]
struct LegacyCache {
    recent_blocks: IndexMap<String, BlockSummary>,
    minute_buckets: VecDeque<TimeBucket>,
    ten_minute_buckets: VecDeque<TimeBucket>,
    last_known_chain_block: Option<String>,
}

/// Read a legacy cache file: the engine and the hex hash of its last chain block.
pub fn load_legacy_cache(path: &Path) -> anyhow::Result<(AnalyticsEngine, Option<String>)> {
    let data = std::fs::read(path)?;
    let (magic, cache): (u64, LegacyCache) = bincode::deserialize(&data)?;
    anyhow::ensure!(magic == LEGACY_CACHE_MAGIC, "outdated analytics cache");
    let engine = AnalyticsEngine {
        recent_blocks: cache.recent_blocks,
        minute_buckets: cache.minute_buckets,
        ten_minute_buckets: cache.ten_minute_buckets,
    };
    Ok((engine, cache.last_known_chain_block))
}

/// Write a cache file in the legacy layout (for tests of the import).
#[cfg(test)]
pub(crate) fn save_legacy_cache(
    path: &Path,
    engine: &AnalyticsEngine,
    last_chain_block: Option<String>,
) -> anyhow::Result<()> {
    let cache = LegacyCache {
        recent_blocks: engine.recent_blocks.clone(),
        minute_buckets: engine.minute_buckets.clone(),
        ten_minute_buckets: engine.ten_minute_buckets.clone(),
        last_known_chain_block: last_chain_block,
    };
    std::fs::write(path, bincode::serialize(&(LEGACY_CACHE_MAGIC, cache))?)?;
    Ok(())
}

/// Transaction counts per fixed interval, oldest first, from
/// [`AnalyticsEngine::tx_histogram`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TxHistogram {
    /// Start of the first interval.
    pub start_ms: u64,
    pub interval_ms: u64,
    /// One count per interval, `None` before the data begins. The last interval is the
    /// current one, still filling.
    pub counts: Vec<Option<u64>>,
}

/// Merge a block into the bucket covering its timestamp and return that bucket's start.
/// Chain blocks arrive nearly in time order, so only the last few buckets are searched.
fn add_to_bucket(buckets: &mut VecDeque<TimeBucket>, width_ms: u64, block: &BlockSummary) -> u64 {
    let start = block.timestamp_ms / width_ms * width_ms;
    if let Some(bucket) = buckets
        .iter_mut()
        .rev()
        .take(3)
        .find(|b| b.bucket_start_ms == start)
    {
        bucket.merge_block(block);
        return start;
    }
    let mut bucket = TimeBucket::new(start);
    bucket.merge_block(block);
    // Keep buckets sorted so pruning from the front stays correct.
    let at = buckets.partition_point(|b| b.bucket_start_ms < start);
    buckets.insert(at, bucket);
    start
}

fn build_aggregated_view<'a>(
    window: TimeWindow,
    now_ms: u64,
    items: impl Iterator<Item = (u64, &'a Metrics)>,
) -> AggregatedView {
    let mut total = Metrics::default();
    let mut earliest_ms = None::<u64>;
    for (ts, metrics) in items {
        total.merge(metrics);
        earliest_ms = Some(earliest_ms.map_or(ts, |e| e.min(ts)));
    }

    let covered_ms = earliest_ms.map_or(0, |e| now_ms.saturating_sub(e));
    AggregatedView {
        covered_secs: covered_ms.min(window.duration_ms()) / 1000,
        top_senders: top_n_sorted(std::mem::take(&mut total.senders), TOP_ADDRESSES),
        top_receivers: top_n_sorted(std::mem::take(&mut total.receivers), TOP_ADDRESSES),
        unique_miners: total.miners.len(),
        top_miners: top_n_sorted(std::mem::take(&mut total.miners), TOP_ADDRESSES),
        node_versions: top_n_sorted(std::mem::take(&mut total.node_versions), usize::MAX),
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
    /// Window totals. The address and node-version maps are moved into the
    /// sorted lists below and left empty.
    pub totals: Metrics,
    /// How much of the window has data, in seconds (less than the window right
    /// after a fresh start).
    pub covered_secs: u64,
    pub top_senders: Vec<(String, u64)>,
    pub top_receivers: Vec<(String, u64)>,
    /// Mined blocks by miner address, most first.
    pub top_miners: Vec<(String, u64)>,
    /// Distinct miner addresses. A lower bound over long windows, where each bucket only
    /// keeps its top addresses.
    pub unique_miners: usize,
    /// Mined blocks by node version, most first.
    pub node_versions: Vec<(String, u64)>,
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

    /// Accepted coinbases with a parsed node version entry, i.e. the share denominator.
    pub fn node_version_total(&self) -> u64 {
        self.node_versions.iter().map(|(_, c)| c).sum()
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    const ONE_HOUR_MS: u64 = 3_600_000;
    const TWENTY_FOUR_HOURS_MS: u64 = 86_400_000;

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
        let mut engine = AnalyticsEngine::default();
        engine.add_block(make_block("hash1", 1000, 5, 100));
        assert_eq!(engine.recent_blocks.len(), 1);

        assert!(engine.remove_block("hash1"));
        assert!(engine.recent_blocks.is_empty());
    }

    #[test]
    fn remove_nonexistent_block_returns_false() {
        let mut engine = AnalyticsEngine::default();
        assert!(!engine.remove_block("doesnt_exist"));
    }

    #[test]
    fn finalize_old_blocks_moves_to_buckets() {
        let mut engine = AnalyticsEngine::default();
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
        let mut engine = AnalyticsEngine::default();
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
        let mut engine = AnalyticsEngine::default();
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
        let mut engine = AnalyticsEngine::default();
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
        let mut engine = AnalyticsEngine::default();
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
    fn miners_ranked_in_view() {
        let mut engine = AnalyticsEngine::default();
        engine.add_block(make_block_with("b1", 1000, |m| {
            m.miners.insert("kaspa:a".into(), 3);
            m.miners.insert("kaspa:b".into(), 1);
        }));
        engine.add_block(make_block_with("b2", 2000, |m| {
            m.miners.insert("kaspa:b".into(), 4);
        }));
        let view = engine.get_view(TimeWindow::OneMin, 3000);
        assert_eq!(view.unique_miners, 2);
        assert_eq!(
            view.top_miners,
            vec![("kaspa:b".to_string(), 5), ("kaspa:a".to_string(), 3)]
        );
    }

    #[test]
    fn empty_view_has_no_rates() {
        let view = AnalyticsEngine::default().get_view(TimeWindow::OneHour, ONE_HOUR_MS);
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
        let mut engine = AnalyticsEngine::default();
        engine.add_block(block);
        let view = engine.get_view(TimeWindow::OneMin, 2000);
        assert_eq!(view.avg_fee(), Some(1500.0));
    }

    #[test]
    fn protocol_and_node_version_counts_in_view() {
        let mut engine = AnalyticsEngine::default();
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
    fn finalize_reports_touched_buckets_and_prune_the_removed() {
        let mut engine = AnalyticsEngine::default();
        let now = 10 * ONE_HOUR_MS;
        // Two blocks in the same minute, one in another ten-minute bucket, one recent.
        engine.add_block(make_block("a", now - 2 * ONE_MINUTE_MS, 1, 10));
        engine.add_block(make_block("b", now - 2 * ONE_MINUTE_MS + 5, 1, 10));
        engine.add_block(make_block("c", now - 30 * ONE_MINUTE_MS, 1, 10));
        engine.add_block(make_block("d", now, 1, 10));
        let touched = engine.finalize_old_blocks(now);
        let minute = |t: u64| (BucketWidth::Minute, t / ONE_MINUTE_MS * ONE_MINUTE_MS);
        let ten = |t: u64| (BucketWidth::TenMinute, t / TEN_MINUTES_MS * TEN_MINUTES_MS);
        assert_eq!(
            touched,
            BTreeSet::from([
                minute(now - 2 * ONE_MINUTE_MS),
                minute(now - 30 * ONE_MINUTE_MS),
                ten(now - 2 * ONE_MINUTE_MS),
                ten(now - 30 * ONE_MINUTE_MS),
            ])
        );
        assert_eq!(engine.recent_blocks.len(), 1);
        assert_eq!(
            engine
                .bucket(BucketWidth::Minute, now - 2 * ONE_MINUTE_MS)
                .map(|b| b.metrics.tx_count),
            Some(2)
        );
        assert!(engine.bucket(BucketWidth::Minute, now).is_none());

        // An hour later the minute buckets are out of their window, the ten-minute
        // buckets are not.
        let removed = engine.prune_buckets(now + ONE_HOUR_MS);
        assert_eq!(
            removed,
            BTreeSet::from([
                minute(now - 2 * ONE_MINUTE_MS),
                minute(now - 30 * ONE_MINUTE_MS)
            ])
        );
        assert!(engine.minute_buckets.is_empty());
        assert_eq!(engine.ten_minute_buckets.len(), 2);
    }

    #[test]
    fn prune_enforces_the_bucket_cap() {
        let mut engine = AnalyticsEngine::default();
        let now = 100 * ONE_HOUR_MS;
        // Future-dated buckets never age out; the cap drops the oldest.
        for i in 0..(MAX_MINUTE_BUCKETS as u64 + 5) {
            engine.insert_bucket(
                BucketWidth::Minute,
                TimeBucket::new(now + i * ONE_MINUTE_MS),
            );
        }
        let removed = engine.prune_buckets(now);
        assert_eq!(removed.len(), 5);
        assert_eq!(removed.iter().next(), Some(&(BucketWidth::Minute, now)));
        assert_eq!(engine.minute_buckets.len(), MAX_MINUTE_BUCKETS);
        assert_eq!(
            engine.minute_buckets[0].bucket_start_ms,
            now + 5 * ONE_MINUTE_MS
        );
    }

    #[test]
    fn insert_bucket_keeps_order() {
        let mut engine = AnalyticsEngine::default();
        engine.insert_bucket(BucketWidth::TenMinute, TimeBucket::new(3 * TEN_MINUTES_MS));
        engine.insert_bucket(BucketWidth::TenMinute, TimeBucket::new(TEN_MINUTES_MS));
        engine.insert_bucket(BucketWidth::TenMinute, TimeBucket::new(2 * TEN_MINUTES_MS));
        let starts: Vec<u64> = engine
            .ten_minute_buckets
            .iter()
            .map(|b| b.bucket_start_ms)
            .collect();
        assert_eq!(
            starts,
            vec![TEN_MINUTES_MS, 2 * TEN_MINUTES_MS, 3 * TEN_MINUTES_MS]
        );
        assert!(
            engine
                .bucket(BucketWidth::TenMinute, 2 * TEN_MINUTES_MS)
                .is_some()
        );
        assert!(engine.bucket(BucketWidth::TenMinute, 0).is_none());
    }

    #[test]
    fn legacy_cache_round_trip() {
        let mut engine = AnalyticsEngine::default();
        engine.add_block(make_block("b1", 1000, 5, 100));
        engine.add_block(make_block_with("b2", 2000, |m| {
            m.protocols.insert(TransactionProtocol::Krc, 1);
        }));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("analytics_cache.bin");
        save_legacy_cache(&path, &engine, Some("b2".to_string())).unwrap();

        let (loaded, last) = load_legacy_cache(&path).unwrap();
        assert_eq!(loaded.recent_blocks.len(), 2);
        assert_eq!(last, Some("b2".to_string()));

        // A cache written in another format is rejected
        std::fs::write(&path, bincode::serialize(&(1u64, 2u64)).unwrap()).unwrap();
        assert!(load_legacy_cache(&path).is_err());
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

    #[test]
    fn tx_histogram_covers_24h_in_10_minute_intervals() {
        let now = 2 * TWENTY_FOUR_HOURS_MS + 5 * 60_000;
        let mut engine = AnalyticsEngine::default();
        // Two hours ago, finalized into a bucket
        engine.add_block(make_block("old", now - 2 * ONE_HOUR_MS, 4, 1));
        engine.finalize_old_blocks(now);
        // Still in the recent cache
        engine.add_block(make_block("new", now - 1_000, 7, 1));

        let h = engine.tx_histogram(now);
        assert_eq!(h.counts.len(), 144);
        assert_eq!(h.interval_ms, TEN_MINUTES_MS);
        assert_eq!(
            h.start_ms + 143 * TEN_MINUTES_MS,
            now / TEN_MINUTES_MS * TEN_MINUTES_MS
        );
        assert_eq!(h.counts[143], Some(7));
        assert_eq!(h.counts[143 - 12], Some(4));
        // No data before the first block; zeros after it
        assert_eq!(h.counts[143 - 13], None);
        assert_eq!(h.counts[143 - 11], Some(0));
    }
}
