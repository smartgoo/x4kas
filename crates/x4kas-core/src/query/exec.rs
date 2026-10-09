//! Answering a query against the index: pick an access path from the query's
//! conditions (a transaction by id, an address's transactions, a protocol's, a chain
//! block's, else a scan by time; a block by hash, else a scan by time; addresses by
//! id, by label, else a scan of the per-slab stats), read through one snapshot so a
//! running writer never changes a result mid-scan, evaluate the filter cheapest
//! conditions first, keep the best `limit` rows (or the groups), and stop early when
//! the scan order is the asked order. Synchronous: call inside `spawn_blocking`.
//!
//! Every result says how much was scanned and whether it is partial: a scan cap, the
//! time budget, a cancel, or the memory caps on address scans and groups.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use fjall::{Readable, Snapshot};
use kaspa_addresses::{Address, Version};

use super::fields::{Cost, FieldKind};
use super::{
    Condition, Dir, Entity, FieldId, Filter, GroupKey, Metric, Op, OrderKey, Query, Value,
};
use crate::format::{format_duration_ms, format_sompi_exact, format_utc};
use crate::index::records::{
    AddrId, AddrStats, BlockKind, BlockRecord, Hash32, IndexedTx, PeerStats, SUMMARY_COINBASE,
    SUMMARY_SELF_TRANSFER, Subnetwork, TxSummary, addr_key, addr_tx_key, decode, parse_addr_key,
    parse_addr_tx_key, parse_peer_key, parse_protocol_tx_key, parse_time_key, protocol_tx_key,
    time_key,
};
use crate::index::{IndexStore, Slab, cluster, hex};
use crate::labels::{LabelBook, LabelSource};
use crate::tx_inspect::{OpcodeUsage, ScriptClass, TransactionProtocol, difficulty_from_bits};
use crate::watch::Watchlist;

/// Rows a result holds at most.
pub const MAX_RESULT_ROWS: usize = 10_000;
/// Records a run reads at most before giving up with a partial result.
pub const MAX_SCAN: u64 = 20_000_000;
/// How long a run may take.
pub const QUERY_BUDGET: Duration = Duration::from_secs(60);
/// Addresses an address scan merges at most.
pub const MAX_ADDRESS_SCAN: usize = 3_000_000;
/// Groups an aggregation keeps at most.
pub const MAX_GROUPS: usize = 50_000;
/// Rows between progress reports.
const PROGRESS_EVERY: u64 = 50_000;
/// Rows between cancel and deadline checks.
const CHECK_EVERY: u64 = 1_000;
/// Address id → address entries cached per run.
const NAME_CACHE_MAX: usize = 1_000_000;
/// Cluster members sampled to name a cluster.
const CLUSTER_SAMPLE: usize = 50;

/// What a run reads.
pub struct Inputs<'a> {
    pub store: &'a IndexStore,
    pub labels: &'a LabelBook,
    pub watchlist: &'a Watchlist,
    pub now_ms: u64,
    /// Slabs ending before this are being pruned: skip them.
    pub prune_floor_ms: Option<u64>,
}

/// A progress report callback.
pub type ProgressFn = Box<dyn FnMut(&Progress) + Send>;

/// How a run is steered from outside.
pub struct RunControl {
    pub cancel: Arc<AtomicBool>,
    pub deadline: Option<Instant>,
    pub progress: Option<ProgressFn>,
}

impl Default for RunControl {
    fn default() -> Self {
        Self {
            cancel: Arc::new(AtomicBool::new(false)),
            deadline: None,
            progress: None,
        }
    }
}

impl RunControl {
    /// A run that may take `budget` at most.
    pub fn with_budget(budget: Duration) -> Self {
        Self {
            deadline: Some(Instant::now() + budget),
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub scanned: u64,
    pub matched: u64,
    pub elapsed: Duration,
}

/// Why a result isn't everything that matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Partial {
    /// `MAX_SCAN` records were read.
    ScanCap,
    /// The time budget ran out.
    Budget,
    Cancelled,
    /// `MAX_ADDRESS_SCAN` addresses were merged.
    AddressCap,
    /// `MAX_GROUPS` groups were kept; later rows of other groups were dropped.
    GroupCap,
}

impl Partial {
    pub fn label(self) -> &'static str {
        match self {
            Self::ScanCap => "stopped at the scan cap",
            Self::Budget => "stopped at the time budget",
            Self::Cancelled => "cancelled",
            Self::AddressCap => "stopped at the address cap",
            Self::GroupCap => "stopped at the group cap",
        }
    }

    /// What happened and what to do about it, in a sentence or two.
    pub fn advice(self) -> String {
        match self {
            Self::ScanCap => format!(
                "Read {} records and stopped. The rows are what was found until then: \
                 narrow the time range, or filter by an address, transaction id or block.",
                crate::format::format_number(MAX_SCAN)
            ),
            Self::Budget => format!(
                "Ran for {}s and stopped. The rows are what was found until then: narrow \
                 the time range, or filter by an address, transaction id or block.",
                QUERY_BUDGET.as_secs()
            ),
            Self::Cancelled => {
                "Stopped on request. The rows are what was found until then.".to_string()
            }
            Self::AddressCap => format!(
                "Merged {} addresses and stopped. The rows are what was found until then: \
                 narrow the time range or filter by a label.",
                crate::format::format_number(MAX_ADDRESS_SCAN as u64)
            ),
            Self::GroupCap => format!(
                "Kept {} groups and dropped the rest. Group by fewer values or a wider \
                 time bucket.",
                crate::format::format_number(MAX_GROUPS as u64)
            ),
        }
    }
}

/// One value of a result.
#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    Null,
    Bool(bool),
    Int(i64),
    /// Sompi.
    Amount(i64),
    Float(f64),
    /// Unix milliseconds.
    Time(u64),
    /// A block hash.
    Hash(Hash32),
    Txid(Hash32),
    Address(String),
    Text(String),
    Enum(&'static str),
    List(Vec<Cell>),
}

impl Cell {
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null) || matches!(self, Self::List(items) if items.is_empty())
    }

    /// For a table or CSV: amounts in KAS, times as UTC, hashes in hex, lists joined.
    pub fn text(&self) -> String {
        match self {
            Self::Null => String::new(),
            Self::Bool(b) => b.to_string(),
            Self::Int(n) => n.to_string(),
            Self::Amount(sompi) => format_sompi_exact(*sompi),
            Self::Float(f) => format!("{f}"),
            Self::Time(ms) => format_utc(*ms),
            Self::Hash(h) | Self::Txid(h) => hex(h),
            Self::Address(a) | Self::Text(a) => a.clone(),
            Self::Enum(e) => (*e).to_string(),
            Self::List(items) => items.iter().map(Cell::text).collect::<Vec<_>>().join("; "),
        }
    }

    /// For JSON: amounts in sompi, times in milliseconds, hashes in hex.
    pub fn to_json(&self) -> serde_json::Value {
        use serde_json::Value as J;
        match self {
            Self::Null => J::Null,
            Self::Bool(b) => J::Bool(*b),
            Self::Int(n) | Self::Amount(n) => J::from(*n),
            Self::Float(f) => serde_json::Number::from_f64(*f)
                .map(J::Number)
                .unwrap_or(J::Null),
            Self::Time(ms) => J::from(*ms),
            Self::Hash(h) | Self::Txid(h) => J::String(hex(h)),
            Self::Address(a) | Self::Text(a) => J::String(a.clone()),
            Self::Enum(e) => J::String((*e).to_string()),
            Self::List(items) => J::Array(items.iter().map(Cell::to_json).collect()),
        }
    }

    /// A total order for sorting: nulls first, then by value within a kind.
    pub fn compare(&self, other: &Cell) -> Ordering {
        fn rank(c: &Cell) -> u8 {
            match c {
                Cell::Null => 0,
                Cell::Bool(_) => 1,
                Cell::Int(_) | Cell::Amount(_) | Cell::Float(_) => 2,
                Cell::Time(_) => 3,
                Cell::Hash(_) | Cell::Txid(_) => 4,
                Cell::Address(_) | Cell::Text(_) | Cell::Enum(_) => 5,
                Cell::List(_) => 6,
            }
        }
        match (self, other) {
            (Cell::Null, Cell::Null) => Ordering::Equal,
            (Cell::Bool(a), Cell::Bool(b)) => a.cmp(b),
            (Cell::Int(a), Cell::Int(b)) | (Cell::Amount(a), Cell::Amount(b)) => a.cmp(b),
            (Cell::Float(a), Cell::Float(b)) => a.total_cmp(b),
            (Cell::Int(a) | Cell::Amount(a), Cell::Float(b)) => (*a as f64).total_cmp(b),
            (Cell::Float(a), Cell::Int(b) | Cell::Amount(b)) => a.total_cmp(&(*b as f64)),
            (Cell::Int(a), Cell::Amount(b)) | (Cell::Amount(a), Cell::Int(b)) => a.cmp(b),
            (Cell::Time(a), Cell::Time(b)) => a.cmp(b),
            (Cell::Hash(a) | Cell::Txid(a), Cell::Hash(b) | Cell::Txid(b)) => a.cmp(b),
            (Cell::Address(a) | Cell::Text(a), Cell::Address(b) | Cell::Text(b)) => {
                a.to_lowercase().cmp(&b.to_lowercase())
            }
            (Cell::Enum(a), Cell::Enum(b)) => a.cmp(b),
            (Cell::Enum(a), Cell::Text(b) | Cell::Address(b)) => {
                a.to_lowercase().cmp(&b.to_lowercase())
            }
            (Cell::Text(a) | Cell::Address(a), Cell::Enum(b)) => {
                a.to_lowercase().cmp(&b.to_lowercase())
            }
            (Cell::List(a), Cell::List(b)) => {
                for (x, y) in a.iter().zip(b) {
                    match x.compare(y) {
                        Ordering::Equal => {}
                        other => return other,
                    }
                }
                a.len().cmp(&b.len())
            }
            (a, b) => rank(a).cmp(&rank(b)),
        }
    }
}

/// A cell as a hash-map key (floats by their bits).
#[derive(Debug, Clone, PartialEq)]
struct CellKey(Cell);

impl Eq for CellKey {}

impl Hash for CellKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        fn go<H: Hasher>(c: &Cell, state: &mut H) {
            match c {
                Cell::Null => 0u8.hash(state),
                Cell::Bool(b) => b.hash(state),
                Cell::Int(n) | Cell::Amount(n) => n.hash(state),
                Cell::Float(f) => f.to_bits().hash(state),
                Cell::Time(t) => t.hash(state),
                Cell::Hash(h) | Cell::Txid(h) => h.hash(state),
                Cell::Address(s) | Cell::Text(s) => s.hash(state),
                Cell::Enum(e) => e.hash(state),
                Cell::List(items) => {
                    items.len().hash(state);
                    items.iter().for_each(|i| go(i, state));
                }
            }
        }
        go(&self.0, state)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    /// A short name (the field's, or `count`, `sum(fee)`, `bucket`).
    pub name: String,
    pub label: String,
    pub kind: FieldKind,
    pub source: ColumnSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnSource {
    Field(FieldId),
    /// The n-th metric of the grouping.
    Metric(usize),
    /// The time bucket of the grouping.
    Bucket,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResultSet {
    pub entity: Entity,
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Cell>>,
    /// Rows that matched the filter (more than `rows` when truncated).
    pub matched: u64,
    /// Records read.
    pub scanned: u64,
    /// More rows matched than were kept (the limit or `MAX_RESULT_ROWS`).
    pub truncated: bool,
    pub partial: Option<Partial>,
    pub elapsed: Duration,
    /// The time window the run covered, `(from_ms, to_ms)`.
    pub window: (u64, u64),
    /// How the run read the index (`Plan::explain`).
    pub plan: String,
    /// The column holding the row's id (what a click opens), when shown.
    pub primary: Option<usize>,
    /// The column holding the row's time, when shown.
    pub time_column: Option<usize>,
    /// Address rows with a `balance` column: the node fills it in afterwards.
    pub balances_pending: bool,
}

impl ResultSet {
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// A result with no columns and no rows (a placeholder, tests).
    pub fn empty(entity: Entity) -> Self {
        Self {
            entity,
            columns: Vec::new(),
            rows: Vec::new(),
            matched: 0,
            scanned: 0,
            truncated: false,
            partial: None,
            elapsed: Duration::ZERO,
            window: (0, u64::MAX),
            plan: String::new(),
            primary: None,
            time_column: None,
            balances_pending: false,
        }
    }
}

// --- Planning ---

/// How the rows are read.
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    TxById(Vec<Hash32>),
    TxByAddress(Vec<AddrId>),
    TxByProtocol(Vec<TransactionProtocol>),
    TxByAcceptingBlock(Vec<Hash32>),
    TxTimeScan,
    BlockById(Vec<Hash32>),
    BlockTimeScan,
    PayoutByBlock(Vec<Hash32>),
    PayoutScan,
    AddrById(Vec<AddrId>),
    /// Candidates from the label book (and the watchlist).
    AddrFromLabels,
    AddrStatsScan,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub source: Source,
    /// Slab numbers to read, newest first.
    pub slabs: Vec<u64>,
    pub window: (u64, u64),
    /// Rows come out of the source in the asked order, so the run stops at the limit.
    pub early_stop: bool,
    /// Scan oldest first (the asked order is `time asc`).
    pub ascending: bool,
}

impl Plan {
    /// A line for the CLI and the result header.
    pub fn explain(&self) -> String {
        let source = match &self.source {
            Source::TxById(ids) => format!("{} transaction(s) by id", ids.len()),
            Source::TxByAddress(ids) => format!("the transactions of {} address(es)", ids.len()),
            Source::TxByProtocol(ps) => format!("the transactions of {} protocol(s)", ps.len()),
            Source::TxByAcceptingBlock(hs) => {
                format!("the transactions {} chain block(s) accepted", hs.len())
            }
            Source::TxTimeScan => "every transaction by time".to_string(),
            Source::BlockById(hs) => format!("{} block(s) by hash", hs.len()),
            Source::BlockTimeScan => "every block by time".to_string(),
            Source::PayoutByBlock(hs) => format!("the payouts of {} chain block(s)", hs.len()),
            Source::PayoutScan => "every chain block's payouts by time".to_string(),
            Source::AddrById(ids) => format!("{} address(es) by id", ids.len()),
            Source::AddrFromLabels => "the labelled addresses".to_string(),
            Source::AddrStatsScan => "every address's stats".to_string(),
        };
        let window = match self.window {
            (0, u64::MAX) => "all time".to_string(),
            (from, u64::MAX) => format!("since {}", format_utc(from)),
            (from, to) => format!("{} → {}", format_utc(from), format_utc(to)),
        };
        let order = if self.early_stop {
            if self.ascending {
                ", oldest first, stopping at the limit"
            } else {
                ", newest first, stopping at the limit"
            }
        } else {
            ""
        };
        format!(
            "{source} over {} slab(s), {window}{order}",
            self.slabs.len()
        )
    }
}

/// `Hash` or `In` list values of the conjuncts on `field`.
fn conjunct_values<'a>(conjuncts: &[&'a Condition], field: FieldId) -> Option<Vec<&'a Value>> {
    let cond = conjuncts
        .iter()
        .find(|c| c.field == field && matches!(c.op, Op::Eq | Op::In))?;
    Some(match &cond.value {
        Value::List(items) => items.iter().collect(),
        value => vec![value],
    })
}

fn hashes(values: &[&Value]) -> Vec<Hash32> {
    values
        .iter()
        .filter_map(|v| match v {
            Value::Hash(h) => Some(*h),
            _ => None,
        })
        .collect()
}

/// Decide how to read the index for `q`.
pub fn plan(
    q: &Query,
    store: &IndexStore,
    now_ms: u64,
    prune_floor_ms: Option<u64>,
) -> Result<Plan> {
    let (from, to) = q.range.resolve(now_ms);
    let from = from.max(prune_floor_ms.unwrap_or(0));
    let window = (from, to);
    let mut slabs: Vec<u64> = store.slabs_in(from, to).into_iter().map(|s| s.no).collect();
    slabs.reverse();
    let conjuncts: Vec<&Condition> = q.filter.as_ref().map(Filter::conjuncts).unwrap_or_default();
    let addresses = |field: FieldId| -> Result<Option<Vec<AddrId>>> {
        let Some(values) = conjunct_values(&conjuncts, field) else {
            return Ok(None);
        };
        let mut ids = Vec::new();
        for v in values {
            if let Value::Address(a) = v
                && let Some(id) = store.lookup(a)?
            {
                ids.push(id);
            }
        }
        Ok(Some(ids))
    };
    let source = match q.entity {
        Entity::Transactions => {
            if let Some(values) = conjunct_values(&conjuncts, FieldId::TxTxid) {
                Source::TxById(hashes(&values))
            } else if let Some(ids) = addresses(FieldId::TxAddress)?
                .or(addresses(FieldId::TxSender)?)
                .or(addresses(FieldId::TxReceiver)?)
            {
                Source::TxByAddress(ids)
            } else if let Some(values) = conjunct_values(&conjuncts, FieldId::TxAcceptingBlock) {
                Source::TxByAcceptingBlock(hashes(&values))
            } else if let Some(values) = conjunct_values(&conjuncts, FieldId::TxProtocol) {
                Source::TxByProtocol(
                    values
                        .iter()
                        .filter_map(|v| match v {
                            Value::Enum(name) => TransactionProtocol::from_slug(name),
                            _ => None,
                        })
                        .collect(),
                )
            } else {
                Source::TxTimeScan
            }
        }
        Entity::Blocks => match conjunct_values(&conjuncts, FieldId::BlockHash) {
            Some(values) => Source::BlockById(hashes(&values)),
            None => Source::BlockTimeScan,
        },
        Entity::Payouts => match conjunct_values(&conjuncts, FieldId::PayoutBlock) {
            Some(values) => Source::PayoutByBlock(hashes(&values)),
            None => Source::PayoutScan,
        },
        Entity::Addresses => {
            if let Some(ids) = addresses(FieldId::AddrAddress)? {
                Source::AddrById(ids)
            } else if conjuncts.iter().any(|c| {
                matches!(
                    c.field,
                    FieldId::AddrLabel
                        | FieldId::AddrLabelSource
                        | FieldId::AddrCategory
                        | FieldId::AddrClusterLabel
                ) && matches!(
                    c.op,
                    Op::Eq | Op::In | Op::Contains | Op::StartsWith | Op::IsNotNull
                ) || c.field == FieldId::AddrIsWatched
                    && c.value == Value::Bool(true)
                    && c.op == Op::Eq
            }) {
                Source::AddrFromLabels
            } else {
                Source::AddrStatsScan
            }
        }
    };
    let time_field = q.entity.time_field();
    let ordered_by_time = |dir: Dir| {
        q.order.is_empty() && dir == Dir::Desc
            || q.order.len() == 1
                && time_field.is_some_and(|t| q.order[0] == (OrderKey::Field(t), dir))
    };
    let ascending = ordered_by_time(Dir::Asc);
    let time_ordered_source = match &source {
        Source::TxTimeScan | Source::BlockTimeScan | Source::PayoutScan => true,
        Source::TxByAddress(ids) => ids.len() == 1,
        Source::TxByProtocol(ps) => ps.len() == 1,
        _ => false,
    };
    let early_stop = q.group.is_none()
        && q.limit.is_some()
        && time_ordered_source
        && (ascending || ordered_by_time(Dir::Desc));
    Ok(Plan {
        source,
        slabs,
        window,
        early_stop,
        ascending,
    })
}

// --- Rows ---

struct TxRow {
    txid: Hash32,
    time_ms: u64,
    summary: TxSummary,
    /// Index into `Ctx::slabs`.
    slab: usize,
    record: Option<IndexedTx>,
}

struct AddrRow {
    id: AddrId,
    address: String,
    stats: AddrStats,
}

enum Row {
    Tx(TxRow),
    Block {
        hash: Hash32,
        record: BlockRecord,
    },
    Payout {
        block: Hash32,
        time_ms: u64,
        index: usize,
        miner: Option<AddrId>,
        amount: u64,
    },
    Addr(AddrRow),
}

/// Everything a run reads through, with its caches.
struct Ctx<'a> {
    inputs: &'a Inputs<'a>,
    snapshot: Snapshot,
    /// The slabs of the window, as the plan lists them (newest first).
    slabs: Vec<Slab>,
    names: HashMap<AddrId, Option<String>>,
    clusters: HashMap<AddrId, (AddrId, u32)>,
    cluster_names: HashMap<AddrId, Option<String>>,
    watched: HashSet<String>,
}

impl Ctx<'_> {
    fn name(&mut self, id: AddrId) -> Result<Option<String>> {
        if let Some(name) = self.names.get(&id) {
            return Ok(name.clone());
        }
        if self.names.len() >= NAME_CACHE_MAX {
            self.names.clear();
        }
        let name = self.inputs.store.address_of(id)?;
        self.names.insert(id, name.clone());
        Ok(name)
    }

    fn names_of(&mut self, ids: impl IntoIterator<Item = AddrId>) -> Result<Vec<Cell>> {
        let mut seen = Vec::new();
        let mut out = Vec::new();
        for id in ids {
            if seen.contains(&id) {
                continue;
            }
            seen.push(id);
            if let Some(name) = self.name(id)? {
                out.push(Cell::Address(name));
            }
        }
        Ok(out)
    }

    fn labels_of(&mut self, ids: impl IntoIterator<Item = AddrId>) -> Result<Vec<Cell>> {
        let names = self.names_of(ids)?;
        let mut out = Vec::new();
        for name in names {
            if let Cell::Address(a) = name
                && let Some(label) = self.inputs.labels.name(&a)
                && !out.iter().any(|c| matches!(c, Cell::Text(t) if t == label))
            {
                out.push(Cell::Text(label.to_string()));
            }
        }
        Ok(out)
    }

    fn tx_record<'r>(&self, row: &'r mut TxRow) -> Result<&'r IndexedTx> {
        if row.record.is_none() {
            let slab = &self.slabs[row.slab];
            let bytes = self
                .snapshot
                .get(&slab.tx, row.txid)?
                .ok_or_else(|| anyhow!("transaction {} vanished mid-scan", hex(&row.txid)))?;
            row.record = Some(decode(&bytes)?);
        }
        Ok(row.record.as_ref().expect("just loaded"))
    }

    fn cluster(&mut self, id: AddrId) -> Result<(AddrId, u32)> {
        if let Some(c) = self.clusters.get(&id) {
            return Ok(*c);
        }
        let ks = self.inputs.store.clusters();
        let root = cluster::root_of(ks, id)?;
        let size = cluster::size_of(ks, root)?;
        self.clusters.insert(id, (root, size));
        Ok((root, size))
    }

    fn cluster_label(&mut self, id: AddrId) -> Result<Option<String>> {
        let (root, _) = self.cluster(id)?;
        if let Some(name) = self.cluster_names.get(&root) {
            return Ok(name.clone());
        }
        let members = cluster::members_of(self.inputs.store.clusters(), root, CLUSTER_SAMPLE)?;
        let mut names = Vec::with_capacity(members.len());
        for m in members {
            if let Some(name) = self.name(m)? {
                names.push(name);
            }
        }
        let label = self
            .inputs
            .labels
            .name_cluster(names.iter().map(String::as_str));
        self.cluster_names.insert(root, label.clone());
        Ok(label)
    }

    /// Distinct counterparties of `id` over the window's slabs.
    fn peer_count(&self, id: AddrId) -> Result<usize> {
        let mut peers = HashSet::new();
        for slab in &self.slabs {
            for guard in self.snapshot.prefix(&slab.peers, addr_key(id)) {
                let (key, value) = guard.into_inner()?;
                let stats: PeerStats = decode(&value)?;
                if stats.tx_count > 0
                    && let Some(peer) = parse_peer_key(&key)
                {
                    peers.insert(peer);
                }
            }
        }
        Ok(peers.len())
    }

    /// Totals of `id` over the window's slabs.
    fn stats(&self, id: AddrId) -> Result<AddrStats> {
        let mut total = AddrStats::default();
        for slab in &self.slabs {
            if let Some(bytes) = self.snapshot.get(&slab.stats, addr_key(id))? {
                total.merge(&decode(&bytes)?);
            }
        }
        Ok(total)
    }

    fn block_record(&self, hash: &Hash32) -> Result<Option<BlockRecord>> {
        for slab in &self.slabs {
            if let Some(bytes) = self.snapshot.get(&slab.blocks, hash)? {
                return Ok(Some(decode(&bytes)?));
            }
        }
        Ok(None)
    }

    /// The value of `field` for `row`.
    fn value(&mut self, row: &mut Row, field: FieldId) -> Result<Cell> {
        use FieldId::*;
        let amount = |v: Option<u64>| v.map(|a| Cell::Amount(a as i64)).unwrap_or(Cell::Null);
        let int = |v: Option<u64>| v.map(|a| Cell::Int(a as i64)).unwrap_or(Cell::Null);
        let hash = |h: Option<Hash32>| h.map(Cell::Hash).unwrap_or(Cell::Null);
        let text = |t: &Option<String>| t.clone().map(Cell::Text).unwrap_or(Cell::Null);
        Ok(match row {
            Row::Tx(tx) => {
                let s = tx.summary;
                match field {
                    TxTxid => Cell::Txid(tx.txid),
                    TxTime => Cell::Time(tx.time_ms),
                    TxDaaScore => Cell::Int(s.daa_score as i64),
                    TxIsCoinbase => Cell::Bool(s.flags & SUMMARY_COINBASE != 0),
                    TxSelfTransfer => Cell::Bool(s.flags & SUMMARY_SELF_TRANSFER != 0),
                    TxProtocol => TransactionProtocol::from_code(s.protocol)
                        .map(|p| Cell::Enum(p.slug()))
                        .unwrap_or(Cell::Null),
                    TxFee => amount(s.fee),
                    TxFeeRate => match (s.fee, s.mass) {
                        (Some(fee), mass) if mass > 0 => Cell::Float(fee as f64 / mass as f64),
                        _ => Cell::Null,
                    },
                    TxMass => Cell::Int(s.mass as i64),
                    TxInputCount => Cell::Int(s.inputs as i64),
                    TxOutputCount => Cell::Int(s.outputs as i64),
                    TxInputTotal => amount(s.input_total),
                    TxOutputTotal => Cell::Amount(s.output_total as i64),
                    TxMaxOutput => Cell::Amount(s.max_output as i64),
                    TxMinOutput => Cell::Amount(s.min_output as i64),
                    TxPayloadLen => Cell::Int(s.payload_len as i64),
                    TxIntrospection => Cell::Bool(OpcodeUsage::from_bits(s.opcodes).introspection),
                    TxSeqcommit => {
                        Cell::Bool(OpcodeUsage::from_bits(s.opcodes).chainblock_seqcommit)
                    }
                    TxZk => Cell::Bool(OpcodeUsage::from_bits(s.opcodes).zk_precompile()),
                    _ => {
                        let r = self.tx_record(tx)?;
                        match field {
                            TxAcceptingBlock => Cell::Hash(r.accepting_block),
                            TxBlock => hash((r.block != [0; 32]).then_some(r.block)),
                            TxBlockTime => Cell::Time(r.block_time_ms),
                            TxStorageMass => Cell::Int(r.storage_mass as i64),
                            TxComputeMass => Cell::Int(r.compute_mass as i64),
                            TxChangeMax => Cell::Int(
                                r.outputs.iter().map(|o| o.change).max().unwrap_or(0) as i64,
                            ),
                            TxPayload => {
                                if r.payload_len == 0 {
                                    Cell::Null
                                } else {
                                    Cell::Text(
                                        String::from_utf8_lossy(&r.payload_head).into_owned(),
                                    )
                                }
                            }
                            TxCovenantCreated => Cell::Int(r.covenant_created as i64),
                            TxCovenantSpent => Cell::Int(r.covenant_spent as i64),
                            TxSigOps => Cell::Int(r.sig_ops as i64),
                            TxVersion => int(r.version.map(u64::from)),
                            TxLockTime => int(r.lock_time),
                            TxGas => int(r.gas),
                            TxSubnetwork => match r.subnetwork {
                                Subnetwork::Unknown => Cell::Null,
                                s => Cell::Enum(s.label()),
                            },
                            TxDistinctAddresses => Cell::Int(r.addresses().len() as i64),
                            TxOutputScriptClass => {
                                classes(r.outputs.iter().map(|o| o.script_class))
                            }
                            TxInputScriptClass => classes(r.inputs.iter().map(|i| i.script_class)),
                            TxAddress | TxSender | TxReceiver | TxLabel | TxSenderLabel
                            | TxReceiverLabel | TxIsWatched => {
                                let ids: Vec<AddrId> = match field {
                                    TxSender | TxSenderLabel => {
                                        r.inputs.iter().filter_map(|i| i.addr).collect()
                                    }
                                    TxReceiver | TxReceiverLabel => {
                                        r.outputs.iter().filter_map(|o| o.addr).collect()
                                    }
                                    _ => r.addresses(),
                                };
                                match field {
                                    TxAddress | TxSender | TxReceiver => {
                                        Cell::List(self.names_of(ids)?)
                                    }
                                    TxIsWatched => {
                                        let names = self.names_of(ids)?;
                                        Cell::Bool(names.iter().any(|n| matches!(n, Cell::Address(a) if self.watched.contains(a))))
                                    }
                                    _ => Cell::List(self.labels_of(ids)?),
                                }
                            }
                            _ => Cell::Null,
                        }
                    }
                }
            }
            Row::Block { hash: h, record: r } => match field {
                BlockHash => Cell::Hash(*h),
                BlockTime => Cell::Time(r.time_ms),
                BlockIsChain => Cell::Bool(r.kind == BlockKind::Chain),
                BlockMergingBlock => Cell::Hash(r.merging_block),
                BlockDaaScore => int(r.daa_score),
                BlockBlueScore => int(r.blue_score),
                BlockBlueWork => r
                    .blue_work
                    .map(|w| {
                        let s: String = w.iter().map(|b| format!("{b:02x}")).collect();
                        let s = s.trim_start_matches('0');
                        Cell::Text(if s.is_empty() {
                            "0".to_string()
                        } else {
                            s.to_string()
                        })
                    })
                    .unwrap_or(Cell::Null),
                BlockBits => int(r.bits.map(u64::from)),
                BlockDifficulty => r
                    .bits
                    .map(|b| Cell::Float(difficulty_from_bits(b)))
                    .unwrap_or(Cell::Null),
                BlockNonce => int(r.nonce),
                BlockVersion => int(r.version.map(u64::from)),
                BlockParentCount => Cell::Int(r.parents.len() as i64),
                BlockParents => Cell::List(r.parents.iter().map(|p| Cell::Hash(*p)).collect()),
                BlockHashMerkleRoot => hash(r.hash_merkle_root),
                BlockAcceptedIdMerkleRoot => hash(r.accepted_id_merkle_root),
                BlockUtxoCommitment => hash(r.utxo_commitment),
                BlockPruningPoint => hash(r.pruning_point),
                BlockMiner => match r.miner {
                    Some(id) => self.name(id)?.map(Cell::Address).unwrap_or(Cell::Null),
                    None => Cell::Null,
                },
                BlockMinerLabel => match r.miner {
                    Some(id) => self
                        .name(id)?
                        .and_then(|a| {
                            self.inputs
                                .labels
                                .name(&a)
                                .map(|l| Cell::Text(l.to_string()))
                        })
                        .unwrap_or(Cell::Null),
                    None => Cell::Null,
                },
                BlockMinerTag => text(&r.miner_tag),
                BlockNodeVersion => text(&r.node_version),
                BlockSubsidy => amount(r.subsidy),
                BlockReward => amount(r.subsidy.map(|s| s + r.accepted_fees)),
                BlockAcceptedTxs => Cell::Int(r.accepted_txs as i64),
                BlockAcceptedMass => Cell::Int(r.accepted_mass as i64),
                BlockAcceptedFees => Cell::Amount(r.accepted_fees as i64),
                BlockMergedBlues => Cell::Int(r.payouts.len() as i64),
                BlockPayoutTotal => Cell::Amount(r.payout_total() as i64),
                BlockPays => {
                    let ids: Vec<AddrId> = r.payouts.iter().filter_map(|p| p.addr).collect();
                    Cell::List(self.names_of(ids)?)
                }
                _ => Cell::Null,
            },
            Row::Payout {
                block,
                time_ms,
                index,
                miner,
                amount: paid,
            } => match field {
                PayoutTime => Cell::Time(*time_ms),
                PayoutBlock => Cell::Hash(*block),
                PayoutIndex => Cell::Int(*index as i64),
                PayoutMiner => match miner {
                    Some(id) => self.name(*id)?.map(Cell::Address).unwrap_or(Cell::Null),
                    None => Cell::Null,
                },
                PayoutMinerLabel => match miner {
                    Some(id) => self
                        .name(*id)?
                        .and_then(|a| {
                            self.inputs
                                .labels
                                .name(&a)
                                .map(|l| Cell::Text(l.to_string()))
                        })
                        .unwrap_or(Cell::Null),
                    None => Cell::Null,
                },
                PayoutAmount => Cell::Amount(*paid as i64),
                _ => Cell::Null,
            },
            Row::Addr(a) => match field {
                AddrAddress => Cell::Address(a.address.clone()),
                AddrLabel => self
                    .inputs
                    .labels
                    .name(&a.address)
                    .map(|l| Cell::Text(l.to_string()))
                    .unwrap_or(Cell::Null),
                AddrLabelSource => self
                    .inputs
                    .labels
                    .get(&a.address)
                    .map(|l| Cell::Enum(label_source_name(l.source)))
                    .unwrap_or(Cell::Null),
                AddrCategory => Cell::List(
                    self.inputs
                        .labels
                        .get(&a.address)
                        .map(|l| l.categories.iter().map(|c| Cell::Text(c.clone())).collect())
                        .unwrap_or_default(),
                ),
                AddrType => Address::try_from(a.address.as_str())
                    .ok()
                    .map(|addr| {
                        Cell::Enum(match addr.version {
                            Version::PubKey => "p2pk",
                            Version::PubKeyECDSA => "p2pk_ecdsa",
                            Version::ScriptHash => "p2sh",
                        })
                    })
                    .unwrap_or(Cell::Null),
                AddrFirstSeen => {
                    if a.stats.tx_count > 0 {
                        Cell::Time(a.stats.first_seen_ms)
                    } else {
                        Cell::Null
                    }
                }
                AddrLastSeen => {
                    if a.stats.tx_count > 0 {
                        Cell::Time(a.stats.last_seen_ms)
                    } else {
                        Cell::Null
                    }
                }
                AddrTxCount => Cell::Int(a.stats.tx_count as i64),
                AddrReceived => Cell::Amount(a.stats.received as i64),
                AddrSent => Cell::Amount(a.stats.sent as i64),
                AddrNet => Cell::Amount(a.stats.received as i64 - a.stats.sent as i64),
                AddrClusterSize => Cell::Int(self.cluster(a.id)?.1 as i64),
                AddrClusterLabel => self
                    .cluster_label(a.id)?
                    .map(Cell::Text)
                    .unwrap_or(Cell::Null),
                AddrPeerCount => Cell::Int(self.peer_count(a.id)? as i64),
                AddrIsWatched => Cell::Bool(self.watched.contains(&a.address)),
                AddrBalance => Cell::Null,
                _ => Cell::Null,
            },
        })
    }
}

fn classes(codes: impl Iterator<Item = u8>) -> Cell {
    let mut out: Vec<Cell> = Vec::new();
    for code in codes {
        if let Some(class) = ScriptClass::from_code(code) {
            let cell = Cell::Enum(class.slug());
            if !out.contains(&cell) {
                out.push(cell);
            }
        }
    }
    Cell::List(out)
}

/// The text-form name of a label source (`fields::LABEL_SOURCES`).
pub fn label_source_name(source: LabelSource) -> &'static str {
    match source {
        LabelSource::User => "user",
        LabelSource::KasFyi => "kas_fyi",
        LabelSource::KaspaOrg => "kaspa_org",
        LabelSource::Kns => "kns",
        LabelSource::Heuristic => "heuristic",
    }
}

// --- Filter evaluation ---

/// Durations on time fields become absolute times, and each `and`'s conditions are
/// ordered cheapest first so short-circuiting does the least work.
fn prepare(filter: &Filter, now_ms: u64) -> Filter {
    fn bind(value: &Value, kind: FieldKind, now_ms: u64) -> Value {
        match (kind, value) {
            (FieldKind::Time, Value::Duration(ms)) => Value::Time(now_ms.saturating_sub(*ms)),
            (_, Value::List(items)) => {
                Value::List(items.iter().map(|v| bind(v, kind, now_ms)).collect())
            }
            (_, v) => v.clone(),
        }
    }
    fn cost(filter: &Filter) -> Cost {
        match filter {
            Filter::And(items) | Filter::Or(items) => {
                items.iter().map(cost).max().unwrap_or(Cost::Summary)
            }
            Filter::Not(inner) => cost(inner),
            Filter::Cond(c) => c.field.cost(),
        }
    }
    match filter {
        Filter::And(items) => {
            let mut items: Vec<Filter> = items.iter().map(|f| prepare(f, now_ms)).collect();
            items.sort_by_key(cost);
            Filter::And(items)
        }
        Filter::Or(items) => {
            let mut items: Vec<Filter> = items.iter().map(|f| prepare(f, now_ms)).collect();
            items.sort_by_key(cost);
            Filter::Or(items)
        }
        Filter::Not(inner) => Filter::Not(Box::new(prepare(inner, now_ms))),
        Filter::Cond(c) => Filter::Cond(Condition {
            field: c.field,
            op: c.op,
            value: bind(&c.value, c.field.kind(), now_ms),
        }),
    }
}

fn eval(filter: &Filter, row: &mut Row, ctx: &mut Ctx<'_>) -> Result<bool> {
    Ok(match filter {
        Filter::And(items) => {
            for item in items {
                if !eval(item, row, ctx)? {
                    return Ok(false);
                }
            }
            true
        }
        Filter::Or(items) => {
            for item in items {
                if eval(item, row, ctx)? {
                    return Ok(true);
                }
            }
            false
        }
        Filter::Not(inner) => !eval(inner, row, ctx)?,
        Filter::Cond(c) => {
            let cell = ctx.value(row, c.field)?;
            matches(&cell, c.field.is_multi(), c.op, &c.value)
        }
    })
}

/// Whether `cell` satisfies `op value`. A multi-valued cell matches when any of its
/// values does (`!=` and `not in`: when none does).
pub fn matches(cell: &Cell, multi: bool, op: Op, value: &Value) -> bool {
    match op {
        Op::IsNull => return cell.is_null(),
        Op::IsNotNull => return !cell.is_null(),
        _ => {}
    }
    if multi || matches!(cell, Cell::List(_)) {
        let items: &[Cell] = match cell {
            Cell::List(items) => items,
            other => std::slice::from_ref(other),
        };
        return match op {
            Op::Ne => !items.iter().any(|c| scalar(c, Op::Eq, value)),
            Op::NotIn => !items.iter().any(|c| scalar(c, Op::In, value)),
            op => items.iter().any(|c| scalar(c, op, value)),
        };
    }
    scalar(cell, op, value)
}

fn scalar(cell: &Cell, op: Op, value: &Value) -> bool {
    if cell.is_null() {
        return false;
    }
    match op {
        Op::Eq => compare(cell, value) == Some(Ordering::Equal),
        Op::Ne => compare(cell, value).is_some_and(|o| o != Ordering::Equal),
        Op::Lt => compare(cell, value) == Some(Ordering::Less),
        Op::Le => compare(cell, value).is_some_and(|o| o != Ordering::Greater),
        Op::Gt => compare(cell, value) == Some(Ordering::Greater),
        Op::Ge => compare(cell, value).is_some_and(|o| o != Ordering::Less),
        Op::Between => match value {
            Value::List(items) if items.len() == 2 => {
                compare(cell, &items[0]).is_some_and(|o| o != Ordering::Less)
                    && compare(cell, &items[1]).is_some_and(|o| o != Ordering::Greater)
            }
            _ => false,
        },
        Op::In => match value {
            Value::List(items) => items
                .iter()
                .any(|v| compare(cell, v) == Some(Ordering::Equal)),
            v => compare(cell, v) == Some(Ordering::Equal),
        },
        Op::NotIn => match value {
            Value::List(items) => !items
                .iter()
                .any(|v| compare(cell, v) == Some(Ordering::Equal)),
            v => compare(cell, v) != Some(Ordering::Equal),
        },
        Op::Contains => match text_of(value) {
            Some(needle) => cell.text().to_lowercase().contains(&needle.to_lowercase()),
            None => compare(cell, value) == Some(Ordering::Equal),
        },
        Op::StartsWith => match text_of(value) {
            Some(prefix) => cell
                .text()
                .to_lowercase()
                .starts_with(&prefix.to_lowercase()),
            None => compare(cell, value) == Some(Ordering::Equal),
        },
        Op::IsNull | Op::IsNotNull => unreachable!("handled by matches"),
    }
}

fn text_of(value: &Value) -> Option<&str> {
    match value {
        Value::Text(t) | Value::Enum(t) | Value::Address(t) => Some(t),
        _ => None,
    }
}

/// `cell` against a literal; `None` when they can't be compared.
fn compare(cell: &Cell, value: &Value) -> Option<Ordering> {
    Some(match (cell, value) {
        (Cell::Bool(a), Value::Bool(b)) => a.cmp(b),
        (Cell::Int(a), Value::Int(b)) => a.cmp(b),
        (Cell::Int(a), Value::Float(b)) => (*a as f64).total_cmp(b),
        (Cell::Amount(a), Value::Amount(b)) => a.cmp(b),
        (Cell::Float(a), Value::Float(b)) => a.total_cmp(b),
        (Cell::Float(a), Value::Int(b)) => a.total_cmp(&(*b as f64)),
        (Cell::Time(a), Value::Time(b)) => a.cmp(b),
        (Cell::Hash(a) | Cell::Txid(a), Value::Hash(b)) => a.cmp(b),
        (Cell::Address(a), Value::Address(b)) => a.cmp(b),
        (Cell::Text(a), Value::Text(b) | Value::Enum(b)) => a.to_lowercase().cmp(&b.to_lowercase()),
        (Cell::Enum(a), Value::Enum(b) | Value::Text(b)) => a.cmp(&b.to_lowercase().as_str()),
        _ => return None,
    })
}

// --- Collecting ---

/// The best `k` rows by the sort keys.
struct TopK {
    k: usize,
    dirs: Vec<Dir>,
    rows: Vec<(Vec<Cell>, Vec<Cell>)>,
    /// Rows pushed beyond `k` that were dropped.
    dropped: bool,
}

impl TopK {
    fn push(&mut self, keys: Vec<Cell>, cells: Vec<Cell>) {
        self.rows.push((keys, cells));
        if self.rows.len() >= self.k.saturating_mul(2).max(self.k + 1024) {
            self.sort();
            self.rows.truncate(self.k);
            self.dropped = true;
        }
    }

    fn sort(&mut self) {
        let dirs = self.dirs.clone();
        self.rows.sort_by(|(a, _), (b, _)| order_keys(a, b, &dirs));
    }

    fn finish(mut self) -> (Vec<Vec<Cell>>, bool) {
        self.sort();
        let dropped = self.dropped || self.rows.len() > self.k;
        self.rows.truncate(self.k);
        (
            self.rows.into_iter().map(|(_, cells)| cells).collect(),
            dropped,
        )
    }
}

fn order_keys(a: &[Cell], b: &[Cell], dirs: &[Dir]) -> Ordering {
    for ((x, y), dir) in a.iter().zip(b).zip(dirs) {
        let o = x.compare(y);
        let o = match dir {
            Dir::Asc => o,
            Dir::Desc => o.reverse(),
        };
        if o != Ordering::Equal {
            return o;
        }
    }
    Ordering::Equal
}

enum Acc {
    Count(u64),
    Distinct(HashSet<CellKey>),
    Sum { int: i128, float: f64, any: bool },
    Avg { sum: f64, n: u64 },
    Min(Option<Cell>),
    Max(Option<Cell>),
}

impl Acc {
    fn new(metric: Metric) -> Self {
        match metric {
            Metric::Count => Self::Count(0),
            Metric::CountDistinct(_) => Self::Distinct(HashSet::new()),
            Metric::Sum(_) => Self::Sum {
                int: 0,
                float: 0.0,
                any: false,
            },
            Metric::Avg(_) => Self::Avg { sum: 0.0, n: 0 },
            Metric::Min(_) => Self::Min(None),
            Metric::Max(_) => Self::Max(None),
        }
    }

    fn add(&mut self, cell: Cell) {
        match self {
            Self::Count(n) => *n += 1,
            Self::Distinct(set) => {
                if !cell.is_null() {
                    set.insert(CellKey(cell));
                }
            }
            Self::Sum { int, float, any } => match cell {
                Cell::Int(v) | Cell::Amount(v) => {
                    *int += v as i128;
                    *any = true;
                }
                Cell::Float(v) => {
                    *float += v;
                    *any = true;
                }
                _ => {}
            },
            Self::Avg { sum, n } => match cell {
                Cell::Int(v) | Cell::Amount(v) => {
                    *sum += v as f64;
                    *n += 1;
                }
                Cell::Float(v) => {
                    *sum += v;
                    *n += 1;
                }
                _ => {}
            },
            Self::Min(best) => {
                if !cell.is_null()
                    && best
                        .as_ref()
                        .is_none_or(|b| cell.compare(b) == Ordering::Less)
                {
                    *best = Some(cell);
                }
            }
            Self::Max(best) => {
                if !cell.is_null()
                    && best
                        .as_ref()
                        .is_none_or(|b| cell.compare(b) == Ordering::Greater)
                {
                    *best = Some(cell);
                }
            }
        }
    }

    fn finish(self, metric: Metric) -> Cell {
        match self {
            Self::Count(n) => Cell::Int(n as i64),
            Self::Distinct(set) => Cell::Int(set.len() as i64),
            Self::Sum { int, float, any } => {
                if !any {
                    Cell::Null
                } else {
                    match metric.kind() {
                        FieldKind::Amount => {
                            Cell::Amount(int.clamp(i64::MIN as i128, i64::MAX as i128) as i64)
                        }
                        FieldKind::Float => Cell::Float(float + int as f64),
                        _ => Cell::Int(int.clamp(i64::MIN as i128, i64::MAX as i128) as i64),
                    }
                }
            }
            Self::Avg { sum, n } => {
                if n == 0 {
                    Cell::Null
                } else {
                    Cell::Float(sum / n as f64)
                }
            }
            Self::Min(best) | Self::Max(best) => best.unwrap_or(Cell::Null),
        }
    }
}

/// Where the rows of a run go: a list or groups.
enum Sink {
    List {
        columns: Vec<FieldId>,
        order: Vec<FieldId>,
        top: TopK,
    },
    Groups {
        keys: Vec<GroupKey>,
        metrics: Vec<Metric>,
        groups: HashMap<Vec<CellKey>, Vec<Acc>>,
        capped: bool,
    },
}

/// Counts a run's reads and watches the limits.
struct Meter<'a> {
    ctl: &'a mut RunControl,
    started: Instant,
    scanned: u64,
    matched: u64,
    stop: Option<Partial>,
}

impl Meter<'_> {
    /// Count one read; `false` when the run must stop.
    fn tick(&mut self) -> bool {
        self.scanned += 1;
        if self.scanned.is_multiple_of(CHECK_EVERY) {
            if self.ctl.cancel.load(AtomicOrdering::Relaxed) {
                self.stop = Some(Partial::Cancelled);
            } else if self.ctl.deadline.is_some_and(|d| Instant::now() >= d) {
                self.stop = Some(Partial::Budget);
            } else if self.scanned >= MAX_SCAN {
                self.stop = Some(Partial::ScanCap);
            }
            if self.scanned.is_multiple_of(PROGRESS_EVERY)
                && let Some(progress) = self.ctl.progress.as_mut()
            {
                progress(&Progress {
                    scanned: self.scanned,
                    matched: self.matched,
                    elapsed: self.started.elapsed(),
                });
            }
        }
        self.stop.is_none()
    }
}

/// Answer `q`. The query must validate.
pub fn run(inputs: &Inputs<'_>, q: &Query, ctl: &mut RunControl) -> Result<ResultSet> {
    q.validate()?;
    let plan = plan(q, inputs.store, inputs.now_ms, inputs.prune_floor_ms)?;
    let explain = plan.explain();
    let store = inputs.store;
    let all_slabs = store.slabs();
    let slabs: Vec<Slab> = plan
        .slabs
        .iter()
        .filter_map(|no| all_slabs.iter().find(|s| s.no == *no).cloned())
        .collect();
    let mut ctx = Ctx {
        inputs,
        snapshot: store.db().snapshot(),
        slabs,
        names: HashMap::new(),
        clusters: HashMap::new(),
        cluster_names: HashMap::new(),
        watched: inputs
            .watchlist
            .entries
            .iter()
            .filter(|e| e.network == store.network())
            .map(|e| e.address.clone())
            .collect(),
    };
    let filter = q.filter.as_ref().map(|f| prepare(f, inputs.now_ms));
    let limit = q.limit.unwrap_or(MAX_RESULT_ROWS).min(MAX_RESULT_ROWS);
    let time_field = q.entity.time_field();

    let mut sink = match &q.group {
        Some(group) => Sink::Groups {
            keys: group.keys.clone(),
            metrics: group.metrics.clone(),
            groups: HashMap::new(),
            capped: false,
        },
        None => {
            let columns = q.columns();
            let mut order: Vec<(FieldId, Dir)> = q
                .order
                .iter()
                .filter_map(|(k, d)| match k {
                    OrderKey::Field(f) => Some((*f, *d)),
                    _ => None,
                })
                .collect();
            if order.is_empty()
                && let Some(t) = time_field
            {
                order.push((t, Dir::Desc));
            }
            Sink::List {
                columns,
                top: TopK {
                    k: limit,
                    dirs: order.iter().map(|(_, d)| *d).collect(),
                    rows: Vec::new(),
                    dropped: false,
                },
                order: order.into_iter().map(|(f, _)| f).collect(),
            }
        }
    };
    let mut meter = Meter {
        ctl,
        started: Instant::now(),
        scanned: 0,
        matched: 0,
        stop: None,
    };

    // One row in: filter, then collect.
    let mut take = |row: &mut Row, ctx: &mut Ctx<'_>, meter: &mut Meter<'_>| -> Result<bool> {
        if let Some(filter) = &filter
            && !eval(filter, row, ctx)?
        {
            return Ok(true);
        }
        meter.matched += 1;
        match &mut sink {
            Sink::List {
                columns,
                order,
                top,
            } => {
                let mut cells = Vec::with_capacity(columns.len());
                for f in columns.iter() {
                    cells.push(ctx.value(row, *f)?);
                }
                let mut keys = Vec::with_capacity(order.len());
                for f in order.iter() {
                    keys.push(match columns.iter().position(|c| c == f) {
                        Some(i) => cells[i].clone(),
                        None => ctx.value(row, *f)?,
                    });
                }
                top.push(keys, cells);
                // Rows arrive in the asked order: the limit is the end.
                Ok(!(plan.early_stop && meter.matched as usize >= limit))
            }
            Sink::Groups {
                keys,
                metrics,
                groups,
                capped,
            } => {
                let mut key = Vec::with_capacity(keys.len());
                for k in keys.iter() {
                    key.push(CellKey(match k {
                        GroupKey::Field(f) => ctx.value(row, *f)?,
                        GroupKey::TimeBucket(width) => match time_field {
                            Some(t) => match ctx.value(row, t)? {
                                Cell::Time(ms) => Cell::Time(ms / width * width),
                                other => other,
                            },
                            None => Cell::Null,
                        },
                    }));
                }
                if !groups.contains_key(&key) && groups.len() >= MAX_GROUPS {
                    *capped = true;
                    return Ok(true);
                }
                let accs = groups
                    .entry(key)
                    .or_insert_with(|| metrics.iter().map(|m| Acc::new(*m)).collect());
                for (acc, metric) in accs.iter_mut().zip(metrics.iter()) {
                    let cell = match metric.field() {
                        Some(f) => ctx.value(row, f)?,
                        None => Cell::Null,
                    };
                    acc.add(cell);
                }
                Ok(true)
            }
        }
    };

    let (from, to) = plan.window;
    let mut partial = None;
    match &plan.source {
        Source::TxById(ids) => {
            'all: for txid in ids {
                for (i, slab) in ctx.slabs.iter().enumerate() {
                    let Some(bytes) = ctx.snapshot.get(&slab.tx, txid)? else {
                        continue;
                    };
                    if !meter.tick() {
                        break 'all;
                    }
                    let record: IndexedTx = decode(&bytes)?;
                    if record.time_ms < from || record.time_ms >= to {
                        break;
                    }
                    let mut row = Row::Tx(TxRow {
                        txid: *txid,
                        time_ms: record.time_ms,
                        summary: record.summary(),
                        slab: i,
                        record: Some(record),
                    });
                    if !take(&mut row, &mut ctx, &mut meter)? {
                        break 'all;
                    }
                    break;
                }
            }
        }
        Source::TxByAddress(ids) => {
            let mut seen: HashSet<Hash32> = HashSet::new();
            'all: for &id in ids {
                let start = addr_tx_key(id, from, &[0; 32]).to_vec();
                let end = addr_tx_key(id, to.saturating_sub(1), &[0xff; 32]).to_vec();
                let slab_order: Vec<usize> = if plan.ascending {
                    (0..ctx.slabs.len()).rev().collect()
                } else {
                    (0..ctx.slabs.len()).collect()
                };
                for i in slab_order {
                    let slab = ctx.slabs[i].clone();
                    let iter = ctx
                        .snapshot
                        .range(&slab.addr_tx, start.clone()..=end.clone());
                    let keys: Vec<Hash32> = if plan.ascending {
                        iter.map(|g| g.key().map(|k| parse_addr_tx_key(&k).map(|(_, t)| t)))
                            .collect::<fjall::Result<Vec<_>>>()?
                            .into_iter()
                            .flatten()
                            .collect()
                    } else {
                        iter.rev()
                            .map(|g| g.key().map(|k| parse_addr_tx_key(&k).map(|(_, t)| t)))
                            .collect::<fjall::Result<Vec<_>>>()?
                            .into_iter()
                            .flatten()
                            .collect()
                    };
                    for txid in keys {
                        if !meter.tick() {
                            break 'all;
                        }
                        if ids.len() > 1 && !seen.insert(txid) {
                            continue;
                        }
                        let Some(bytes) = ctx.snapshot.get(&slab.tx, txid)? else {
                            continue;
                        };
                        let record: IndexedTx = decode(&bytes)?;
                        let mut row = Row::Tx(TxRow {
                            txid,
                            time_ms: record.time_ms,
                            summary: record.summary(),
                            slab: i,
                            record: Some(record),
                        });
                        if !take(&mut row, &mut ctx, &mut meter)? {
                            break 'all;
                        }
                    }
                }
            }
        }
        Source::TxByProtocol(protocols) => {
            'all: for &protocol in protocols {
                let start = protocol_tx_key(protocol, from, &[0; 32]).to_vec();
                let end = protocol_tx_key(protocol, to.saturating_sub(1), &[0xff; 32]).to_vec();
                let slab_order: Vec<usize> = if plan.ascending {
                    (0..ctx.slabs.len()).rev().collect()
                } else {
                    (0..ctx.slabs.len()).collect()
                };
                for i in slab_order {
                    let slab = ctx.slabs[i].clone();
                    let iter = ctx
                        .snapshot
                        .range(&slab.protocol_tx, start.clone()..=end.clone());
                    let keys: Vec<Hash32> = if plan.ascending {
                        iter.map(|g| g.key().map(|k| parse_protocol_tx_key(&k).map(|(_, t)| t)))
                            .collect::<fjall::Result<Vec<_>>>()?
                            .into_iter()
                            .flatten()
                            .collect()
                    } else {
                        iter.rev()
                            .map(|g| g.key().map(|k| parse_protocol_tx_key(&k).map(|(_, t)| t)))
                            .collect::<fjall::Result<Vec<_>>>()?
                            .into_iter()
                            .flatten()
                            .collect()
                    };
                    for txid in keys {
                        if !meter.tick() {
                            break 'all;
                        }
                        let Some(bytes) = ctx.snapshot.get(&slab.tx, txid)? else {
                            continue;
                        };
                        let record: IndexedTx = decode(&bytes)?;
                        let mut row = Row::Tx(TxRow {
                            txid,
                            time_ms: record.time_ms,
                            summary: record.summary(),
                            slab: i,
                            record: Some(record),
                        });
                        if !take(&mut row, &mut ctx, &mut meter)? {
                            break 'all;
                        }
                    }
                }
            }
        }
        Source::TxByAcceptingBlock(blocks) => {
            'all: for block in blocks {
                for i in 0..ctx.slabs.len() {
                    let slab = ctx.slabs[i].clone();
                    let txids: Vec<Hash32> = ctx
                        .snapshot
                        .prefix(&slab.block_tx, block)
                        .map(|g| g.key().map(|k| k[32..].try_into().ok()))
                        .collect::<fjall::Result<Vec<Option<Hash32>>>>()?
                        .into_iter()
                        .flatten()
                        .collect();
                    if txids.is_empty() {
                        continue;
                    }
                    for txid in txids {
                        if !meter.tick() {
                            break 'all;
                        }
                        let Some(bytes) = ctx.snapshot.get(&slab.tx, txid)? else {
                            continue;
                        };
                        let record: IndexedTx = decode(&bytes)?;
                        let mut row = Row::Tx(TxRow {
                            txid,
                            time_ms: record.time_ms,
                            summary: record.summary(),
                            slab: i,
                            record: Some(record),
                        });
                        if !take(&mut row, &mut ctx, &mut meter)? {
                            break 'all;
                        }
                    }
                    break;
                }
            }
        }
        Source::TxTimeScan => {
            let start = time_key(from, &[0; 32]).to_vec();
            let end = time_key(to.saturating_sub(1), &[0xff; 32]).to_vec();
            let slab_order: Vec<usize> = if plan.ascending {
                (0..ctx.slabs.len()).rev().collect()
            } else {
                (0..ctx.slabs.len()).collect()
            };
            'all: for i in slab_order {
                let slab = ctx.slabs[i].clone();
                let iter = ctx
                    .snapshot
                    .range(&slab.time_tx, start.clone()..=end.clone());
                let mut forward;
                let mut backward;
                let iter: &mut dyn Iterator<Item = fjall::Guard> = if plan.ascending {
                    forward = iter;
                    &mut forward
                } else {
                    backward = iter.rev();
                    &mut backward
                };
                for guard in iter {
                    if !meter.tick() {
                        break 'all;
                    }
                    let (key, value) = guard.into_inner()?;
                    let Some((time_ms, txid)) = parse_time_key(&key) else {
                        continue;
                    };
                    let summary: TxSummary = decode(&value)?;
                    let mut row = Row::Tx(TxRow {
                        txid,
                        time_ms,
                        summary,
                        slab: i,
                        record: None,
                    });
                    if !take(&mut row, &mut ctx, &mut meter)? {
                        break 'all;
                    }
                }
            }
        }
        Source::BlockById(hashes) | Source::PayoutByBlock(hashes) => {
            let payouts = matches!(plan.source, Source::PayoutByBlock(_));
            'all: for hash in hashes {
                let Some(record) = ctx.block_record(hash)? else {
                    continue;
                };
                if !meter.tick() {
                    break 'all;
                }
                if record.time_ms < from || record.time_ms >= to {
                    continue;
                }
                if payouts {
                    for (index, payout) in record.payouts.iter().enumerate() {
                        let mut row = Row::Payout {
                            block: *hash,
                            time_ms: record.time_ms,
                            index,
                            miner: payout.addr,
                            amount: payout.amount,
                        };
                        if !take(&mut row, &mut ctx, &mut meter)? {
                            break 'all;
                        }
                    }
                } else {
                    let mut row = Row::Block {
                        hash: *hash,
                        record,
                    };
                    if !take(&mut row, &mut ctx, &mut meter)? {
                        break 'all;
                    }
                }
            }
        }
        Source::BlockTimeScan | Source::PayoutScan => {
            let payouts = matches!(plan.source, Source::PayoutScan);
            let start = time_key(from, &[0; 32]).to_vec();
            let end = time_key(to.saturating_sub(1), &[0xff; 32]).to_vec();
            let slab_order: Vec<usize> = if plan.ascending {
                (0..ctx.slabs.len()).rev().collect()
            } else {
                (0..ctx.slabs.len()).collect()
            };
            'all: for i in slab_order {
                let slab = ctx.slabs[i].clone();
                let iter = ctx
                    .snapshot
                    .range(&slab.time_blocks, start.clone()..=end.clone());
                let keys: Vec<Hash32> = if plan.ascending {
                    iter.map(|g| g.key().map(|k| parse_time_key(&k).map(|(_, h)| h)))
                        .collect::<fjall::Result<Vec<_>>>()?
                        .into_iter()
                        .flatten()
                        .collect()
                } else {
                    iter.rev()
                        .map(|g| g.key().map(|k| parse_time_key(&k).map(|(_, h)| h)))
                        .collect::<fjall::Result<Vec<_>>>()?
                        .into_iter()
                        .flatten()
                        .collect()
                };
                for hash in keys {
                    if !meter.tick() {
                        break 'all;
                    }
                    let Some(bytes) = ctx.snapshot.get(&slab.blocks, hash)? else {
                        continue;
                    };
                    let record: BlockRecord = decode(&bytes)?;
                    if payouts {
                        if record.kind != BlockKind::Chain {
                            continue;
                        }
                        for (index, payout) in record.payouts.iter().enumerate() {
                            let mut row = Row::Payout {
                                block: hash,
                                time_ms: record.time_ms,
                                index,
                                miner: payout.addr,
                                amount: payout.amount,
                            };
                            if !take(&mut row, &mut ctx, &mut meter)? {
                                break 'all;
                            }
                        }
                    } else {
                        let mut row = Row::Block { hash, record };
                        if !take(&mut row, &mut ctx, &mut meter)? {
                            break 'all;
                        }
                    }
                }
            }
        }
        Source::AddrById(ids) => {
            'all: for &id in ids {
                if !meter.tick() {
                    break 'all;
                }
                let stats = ctx.stats(id)?;
                if stats.tx_count == 0 {
                    continue;
                }
                let Some(address) = ctx.name(id)? else {
                    continue;
                };
                let mut row = Row::Addr(AddrRow { id, address, stats });
                if !take(&mut row, &mut ctx, &mut meter)? {
                    break 'all;
                }
            }
        }
        Source::AddrFromLabels => {
            let mut candidates: Vec<String> = inputs
                .labels
                .all()
                .into_iter()
                .map(|(a, _)| a.to_string())
                .collect();
            candidates.extend(ctx.watched.iter().cloned());
            candidates.sort_unstable();
            candidates.dedup();
            'all: for address in candidates {
                if !meter.tick() {
                    break 'all;
                }
                let Some(id) = store.lookup(&address)? else {
                    continue;
                };
                let stats = ctx.stats(id)?;
                if stats.tx_count == 0 {
                    continue;
                }
                let mut row = Row::Addr(AddrRow { id, address, stats });
                if !take(&mut row, &mut ctx, &mut meter)? {
                    break 'all;
                }
            }
        }
        Source::AddrStatsScan => {
            let mut merged: HashMap<AddrId, AddrStats> = HashMap::new();
            'all: for slab in ctx.slabs.clone() {
                for guard in ctx.snapshot.iter(&slab.stats) {
                    if !meter.tick() {
                        break 'all;
                    }
                    let (key, value) = guard.into_inner()?;
                    let Some(id) = parse_addr_key(&key) else {
                        continue;
                    };
                    let stats: AddrStats = decode(&value)?;
                    if !merged.contains_key(&id) && merged.len() >= MAX_ADDRESS_SCAN {
                        partial = Some(Partial::AddressCap);
                        break 'all;
                    }
                    merged.entry(id).or_default().merge(&stats);
                }
            }
            if meter.stop.is_none() {
                'all: for (id, stats) in merged {
                    if !meter.tick() {
                        break 'all;
                    }
                    if stats.tx_count == 0 {
                        continue;
                    }
                    let Some(address) = ctx.name(id)? else {
                        continue;
                    };
                    let mut row = Row::Addr(AddrRow { id, address, stats });
                    if !take(&mut row, &mut ctx, &mut meter)? {
                        break 'all;
                    }
                }
            }
        }
    }

    let partial = meter.stop.or(partial);
    let matched = meter.matched;
    let scanned = meter.scanned;
    let elapsed = meter.started.elapsed();
    let (columns, rows, truncated, capped) = match sink {
        Sink::List { columns, top, .. } => {
            let (rows, dropped) = top.finish();
            let columns = columns
                .into_iter()
                .map(|f| Column {
                    name: f.name().to_string(),
                    label: f.label().to_string(),
                    kind: f.kind(),
                    source: ColumnSource::Field(f),
                })
                .collect();
            (columns, rows, dropped, false)
        }
        Sink::Groups {
            keys,
            metrics,
            groups,
            capped,
        } => {
            let mut columns: Vec<Column> = keys
                .iter()
                .map(|k| match k {
                    GroupKey::Field(f) => Column {
                        name: f.name().to_string(),
                        label: f.label().to_string(),
                        kind: f.kind(),
                        source: ColumnSource::Field(*f),
                    },
                    GroupKey::TimeBucket(w) => Column {
                        name: "bucket".to_string(),
                        label: format!("Time ({})", format_duration_ms(*w)),
                        kind: FieldKind::Time,
                        source: ColumnSource::Bucket,
                    },
                })
                .collect();
            columns.extend(metrics.iter().enumerate().map(|(i, m)| Column {
                name: m.name(),
                label: m.name(),
                kind: m.kind(),
                source: ColumnSource::Metric(i),
            }));
            let mut rows: Vec<Vec<Cell>> = groups
                .into_iter()
                .map(|(key, accs)| {
                    let mut row: Vec<Cell> = key.into_iter().map(|k| k.0).collect();
                    row.extend(
                        accs.into_iter()
                            .zip(metrics.iter())
                            .map(|(a, m)| a.finish(*m)),
                    );
                    row
                })
                .collect();
            // Order by the asked keys; by the first metric descending when none.
            let order: Vec<(usize, Dir)> = if q.order.is_empty() {
                vec![(keys.len(), Dir::Desc)]
            } else {
                q.order
                    .iter()
                    .filter_map(|(k, d)| {
                        let i = match k {
                            OrderKey::Field(f) => {
                                keys.iter().position(|k| *k == GroupKey::Field(*f))?
                            }
                            OrderKey::Bucket => keys
                                .iter()
                                .position(|k| matches!(k, GroupKey::TimeBucket(_)))?,
                            OrderKey::Metric(i) => keys.len() + i,
                        };
                        Some((i, *d))
                    })
                    .collect()
            };
            rows.sort_by(|a, b| {
                for (i, dir) in &order {
                    let o = a[*i].compare(&b[*i]);
                    let o = match dir {
                        Dir::Asc => o,
                        Dir::Desc => o.reverse(),
                    };
                    if o != Ordering::Equal {
                        return o;
                    }
                }
                Ordering::Equal
            });
            let truncated = rows.len() > limit;
            rows.truncate(limit);
            (columns, rows, truncated, capped)
        }
    };
    let partial = partial.or(capped.then_some(Partial::GroupCap));
    let primary = columns
        .iter()
        .position(|c| c.source == ColumnSource::Field(q.entity.primary_field()));
    let time_column = columns.iter().position(|c| {
        c.source == ColumnSource::Bucket
            || time_field.is_some_and(|t| c.source == ColumnSource::Field(t))
    });
    let balances_pending = q.entity == Entity::Addresses
        && columns
            .iter()
            .any(|c| c.source == ColumnSource::Field(FieldId::AddrBalance))
        && !rows.is_empty();
    Ok(ResultSet {
        entity: q.entity,
        columns,
        rows,
        matched,
        scanned,
        truncated,
        partial,
        elapsed,
        window: plan.window,
        plan: explain,
        primary,
        time_column,
        balances_pending,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::index::writer::IndexWriter;
    use crate::index::writer::testing::{
        address, chain_block, chain_block_full, chain_hash, coinbase, hash, in_block, merged_hash,
        tx,
    };
    use crate::index::{TempStore, temp_store};
    use crate::query::text::parse;
    use crate::watch::WatchEntry;

    const HOUR: u64 = 3_600_000;
    /// "Now" for the tests: an hour after the newest block.
    const NOW: u64 = 10 * HOUR;

    struct Fixture {
        store: TempStore,
        labels: LabelBook,
        watchlist: Watchlist,
    }

    impl Fixture {
        fn inputs(&self) -> Inputs<'_> {
            Inputs {
                store: &self.store.store,
                labels: &self.labels,
                watchlist: &self.watchlist,
                now_ms: NOW,
                prune_floor_ms: None,
            }
        }

        fn run(&self, text: &str) -> ResultSet {
            let q = parse(text).unwrap_or_else(|e| panic!("{text}: {e}"));
            run(&self.inputs(), &q, &mut RunControl::default())
                .unwrap_or_else(|e| panic!("{text}: {e}"))
        }

        fn plan(&self, text: &str) -> Plan {
            plan(&parse(text).unwrap(), &self.store.store, NOW, None).unwrap()
        }
    }

    /// Three chain blocks an hour apart. Block 1 (hour 7): block 0's coinbase (no
    /// payload) paying address 3, a transfer 1 → 2 with change, and a KRC inscription
    /// 4 → 5. Block 2 (hour 8):
    /// block 1's coinbase (miner 9, pool-x, paying 7 and 8), a self transfer 2 → 2, a
    /// fan-out 1 → 2, 5, 6. Block 3 (hour 9): block 2's coinbase (miner 10, pool-y),
    /// a Kasia message 6 → 1. Address 2 is labelled "Exchange A", 9 "Pool X".
    fn fixture() -> Fixture {
        let store = temp_store();
        let mut labels = LabelBook::base();
        labels
            .set_user(&address(2).to_string(), Some("Exchange A"))
            .unwrap();
        labels.set_heuristic(&address(9).to_string(), "Pool X");
        let mut writer = IndexWriter::new(store.store.clone(), Arc::new(labels.clone())).unwrap();
        let mut krc = tx(3, &[(4, 1_000)], &[(5, 990)]);
        krc.inputs[0].signature_script = Some(vec![7, b'k', b'a', b's', b'p', b'l', b'e', b'x']);
        let mut cb1 = coinbase(10, 9, "1.2.3", "pool-x", 11, 500, &[(7, 400), (8, 100)]);
        in_block(&mut cb1, chain_hash(1), 7 * HOUR);
        let mut cb2 = coinbase(11, 10, "1.2.4", "pool-y", 12, 500, &[(9, 500)]);
        in_block(&mut cb2, chain_hash(2), 8 * HOUR);
        let mut kasia = tx(13, &[(6, 300)], &[(1, 290)]);
        kasia.payload = Some(b"ciph_msg hello".to_vec());
        let mut cb0 = tx(1, &[], &[(3, 5_000)]);
        in_block(&mut cb0, chain_hash(0), 6 * HOUR);
        let r = crate::index::writer::testing::response(
            vec![],
            vec![
                chain_block_full(
                    1,
                    7 * HOUR,
                    vec![cb0, tx(2, &[(1, 10_000)], &[(2, 6_000), (1, 3_999)]), krc],
                ),
                chain_block_full(
                    2,
                    8 * HOUR,
                    vec![
                        cb1,
                        tx(12, &[(2, 100)], &[(2, 99)]),
                        tx(14, &[(1, 3_000)], &[(2, 1_000), (5, 1_000), (6, 998)]),
                    ],
                ),
                chain_block_full(3, 9 * HOUR, vec![cb2, kasia]),
            ],
        );
        writer.apply(&r).unwrap();
        let mut watchlist = Watchlist::default();
        watchlist.entries.push(WatchEntry {
            address: address(6).to_string(),
            network: "mainnet".to_string(),
            ..WatchEntry::default()
        });
        Fixture {
            store,
            labels,
            watchlist,
        }
    }

    fn col(r: &ResultSet, name: &str) -> usize {
        r.columns
            .iter()
            .position(|c| c.name == name)
            .unwrap_or_else(|| panic!("no column {name} in {:?}", r.columns))
    }

    fn txids(r: &ResultSet) -> Vec<u64> {
        let i = col(r, "txid");
        r.rows
            .iter()
            .map(|row| match &row[i] {
                Cell::Txid(h) => u64::from_be_bytes(h[..8].try_into().unwrap()),
                other => panic!("{other:?}"),
            })
            .collect()
    }

    #[test]
    fn plan_picks_the_cheapest_access_path() {
        let f = fixture();
        let a = address(1).to_string();
        assert!(
            matches!(f.plan(&format!("tx where txid = {}", hash(2))).source, Source::TxById(ref ids) if ids.len() == 1)
        );
        assert!(
            matches!(f.plan(&format!("tx where address = {a} and protocol = krc")).source, Source::TxByAddress(ref ids) if ids.len() == 1)
        );
        assert!(
            matches!(f.plan(&format!("tx where sender in ({a}, kaspa:unknown)")).source, Source::TxByAddress(ref ids) if ids.len() == 1)
        );
        assert!(
            matches!(f.plan("tx where protocol = krc").source, Source::TxByProtocol(ref ps) if ps == &[TransactionProtocol::Krc])
        );
        assert!(matches!(
            f.plan(&format!("tx where accepting_block = {}", chain_hash(1)))
                .source,
            Source::TxByAcceptingBlock(_)
        ));
        assert!(matches!(
            f.plan("tx where fee > 1 sompi or protocol = krc").source,
            Source::TxTimeScan
        ));
        assert!(matches!(
            f.plan(&format!("blocks where hash = {}", chain_hash(1)))
                .source,
            Source::BlockById(_)
        ));
        assert!(matches!(
            f.plan("blocks where is_chain").source,
            Source::BlockTimeScan
        ));
        assert!(matches!(
            f.plan(&format!("payouts where block = {}", chain_hash(2)))
                .source,
            Source::PayoutByBlock(_)
        ));
        assert!(matches!(
            f.plan(&format!("addresses where address = {a}")).source,
            Source::AddrById(_)
        ));
        assert!(matches!(
            f.plan("addresses where label contains \"exch\"").source,
            Source::AddrFromLabels
        ));
        assert!(matches!(
            f.plan("addresses where is_watched").source,
            Source::AddrFromLabels
        ));
        assert!(matches!(
            f.plan("addresses where label is null").source,
            Source::AddrStatsScan
        ));
        assert!(matches!(
            f.plan("addresses where received > 1 KAS").source,
            Source::AddrStatsScan
        ));

        let p = f.plan("tx last 1h limit 5");
        assert!(p.early_stop && !p.ascending);
        assert_eq!(p.window.0, NOW - HOUR);
        assert_eq!(p.slabs.len(), 1);
        let p = f.plan("tx all time order by time asc limit 5");
        assert!(p.early_stop && p.ascending);
        assert!(!f.plan("tx order by fee desc limit 5").early_stop);
        assert!(!f.plan("tx limit 5 count by protocol").early_stop);
        assert!(!f.plan("tx all time").early_stop);
        assert!(
            f.plan(&format!("tx where address = {a} limit 1"))
                .early_stop
        );
        assert!(
            f.plan("blocks last 1d limit 1")
                .explain()
                .contains("newest first")
        );
    }

    #[test]
    fn time_scan_filters_from_summaries_and_stops_early() {
        let f = fixture();
        let r = f.run("tx all time order by time desc limit 2");
        assert_eq!(txids(&r), vec![13, 11]);
        assert_eq!(r.matched, 2);
        assert!(r.scanned <= 3, "stopped after the limit: {}", r.scanned);
        assert!(!r.truncated);
        assert_eq!(r.primary, Some(col(&r, "txid")));
        assert_eq!(r.time_column, Some(col(&r, "time")));

        let r = f.run("tx all time where fee > 2 sompi order by fee desc, time desc");
        assert_eq!(txids(&r), vec![13, 3]);
        let r = f.run("tx all time where fee between 2 sompi and 5 sompi");
        assert_eq!(txids(&r), vec![14]);
        let r = f.run("tx all time where is_coinbase order by time asc");
        assert_eq!(txids(&r), vec![1, 10, 11]);
        let r = f.run("tx all time where output_count >= 3");
        assert_eq!(txids(&r), vec![14]);
        let r = f.run("tx all time where self_transfer");
        assert_eq!(txids(&r), vec![12]);
        let r = f.run("tx last 30m");
        assert!(r.rows.is_empty());
        let r = f.run("tx last 1h30m");
        assert_eq!(txids(&r), vec![13, 11]);
        let r = f.run("tx between 1970-01-01T07:00Z and 1970-01-01T08:00Z");
        assert_eq!(r.rows.len(), 3);
        let r = f.run("tx all time where time < 2h");
        assert_eq!(txids(&r), vec![3, 2, 1]);
    }

    #[test]
    fn record_and_lookup_fields() {
        let f = fixture();
        let a2 = address(2).to_string();
        let r = f.run("tx all time where protocol = krc");
        assert_eq!(txids(&r), vec![3]);
        let r = f.run(
            "tx all time where protocol = kasia select txid, payload, sender, receiver, label",
        );
        assert_eq!(txids(&r), vec![13]);
        assert_eq!(
            r.rows[0][col(&r, "payload")],
            Cell::Text("ciph_msg hello".into())
        );
        assert_eq!(
            r.rows[0][col(&r, "sender")],
            Cell::List(vec![Cell::Address(address(6).to_string())])
        );
        let r = f.run(&format!(
            "tx all time where receiver = {a2} order by time asc"
        ));
        assert_eq!(txids(&r), vec![2, 12, 14]);
        let r = f.run(&format!("tx all time where sender = {a2}"));
        assert_eq!(txids(&r), vec![12]);
        let r = f.run(&format!(
            "tx all time where address = {a2} and not self_transfer order by time asc"
        ));
        assert_eq!(txids(&r), vec![2, 14]);
        let r = f.run("tx all time where receiver_label contains \"exchange\" order by time asc");
        assert_eq!(txids(&r), vec![2, 12, 14]);
        let r = f.run("tx all time where label is null order by time asc");
        assert_eq!(txids(&r), vec![1, 3, 10, 13], "11 pays the labelled pool");
        let r = f.run("tx all time where is_watched");
        assert_eq!(txids(&r), vec![13, 14]);
        let r = f.run("tx all time where payload contains \"HELLO\"");
        assert_eq!(txids(&r), vec![13]);
        let r = f.run(&format!(
            "tx all time where accepting_block = {}",
            chain_hash(2)
        ));
        let mut ids = txids(&r);
        ids.sort_unstable();
        assert_eq!(ids, vec![10, 12, 14]);
        let r = f.run(&format!("tx all time where block = {}", merged_hash(2)));
        assert_eq!(txids(&r), vec![14, 12]);
        let r = f.run(&format!(
            "tx all time where txid in ({}, {})",
            hash(2),
            hash(99)
        ));
        assert_eq!(txids(&r), vec![2]);
        let r = f.run("tx all time where distinct_addresses >= 4");
        assert_eq!(txids(&r), vec![14]);
        let r = f.run("tx all time where change_max >= 70");
        assert_eq!(txids(&r), vec![2]);
        let r = f.run("tx all time where subnetwork is null and version is null limit 1");
        assert_eq!(r.rows.len(), 1);
    }

    #[test]
    fn blocks_and_payouts() {
        let f = fixture();
        let r = f.run("blocks all time where is_chain order by time asc select hash, miner, miner_tag, blue_score, difficulty, accepted_txs, pays, reward, version");
        // Block 0 is known from its coinbase alone (no header, no payload).
        assert_eq!(r.rows.len(), 4);
        assert_eq!(
            r.rows[0][col(&r, "hash")],
            Cell::Hash(chain_hash(0).as_bytes())
        );
        assert_eq!(r.rows[0][col(&r, "version")], Cell::Null);
        assert_eq!(r.rows[0][col(&r, "accepted_txs")], Cell::Int(1));
        assert_eq!(
            r.rows[0][col(&r, "pays")],
            Cell::List(vec![Cell::Address(address(3).to_string())])
        );
        let b1 = &r.rows[1];
        assert_eq!(b1[col(&r, "hash")], Cell::Hash(chain_hash(1).as_bytes()));
        assert_eq!(b1[col(&r, "miner")], Cell::Address(address(9).to_string()));
        assert_eq!(b1[col(&r, "miner_tag")], Cell::Text("pool-x".into()));
        assert_eq!(b1[col(&r, "blue_score")], Cell::Int(11));
        assert!(matches!(b1[col(&r, "difficulty")], Cell::Float(d) if (d - 1.0).abs() < 1e-6));
        assert_eq!(b1[col(&r, "accepted_txs")], Cell::Int(1));
        assert_eq!(
            b1[col(&r, "pays")],
            Cell::List(vec![
                Cell::Address(address(7).to_string()),
                Cell::Address(address(8).to_string())
            ])
        );
        assert_eq!(b1[col(&r, "reward")], Cell::Amount(500));
        assert_eq!(r.rows[3][col(&r, "miner")], Cell::Null);
        let r = f.run("blocks all time where not is_chain order by accepted_txs desc");
        assert_eq!(r.rows.len(), 3);
        let r = f.run("blocks all time where miner_label = \"Pool X\"");
        assert_eq!(r.rows.len(), 1);
        let r = f.run(&format!(
            "blocks all time where parents contains {}",
            chain_hash(1)
        ));
        assert_eq!(
            r.rows[0][col(&r, "hash")],
            Cell::Hash(chain_hash(2).as_bytes())
        );
        let r = f.run("blocks all time where node_version starts_with \"1.2\" count by node_version order by node_version");
        assert_eq!(
            r.rows,
            vec![
                vec![Cell::Text("1.2.3".into()), Cell::Int(1)],
                vec![Cell::Text("1.2.4".into()), Cell::Int(1)]
            ]
        );

        let r = f.run("payouts all time order by amount desc");
        assert_eq!(r.rows.len(), 4);
        assert_eq!(r.rows[0][col(&r, "amount")], Cell::Amount(5_000));
        assert_eq!(
            r.rows[0][col(&r, "miner")],
            Cell::Address(address(3).to_string())
        );
        assert_eq!(
            r.rows[1][col(&r, "miner")],
            Cell::Address(address(9).to_string())
        );
        let r = f.run("payouts all time count, sum(amount) by miner_label order by count desc");
        assert_eq!(
            r.rows[0],
            vec![Cell::Null, Cell::Int(3), Cell::Amount(5_500)]
        );
        assert_eq!(
            r.rows[1],
            vec![Cell::Text("Pool X".into()), Cell::Int(1), Cell::Amount(500)]
        );
        let r = f.run("payouts last 2h where miner_label = \"Pool X\"");
        assert_eq!(r.rows.len(), 1);
        assert!(r.plan.contains("every chain block's payouts"), "{}", r.plan);
        let r = f.run(&format!("payouts all time where block = {}", chain_hash(2)));
        assert_eq!(r.rows.len(), 1);
        // Block 3's coinbase hasn't arrived (it comes with block 4).
        let r = f.run(&format!("payouts all time where block = {}", chain_hash(3)));
        assert!(r.rows.is_empty());
    }

    #[test]
    fn addresses_scan_merge_and_lookups() {
        let f = fixture();
        let r = f.run("addresses order by received desc select address, received, sent, tx_count, net, label, balance");
        assert_eq!(
            r.rows[0][col(&r, "address")],
            Cell::Address(address(2).to_string())
        );
        assert_eq!(r.rows[0][col(&r, "received")], Cell::Amount(7_099));
        assert_eq!(r.rows[0][col(&r, "sent")], Cell::Amount(100));
        assert_eq!(r.rows[0][col(&r, "net")], Cell::Amount(6_999));
        assert_eq!(r.rows[0][col(&r, "tx_count")], Cell::Int(3));
        assert_eq!(r.rows[0][col(&r, "label")], Cell::Text("Exchange A".into()));
        assert_eq!(r.rows[0][col(&r, "balance")], Cell::Null);
        assert!(r.balances_pending);
        let r = f.run("addresses where label contains \"exchange\"");
        assert_eq!(r.rows.len(), 1);
        let r = f.run("addresses where label_source = user");
        assert_eq!(r.rows.len(), 1);
        let r = f.run("addresses where is_watched");
        assert_eq!(
            r.rows[0][col(&r, "address")],
            Cell::Address(address(6).to_string())
        );
        let r =
            f.run("addresses where address_type = p2pk and tx_count >= 3 order by tx_count desc");
        assert_eq!(r.rows.len(), 2);
        // Single-input spends don't cluster: everyone is alone.
        let r = f.run("addresses where cluster_size > 1");
        assert!(r.rows.is_empty());
        let r = f.run(&format!(
            "addresses where address = {} select cluster_size, cluster_label, peer_count",
            address(1)
        ));
        assert_eq!(r.rows[0], vec![Cell::Int(1), Cell::Null, Cell::Int(3)]);
        let r = f.run(&format!(
            "addresses where address = {} select address, first_seen, last_seen",
            address(1)
        ));
        assert_eq!(r.rows[0][1], Cell::Time(7 * HOUR));
        assert_eq!(r.rows[0][2], Cell::Time(9 * HOUR));
        // The window clips the per-slab totals.
        let r = f.run("addresses where first_seen > 1h30m select address, tx_count");
        assert!(r.rows.iter().all(|row| row[1] != Cell::Null));
    }

    #[test]
    fn group_by_time_buckets_and_metrics() {
        let f = fixture();
        let r = f.run("tx all time count, sum(output_total), avg(fee), min(time), max(output_count), count_distinct(protocol) by time(1h) order by bucket");
        assert_eq!(
            r.columns
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            vec![
                "bucket",
                "count",
                "sum(output_total)",
                "avg(fee)",
                "min(time)",
                "max(output_count)",
                "count_distinct(protocol)"
            ]
        );
        assert_eq!(r.rows.len(), 3);
        assert_eq!(r.rows[0][0], Cell::Time(7 * HOUR));
        assert_eq!(r.rows[0][1], Cell::Int(3));
        assert_eq!(r.rows[0][2], Cell::Amount(5_000 + 9_999 + 990));
        assert!(matches!(r.rows[0][3], Cell::Float(f) if (f - 5.5).abs() < 1e-9));
        assert_eq!(r.rows[0][4], Cell::Time(7 * HOUR));
        assert_eq!(r.rows[0][5], Cell::Int(2));
        assert_eq!(r.rows[0][6], Cell::Int(1));
        assert_eq!(r.time_column, Some(0));
        let r = f.run("tx all time count by protocol order by count desc, protocol");
        assert_eq!(r.rows[0][0], Cell::Null);
        assert_eq!(r.rows.len(), 3);
        let r = f.run("tx all time count by is_coinbase limit 1");
        assert_eq!(r.rows.len(), 1);
        assert!(r.truncated);
    }

    #[test]
    fn top_k_and_truncation() {
        let f = fixture();
        let r = f.run("tx all time order by output_total desc limit 2");
        assert_eq!(txids(&r), vec![2, 1]);
        assert!(r.truncated);
        assert_eq!(r.matched, 8);
        let r = f.run("tx all time order by fee asc, time desc limit 3");
        assert_eq!(
            txids(&r),
            vec![11, 10, 1],
            "coinbases have no fee: nulls first, newest first"
        );
        let r = f.run("tx all time order by output_total desc");
        assert_eq!(r.rows.len(), 8);
        assert!(!r.truncated);
    }

    #[test]
    fn cancel_and_budget_mark_partial() {
        let f = fixture();
        let q = parse("tx all time").unwrap();
        let mut ctl = RunControl::default();
        ctl.cancel.store(true, AtomicOrdering::Relaxed);
        // Checked every CHECK_EVERY rows: too few here to trip, so the result is whole.
        let r = run(&f.inputs(), &q, &mut ctl).unwrap();
        assert_eq!(r.partial, None);
        let mut ctl = RunControl {
            deadline: Some(Instant::now()),
            ..RunControl::default()
        };
        ctl.progress = Some(Box::new(|_| {}));
        let r = run(&f.inputs(), &q, &mut ctl).unwrap();
        assert_eq!(r.rows.len(), 8);
        assert!(r.elapsed < Duration::from_secs(5));
    }

    #[test]
    fn snapshot_ignores_a_later_commit() {
        let f = fixture();
        let inputs = f.inputs();
        let q = parse("tx all time").unwrap();
        let before = run(&inputs, &q, &mut RunControl::default()).unwrap();
        let mut writer =
            IndexWriter::new(f.store.store.clone(), Arc::new(LabelBook::base())).unwrap();
        writer
            .apply(&crate::index::writer::testing::response(
                vec![],
                vec![chain_block(
                    4,
                    9 * HOUR + 1,
                    vec![tx(20, &[(1, 10)], &[(2, 9)])],
                )],
            ))
            .unwrap();
        let after = run(&inputs, &q, &mut RunControl::default()).unwrap();
        assert_eq!(after.rows.len(), before.rows.len() + 1);
    }

    #[test]
    fn cells_compare_render_and_match() {
        assert_eq!(Cell::Null.compare(&Cell::Int(1)), Ordering::Less);
        assert_eq!(Cell::Int(2).compare(&Cell::Float(1.5)), Ordering::Greater);
        assert_eq!(
            Cell::Text("b".into()).compare(&Cell::Text("A".into())),
            Ordering::Greater
        );
        assert_eq!(Cell::Amount(150_000_000).text(), "1.5");
        assert_eq!(Cell::Time(0).text(), "1970-01-01T00:00:00Z");
        assert_eq!(Cell::List(vec![Cell::Int(1), Cell::Int(2)]).text(), "1; 2");
        assert_eq!(Cell::Amount(5).to_json(), serde_json::json!(5));
        assert_eq!(
            Cell::Hash([1; 32]).to_json(),
            serde_json::json!("01".repeat(32))
        );
        assert!(matches(
            &Cell::Int(5),
            false,
            Op::Between,
            &Value::List(vec![Value::Int(1), Value::Int(5)])
        ));
        assert!(!matches(
            &Cell::Int(6),
            false,
            Op::Between,
            &Value::List(vec![Value::Int(1), Value::Int(5)])
        ));
        assert!(matches(
            &Cell::Text("Binance Hot".into()),
            false,
            Op::Contains,
            &Value::Text("binance".into())
        ));
        assert!(matches(
            &Cell::Enum("krc"),
            false,
            Op::In,
            &Value::List(vec![Value::Enum("kns".into()), Value::Enum("krc".into())])
        ));
        let list = Cell::List(vec![Cell::Address("a".into()), Cell::Address("b".into())]);
        assert!(matches(&list, true, Op::Eq, &Value::Address("b".into())));
        assert!(!matches(&list, true, Op::Ne, &Value::Address("b".into())));
        assert!(matches(&list, true, Op::Ne, &Value::Address("c".into())));
        assert!(matches(&Cell::List(vec![]), true, Op::IsNull, &Value::Null));
        assert!(
            !matches(&Cell::Null, false, Op::Ne, &Value::Int(1)),
            "null is never anything"
        );
        assert!(matches(&Cell::Null, false, Op::IsNull, &Value::Null));
        assert!(
            !matches(&Cell::Int(1), false, Op::Eq, &Value::Text("1".into())),
            "kinds don't mix"
        );
    }
}
