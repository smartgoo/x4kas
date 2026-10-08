use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::analytics::{AggregatedView, AnalyticsEngine, TxHistogram};
use crate::emission::{BlockReward, Emission};
use crate::index::query::{AddressProfile, ClusterInfo, FlowGraph, Page, Peer, TxRow};
use crate::labels::{LabelBook, LabelSettings};
use crate::rpc::hash_links::{HashLink, block_hash_links};
use crate::rpc::methods::{RPC_METHODS, RpcMethod};
use crate::rpc::types::*;
use crate::watch::{AddressEvent, Alert, WatchEntry, Watchlist};

/// A block from the node's `BlockAdded` stream, as the DAG visualizer needs it.
#[derive(Debug, Clone, PartialEq)]
pub struct DagBlock {
    pub hash: String,
    pub daa_score: u64,
    /// Direct (level 0) parents.
    pub parents: Vec<String>,
}

/// How many of the newest DAA scores the visualizer keeps.
pub const DAG_MAX_DAA_SCORES: usize = 100;

/// Recent blocks grouped by DAA score (one column per score), for the DAG visualizer.
#[derive(Debug, Clone, Default)]
pub struct DagVisualizer {
    /// Blocks per DAA score in arrival order, oldest score first.
    pub columns: BTreeMap<u64, Vec<DagBlock>>,
    hashes: HashSet<String>,
}

impl DagVisualizer {
    /// Add a block. Returns false for a duplicate, or a block older than every kept
    /// score once the window is full.
    pub fn add(&mut self, block: DagBlock) -> bool {
        if self.hashes.contains(&block.hash) {
            return false;
        }
        if self.columns.len() >= DAG_MAX_DAA_SCORES
            && !self.columns.contains_key(&block.daa_score)
            && self
                .columns
                .first_key_value()
                .is_some_and(|(&oldest, _)| block.daa_score < oldest)
        {
            return false;
        }
        self.hashes.insert(block.hash.clone());
        self.columns.entry(block.daa_score).or_default().push(block);
        while self.columns.len() > DAG_MAX_DAA_SCORES {
            if let Some((_, dropped)) = self.columns.pop_first() {
                for b in dropped {
                    self.hashes.remove(&b.hash);
                }
            }
        }
        true
    }

    pub fn is_empty(&self) -> bool {
        self.hashes.is_empty()
    }

    pub fn blocks(&self) -> impl Iterator<Item = &DagBlock> {
        self.columns.values().flatten()
    }

    /// Blocks no other kept block names as a parent: the DAG tips, as far as we can see.
    pub fn tips(&self) -> HashSet<&str> {
        let referenced: HashSet<&str> = self
            .blocks()
            .flat_map(|b| b.parents.iter().map(String::as_str))
            .collect();
        self.blocks()
            .map(|b| b.hash.as_str())
            .filter(|h| !referenced.contains(h))
            .collect()
    }
}

#[derive(Debug, Clone)]
pub struct DagSample {
    pub timestamp: Instant,
    pub blue_score: u64,
    pub tip_count: usize,
}

/// How far back [`DagStats::avg_dag_width`] averages.
pub const DAG_WIDTH_WINDOW: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Default)]
pub struct DagStats {
    pub samples: VecDeque<DagSample>,
}

impl DagStats {
    pub fn update(&mut self, dag_info: &DagInfo, blue_score: Option<u64>) {
        self.samples.push_back(DagSample {
            timestamp: Instant::now(),
            blue_score: blue_score.unwrap_or(0),
            tip_count: dag_info.tip_hashes.len(),
        });
        while self.samples.len() > 120 {
            self.samples.pop_front();
        }
    }

    pub fn blue_block_rate(&self) -> Option<f64> {
        if self.samples.len() < 2 {
            return None;
        }
        let first = self.samples.front()?;
        let last = self.samples.back()?;
        let elapsed = last.timestamp.duration_since(first.timestamp).as_secs_f64();
        if elapsed < 0.1 {
            return None;
        }
        let delta = last.blue_score.saturating_sub(first.blue_score) as f64;
        Some(delta / elapsed)
    }

    /// The average tip count over the samples from the last [`DAG_WIDTH_WINDOW`].
    pub fn avg_dag_width(&self) -> Option<f64> {
        let last = self.samples.back()?;
        let recent: Vec<usize> = self
            .samples
            .iter()
            .rev()
            .take_while(|s| last.timestamp.duration_since(s.timestamp) <= DAG_WIDTH_WINDOW)
            .map(|s| s.tip_count)
            .collect();
        Some(recent.iter().sum::<usize>() as f64 / recent.len() as f64)
    }

    pub fn block_interval_ms(&self) -> Option<f64> {
        if self.samples.len() < 2 {
            return None;
        }
        let first = self.samples.front()?;
        let last = self.samples.back()?;
        let delta = last.blue_score.saturating_sub(first.blue_score);
        if delta == 0 {
            return None;
        }
        let elapsed_ms = last.timestamp.duration_since(first.timestamp).as_secs_f64() * 1000.0;
        Some(elapsed_ms / delta as f64)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tab {
    #[default]
    Dashboard,
    Monitoring,
    Mempool,
    RpcExplorer,
}

impl Tab {
    pub fn all() -> &'static [Tab] {
        &[
            Tab::Dashboard,
            Tab::Monitoring,
            Tab::Mempool,
            Tab::RpcExplorer,
        ]
    }

    /// Name in the tab strip, after its number shortcut (its position in [`Self::all`] + 1).
    pub fn label(&self) -> &'static str {
        match self {
            Tab::Dashboard => "Dashboard",
            Tab::Monitoring => "Monitoring",
            Tab::Mempool => "Mempool",
            Tab::RpcExplorer => "RPC Cmds",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TimeWindow {
    #[default]
    OneMin,
    OneHour,
    TwentyFourHour,
}

impl TimeWindow {
    pub const ALL: [Self; 3] = [Self::OneMin, Self::OneHour, Self::TwentyFourHour];

    pub fn label(&self) -> &'static str {
        match self {
            Self::OneMin => "1m",
            Self::OneHour => "1h",
            Self::TwentyFourHour => "24h",
        }
    }

    /// Position in [`Self::ALL`].
    pub fn index(&self) -> usize {
        *self as usize
    }

    pub fn duration_ms(&self) -> u64 {
        match self {
            Self::OneMin => 60_000,
            Self::OneHour => 3_600_000,
            Self::TwentyFourHour => 86_400_000,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub enum ConnectionStatus {
    #[default]
    Disconnected,
    Connecting,
    Connected,
    Error(String),
}

/// What the app is currently connected to (or trying to connect to).
#[derive(Debug, Clone, PartialEq, Default)]
pub enum ActiveConnection {
    #[default]
    None,
    Url(String),
    Resolver,
}

impl ActiveConnection {
    pub fn label(&self) -> &str {
        match self {
            ActiveConnection::None => "Not connected",
            ActiveConnection::Url(url) => url,
            ActiveConnection::Resolver => "Public resolver",
        }
    }

    /// Connected (or connecting) straight to a node by URL, which mining and analytics
    /// need; the resolver's public nodes are not used for them.
    pub fn is_direct(&self) -> bool {
        matches!(self, ActiveConnection::Url(_))
    }
}

pub struct RpcExplorerState {
    pub selected_method: usize,
    /// Every method in [`RPC_METHODS`], sorted by name.
    pub available_methods: Vec<&'static RpcMethod>,
    /// Argument inputs for the selected method, one per parameter.
    pub args: Vec<String>,
    /// Set through `set_response` so `hash_links` stays in sync.
    pub last_response: Option<String>,
    /// Block hashes in `last_response`, linked to `get_block` in the result viewer.
    pub hash_links: Vec<HashLink>,
    pub is_loading: bool,
    /// Re-run the selected (parameterless) method every `loop_interval_secs`.
    pub loop_enabled: bool,
    pub loop_interval_secs: f64,
    pub last_run: Option<Instant>,
}

impl Default for RpcExplorerState {
    fn default() -> Self {
        let mut available_methods: Vec<_> = RPC_METHODS.iter().collect();
        available_methods.sort_unstable_by_key(|m| m.name);
        let mut state = Self {
            selected_method: 0,
            available_methods,
            args: Vec::new(),
            last_response: None,
            hash_links: Vec::new(),
            is_loading: false,
            loop_enabled: false,
            loop_interval_secs: 1.0,
            last_run: None,
        };
        state.select(0);
        state
    }
}

impl RpcExplorerState {
    pub fn method(&self) -> Option<&'static RpcMethod> {
        self.available_methods.get(self.selected_method).copied()
    }

    /// Time left until the next loop run (`Duration::ZERO` if due), or `None` when
    /// not looping or a request is still in flight.
    pub fn loop_wait(&self, now: Instant) -> Option<Duration> {
        if !self.loop_enabled || self.is_loading {
            return None;
        }
        let interval = Duration::from_secs_f64(self.loop_interval_secs.max(0.1));
        Some(match self.last_run {
            Some(t) => interval.saturating_sub(now.saturating_duration_since(t)),
            None => Duration::ZERO,
        })
    }

    pub fn set_response(&mut self, response: Option<String>) {
        self.hash_links = response
            .as_deref()
            .map(block_hash_links)
            .unwrap_or_default();
        self.last_response = response;
    }

    /// Select `get_block` with `hash` filled in (the GUI then runs it).
    pub fn open_block(&mut self, hash: &str) {
        if let Some(i) = self
            .available_methods
            .iter()
            .position(|m| m.name == "get_block")
        {
            self.select(i);
            if let Some(arg) = self.args.first_mut() {
                *arg = hash.to_string();
            }
        }
    }

    /// Select a method, reset the argument inputs to its defaults and clear the last
    /// response. Stops any loop.
    pub fn select(&mut self, index: usize) {
        self.selected_method = index;
        self.loop_enabled = false;
        self.set_response(None);
        self.args = self
            .method()
            .map(|m| {
                m.params
                    .iter()
                    .map(|p| p.default.unwrap_or_default().to_string())
                    .collect()
            })
            .unwrap_or_default();
    }
}

#[derive(Default)]
pub struct NodeState {
    pub server_info: Option<ServerInfo>,
    pub dag_info: Option<DagInfo>,
    pub mempool_state: Option<MempoolState>,
    pub coin_supply: Option<CoinSupplyInfo>,
    pub fee_estimate: Option<FeeEstimateInfo>,
    /// Balance of the network's burn address in sompi (needs the node's UTXO index).
    pub burn_balance: Option<u64>,
    /// Estimated network hashrate in hashes per second.
    pub hashrate: Option<f64>,
    pub dag_visualizer: DagVisualizer,
    pub dag_stats: DagStats,
    pub sink_blue_score: Option<u64>,
    /// Header timestamp (unix ms) of the sink, the node's newest selected tip.
    pub sink_timestamp_ms: Option<u64>,
    /// Header timestamp (unix ms) of the pruning point: the address index prunes
    /// everything older.
    pub pruning_point_timestamp_ms: Option<u64>,
    pub node_url: Option<String>,
    pub node_uid: Option<String>,
    pub connection_status: ConnectionStatus,
    pub last_refresh: Option<Instant>,
    pub last_poll_duration_ms: Option<f64>,
    pub last_error: Option<String>,
}

impl NodeState {
    /// The current block reward and next reduction, from the network and DAA score.
    /// `None` until both are known, or on a network with an unknown schedule.
    pub fn block_reward(&self) -> Option<BlockReward> {
        let emission = Emission::for_network(&self.server_info.as_ref()?.network_id)?;
        emission.block_reward(self.dag_info.as_ref()?.virtual_daa_score)
    }
}

/// Analytics cards with their own time window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnalyticsPanel {
    TxSummary,
    Inspection,
    NodeVersions,
    Miners,
    TopSenders,
    TopReceivers,
}

/// DAA score growth per second: 10 blocks per second since Crescendo, on mainnet and
/// testnet-10.
pub const DAA_SCORE_PER_SEC: f64 = 10.0;

/// What the analytics streaming task is doing.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum AnalyticsPhase {
    /// No task running (not connected, or connected through the resolver).
    #[default]
    Idle,
    LoadingCache,
    /// Waiting for the node to connect and finish syncing.
    WaitingForNode,
    /// Finding where to start: skipping chain blocks older than the 24h window.
    Seeking,
    /// Fetching chain blocks quickly to reach the tip.
    CatchingUp,
    /// At the tip, fetching new chain blocks every second.
    Live,
    /// The last request failed; the task retries.
    Error(String),
}

/// Where analytics started reading the chain.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StartPoint {
    /// The last chain block in the saved cache, saved at the given time if known.
    Cache(Option<std::time::SystemTime>),
    PruningPoint,
    /// Skipped ahead from the cache or pruning point to the start of the 24h window.
    LastDay,
}

/// Progress of the chain stream that feeds analytics and the address index, shown in
/// the status bar and on the Dashboard tab.
#[derive(Debug, Clone, Default)]
pub struct AnalyticsStatus {
    pub phase: AnalyticsPhase,
    pub started_from: Option<StartPoint>,
    /// DAA score of the first processed chain block.
    pub start_daa: Option<u64>,
    /// DAA score of the newest processed chain block.
    pub current_daa: Option<u64>,
    /// Chain blocks processed since the task started.
    pub blocks_processed: u64,
    /// Smoothed catch-up speed in DAA score per second.
    pub daa_per_sec: Option<f64>,
    pub last_batch_at: Option<Instant>,
}

impl AnalyticsStatus {
    /// Catch-up progress between the start point and `tip_daa`, in `0.0..=1.0`.
    pub fn fraction(&self, tip_daa: u64) -> Option<f32> {
        let (start, current) = (self.start_daa?, self.current_daa?);
        if tip_daa <= start {
            return Some(1.0);
        }
        Some((current.saturating_sub(start) as f32 / (tip_daa - start) as f32).min(1.0))
    }

    /// How far the processed chain lags `tip_daa`: the DAA score difference and the
    /// time it spans on the network.
    pub fn behind(&self, tip_daa: u64) -> Option<(u64, Duration)> {
        let daa = tip_daa.saturating_sub(self.current_daa?);
        Some((daa, Duration::from_secs_f64(daa as f64 / DAA_SCORE_PER_SEC)))
    }

    /// Estimated time left to reach `tip_daa` at the current speed.
    pub fn eta(&self, tip_daa: u64) -> Option<Duration> {
        let rate = self.daa_per_sec.filter(|r| *r > 0.0)?;
        let remaining = tip_daa.saturating_sub(self.current_daa?);
        Some(Duration::from_secs_f64(remaining as f64 / rate))
    }

    /// Record a processed batch: `newest_daa` is its highest DAA score (0 if empty).
    pub fn record_batch(&mut self, blocks: usize, newest_daa: u64, now: Instant) {
        self.blocks_processed += blocks as u64;
        if newest_daa > 0 {
            self.start_daa.get_or_insert(newest_daa);
            if let (Some(prev_daa), Some(prev_at)) = (self.current_daa, self.last_batch_at) {
                let secs = now.duration_since(prev_at).as_secs_f64();
                if secs > 0.0 && newest_daa > prev_daa {
                    let rate = (newest_daa - prev_daa) as f64 / secs;
                    // Exponential moving average, so the estimate doesn't jump around.
                    self.daa_per_sec = Some(match self.daa_per_sec {
                        Some(r) => r * 0.8 + rate * 0.2,
                        None => rate,
                    });
                }
            }
            self.current_daa = Some(newest_daa);
        }
        self.last_batch_at = Some(now);
    }
}

pub struct AnalyticsState {
    pub engine: Option<Arc<tokio::sync::RwLock<AnalyticsEngine>>>,
    /// Each panel's time window, indexed by [`AnalyticsPanel`].
    pub windows: [TimeWindow; 6],
    pub status: AnalyticsStatus,
    pub reorg_notification: Option<String>,
    /// One view per window, indexed by [`TimeWindow::index`].
    pub cached_views: Option<[AggregatedView; 3]>,
    /// Transactions per 10 minutes over the last 24h, updated with the views.
    pub tx_histogram: Option<TxHistogram>,
}

impl Default for AnalyticsState {
    fn default() -> Self {
        use TimeWindow::*;
        Self {
            engine: None,
            // Same windows as the Kaspalytics home page
            windows: [
                TwentyFourHour,
                TwentyFourHour,
                OneHour,
                OneHour,
                OneHour,
                OneHour,
            ],
            status: AnalyticsStatus::default(),
            reorg_notification: None,
            cached_views: None,
            tx_histogram: None,
        }
    }
}

impl AnalyticsState {
    pub fn window(&self, panel: AnalyticsPanel) -> TimeWindow {
        self.windows[panel as usize]
    }

    pub fn window_mut(&mut self, panel: AnalyticsPanel) -> &mut TimeWindow {
        &mut self.windows[panel as usize]
    }

    pub fn view(&self, window: TimeWindow) -> Option<&AggregatedView> {
        self.cached_views.as_ref().map(|v| &v[window.index()])
    }
}

/// What the address index writer is doing.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum IndexPhase {
    /// No writer running (not connected, or connected through the resolver).
    #[default]
    Idle,
    Opening,
    /// Applying batches from the chain stream (whose phase says whether that's a
    /// catch-up or live polling).
    Indexing,
    /// The last batch failed; the writer goes on with the next.
    Error(String),
}

/// Progress of the address index, shown in the status bar.
#[derive(Debug, Clone, Default)]
pub struct IndexStatus {
    pub phase: IndexPhase,
    pub txs_indexed: u64,
    pub addresses: u64,
    /// The last indexed chain block.
    pub position: Option<crate::index::Position>,
    pub disk_bytes: u64,
    pub slabs: usize,
    /// The time span the index covers, `(from_ms, to_ms)`.
    pub coverage: Option<(u64, u64)>,
    /// Smoothed write speed, in transactions per second of writer time.
    pub tx_per_sec: Option<f64>,
    /// Batches waiting for the writer.
    pub backlog: usize,
    /// Removed chain blocks the index didn't have (reorgs past its coverage).
    pub unresolved_reorgs: u64,
    /// Owner merges the cluster size cap refused this session: a heuristic failure
    /// (a bridge or service whose spends pool strangers), never a discovery.
    pub cluster_cap_hits: u64,
    pub last_batch_at: Option<Instant>,
}

impl IndexStatus {
    /// Record a written batch of `txs` transactions that took `elapsed`.
    pub fn record_batch(&mut self, txs: usize, elapsed: Duration) {
        let secs = elapsed.as_secs_f64();
        if txs > 0 && secs > 0.0 {
            let rate = txs as f64 / secs;
            self.tx_per_sec = Some(match self.tx_per_sec {
                Some(r) => r * 0.8 + rate * 0.2,
                None => rate,
            });
        }
        self.last_batch_at = Some(Instant::now());
    }
}

#[derive(Default)]
pub struct IndexState {
    pub status: IndexStatus,
}

/// What the watchlist task is doing.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum WatchPhase {
    /// Nothing to watch, or not connected.
    #[default]
    Idle,
    /// Waiting for the node to connect and sync.
    Waiting,
    /// The node can't serve address queries (no UTXO index).
    Unavailable(String),
    /// Subscribed for this many addresses.
    Active(usize),
}

#[derive(Debug, Clone, Default)]
pub struct WatchStatus {
    pub phase: WatchPhase,
    pub last_error: Option<String>,
    pub last_event_at: Option<Instant>,
}

/// The watchlist with its live data: balances, pending activity, events and alerts.
#[derive(Default)]
pub struct WatchState {
    pub list: Watchlist,
    /// Balance in sompi per watched address, from the node.
    pub balances: HashMap<String, u64>,
    /// Unconfirmed `(incoming, outgoing)` sompi per watched address.
    pub pending: HashMap<String, (u64, u64)>,
    /// Newest first.
    pub events: VecDeque<AddressEvent>,
    /// Newest first.
    pub alerts: VecDeque<Alert>,
    /// Alerts raised since the Monitoring tab was last shown.
    pub unread_alerts: usize,
    /// Alerts raised in total, so a frontend can tell which of `alerts` are new to it.
    pub alerts_raised: u64,
    pub status: WatchStatus,
}

impl WatchState {
    pub fn push_event(&mut self, event: AddressEvent) {
        self.events.push_front(event);
        self.events.truncate(crate::watch::MAX_EVENTS);
    }

    pub fn push_alert(&mut self, alert: Alert) {
        self.alerts.push_front(alert);
        self.alerts.truncate(crate::watch::MAX_ALERTS);
        self.unread_alerts += 1;
        self.alerts_raised += 1;
    }

    pub fn entry(&self, address: &str) -> Option<&WatchEntry> {
        self.list.entries.iter().find(|e| e.address == address)
    }

    /// Drop node data (balances, pending, events, alerts) but keep the list.
    pub fn clear_node_data(&mut self) {
        self.balances.clear();
        self.pending.clear();
        self.events.clear();
        self.alerts.clear();
        self.unread_alerts = 0;
        self.status = WatchStatus::default();
    }
}

/// Everything the Address Info window shows for one address.
#[derive(Debug, Clone, PartialEq)]
pub struct AddressView {
    pub profile: AddressProfile,
    /// Balance in sompi from the node (needs its UTXO index).
    pub balance: Option<u64>,
    pub page: Page<TxRow>,
    pub peers: Vec<Peer>,
    /// Balance over the indexed window, `(time_ms, balance)` oldest first (see
    /// `query::balance_curve`); relative to 0 when the node balance is unknown.
    pub curve: Vec<(u64, i64)>,
    /// The likely-owner cluster, with a sample of members; `None` when unindexed.
    pub cluster: Option<ClusterInfo>,
}

/// The flow graph window: money followed hop by hop from one or more addresses.
#[derive(Debug, Clone, PartialEq)]
pub struct FlowState {
    pub open: bool,
    /// The addresses the graph was started from.
    pub roots: Vec<String>,
    /// Everything fetched so far.
    pub graph: FlowGraph,
    /// `graph` with pass-through chains folded (`FlowGraph::collapse_chains`), kept in
    /// step with it.
    pub collapsed: FlowGraph,
    /// Show `collapsed` rather than `graph`.
    pub collapse: bool,
    pub loading: bool,
    pub error: Option<String>,
}

impl Default for FlowState {
    fn default() -> Self {
        Self {
            open: false,
            roots: Vec::new(),
            graph: FlowGraph::default(),
            collapsed: FlowGraph::default(),
            collapse: true,
            loading: false,
            error: None,
        }
    }
}

impl FlowState {
    /// Start a new graph from `address`; the controller fills it in.
    pub fn start(&mut self, address: String) {
        self.open = true;
        self.roots = vec![address];
        self.graph = FlowGraph::default();
        self.collapsed = FlowGraph::default();
        self.loading = true;
        self.error = None;
    }

    pub fn set_result(&mut self, result: Result<FlowGraph, String>) {
        self.loading = false;
        match result {
            Ok(graph) => {
                self.graph.merge(graph);
                self.collapsed = self.graph.collapse_chains();
            }
            Err(e) => self.error = Some(e),
        }
    }

    /// The graph to draw or export, per `collapse`.
    pub fn shown(&self) -> &FlowGraph {
        if self.collapse {
            &self.collapsed
        } else {
            &self.graph
        }
    }

    pub fn close(&mut self) {
        let collapse = self.collapse;
        *self = Self {
            collapse,
            ..Self::default()
        };
    }
}

/// The last export (`UiCommand::Export`) from the Address Info or flow graph window.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExportStatus {
    pub running: bool,
    /// The file written, or why not.
    pub last: Option<Result<PathBuf, String>>,
}

impl ExportStatus {
    pub fn start(&mut self) {
        self.running = true;
        self.last = None;
    }

    pub fn finish(&mut self, result: Result<PathBuf, String>) {
        self.running = false;
        self.last = Some(result);
    }
}

/// The Address Info window, shown on any tab while `Some`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AddressWindow {
    pub address: String,
    pub view: Option<AddressView>,
    pub loading: bool,
    pub loading_more: bool,
    pub error: Option<String>,
    /// What the online sources said, once asked.
    pub online_result: Option<Vec<crate::labels::OnlineEntry>>,
}

#[derive(Default)]
pub struct AddressState {
    pub open: Option<AddressWindow>,
    pub flows: FlowState,
    pub export: ExportStatus,
}

impl AddressState {
    /// Start loading `address` (the controller then fills in the view).
    pub fn request(&mut self, address: String) {
        self.open = Some(AddressWindow {
            address,
            loading: true,
            ..Default::default()
        });
    }

    pub fn set_view(&mut self, address: &str, result: Result<AddressView, String>) {
        let Some(window) = self.open.as_mut().filter(|w| w.address == address) else {
            return;
        };
        window.loading = false;
        match result {
            Ok(view) => window.view = Some(view),
            Err(e) => window.error = Some(e),
        }
    }

    /// Append an older page of transactions.
    pub fn append_page(&mut self, address: &str, result: Result<Page<TxRow>, String>) {
        let Some(window) = self.open.as_mut().filter(|w| w.address == address) else {
            return;
        };
        window.loading_more = false;
        match (result, window.view.as_mut()) {
            (Ok(page), Some(view)) => {
                view.page.items.extend(page.items);
                view.page.next = page.next;
            }
            (Err(e), _) => window.error = Some(e),
            _ => {}
        }
    }

    pub fn close(&mut self) {
        self.open = None;
    }
}

#[derive(Default)]
pub struct DagSelection {
    /// The block shown (or loading) in the Block Info window.
    pub block_hash: Option<String>,
    /// `get_block` JSON, set through `set_detail` so `hash_links` stays in sync.
    pub block_detail: Option<String>,
    /// Block hashes in `block_detail`, linked to their own Block Info.
    pub hash_links: Vec<HashLink>,
    pub block_loading: bool,
}

impl DagSelection {
    /// Start loading `hash` (the controller then fills in the detail).
    pub fn request(&mut self, hash: String) {
        self.block_hash = Some(hash);
        self.set_detail(None);
        self.block_loading = true;
    }

    pub fn set_detail(&mut self, detail: Option<String>) {
        self.hash_links = detail.as_deref().map(block_hash_links).unwrap_or_default();
        self.block_detail = detail;
    }

    pub fn close(&mut self) {
        self.block_hash = None;
        self.set_detail(None);
        self.block_loading = false;
    }
}

pub type RepaintFn = Arc<dyn Fn() + Send + Sync>;

#[derive(Default)]
pub struct App {
    pub active_tab: Tab,

    pub node: NodeState,
    pub analytics: AnalyticsState,
    pub index: IndexState,
    pub watch: WatchState,
    pub address: AddressState,
    /// Known address labels; replaced as a whole when a source changes.
    pub labels: Arc<LabelBook>,
    /// Opt-in online label sources.
    pub label_settings: LabelSettings,
    /// The public list fetch (on launch and periodically, `labels::start_label_refresh`).
    pub label_refresh: LabelRefresh,
    pub dag_selection: DagSelection,
    pub market_data: Option<MarketData>,

    pub rpc_explorer: RpcExplorerState,

    /// Mempool entry shown in the Transaction Detail window, highlighted in the table.
    /// A copy, so the window stays open once the transaction leaves the mempool.
    pub mempool_open: Option<MempoolEntryInfo>,

    pub paused: bool,
    /// Called by `mark_dirty()` so a frontend can wake up and redraw.
    pub repaint: Option<RepaintFn>,
    pub connection: ActiveConnection,
}

/// The state of the public label list's fetch.
#[derive(Debug, Clone, Default)]
pub struct LabelRefresh {
    pub fetching: bool,
    /// Why the last fetch failed; cleared by the next success.
    pub last_error: Option<String>,
}

impl App {
    /// Ask the frontend to redraw after a state change.
    pub fn mark_dirty(&mut self) {
        if let Some(ref repaint) = self.repaint {
            repaint();
        }
    }

    /// How far the app's view lags the DAG tip: `now_ms` minus the sink's timestamp.
    /// Clamped at zero, since block timestamps can run slightly ahead of the local clock.
    pub fn seconds_behind_tip(&self, now_ms: u64) -> Option<f64> {
        self.node
            .sink_timestamp_ms
            .map(|ts| now_ms.saturating_sub(ts) as f64 / 1000.0)
    }

    /// Drop all data fetched from the current node, e.g. before switching nodes.
    pub fn clear_node_data(&mut self) {
        self.node = NodeState::default();
        self.analytics.engine = None;
        self.analytics.status = AnalyticsStatus::default();
        self.analytics.cached_views = None;
        self.analytics.tx_histogram = None;
        self.analytics.reorg_notification = None;
        self.index.status = IndexStatus::default();
        self.watch.clear_node_data();
        self.address.close();
        self.address.flows.close();
        self.address.export = ExportStatus::default();
        self.mempool_open = None;
        self.dag_selection.set_detail(None);
        self.dag_selection.block_loading = false;
        self.rpc_explorer.set_response(None);
        self.rpc_explorer.is_loading = false;
    }

    /// Open the detail window for the mempool entry at `index`, if it exists.
    pub fn open_mempool_entry(&mut self, index: usize) {
        if let Some(entry) = self
            .node
            .mempool_state
            .as_ref()
            .and_then(|m| m.entries.get(index))
        {
            self.mempool_open = Some(entry.clone());
        }
    }

    pub fn tab_index(&self) -> usize {
        Tab::all()
            .iter()
            .position(|t| *t == self.active_tab)
            .unwrap_or(0)
    }

    pub fn next_tab(&mut self) {
        let idx = (self.tab_index() + 1) % Tab::all().len();
        self.active_tab = Tab::all()[idx];
    }

    pub fn prev_tab(&mut self) {
        let idx = if self.tab_index() == 0 {
            Tab::all().len() - 1
        } else {
            self.tab_index() - 1
        };
        self.active_tab = Tab::all()[idx];
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    // --- Connection ---

    #[test]
    fn seconds_behind_tip_from_sink_timestamp() {
        let mut app = App::default();
        assert_eq!(app.seconds_behind_tip(10_000), None);
        app.node.sink_timestamp_ms = Some(7_500);
        assert_eq!(app.seconds_behind_tip(10_000), Some(2.5));
        // A sink timestamp ahead of the local clock counts as caught up.
        assert_eq!(app.seconds_behind_tip(5_000), Some(0.0));
    }

    #[test]
    fn clear_node_data_resets_fetched_state() {
        let mut app = App::default();
        app.node.node_url = Some("ws://node:17110".to_string());
        app.node.last_error = Some("boom".to_string());
        app.node.connection_status = ConnectionStatus::Connected;
        app.mempool_open = Some(MempoolEntryInfo {
            transaction_id: "tx".to_string(),
            fee: 1,
            is_orphan: false,
        });
        app.dag_selection.block_loading = true;
        app.rpc_explorer.last_response = Some("resp".to_string());
        app.paused = true;

        app.clear_node_data();

        assert_eq!(app.node.node_url, None);
        assert_eq!(app.node.last_error, None);
        assert!(matches!(
            app.node.connection_status,
            ConnectionStatus::Disconnected
        ));
        assert!(app.mempool_open.is_none());
        assert!(!app.dag_selection.block_loading);
        assert_eq!(app.rpc_explorer.last_response, None);
        assert!(app.paused, "user settings survive a reconnect");
    }

    #[test]
    fn clear_node_data_resets_analytics_status() {
        let mut app = App::default();
        app.analytics.status.phase = AnalyticsPhase::Live;
        app.analytics.status.blocks_processed = 10;
        app.clear_node_data();
        assert_eq!(app.analytics.status.phase, AnalyticsPhase::Idle);
        assert_eq!(app.analytics.status.blocks_processed, 0);
    }

    // --- Analytics status ---

    #[test]
    fn analytics_status_fraction_is_relative_to_start() {
        let mut s = AnalyticsStatus::default();
        assert_eq!(s.fraction(2_000), None);
        let t0 = Instant::now();
        s.record_batch(5, 1_000, t0);
        assert_eq!(s.fraction(2_000), Some(0.0));
        s.record_batch(5, 1_500, t0 + Duration::from_secs(1));
        assert_eq!(s.fraction(2_000), Some(0.5));
        // The tip can't be behind the processed blocks.
        assert_eq!(s.fraction(1_200), Some(1.0));
        assert_eq!(s.fraction(900), Some(1.0));
    }

    #[test]
    fn analytics_status_behind_tip() {
        let mut s = AnalyticsStatus::default();
        assert_eq!(s.behind(2_000), None);
        s.record_batch(1, 1_000, Instant::now());
        assert_eq!(s.behind(1_600), Some((600, Duration::from_secs(60))));
        assert_eq!(s.behind(900), Some((0, Duration::ZERO)));
    }

    #[test]
    fn analytics_status_rate_and_eta() {
        let mut s = AnalyticsStatus::default();
        let t0 = Instant::now();
        s.record_batch(3, 1_000, t0);
        assert_eq!(s.daa_per_sec, None);
        assert_eq!(s.eta(2_000), None);
        s.record_batch(3, 1_100, t0 + Duration::from_secs(1));
        assert_eq!(s.daa_per_sec, Some(100.0));
        assert_eq!(s.eta(2_100), Some(Duration::from_secs(10)));
        // Smoothed: 100 * 0.8 + 300 * 0.2.
        s.record_batch(3, 1_400, t0 + Duration::from_secs(2));
        assert!((s.daa_per_sec.unwrap() - 140.0).abs() < 1e-9);
        assert_eq!(s.blocks_processed, 9);
    }

    #[test]
    fn analytics_status_empty_batch_keeps_progress() {
        let mut s = AnalyticsStatus::default();
        let t0 = Instant::now();
        s.record_batch(2, 1_000, t0);
        s.record_batch(0, 0, t0 + Duration::from_secs(1));
        assert_eq!(s.current_daa, Some(1_000));
        assert_eq!(s.blocks_processed, 2);
        assert_eq!(s.last_batch_at, Some(t0 + Duration::from_secs(1)));
    }

    #[test]
    fn active_connection_labels() {
        assert_eq!(ActiveConnection::None.label(), "Not connected");
        assert_eq!(ActiveConnection::Url("ws://x:1".into()).label(), "ws://x:1");
        assert_eq!(ActiveConnection::Resolver.label(), "Public resolver");
    }

    #[test]
    fn only_url_connections_are_direct() {
        assert!(ActiveConnection::Url("ws://x:1".into()).is_direct());
        assert!(!ActiveConnection::Resolver.is_direct());
        assert!(!ActiveConnection::None.is_direct());
    }

    // --- Tab ---

    #[test]
    fn tab_labels() {
        let labels: Vec<_> = Tab::all().iter().map(Tab::label).collect();
        assert_eq!(labels, ["Dashboard", "Monitoring", "Mempool", "RPC Cmds"]);
    }

    #[test]
    fn tab_index_matches_all_order() {
        let mut app = App::default();
        for (i, tab) in Tab::all().iter().enumerate() {
            app.active_tab = *tab;
            assert_eq!(app.tab_index(), i);
        }
    }

    #[test]
    fn next_tab_cycles_forward() {
        let mut app = App::default();
        assert_eq!(app.active_tab, Tab::Dashboard);
        app.next_tab();
        assert_eq!(app.active_tab, Tab::Monitoring);
        app.next_tab();
        assert_eq!(app.active_tab, Tab::Mempool);
        app.next_tab();
        assert_eq!(app.active_tab, Tab::RpcExplorer);
        app.next_tab();
        assert_eq!(app.active_tab, Tab::Dashboard); // wraps
    }

    #[test]
    fn prev_tab_cycles_backward() {
        let mut app = App::default();
        app.prev_tab();
        assert_eq!(app.active_tab, Tab::RpcExplorer); // wraps from 0
        app.prev_tab();
        assert_eq!(app.active_tab, Tab::Mempool);
    }

    // --- RpcExplorerState ---

    #[test]
    fn rpc_explorer_default_has_all_methods() {
        let state = RpcExplorerState::default();
        assert_eq!(state.available_methods.len(), RPC_METHODS.len());
    }

    // --- DagVisualizer ---

    fn dag_block(hash: &str, daa_score: u64, parents: &[&str]) -> DagBlock {
        DagBlock {
            hash: hash.to_string(),
            daa_score,
            parents: parents.iter().map(|p| p.to_string()).collect(),
        }
    }

    #[test]
    fn dag_visualizer_starts_empty() {
        let vis = DagVisualizer::default();
        assert!(vis.is_empty());
        assert!(vis.tips().is_empty());
    }

    #[test]
    fn dag_visualizer_groups_by_daa_score() {
        let mut vis = DagVisualizer::default();
        assert!(vis.add(dag_block("a", 10, &[])));
        assert!(vis.add(dag_block("b", 11, &["a"])));
        assert!(vis.add(dag_block("c", 11, &["a"])));
        assert_eq!(vis.blocks().count(), 3);
        assert_eq!(vis.columns.len(), 2);
        assert_eq!(vis.columns[&11].len(), 2);
    }

    #[test]
    fn dag_visualizer_skips_duplicate() {
        let mut vis = DagVisualizer::default();
        assert!(vis.add(dag_block("a", 10, &[])));
        assert!(!vis.add(dag_block("a", 10, &[])));
        assert_eq!(vis.blocks().count(), 1);
    }

    #[test]
    fn dag_visualizer_keeps_newest_daa_scores() {
        let mut vis = DagVisualizer::default();
        for i in 0..(DAG_MAX_DAA_SCORES as u64 + 5) {
            vis.add(dag_block(&format!("h{i}"), i, &[]));
        }
        assert_eq!(vis.columns.len(), DAG_MAX_DAA_SCORES);
        assert_eq!(vis.blocks().count(), DAG_MAX_DAA_SCORES);
        assert_eq!(*vis.columns.first_key_value().unwrap().0, 5);
        // A late block older than the window is dropped, even one evicted earlier.
        assert!(!vis.add(dag_block("late", 2, &[])));
        assert!(!vis.add(dag_block("h0", 0, &[])));
    }

    #[test]
    fn dag_visualizer_tips_are_unreferenced_blocks() {
        let mut vis = DagVisualizer::default();
        vis.add(dag_block("a", 10, &[]));
        vis.add(dag_block("b", 11, &["a"]));
        vis.add(dag_block("c", 11, &["a"]));
        vis.add(dag_block("d", 12, &["b"]));
        assert_eq!(vis.tips(), HashSet::from(["c", "d"]));
    }

    // --- DagStats ---

    fn make_dag_info(tips: usize, parents: usize, blocks: u64, headers: u64) -> DagInfo {
        DagInfo {
            network: "mainnet".to_string(),
            block_count: blocks,
            header_count: headers,
            tip_hashes: (0..tips).map(|i| format!("tip{}", i)).collect(),
            difficulty: 1.0,
            past_median_time: 0,
            virtual_parent_hashes: (0..parents).map(|i| format!("tip{}", i)).collect(),
            pruning_point_hash: "pruning".to_string(),
            virtual_daa_score: 1000,
            sink: "sink".to_string(),
        }
    }

    #[test]
    fn dag_stats_default_empty() {
        let stats = DagStats::default();
        assert!(stats.samples.is_empty());
        assert!(stats.blue_block_rate().is_none());
        assert!(stats.avg_dag_width().is_none());
        assert!(stats.block_interval_ms().is_none());
    }

    #[test]
    fn dag_stats_update_adds_sample() {
        let mut stats = DagStats::default();
        let dag = make_dag_info(4, 3, 1000, 1010);
        stats.update(&dag, Some(500));
        assert_eq!(stats.samples.len(), 1);
        assert_eq!(stats.samples[0].blue_score, 500);
    }

    #[test]
    fn dag_stats_caps_at_120() {
        let mut stats = DagStats::default();
        let dag = make_dag_info(4, 3, 1000, 1010);
        for _ in 0..130 {
            stats.update(&dag, Some(500));
        }
        assert_eq!(stats.samples.len(), 120);
    }

    #[test]
    fn dag_stats_avg_dag_width() {
        let mut stats = DagStats::default();
        // 3 samples with tip counts 2, 4, 6 → avg 4.0
        for tips in [2, 4, 6] {
            let dag = make_dag_info(tips, 1, 1000, 1010);
            stats.update(&dag, Some(100));
        }
        let avg = stats.avg_dag_width().unwrap();
        assert!((avg - 4.0).abs() < f64::EPSILON);
    }

    #[test]
    fn dag_stats_avg_dag_width_last_minute_only() {
        let mut stats = DagStats::default();
        let dag = make_dag_info(4, 1, 1000, 1010);
        stats.update(&dag, Some(100));
        stats.update(&dag, Some(101));
        // The first sample is older than the window, so only the other two count.
        let now = stats.samples[1].timestamp;
        stats.samples[0].timestamp = now - DAG_WIDTH_WINDOW - Duration::from_secs(1);
        stats.samples[0].tip_count = 100;
        stats.update(&make_dag_info(6, 1, 1000, 1010), Some(102));
        let avg = stats.avg_dag_width().unwrap();
        assert!((avg - 5.0).abs() < f64::EPSILON);
    }

    #[test]
    fn dag_stats_blue_block_rate_needs_two_samples() {
        let mut stats = DagStats::default();
        let dag = make_dag_info(4, 3, 1000, 1010);
        stats.update(&dag, Some(500));
        assert!(stats.blue_block_rate().is_none());
    }

    #[test]
    fn rpc_explorer_loop_wait() {
        let mut state = RpcExplorerState::default();
        let now = Instant::now();
        assert_eq!(state.loop_wait(now), None);

        state.loop_enabled = true;
        assert_eq!(state.loop_wait(now), Some(Duration::ZERO));

        state.last_run = Some(now);
        state.loop_interval_secs = 2.0;
        assert_eq!(
            state.loop_wait(now + Duration::from_millis(500)),
            Some(Duration::from_millis(1500))
        );
        assert_eq!(
            state.loop_wait(now + Duration::from_secs(3)),
            Some(Duration::ZERO)
        );

        state.is_loading = true;
        assert_eq!(state.loop_wait(now), None);

        state.is_loading = false;
        state.select(1);
        assert!(!state.loop_enabled);
    }

    #[test]
    fn rpc_explorer_open_block_prefills_get_block() {
        let mut state = RpcExplorerState::default();
        let hash = "ab".repeat(32);
        state.set_response(Some(format!("{{\n  \"sink\": \"{hash}\"\n}}")));
        assert_eq!(state.hash_links.len(), 1);

        state.open_block(&hash);
        assert_eq!(state.method().unwrap().name, "get_block");
        assert_eq!(state.args[0], hash);
        assert_eq!(state.args[1], "true");
        assert_eq!(state.last_response, None);
        assert!(state.hash_links.is_empty());
    }

    #[test]
    fn rpc_explorer_methods_are_sorted() {
        let state = RpcExplorerState::default();
        assert!(state.available_methods.is_sorted_by_key(|m| m.name));
    }

    #[test]
    fn rpc_explorer_select_resets_args_to_defaults() {
        let mut state = RpcExplorerState::default();
        let i = state
            .available_methods
            .iter()
            .position(|m| m.name == "get_block")
            .unwrap();
        state.select(i);
        assert_eq!(state.args, vec![String::new(), "true".to_string()]);
        state.args[0] = "abc".into();
        let ping = state
            .available_methods
            .iter()
            .position(|m| m.name == "ping")
            .unwrap();
        state.select(ping);
        assert!(state.args.is_empty());
        state.select(i);
        assert_eq!(state.args[0], "");
    }

    #[test]
    fn rpc_explorer_select_clears_response() {
        let mut state = RpcExplorerState::default();
        state.set_response(Some("resp".into()));
        state.select(0);
        assert_eq!(state.last_response, None);
    }

    #[test]
    fn open_mempool_entry_copies_entry_and_ignores_out_of_range() {
        let mut app = App::default();
        app.node.mempool_state = Some(MempoolState {
            entries: vec![MempoolEntryInfo {
                transaction_id: "abc123".to_string(),
                fee: 150_000_000,
                is_orphan: true,
            }],
            total_fees: 150_000_000,
        });

        app.open_mempool_entry(5);
        assert!(app.mempool_open.is_none());

        app.open_mempool_entry(0);
        assert_eq!(
            app.mempool_open.as_ref().map(|e| e.transaction_id.as_str()),
            Some("abc123")
        );
    }

    #[test]
    fn dag_selection_tracks_hash_links() {
        let mut sel = DagSelection::default();
        let hash = "cd".repeat(32);
        sel.request(hash.clone());
        assert!(sel.block_loading);
        assert_eq!(sel.block_hash.as_deref(), Some(hash.as_str()));
        sel.set_detail(Some(format!(
            "{{\n  \"selectedParentHash\": \"{hash}\"\n}}"
        )));
        assert_eq!(sel.hash_links.len(), 1);
        sel.close();
        assert!(sel.block_hash.is_none() && sel.hash_links.is_empty());
    }

    #[test]
    fn time_window_index_matches_all() {
        for (i, w) in TimeWindow::ALL.iter().enumerate() {
            assert_eq!(w.index(), i);
        }
    }

    #[test]
    fn analytics_panels_default_to_kaspalytics_windows() {
        let state = AnalyticsState::default();
        assert_eq!(
            state.window(AnalyticsPanel::TxSummary),
            TimeWindow::TwentyFourHour
        );
        assert_eq!(state.window(AnalyticsPanel::Miners), TimeWindow::OneHour);
        assert!(state.view(TimeWindow::OneMin).is_none());
    }
}
