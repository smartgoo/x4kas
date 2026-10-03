use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::analytics::{AggregatedView, AnalyticsEngine};
use crate::rpc::hash_links::{HashLink, block_hash_links};
use crate::rpc::types::*;

#[derive(Debug, Clone)]
pub struct DagVisualizerBlock {
    pub hash_full: String,
    pub is_selected_parent: bool,
}

#[derive(Debug, Clone)]
pub struct DagVisualizerColumn {
    pub blocks: Vec<DagVisualizerBlock>,
}

#[derive(Debug, Clone, Default)]
pub struct DagVisualizer {
    pub columns: VecDeque<DagVisualizerColumn>,
}

impl DagVisualizer {
    pub fn update(&mut self, tip_hashes: &[String], virtual_parents: &[String]) {
        let parent_set: HashSet<&str> = virtual_parents.iter().map(|s| s.as_str()).collect();
        let blocks: Vec<DagVisualizerBlock> = tip_hashes
            .iter()
            .map(|h| DagVisualizerBlock {
                hash_full: h.clone(),
                is_selected_parent: parent_set.contains(h.as_str()),
            })
            .collect();

        if !blocks.is_empty() {
            // Only add if tips changed from last column (compare full hashes)
            let should_add = self.columns.back().is_none_or(|last| {
                let last_hashes: Vec<&str> =
                    last.blocks.iter().map(|b| b.hash_full.as_str()).collect();
                let new_hashes: Vec<&str> = blocks.iter().map(|b| b.hash_full.as_str()).collect();
                last_hashes != new_hashes
            });

            if should_add {
                self.columns.push_back(DagVisualizerColumn { blocks });
                // Keep last 30 columns
                if self.columns.len() > 30 {
                    self.columns.pop_front();
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct DagSample {
    pub timestamp: Instant,
    pub blue_score: u64,
    pub block_count: u64,
    pub header_count: u64,
    pub tip_count: usize,
    pub virtual_parent_count: usize,
}

#[derive(Debug, Clone, Default)]
pub struct DagStats {
    pub samples: VecDeque<DagSample>,
    pub sink_blue_score: Option<u64>,
}

impl DagStats {
    pub fn update(&mut self, dag_info: &DagInfo, blue_score: Option<u64>) {
        self.sink_blue_score = blue_score;
        self.samples.push_back(DagSample {
            timestamp: Instant::now(),
            blue_score: blue_score.unwrap_or(0),
            block_count: dag_info.block_count,
            header_count: dag_info.header_count,
            tip_count: dag_info.tip_hashes.len(),
            virtual_parent_count: dag_info.virtual_parent_hashes.len(),
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

    pub fn avg_dag_width(&self) -> Option<f64> {
        if self.samples.is_empty() {
            return None;
        }
        let sum: usize = self.samples.iter().map(|s| s.tip_count).sum();
        Some(sum as f64 / self.samples.len() as f64)
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

    pub fn blue_red_ratio(&self) -> Option<(usize, usize)> {
        let last = self.samples.back()?;
        let red = last.tip_count.saturating_sub(last.virtual_parent_count);
        Some((last.virtual_parent_count, red))
    }

    pub fn headers_blocks_delta(&self) -> Option<u64> {
        let last = self.samples.back()?;
        Some(last.header_count.saturating_sub(last.block_count))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DagFocus {
    #[default]
    Tips,
    Parents,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Dashboard,
    Mempool,
    BlockDag,
    Analytics,
    RpcExplorer,
}

impl Tab {
    pub fn all() -> &'static [Tab] {
        &[
            Tab::Dashboard,
            Tab::Mempool,
            Tab::BlockDag,
            Tab::Analytics,
            Tab::RpcExplorer,
        ]
    }

    pub fn title(&self) -> &'static str {
        match self {
            Tab::Dashboard => "1:Dashboard",
            Tab::Mempool => "2:Mempool",
            Tab::BlockDag => "3:BlockDAG",
            Tab::Analytics => "4:Analytics",
            Tab::RpcExplorer => "5:RPC Cmds",
        }
    }

    /// Title without the numeric shortcut prefix.
    pub fn label(&self) -> &'static str {
        self.title()
            .split_once(':')
            .map_or(self.title(), |(_, name)| name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ViewMode {
    #[default]
    Table,
    Chart,
}

impl ViewMode {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Table => "Table",
            Self::Chart => "Chart",
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

    /// Width of one bar in the transaction chart.
    pub fn series_bin_ms(&self) -> u64 {
        match self {
            Self::OneMin => 5_000,
            Self::OneHour => 60_000,
            Self::TwentyFourHour => 3_600_000,
        }
    }
}

#[derive(Debug, Clone)]
pub enum ConnectionStatus {
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
}

pub struct RpcExplorerState {
    pub selected_method: usize,
    pub available_methods: Vec<&'static str>,
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
        let mut available_methods: Vec<_> = crate::rpc::methods::RPC_METHODS
            .iter()
            .map(|m| m.name)
            .collect();
        available_methods.sort_unstable();
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
    pub fn method(&self) -> Option<&'static crate::rpc::methods::RpcMethod> {
        self.available_methods
            .get(self.selected_method)
            .and_then(|name| crate::rpc::methods::find(name))
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
            .position(|m| *m == "get_block")
        {
            self.select(i);
            if let Some(arg) = self.args.first_mut() {
                *arg = hash.to_string();
            }
            self.set_response(None);
        }
    }

    /// Select a method and reset the argument inputs to its defaults. Stops any loop.
    pub fn select(&mut self, index: usize) {
        self.selected_method = index;
        self.loop_enabled = false;
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

/// Command palette state. Results are pushed here by the controller.
#[derive(Default)]
pub struct CommandLine {
    pub active: bool,
    pub input: String,
    pub output: VecDeque<CommandOutput>,
    pub history: VecDeque<String>,
    pub history_index: Option<usize>,
}

pub struct CommandOutput {
    pub command: String,
    pub result: String,
    pub is_error: bool,
}

impl CommandLine {
    pub fn open(&mut self) {
        self.active = true;
        self.input.clear();
        self.history_index = None;
    }

    pub fn close(&mut self) {
        self.active = false;
        self.input.clear();
        self.history_index = None;
    }

    pub fn history_up(&mut self) {
        if self.history.is_empty() {
            return;
        }
        match self.history_index {
            None => {
                self.history_index = Some(self.history.len() - 1);
            }
            Some(i) if i > 0 => {
                self.history_index = Some(i - 1);
            }
            _ => return,
        }
        if let Some(i) = self.history_index {
            self.input = self.history[i].clone();
        }
    }

    pub fn history_down(&mut self) {
        match self.history_index {
            Some(i) if i < self.history.len() - 1 => {
                self.history_index = Some(i + 1);
                self.input = self.history[i + 1].clone();
            }
            Some(_) => {
                self.history_index = None;
                self.input.clear();
            }
            None => {}
        }
    }

    pub fn submit(&mut self) -> Option<String> {
        let cmd = self.input.trim().to_string();
        if cmd.is_empty() {
            return None;
        }
        self.history.push_back(cmd.clone());
        if self.history.len() > 100 {
            self.history.pop_front();
        }
        self.history_index = None;
        self.input.clear();
        Some(cmd)
    }

    pub fn push_output(&mut self, command: String, result: String, is_error: bool) {
        self.output.push_back(CommandOutput {
            command,
            result,
            is_error,
        });
        if self.output.len() > 50 {
            self.output.pop_front();
        }
    }

    /// Commands whose name starts with the first word of `input` (all commands if empty).
    pub fn suggestions(&self) -> Vec<(&'static str, &'static str)> {
        let prefix = self.input.trim().split(' ').next().unwrap_or_default();
        Self::available_commands()
            .into_iter()
            .filter(|(name, _)| name.starts_with(prefix))
            .collect()
    }

    pub fn available_commands() -> Vec<(&'static str, &'static str)> {
        let mut cmds = vec![
            ("help", "Show this help message"),
            ("clear", "Clear command output"),
        ];
        cmds.extend(
            crate::rpc::methods::RPC_METHODS
                .iter()
                .map(|m| (m.name, m.description)),
        );
        cmds
    }
}

pub struct NodeState {
    pub server_info: Option<ServerInfo>,
    pub dag_info: Option<DagInfo>,
    pub mempool_state: Option<MempoolState>,
    pub coin_supply: Option<CoinSupplyInfo>,
    pub fee_estimate: Option<FeeEstimateInfo>,
    pub mining_info: Option<MiningInfo>,
    pub dag_visualizer: DagVisualizer,
    pub dag_stats: DagStats,
    pub sink_blue_score: Option<u64>,
    /// Header timestamp (unix ms) of the sink, the node's newest selected tip.
    pub sink_timestamp_ms: Option<u64>,
    pub node_url: Option<String>,
    pub node_uid: Option<String>,
    pub connection_status: ConnectionStatus,
    pub last_refresh: Option<Instant>,
    pub last_poll_duration_ms: Option<f64>,
    pub last_error: Option<String>,
}

impl Default for NodeState {
    fn default() -> Self {
        Self {
            server_info: None,
            dag_info: None,
            mempool_state: None,
            coin_supply: None,
            fee_estimate: None,
            mining_info: None,
            dag_visualizer: DagVisualizer::default(),
            dag_stats: DagStats::default(),
            sink_blue_score: None,
            sink_timestamp_ms: None,
            node_url: None,
            node_uid: None,
            connection_status: ConnectionStatus::Disconnected,
            last_refresh: None,
            last_poll_duration_ms: None,
            last_error: None,
        }
    }
}

/// Analytics cards with their own time window and table/chart toggle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnalyticsPanel {
    TxSummary,
    Inspection,
    NodeVersions,
    TopSenders,
    TopReceivers,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PanelState {
    pub window: TimeWindow,
    pub mode: ViewMode,
}

pub struct AnalyticsState {
    pub engine: Option<Arc<tokio::sync::RwLock<AnalyticsEngine>>>,
    /// Indexed by [`AnalyticsPanel`].
    pub panels: [PanelState; 5],
    pub sync_progress: Option<(u64, u64)>,
    pub reorg_notification: Option<String>,
    /// One view per window, indexed by [`TimeWindow::index`].
    pub cached_views: Option<[AggregatedView; 3]>,
}

impl Default for AnalyticsState {
    fn default() -> Self {
        let panel = |window| PanelState {
            window,
            mode: ViewMode::Table,
        };
        Self {
            engine: None,
            // Same windows as the Kaspalytics home page
            panels: [
                panel(TimeWindow::TwentyFourHour),
                panel(TimeWindow::TwentyFourHour),
                panel(TimeWindow::OneHour),
                panel(TimeWindow::OneHour),
                panel(TimeWindow::OneHour),
            ],
            sync_progress: None,
            reorg_notification: None,
            cached_views: None,
        }
    }
}

impl AnalyticsState {
    pub fn panel(&mut self, panel: AnalyticsPanel) -> &mut PanelState {
        &mut self.panels[panel as usize]
    }

    pub fn view(&self, window: TimeWindow) -> Option<&AggregatedView> {
        self.cached_views.as_ref().map(|v| &v[window.index()])
    }
}

#[derive(Default)]
pub struct DagSelection {
    pub focus: DagFocus,
    pub tip_selected: usize,
    pub parent_selected: usize,
    pub block_detail: Option<String>,
    pub block_loading: bool,
}

pub type RepaintFn = Arc<dyn Fn() + Send + Sync>;

pub struct App {
    pub active_tab: Tab,

    pub node: NodeState,
    pub analytics: AnalyticsState,
    pub dag_selection: DagSelection,
    pub market_data: Option<MarketData>,

    pub rpc_explorer: RpcExplorerState,
    pub command_line: CommandLine,

    pub mempool_selected: usize,
    pub mempool_detail: Option<String>,

    pub paused: bool,
    /// Called by `mark_dirty()` so a frontend can wake up and redraw.
    pub repaint: Option<RepaintFn>,
    pub has_direct_node: bool,
    pub connection: ActiveConnection,
}

impl Default for App {
    fn default() -> Self {
        Self {
            active_tab: Tab::Dashboard,
            node: NodeState::default(),
            analytics: AnalyticsState::default(),
            dag_selection: DagSelection::default(),
            market_data: None,
            rpc_explorer: RpcExplorerState::default(),
            command_line: CommandLine::default(),
            mempool_selected: 0,
            mempool_detail: None,
            paused: false,
            repaint: None,
            has_direct_node: false,
            connection: ActiveConnection::None,
        }
    }
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
        self.analytics.sync_progress = None;
        self.analytics.cached_views = None;
        self.analytics.reorg_notification = None;
        self.mempool_selected = 0;
        self.mempool_detail = None;
        self.dag_selection.block_detail = None;
        self.dag_selection.block_loading = false;
        self.rpc_explorer.set_response(None);
        self.rpc_explorer.is_loading = false;
    }

    /// Open the detail popup for the mempool entry at `index`, if it exists.
    pub fn open_mempool_detail(&mut self, index: usize) {
        let Some(entry) = self
            .node
            .mempool_state
            .as_ref()
            .and_then(|m| m.entries.get(index))
        else {
            return;
        };
        self.mempool_detail = Some(format!(
            "Transaction ID: {}\nFee: {:.8} KAS ({} sompi)\nOrphan: {}",
            entry.transaction_id,
            sompi_to_kas(entry.fee),
            entry.fee,
            if entry.is_orphan { "Yes" } else { "No" },
        ));
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
        app.mempool_selected = 3;
        app.mempool_detail = Some("tx".to_string());
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
        assert_eq!(app.mempool_selected, 0);
        assert_eq!(app.mempool_detail, None);
        assert!(!app.dag_selection.block_loading);
        assert_eq!(app.rpc_explorer.last_response, None);
        assert!(app.paused, "user settings survive a reconnect");
    }

    #[test]
    fn active_connection_labels() {
        assert_eq!(ActiveConnection::None.label(), "Not connected");
        assert_eq!(ActiveConnection::Url("ws://x:1".into()).label(), "ws://x:1");
        assert_eq!(ActiveConnection::Resolver.label(), "Public resolver");
    }

    // --- Tab ---

    #[test]
    fn tab_titles() {
        assert_eq!(Tab::Dashboard.title(), "1:Dashboard");
        assert_eq!(Tab::Mempool.title(), "2:Mempool");
        assert_eq!(Tab::BlockDag.title(), "3:BlockDAG");
        assert_eq!(Tab::Analytics.title(), "4:Analytics");
        assert_eq!(Tab::RpcExplorer.title(), "5:RPC Cmds");
    }

    #[test]
    fn tab_labels_strip_shortcut_prefix() {
        assert_eq!(Tab::Dashboard.label(), "Dashboard");
        assert_eq!(Tab::RpcExplorer.label(), "RPC Cmds");
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
        assert_eq!(app.active_tab, Tab::Mempool);
        app.next_tab();
        assert_eq!(app.active_tab, Tab::BlockDag);
        app.next_tab();
        assert_eq!(app.active_tab, Tab::Analytics);
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
        // Navigate back a couple
        app.active_tab = Tab::Analytics;
        app.prev_tab();
        assert_eq!(app.active_tab, Tab::BlockDag);
    }

    // --- CommandLine: history ---

    #[test]
    fn history_up_empty() {
        let mut cl = CommandLine::default();
        cl.history_up();
        assert_eq!(cl.history_index, None);
        assert_eq!(cl.input, "");
    }

    #[test]
    fn history_up_navigates() {
        let mut cl = CommandLine::default();
        cl.history = VecDeque::from(vec!["first".to_string(), "second".to_string()]);
        cl.history_up();
        assert_eq!(cl.input, "second");
        assert_eq!(cl.history_index, Some(1));
        cl.history_up();
        assert_eq!(cl.input, "first");
        assert_eq!(cl.history_index, Some(0));
        // At top, stays
        cl.history_up();
        assert_eq!(cl.input, "first");
        assert_eq!(cl.history_index, Some(0));
    }

    #[test]
    fn history_down_restores_empty() {
        let mut cl = CommandLine::default();
        cl.history = VecDeque::from(vec!["cmd".to_string()]);
        cl.history_up();
        assert_eq!(cl.input, "cmd");
        cl.history_down();
        assert_eq!(cl.input, "");
        assert_eq!(cl.history_index, None);
    }

    #[test]
    fn history_down_without_history_noop() {
        let mut cl = CommandLine::default();
        cl.input = "typing".to_string();
        cl.history_down();
        assert_eq!(cl.input, "typing");
    }

    // --- CommandLine: submit ---

    #[test]
    fn submit_empty_returns_none() {
        let mut cl = CommandLine::default();
        cl.input = "   ".to_string();
        assert!(cl.submit().is_none());
    }

    #[test]
    fn submit_returns_trimmed_command() {
        let mut cl = CommandLine::default();
        cl.input = "  ping  ".to_string();
        let cmd = cl.submit();
        assert_eq!(cmd, Some("ping".to_string()));
        assert_eq!(cl.input, "");
        assert_eq!(cl.history_index, None);
    }

    #[test]
    fn submit_appends_to_history() {
        let mut cl = CommandLine::default();
        cl.input = "get_server_info".to_string();
        cl.submit();
        assert_eq!(
            cl.history,
            VecDeque::from(vec!["get_server_info".to_string()])
        );
    }

    #[test]
    fn submit_caps_history_at_100() {
        let mut cl = CommandLine::default();
        for i in 0..105 {
            cl.input = format!("cmd{}", i);
            cl.submit();
        }
        assert_eq!(cl.history.len(), 100);
        assert_eq!(cl.history.front().unwrap(), "cmd5");
        assert_eq!(cl.history.back().unwrap(), "cmd104");
    }

    // --- CommandLine: output ---

    #[test]
    fn push_output_caps_at_50() {
        let mut cl = CommandLine::default();
        for i in 0..55 {
            cl.push_output(format!("cmd{}", i), "ok".to_string(), false);
        }
        assert_eq!(cl.output.len(), 50);
        assert_eq!(cl.output.front().unwrap().command, "cmd5");
    }

    #[test]
    fn push_output_tracks_errors() {
        let mut cl = CommandLine::default();
        cl.push_output("bad".to_string(), "fail".to_string(), true);
        assert!(cl.output.front().unwrap().is_error);
    }

    // --- CommandLine: open/close/suggestions ---

    #[test]
    fn open_and_close_clear_state() {
        let mut cl = CommandLine::default();
        cl.input = "leftover".to_string();
        cl.history_index = Some(2);
        cl.open();
        assert!(cl.active);
        assert_eq!(cl.input, "");
        assert_eq!(cl.history_index, None);

        cl.input = "something".to_string();
        cl.close();
        assert!(!cl.active);
        assert_eq!(cl.input, "");
    }

    #[test]
    fn suggestions_filter_by_prefix() {
        let mut cl = CommandLine::default();
        assert_eq!(
            cl.suggestions().len(),
            CommandLine::available_commands().len()
        );
        cl.input = "cl".to_string();
        assert_eq!(cl.suggestions(), vec![("clear", "Clear command output")]);
        cl.input = "get_".to_string();
        assert!(cl.suggestions().iter().all(|(n, _)| n.starts_with("get_")));
        cl.input = "nope".to_string();
        assert!(cl.suggestions().is_empty());
    }

    // --- RpcExplorerState ---

    #[test]
    fn rpc_explorer_default_has_all_methods() {
        let state = RpcExplorerState::default();
        assert!(state.available_methods.len() >= 18);
        assert!(state.available_methods.contains(&"ping"));
        assert!(state.available_methods.contains(&"get_server_info"));
        assert!(state.available_methods.contains(&"get_sink"));
        assert!(state.available_methods.contains(&"get_sink_blue_score"));
        assert!(state.available_methods.contains(&"get_info"));
        assert!(state.available_methods.contains(&"get_peer_addresses"));
        assert!(state.available_methods.contains(&"get_current_network"));
        assert!(
            state
                .available_methods
                .contains(&"get_fee_estimate_experimental")
        );
        assert!(
            state
                .available_methods
                .contains(&"estimate_network_hashes_per_second")
        );
    }

    // --- DagVisualizer ---

    #[test]
    fn dag_visualizer_starts_empty() {
        let vis = DagVisualizer::default();
        assert!(vis.columns.is_empty());
    }

    #[test]
    fn dag_visualizer_adds_column() {
        let mut vis = DagVisualizer::default();
        let tips = vec!["abc123".to_string(), "def456".to_string()];
        let parents = vec!["abc123".to_string()];
        vis.update(&tips, &parents);
        assert_eq!(vis.columns.len(), 1);
        assert_eq!(vis.columns[0].blocks.len(), 2);
        assert!(vis.columns[0].blocks[0].is_selected_parent);
        assert!(!vis.columns[0].blocks[1].is_selected_parent);
    }

    #[test]
    fn dag_visualizer_skips_duplicate() {
        let mut vis = DagVisualizer::default();
        let tips = vec!["abc12345".to_string()];
        let parents = vec![];
        vis.update(&tips, &parents);
        vis.update(&tips, &parents);
        assert_eq!(vis.columns.len(), 1);
    }

    #[test]
    fn dag_visualizer_caps_at_30() {
        let mut vis = DagVisualizer::default();
        for i in 0..35 {
            let tips = vec![format!("hash{:04}", i)];
            vis.update(&tips, &[]);
        }
        assert_eq!(vis.columns.len(), 30);
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
        assert!(stats.sink_blue_score.is_none());
        assert!(stats.blue_block_rate().is_none());
        assert!(stats.avg_dag_width().is_none());
        assert!(stats.block_interval_ms().is_none());
        assert!(stats.blue_red_ratio().is_none());
        assert!(stats.headers_blocks_delta().is_none());
    }

    #[test]
    fn dag_stats_update_adds_sample() {
        let mut stats = DagStats::default();
        let dag = make_dag_info(4, 3, 1000, 1010);
        stats.update(&dag, Some(500));
        assert_eq!(stats.samples.len(), 1);
        assert_eq!(stats.sink_blue_score, Some(500));
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
    fn dag_stats_blue_red_ratio() {
        let mut stats = DagStats::default();
        let dag = make_dag_info(5, 3, 1000, 1010);
        stats.update(&dag, Some(500));
        let (blue, red) = stats.blue_red_ratio().unwrap();
        assert_eq!(blue, 3);
        assert_eq!(red, 2);
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
    fn dag_stats_headers_blocks_delta() {
        let mut stats = DagStats::default();
        let dag = make_dag_info(4, 3, 1000, 1050);
        stats.update(&dag, Some(500));
        assert_eq!(stats.headers_blocks_delta(), Some(50));
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
        assert!(state.available_methods.is_sorted());
    }

    #[test]
    fn rpc_explorer_select_resets_args_to_defaults() {
        let mut state = RpcExplorerState::default();
        let i = state
            .available_methods
            .iter()
            .position(|m| *m == "get_block")
            .unwrap();
        state.select(i);
        assert_eq!(state.args, vec![String::new(), "true".to_string()]);
        state.args[0] = "abc".into();
        let ping = state
            .available_methods
            .iter()
            .position(|m| *m == "ping")
            .unwrap();
        state.select(ping);
        assert!(state.args.is_empty());
        state.select(i);
        assert_eq!(state.args[0], "");
    }

    #[test]
    fn rpc_explorer_methods_match_available_commands() {
        let state = RpcExplorerState::default();
        let commands = CommandLine::available_commands();
        // Every explorer method should have a corresponding command entry
        for method in &state.available_methods {
            assert!(
                commands.iter().any(|(name, _)| name == method),
                "Explorer method '{}' missing from available_commands",
                method
            );
        }
    }

    #[test]
    fn open_mempool_detail_formats_entry_and_ignores_out_of_range() {
        let mut app = App::default();
        app.node.mempool_state = Some(MempoolState {
            entry_count: 1,
            entries: vec![MempoolEntryInfo {
                transaction_id: "abc123".to_string(),
                fee: 150_000_000,
                is_orphan: true,
            }],
            total_fees: 150_000_000,
        });

        app.open_mempool_detail(5);
        assert!(app.mempool_detail.is_none());

        app.open_mempool_detail(0);
        let detail = app.mempool_detail.as_deref().unwrap();
        assert!(detail.contains("Transaction ID: abc123"));
        assert!(detail.contains("1.50000000 KAS (150000000 sompi)"));
        assert!(detail.contains("Orphan: Yes"));
    }

    #[test]
    fn time_window_index_matches_all() {
        for (i, w) in TimeWindow::ALL.iter().enumerate() {
            assert_eq!(w.index(), i);
            assert_eq!(w.duration_ms() % w.series_bin_ms(), 0);
        }
    }

    #[test]
    fn analytics_panels_default_to_kaspalytics_windows() {
        let mut state = AnalyticsState::default();
        assert_eq!(
            state.panel(AnalyticsPanel::TxSummary).window,
            TimeWindow::TwentyFourHour
        );
        assert_eq!(
            state.panel(AnalyticsPanel::NodeVersions).window,
            TimeWindow::OneHour
        );
        assert!(state.view(TimeWindow::OneMin).is_none());
    }
}
