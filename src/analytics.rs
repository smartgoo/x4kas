use std::collections::HashMap;
use std::path::Path;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

use crate::app::TimeWindow;

// --- Protocol Detection ---

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TransactionProtocol {
    Krc,
    Kns,
    Kasia,
    Kasplex,
    KSocial,
    Igra,
}

impl TransactionProtocol {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Krc => "KRC-20",
            Self::Kns => "KNS",
            Self::Kasia => "Kasia",
            Self::Kasplex => "Kasplex",
            Self::KSocial => "KSocial",
            Self::Igra => "Igra",
        }
    }
}

/// Detect the protocol of a transaction from its payload and input signature scripts.
/// Returns `None` for standard (non-protocol) transactions.
pub fn detect_protocol(payload: &[u8], input_scripts: &[&[u8]]) -> Option<TransactionProtocol> {
    // Payload-based detection
    if !payload.is_empty() {
        if let Ok(s) = std::str::from_utf8(payload) {
            if s.contains("ciph_msg") {
                return Some(TransactionProtocol::Kasia);
            }
            if s.contains("kasplex") {
                return Some(TransactionProtocol::Kasplex);
            }
            if s.starts_with("k:") {
                return Some(TransactionProtocol::KSocial);
            }
        }
        // Byte-level: Igra uses first byte 0x91..=0x97
        if matches!(payload.first(), Some(b) if (0x91..=0x97).contains(b)) {
            return Some(TransactionProtocol::Igra);
        }
    }

    // Input script-based detection (OP_PUSH inspection)
    for script in input_scripts {
        if let Some(proto) = scan_script_for_protocol(script) {
            return Some(proto);
        }
    }

    None
}

/// Scan a signature script for OP_PUSH data containing protocol markers.
fn scan_script_for_protocol(script: &[u8]) -> Option<TransactionProtocol> {
    let mut i = 0;
    while i < script.len() {
        let op = script[i];
        i += 1;

        // OP_PUSH: opcodes 0x01..=0x4b push that many bytes
        let push_len = if (1..=0x4b).contains(&op) {
            op as usize
        } else if op == 0x4c {
            // OP_PUSHDATA1
            if i >= script.len() {
                break;
            }
            let len = script[i] as usize;
            i += 1;
            len
        } else if op == 0x4d {
            // OP_PUSHDATA2
            if i + 1 >= script.len() {
                break;
            }
            let len = u16::from_le_bytes([script[i], script[i + 1]]) as usize;
            i += 2;
            len
        } else {
            continue;
        };

        if i + push_len > script.len() {
            break;
        }

        let data = &script[i..i + push_len];
        if let Ok(s) = std::str::from_utf8(data) {
            if s.contains("kasplex") || s.contains("kspr") {
                return Some(TransactionProtocol::Krc);
            }
            if s.contains("kns") {
                return Some(TransactionProtocol::Kns);
            }
        }

        i += push_len;
    }
    None
}

// --- Data Structures ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockSummary {
    pub hash: String,
    pub timestamp_ms: u64,
    pub tx_count: usize,
    pub total_fees: u64,
    pub min_fee: u64,
    pub max_fee: u64,
    pub fee_count: usize,
    pub sender_counts: HashMap<String, usize>,
    pub receiver_counts: HashMap<String, usize>,
    pub protocol_counts: HashMap<TransactionProtocol, usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeBucket {
    pub bucket_start_ms: u64,
    pub tx_count: usize,
    pub total_fees: u64,
    pub min_fee: u64,
    pub max_fee: u64,
    pub fee_count: usize,
    pub sender_counts: HashMap<String, usize>,
    pub receiver_counts: HashMap<String, usize>,
    pub protocol_counts: HashMap<TransactionProtocol, usize>,
}

impl TimeBucket {
    fn new(bucket_start_ms: u64) -> Self {
        Self {
            bucket_start_ms,
            tx_count: 0,
            total_fees: 0,
            min_fee: u64::MAX,
            max_fee: 0,
            fee_count: 0,
            sender_counts: HashMap::new(),
            receiver_counts: HashMap::new(),
            protocol_counts: HashMap::new(),
        }
    }

    fn merge_block(&mut self, block: &BlockSummary) {
        self.tx_count += block.tx_count;
        self.total_fees += block.total_fees;
        if block.fee_count > 0 {
            self.min_fee = self.min_fee.min(block.min_fee);
            self.max_fee = self.max_fee.max(block.max_fee);
        }
        self.fee_count += block.fee_count;
        for (addr, count) in &block.sender_counts {
            *self.sender_counts.entry(addr.clone()).or_insert(0) += count;
        }
        for (addr, count) in &block.receiver_counts {
            *self.receiver_counts.entry(addr.clone()).or_insert(0) += count;
        }
        for (proto, count) in &block.protocol_counts {
            *self.protocol_counts.entry(*proto).or_insert(0) += count;
        }
        // Amortized cap: only sort when 2x over limit
        if self.sender_counts.len() > MAX_ADDRESSES_PER_BUCKET * 2 {
            cap_hashmap(&mut self.sender_counts, MAX_ADDRESSES_PER_BUCKET);
        }
        if self.receiver_counts.len() > MAX_ADDRESSES_PER_BUCKET * 2 {
            cap_hashmap(&mut self.receiver_counts, MAX_ADDRESSES_PER_BUCKET);
        }
    }
}

fn cap_hashmap(map: &mut HashMap<String, usize>, max_entries: usize) {
    if map.len() <= max_entries {
        return;
    }
    let mut entries: Vec<(String, usize)> = map.drain().collect();
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalyticsEngine {
    pub recent_blocks: IndexMap<String, BlockSummary>,
    pub minute_buckets: VecDeque<TimeBucket>,
    pub ten_minute_buckets: VecDeque<TimeBucket>,
    pub total_blocks_processed: u64,
    pub total_transactions: u64,
    pub last_known_chain_block: Option<String>,
    last_minute_bucket_ts: u64,
    last_ten_minute_bucket_ts: u64,
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
            last_minute_bucket_ts: 0,
            last_ten_minute_bucket_ts: 0,
        }
    }

    pub fn add_block(&mut self, summary: BlockSummary) {
        self.total_blocks_processed += 1;
        self.total_transactions += summary.tx_count as u64;
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
                .saturating_sub(block.tx_count as u64);
            true
        } else {
            false
        }
    }

    /// Move blocks older than 1 minute from recent cache into time buckets.
    pub fn finalize_old_blocks(&mut self, now_ms: u64) {
        let cutoff = now_ms.saturating_sub(ONE_MINUTE_MS);

        // Collect blocks to finalize (those older than cutoff)
        let to_finalize: Vec<String> = self
            .recent_blocks
            .iter()
            .filter(|(_, b)| b.timestamp_ms < cutoff)
            .map(|(hash, _)| hash.clone())
            .collect();

        for hash in to_finalize {
            if let Some(block) = self.recent_blocks.swap_remove(&hash) {
                self.add_to_minute_bucket(&block);
                self.add_to_ten_minute_bucket(&block);
            }
        }
    }

    fn add_to_minute_bucket(&mut self, block: &BlockSummary) {
        let bucket_ts = block.timestamp_ms / ONE_MINUTE_MS * ONE_MINUTE_MS;

        if let Some(last) = self.minute_buckets.back_mut()
            && last.bucket_start_ms == bucket_ts
        {
            last.merge_block(block);
            return;
        }

        let mut bucket = TimeBucket::new(bucket_ts);
        bucket.merge_block(block);
        self.minute_buckets.push_back(bucket);
        self.last_minute_bucket_ts = bucket_ts;
    }

    fn add_to_ten_minute_bucket(&mut self, block: &BlockSummary) {
        let bucket_ts = block.timestamp_ms / TEN_MINUTES_MS * TEN_MINUTES_MS;

        if let Some(last) = self.ten_minute_buckets.back_mut()
            && last.bucket_start_ms == bucket_ts
        {
            last.merge_block(block);
            return;
        }

        let mut bucket = TimeBucket::new(bucket_ts);
        bucket.merge_block(block);
        self.ten_minute_buckets.push_back(bucket);
        self.last_ten_minute_bucket_ts = bucket_ts;
    }

    /// Prune old buckets and cap address maps.
    pub fn prune_buckets(&mut self, now_ms: u64) {
        let hour_cutoff = now_ms.saturating_sub(ONE_HOUR_MS);
        while self
            .minute_buckets
            .front()
            .is_some_and(|b| b.bucket_start_ms < hour_cutoff)
        {
            self.minute_buckets.pop_front();
        }

        let day_cutoff = now_ms.saturating_sub(TWENTY_FOUR_HOURS_MS);
        while self
            .ten_minute_buckets
            .front()
            .is_some_and(|b| b.bucket_start_ms < day_cutoff)
        {
            self.ten_minute_buckets.pop_front();
        }

        // Enforce max bucket counts
        while self.minute_buckets.len() > MAX_MINUTE_BUCKETS {
            self.minute_buckets.pop_front();
        }
        while self.ten_minute_buckets.len() > MAX_TEN_MINUTE_BUCKETS {
            self.ten_minute_buckets.pop_front();
        }
    }

    /// Compute an aggregated view for the UI for a given time window.
    pub fn get_view(&self, window: TimeWindow) -> AggregatedView {
        match window {
            TimeWindow::OneMin => self.aggregate_recent_blocks(),
            TimeWindow::OneHour => self.aggregate_buckets(&self.minute_buckets),
            TimeWindow::TwentyFourHour => self.aggregate_buckets(&self.ten_minute_buckets),
        }
    }

    fn aggregate_recent_blocks(&self) -> AggregatedView {
        let items = self.recent_blocks.values().map(|b| AggregateItem {
            tx_count: b.tx_count,
            total_fees: b.total_fees,
            fee_count: b.fee_count,
            min_fee: b.min_fee,
            max_fee: b.max_fee,
            sender_counts: &b.sender_counts,
            receiver_counts: &b.receiver_counts,
            protocol_counts: &b.protocol_counts,
            timestamp_ms: b.timestamp_ms as f64,
        });
        let mut view = build_aggregated_view(items);
        // Sort time series by timestamp for recent blocks (may arrive unordered)
        view.fee_over_time
            .sort_unstable_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        view.tx_over_time
            .sort_unstable_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        view
    }

    fn aggregate_buckets(&self, buckets: &VecDeque<TimeBucket>) -> AggregatedView {
        let items = buckets.iter().map(|b| AggregateItem {
            tx_count: b.tx_count,
            total_fees: b.total_fees,
            fee_count: b.fee_count,
            min_fee: b.min_fee,
            max_fee: b.max_fee,
            sender_counts: &b.sender_counts,
            receiver_counts: &b.receiver_counts,
            protocol_counts: &b.protocol_counts,
            timestamp_ms: b.bucket_start_ms as f64,
        });
        build_aggregated_view(items)
    }

    // --- Persistence ---

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let data = bincode::serialize(self)?;
        std::fs::write(path, data)?;
        Ok(())
    }

    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let data = std::fs::read(path)?;
        let engine: Self = bincode::deserialize(&data)?;
        Ok(engine)
    }
}

struct AggregateItem<'a> {
    tx_count: usize,
    total_fees: u64,
    fee_count: usize,
    min_fee: u64,
    max_fee: u64,
    sender_counts: &'a HashMap<String, usize>,
    receiver_counts: &'a HashMap<String, usize>,
    protocol_counts: &'a HashMap<TransactionProtocol, usize>,
    timestamp_ms: f64,
}

fn build_aggregated_view<'a>(items: impl Iterator<Item = AggregateItem<'a>>) -> AggregatedView {
    let mut view = AggregatedView::default();
    let mut sender_totals: HashMap<String, usize> = HashMap::new();
    let mut receiver_totals: HashMap<String, usize> = HashMap::new();
    let mut protocol_totals: HashMap<TransactionProtocol, usize> = HashMap::new();

    for item in items {
        view.blocks_analyzed += 1;
        view.tx_count += item.tx_count;
        view.total_fees += item.total_fees;
        view.fee_count += item.fee_count;
        if item.fee_count > 0 {
            view.min_fee = view.min_fee.min(item.min_fee);
            view.max_fee = view.max_fee.max(item.max_fee);
        }
        for (addr, count) in item.sender_counts {
            *sender_totals.entry(addr.clone()).or_insert(0) += count;
        }
        for (addr, count) in item.receiver_counts {
            *receiver_totals.entry(addr.clone()).or_insert(0) += count;
        }
        for (proto, count) in item.protocol_counts {
            *protocol_totals.entry(*proto).or_insert(0) += count;
        }

        let avg = if item.fee_count > 0 {
            item.total_fees as f64 / item.fee_count as f64
        } else {
            0.0
        };
        view.fee_over_time.push((item.timestamp_ms, avg));
        view.tx_over_time
            .push((item.timestamp_ms, item.tx_count as f64));
    }

    view.avg_fee = if view.fee_count > 0 {
        view.total_fees as f64 / view.fee_count as f64
    } else {
        0.0
    };

    view.top_senders = top_n_sorted(sender_totals, 20);
    view.top_receivers = top_n_sorted(receiver_totals, 20);
    view.protocol_counts = {
        let mut v: Vec<_> = protocol_totals.into_iter().collect();
        v.sort_unstable_by_key(|a| std::cmp::Reverse(a.1));
        v
    };

    view
}

fn top_n_sorted(map: HashMap<String, usize>, n: usize) -> Vec<(String, usize)> {
    let mut entries: Vec<(String, usize)> = map.into_iter().collect();
    entries.sort_unstable_by_key(|a| std::cmp::Reverse(a.1));
    entries.truncate(n);
    entries
}

// --- Aggregated View (for UI consumption) ---

#[derive(Debug, Clone, Default)]
pub struct AggregatedView {
    pub avg_fee: f64,
    pub total_fees: u64,
    pub min_fee: u64,
    pub max_fee: u64,
    pub fee_count: usize,
    pub tx_count: usize,
    pub blocks_analyzed: usize,
    pub top_senders: Vec<(String, usize)>,
    pub top_receivers: Vec<(String, usize)>,
    pub protocol_counts: Vec<(TransactionProtocol, usize)>,
    pub fee_over_time: Vec<(f64, f64)>,
    pub tx_over_time: Vec<(f64, f64)>,
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    fn make_block(hash: &str, timestamp_ms: u64, tx_count: usize, fee: u64) -> BlockSummary {
        BlockSummary {
            hash: hash.to_string(),
            timestamp_ms,
            tx_count,
            total_fees: fee,
            min_fee: fee,
            max_fee: fee,
            fee_count: if fee > 0 { tx_count } else { 0 },
            sender_counts: HashMap::from([("sender1".to_string(), tx_count)]),
            receiver_counts: HashMap::from([("receiver1".to_string(), tx_count)]),
            protocol_counts: HashMap::new(),
        }
    }

    fn make_block_with_protocol(
        hash: &str,
        timestamp_ms: u64,
        proto: TransactionProtocol,
    ) -> BlockSummary {
        BlockSummary {
            hash: hash.to_string(),
            timestamp_ms,
            tx_count: 1,
            total_fees: 100,
            min_fee: 100,
            max_fee: 100,
            fee_count: 1,
            sender_counts: HashMap::new(),
            receiver_counts: HashMap::new(),
            protocol_counts: HashMap::from([(proto, 1)]),
        }
    }

    // --- Protocol Detection ---

    #[test]
    fn detect_kasia_from_payload() {
        let payload = b"some ciph_msg data here";
        assert_eq!(
            detect_protocol(payload, &[]),
            Some(TransactionProtocol::Kasia)
        );
    }

    #[test]
    fn detect_kasplex_from_payload() {
        let payload = b"kasplex operation";
        assert_eq!(
            detect_protocol(payload, &[]),
            Some(TransactionProtocol::Kasplex)
        );
    }

    #[test]
    fn detect_ksocial_from_payload() {
        let payload = b"k:post data";
        assert_eq!(
            detect_protocol(payload, &[]),
            Some(TransactionProtocol::KSocial)
        );
    }

    #[test]
    fn detect_igra_from_payload() {
        let payload = &[0x93, 0x01, 0x02];
        assert_eq!(
            detect_protocol(payload, &[]),
            Some(TransactionProtocol::Igra)
        );
    }

    #[test]
    fn detect_igra_boundary_values() {
        assert_eq!(
            detect_protocol(&[0x91], &[]),
            Some(TransactionProtocol::Igra)
        );
        assert_eq!(
            detect_protocol(&[0x97], &[]),
            Some(TransactionProtocol::Igra)
        );
        assert_eq!(detect_protocol(&[0x90], &[]), None);
        assert_eq!(detect_protocol(&[0x98], &[]), None);
    }

    #[test]
    fn detect_krc_from_input_script() {
        // Build a script: OP_PUSH(7) "kasplex"
        let mut script = vec![7u8]; // push 7 bytes
        script.extend_from_slice(b"kasplex");
        assert_eq!(
            detect_protocol(&[], &[&script]),
            Some(TransactionProtocol::Krc)
        );
    }

    #[test]
    fn detect_krc_kspr_from_input_script() {
        let mut script = vec![4u8];
        script.extend_from_slice(b"kspr");
        assert_eq!(
            detect_protocol(&[], &[&script]),
            Some(TransactionProtocol::Krc)
        );
    }

    #[test]
    fn detect_kns_from_input_script() {
        let mut script = vec![3u8];
        script.extend_from_slice(b"kns");
        assert_eq!(
            detect_protocol(&[], &[&script]),
            Some(TransactionProtocol::Kns)
        );
    }

    #[test]
    fn detect_none_for_standard_tx() {
        assert_eq!(detect_protocol(&[], &[]), None);
        assert_eq!(detect_protocol(b"random data", &[]), None);
    }

    // --- AnalyticsEngine ---

    #[test]
    fn add_and_remove_block() {
        let mut engine = AnalyticsEngine::new();
        engine.add_block(make_block("hash1", 1000, 5, 100));

        assert_eq!(engine.total_blocks_processed, 1);
        assert_eq!(engine.total_transactions, 5);
        assert_eq!(engine.recent_blocks.len(), 1);

        let removed = engine.remove_block("hash1");
        assert!(removed);
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

        // Old block should be finalized
        assert!(!engine.recent_blocks.contains_key("old"));
        // Recent block should still be in cache
        assert!(engine.recent_blocks.contains_key("recent"));
        // Should have created minute bucket(s)
        assert!(!engine.minute_buckets.is_empty());
    }

    #[test]
    fn prune_buckets_removes_old() {
        let mut engine = AnalyticsEngine::new();
        let now = TWENTY_FOUR_HOURS_MS + ONE_HOUR_MS + 1000;

        // Add an old minute bucket (> 1 hour ago)
        engine
            .minute_buckets
            .push_back(TimeBucket::new(now - ONE_HOUR_MS - 1000));
        // Add a recent minute bucket
        engine
            .minute_buckets
            .push_back(TimeBucket::new(now - 30_000));

        // Add an old ten-minute bucket (> 24 hours ago)
        engine
            .ten_minute_buckets
            .push_back(TimeBucket::new(now - TWENTY_FOUR_HOURS_MS - 1000));
        // Add a recent ten-minute bucket
        engine
            .ten_minute_buckets
            .push_back(TimeBucket::new(now - 1000));

        engine.prune_buckets(now);

        assert_eq!(engine.minute_buckets.len(), 1);
        assert_eq!(engine.ten_minute_buckets.len(), 1);
    }

    #[test]
    fn get_view_one_min_uses_recent_blocks() {
        let mut engine = AnalyticsEngine::new();
        engine.add_block(make_block("b1", 1000, 5, 100));
        engine.add_block(make_block("b2", 2000, 3, 200));

        let view = engine.get_view(TimeWindow::OneMin);
        assert_eq!(view.tx_count, 8);
        assert_eq!(view.total_fees, 300);
        assert_eq!(view.blocks_analyzed, 2);
        assert!(!view.top_senders.is_empty());
        assert!(!view.top_receivers.is_empty());
    }

    #[test]
    fn get_view_one_hour_uses_minute_buckets() {
        let mut engine = AnalyticsEngine::new();
        let mut bucket = TimeBucket::new(60_000);
        bucket.tx_count = 10;
        bucket.total_fees = 500;
        bucket.fee_count = 10;
        bucket.min_fee = 10;
        bucket.max_fee = 100;
        engine.minute_buckets.push_back(bucket);

        let view = engine.get_view(TimeWindow::OneHour);
        assert_eq!(view.tx_count, 10);
        assert_eq!(view.total_fees, 500);
    }

    #[test]
    fn protocol_counts_in_view() {
        let mut engine = AnalyticsEngine::new();
        engine.add_block(make_block_with_protocol(
            "b1",
            1000,
            TransactionProtocol::Krc,
        ));
        engine.add_block(make_block_with_protocol(
            "b2",
            2000,
            TransactionProtocol::Krc,
        ));
        engine.add_block(make_block_with_protocol(
            "b3",
            3000,
            TransactionProtocol::Kns,
        ));

        let view = engine.get_view(TimeWindow::OneMin);
        assert_eq!(view.protocol_counts.len(), 2);
        assert_eq!(view.protocol_counts[0], (TransactionProtocol::Krc, 2));
        assert_eq!(view.protocol_counts[1], (TransactionProtocol::Kns, 1));
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
        engine.add_block(make_block_with_protocol(
            "b2",
            2000,
            TransactionProtocol::Krc,
        ));

        let dir = std::env::temp_dir().join("tui4kas_test_analytics");
        let path = dir.join("test_cache.bin");
        engine.save(&path).unwrap();

        let loaded = AnalyticsEngine::load(&path).unwrap();
        assert_eq!(loaded.total_blocks_processed, 2);
        assert_eq!(loaded.total_transactions, 6);
        assert_eq!(loaded.recent_blocks.len(), 2);
        assert_eq!(loaded.last_known_chain_block, Some("b2".to_string()));

        // Cleanup
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cap_hashmap_limits_entries() {
        let mut map: HashMap<String, usize> = (0..200).map(|i| (format!("addr{}", i), i)).collect();
        cap_hashmap(&mut map, 10);
        assert_eq!(map.len(), 10);
        // Top entries should be the highest counts
        assert!(map.contains_key("addr199"));
        assert!(map.contains_key("addr190"));
    }

    #[test]
    fn time_bucket_merge_block() {
        let mut bucket = TimeBucket::new(0);
        let block = make_block("b1", 100, 5, 50);
        bucket.merge_block(&block);

        assert_eq!(bucket.tx_count, 5);
        assert_eq!(bucket.total_fees, 50);
        assert_eq!(bucket.min_fee, 50);
        assert_eq!(bucket.max_fee, 50);
        assert_eq!(bucket.fee_count, 5);

        let block2 = make_block("b2", 200, 3, 100);
        bucket.merge_block(&block2);

        assert_eq!(bucket.tx_count, 8);
        assert_eq!(bucket.total_fees, 150);
        assert_eq!(bucket.min_fee, 50);
        assert_eq!(bucket.max_fee, 100);
    }

    #[test]
    fn protocol_label_display() {
        assert_eq!(TransactionProtocol::Krc.label(), "KRC-20");
        assert_eq!(TransactionProtocol::Kns.label(), "KNS");
        assert_eq!(TransactionProtocol::Kasia.label(), "Kasia");
        assert_eq!(TransactionProtocol::Kasplex.label(), "Kasplex");
        assert_eq!(TransactionProtocol::KSocial.label(), "KSocial");
        assert_eq!(TransactionProtocol::Igra.label(), "Igra");
    }

    #[test]
    fn detect_payload_priority_kasia_over_kasplex() {
        // "kasplex" appears but "ciph_msg" also appears — Kasia should win (checked first)
        let payload = b"ciph_msg kasplex data";
        assert_eq!(
            detect_protocol(payload, &[]),
            Some(TransactionProtocol::Kasia)
        );
    }

    #[test]
    fn op_pushdata1_script_detection() {
        // OP_PUSHDATA1 (0x4c), length=7, "kasplex"
        let mut script = vec![0x4c, 7];
        script.extend_from_slice(b"kasplex");
        assert_eq!(
            detect_protocol(&[], &[&script]),
            Some(TransactionProtocol::Krc)
        );
    }

    #[test]
    fn op_pushdata2_script_detection() {
        // OP_PUSHDATA2 (0x4d), length=3 (little-endian u16), "kns"
        let mut script = vec![0x4d, 3, 0];
        script.extend_from_slice(b"kns");
        assert_eq!(
            detect_protocol(&[], &[&script]),
            Some(TransactionProtocol::Kns)
        );
    }
}
