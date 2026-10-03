use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::Instant;

use crate::analytics::{AggregatedView, AnalyticsEngine};
use crate::config::DaemonConfig;
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
#[allow(dead_code)]
pub struct DagSample {
    pub timestamp: Instant,
    pub blue_score: u64,
    pub daa_score: u64,
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
            daa_score: dag_info.virtual_daa_score,
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
    IntegratedNode,
}

impl Tab {
    pub fn all() -> &'static [Tab] {
        &[
            Tab::Dashboard,
            Tab::Mempool,
            Tab::BlockDag,
            Tab::Analytics,
            Tab::RpcExplorer,
            Tab::IntegratedNode,
        ]
    }

    pub fn title(&self) -> &'static str {
        match self {
            Tab::Dashboard => "1:Dashboard",
            Tab::Mempool => "2:Mempool",
            Tab::BlockDag => "3:BlockDAG",
            Tab::Analytics => "4:Analytics",
            Tab::RpcExplorer => "5:RPC Cmds",
            Tab::IntegratedNode => "6:Node",
        }
    }

    /// Title without the numeric shortcut prefix.
    pub fn label(&self) -> &'static str {
        self.title().split_once(':').map_or(self.title(), |(_, name)| name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ViewMode {
    #[default]
    Table,
    Chart,
}

impl ViewMode {
    pub fn toggle(&mut self) {
        *self = match self {
            Self::Table => Self::Chart,
            Self::Chart => Self::Table,
        };
    }

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
    pub fn cycle(&mut self) {
        *self = match self {
            Self::OneMin => Self::OneHour,
            Self::OneHour => Self::TwentyFourHour,
            Self::TwentyFourHour => Self::OneMin,
        };
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::OneMin => "1m",
            Self::OneHour => "1h",
            Self::TwentyFourHour => "24h",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonStatus {
    Stopped,
    Starting,
    Running,
    Stopping,
    Error(String),
}

pub struct IntegratedNodeState {
    pub config: DaemonConfig,
    pub status: DaemonStatus,
    pub selected_field: usize,
    pub editing: bool,
    pub edit_buffer: String,
    pub log_lines: VecDeque<String>,
    pub log_scroll: usize,
    pub log_auto_scroll: bool,
    pub started_at: Option<std::time::Instant>,
    pub status_message: Option<(String, bool)>, // (message, is_error)
}

impl IntegratedNodeState {
    pub fn new(config: DaemonConfig) -> Self {
        Self {
            config,
            status: DaemonStatus::Stopped,
            selected_field: 0,
            editing: false,
            edit_buffer: String::new(),
            log_lines: VecDeque::new(),
            log_scroll: 0,
            log_auto_scroll: true,
            started_at: None,
            status_message: None,
        }
    }

    pub fn is_running(&self) -> bool {
        matches!(self.status, DaemonStatus::Running | DaemonStatus::Starting)
    }
}

#[derive(Debug, Clone)]
pub enum ConnectionStatus {
    Disconnected,
    Connecting,
    Connected,
    Error(#[allow(dead_code)] String),
}

pub struct RpcExplorerState {
    pub selected_method: usize,
    pub available_methods: Vec<&'static str>,
    pub last_response: Option<String>,
    pub is_loading: bool,
    pub scroll_offset: usize,
}

impl Default for RpcExplorerState {
    fn default() -> Self {
        Self {
            selected_method: 0,
            available_methods: crate::rpc::types::RPC_METHODS
                .iter()
                .map(|(name, _)| *name)
                .collect(),
            last_response: None,
            is_loading: false,
            scroll_offset: 0,
        }
    }
}

#[derive(Default)]
pub struct CommandLine {
    pub active: bool,
    pub input: String,
    pub cursor_pos: usize,
    pub output: VecDeque<CommandOutput>,
    pub output_scroll: usize,
    pub history: VecDeque<String>,
    pub history_index: Option<usize>,
    pub show_output: bool,
}

pub struct CommandOutput {
    pub command: String,
    pub result: String,
    pub is_error: bool,
}

impl CommandLine {
    pub fn activate(&mut self) {
        self.active = true;
        self.input.clear();
        self.cursor_pos = 0;
        self.history_index = None;
    }

    pub fn deactivate(&mut self) {
        self.active = false;
        self.input.clear();
        self.cursor_pos = 0;
        self.history_index = None;
    }

    pub fn insert_char(&mut self, c: char) {
        self.input.insert(self.cursor_pos, c);
        self.cursor_pos += c.len_utf8();
    }

    pub fn delete_char(&mut self) {
        if self.cursor_pos < self.input.len() {
            let next = self.next_char_boundary();
            self.input.drain(self.cursor_pos..next);
        }
    }

    pub fn backspace(&mut self) {
        if self.cursor_pos > 0 {
            let prev = self.prev_char_boundary();
            self.input.drain(prev..self.cursor_pos);
            self.cursor_pos = prev;
        }
    }

    pub fn move_left(&mut self) {
        if self.cursor_pos > 0 {
            self.cursor_pos = self.prev_char_boundary();
        }
    }

    pub fn move_right(&mut self) {
        if self.cursor_pos < self.input.len() {
            self.cursor_pos = self.next_char_boundary();
        }
    }

    fn prev_char_boundary(&self) -> usize {
        let mut pos = self.cursor_pos - 1;
        while pos > 0 && !self.input.is_char_boundary(pos) {
            pos -= 1;
        }
        pos
    }

    fn next_char_boundary(&self) -> usize {
        let mut pos = self.cursor_pos + 1;
        while pos < self.input.len() && !self.input.is_char_boundary(pos) {
            pos += 1;
        }
        pos
    }

    pub fn move_home(&mut self) {
        self.cursor_pos = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor_pos = self.input.len();
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
            self.cursor_pos = self.input.len();
        }
    }

    pub fn history_down(&mut self) {
        match self.history_index {
            Some(i) if i < self.history.len() - 1 => {
                self.history_index = Some(i + 1);
                self.input = self.history[i + 1].clone();
                self.cursor_pos = self.input.len();
            }
            Some(_) => {
                self.history_index = None;
                self.input.clear();
                self.cursor_pos = 0;
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
        self.cursor_pos = 0;
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
        self.output_scroll = 0;
        self.show_output = true;
    }

    pub fn available_commands() -> Vec<(&'static str, &'static str)> {
        let mut cmds = vec![
            ("help", "Show this help message"),
            ("clear", "Clear command output"),
        ];
        cmds.extend_from_slice(crate::rpc::types::RPC_METHODS);
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
            node_url: None,
            node_uid: None,
            connection_status: ConnectionStatus::Disconnected,
            last_refresh: None,
            last_poll_duration_ms: None,
            last_error: None,
        }
    }
}

#[derive(Default)]
pub struct AnalyticsState {
    pub engine: Option<Arc<tokio::sync::RwLock<AnalyticsEngine>>>,
    pub focus: usize,
    pub view_modes: [ViewMode; 5],
    pub time_windows: [TimeWindow; 5],
    pub sync_progress: Option<(u64, u64)>,
    pub reorg_notification: Option<String>,
    pub cached_views: Option<[AggregatedView; 5]>,
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
    pub should_quit: bool,

    pub node: NodeState,
    pub analytics: AnalyticsState,
    pub dag_selection: DagSelection,
    pub market_data: Option<MarketData>,

    pub rpc_explorer: RpcExplorerState,
    pub command_line: CommandLine,

    pub mempool_selected: usize,
    pub mempool_detail: Option<String>,

    pub paused: bool,
    pub show_help: bool,
    pub quit_confirm: bool,
    pub dirty: bool,
    /// Called by `mark_dirty()` so a frontend can wake up and redraw.
    pub repaint: Option<RepaintFn>,
    pub has_direct_node: bool,

    pub integrated_node: IntegratedNodeState,
}

impl App {
    pub fn new(daemon_config: DaemonConfig) -> Self {
        Self {
            active_tab: Tab::Dashboard,
            should_quit: false,
            node: NodeState::default(),
            analytics: AnalyticsState::default(),
            dag_selection: DagSelection::default(),
            market_data: None,
            rpc_explorer: RpcExplorerState::default(),
            command_line: CommandLine::default(),
            mempool_selected: 0,
            mempool_detail: None,
            paused: false,
            show_help: false,
            quit_confirm: false,
            dirty: true,
            repaint: None,
            has_direct_node: false,
            integrated_node: IntegratedNodeState::new(daemon_config),
        }
    }

    /// Flag state as changed and ask the frontend to redraw.
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
        if let Some(ref repaint) = self.repaint {
            repaint();
        }
    }

    pub fn is_daemon_active(&self) -> bool {
        matches!(self.integrated_node.status, DaemonStatus::Running)
    }

    pub fn is_node_syncing(&self) -> bool {
        self.is_daemon_active()
            && !self
                .node
                .server_info
                .as_ref()
                .is_some_and(|s| s.is_synced)
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

    // --- Tab ---

    #[test]
    fn tab_titles() {
        assert_eq!(Tab::Dashboard.title(), "1:Dashboard");
        assert_eq!(Tab::Mempool.title(), "2:Mempool");
        assert_eq!(Tab::BlockDag.title(), "3:BlockDAG");
        assert_eq!(Tab::Analytics.title(), "4:Analytics");
        assert_eq!(Tab::RpcExplorer.title(), "5:RPC Cmds");
        assert_eq!(Tab::IntegratedNode.title(), "6:Node");
    }

    #[test]
    fn tab_labels_strip_shortcut_prefix() {
        assert_eq!(Tab::Dashboard.label(), "Dashboard");
        assert_eq!(Tab::RpcExplorer.label(), "RPC Cmds");
        assert_eq!(Tab::IntegratedNode.label(), "Node");
    }

    #[test]
    fn tab_index_matches_all_order() {
        let mut app = App::new(DaemonConfig::default());
        for (i, tab) in Tab::all().iter().enumerate() {
            app.active_tab = *tab;
            assert_eq!(app.tab_index(), i);
        }
    }

    #[test]
    fn next_tab_cycles_forward() {
        let mut app = App::new(DaemonConfig::default());
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
        // With daemon feature, next is IntegratedNode; without, wraps to Dashboard
        let last = *Tab::all().last().unwrap();
        if last == Tab::RpcExplorer {
            assert_eq!(app.active_tab, Tab::Dashboard); // wraps
        } else {
            assert_eq!(app.active_tab, last);
            app.next_tab();
            assert_eq!(app.active_tab, Tab::Dashboard); // wraps
        }
    }

    #[test]
    fn prev_tab_cycles_backward() {
        let mut app = App::new(DaemonConfig::default());
        app.prev_tab();
        let last = *Tab::all().last().unwrap();
        assert_eq!(app.active_tab, last); // wraps from 0
        // Navigate back a couple
        app.active_tab = Tab::Analytics;
        app.prev_tab();
        assert_eq!(app.active_tab, Tab::BlockDag);
    }

    // --- CommandLine: editing ---

    #[test]
    fn insert_char_ascii() {
        let mut cl = CommandLine::default();
        cl.insert_char('a');
        cl.insert_char('b');
        assert_eq!(cl.input, "ab");
        assert_eq!(cl.cursor_pos, 2);
    }

    #[test]
    fn insert_char_utf8_emoji() {
        let mut cl = CommandLine::default();
        cl.insert_char('🦀');
        assert_eq!(cl.input, "🦀");
        assert_eq!(cl.cursor_pos, 4); // 🦀 is 4 bytes
        cl.insert_char('!');
        assert_eq!(cl.input, "🦀!");
    }

    #[test]
    fn insert_char_mid_string() {
        let mut cl = CommandLine::default();
        cl.input = "ac".to_string();
        cl.cursor_pos = 1;
        cl.insert_char('b');
        assert_eq!(cl.input, "abc");
        assert_eq!(cl.cursor_pos, 2);
    }

    #[test]
    fn delete_char_at_cursor() {
        let mut cl = CommandLine::default();
        cl.input = "abc".to_string();
        cl.cursor_pos = 1;
        cl.delete_char();
        assert_eq!(cl.input, "ac");
        assert_eq!(cl.cursor_pos, 1);
    }

    #[test]
    fn delete_char_at_end_noop() {
        let mut cl = CommandLine::default();
        cl.input = "abc".to_string();
        cl.cursor_pos = 3;
        cl.delete_char();
        assert_eq!(cl.input, "abc");
    }

    #[test]
    fn delete_char_empty_noop() {
        let mut cl = CommandLine::default();
        cl.delete_char();
        assert_eq!(cl.input, "");
    }

    #[test]
    fn backspace_removes_previous() {
        let mut cl = CommandLine::default();
        cl.input = "abc".to_string();
        cl.cursor_pos = 2;
        cl.backspace();
        assert_eq!(cl.input, "ac");
        assert_eq!(cl.cursor_pos, 1);
    }

    #[test]
    fn backspace_at_start_noop() {
        let mut cl = CommandLine::default();
        cl.input = "abc".to_string();
        cl.cursor_pos = 0;
        cl.backspace();
        assert_eq!(cl.input, "abc");
        assert_eq!(cl.cursor_pos, 0);
    }

    #[test]
    fn backspace_utf8() {
        let mut cl = CommandLine::default();
        cl.input = "a🦀b".to_string();
        cl.cursor_pos = 5; // after 🦀
        cl.backspace();
        assert_eq!(cl.input, "ab");
        assert_eq!(cl.cursor_pos, 1);
    }

    // --- CommandLine: cursor movement ---

    #[test]
    fn move_left_right() {
        let mut cl = CommandLine::default();
        cl.input = "abc".to_string();
        cl.cursor_pos = 2;
        cl.move_left();
        assert_eq!(cl.cursor_pos, 1);
        cl.move_right();
        assert_eq!(cl.cursor_pos, 2);
    }

    #[test]
    fn move_left_at_start_noop() {
        let mut cl = CommandLine::default();
        cl.input = "abc".to_string();
        cl.cursor_pos = 0;
        cl.move_left();
        assert_eq!(cl.cursor_pos, 0);
    }

    #[test]
    fn move_right_at_end_noop() {
        let mut cl = CommandLine::default();
        cl.input = "abc".to_string();
        cl.cursor_pos = 3;
        cl.move_right();
        assert_eq!(cl.cursor_pos, 3);
    }

    #[test]
    fn move_left_right_utf8() {
        let mut cl = CommandLine::default();
        cl.input = "a🦀b".to_string();
        cl.cursor_pos = 5; // after 🦀
        cl.move_left();
        assert_eq!(cl.cursor_pos, 1); // before 🦀
        cl.move_right();
        assert_eq!(cl.cursor_pos, 5); // after 🦀
    }

    #[test]
    fn move_home_end() {
        let mut cl = CommandLine::default();
        cl.input = "hello".to_string();
        cl.cursor_pos = 3;
        cl.move_home();
        assert_eq!(cl.cursor_pos, 0);
        cl.move_end();
        assert_eq!(cl.cursor_pos, 5);
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
        assert_eq!(cl.cursor_pos, 0);
        assert_eq!(cl.history_index, None);
    }

    #[test]
    fn submit_appends_to_history() {
        let mut cl = CommandLine::default();
        cl.input = "get_server_info".to_string();
        cl.submit();
        assert_eq!(cl.history, VecDeque::from(vec!["get_server_info".to_string()]));
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
    fn push_output_resets_scroll() {
        let mut cl = CommandLine::default();
        cl.output_scroll = 10;
        cl.push_output("test".to_string(), "result".to_string(), false);
        assert_eq!(cl.output_scroll, 0);
        assert!(cl.show_output);
    }

    #[test]
    fn push_output_tracks_errors() {
        let mut cl = CommandLine::default();
        cl.push_output("bad".to_string(), "fail".to_string(), true);
        assert!(cl.output.front().unwrap().is_error);
    }

    // --- CommandLine: activate/deactivate ---

    #[test]
    fn activate_clears_state() {
        let mut cl = CommandLine::default();
        cl.input = "leftover".to_string();
        cl.cursor_pos = 5;
        cl.history_index = Some(2);
        cl.activate();
        assert!(cl.active);
        assert_eq!(cl.input, "");
        assert_eq!(cl.cursor_pos, 0);
        assert_eq!(cl.history_index, None);
    }

    #[test]
    fn deactivate_clears_state() {
        let mut cl = CommandLine::default();
        cl.active = true;
        cl.input = "something".to_string();
        cl.deactivate();
        assert!(!cl.active);
        assert_eq!(cl.input, "");
        assert_eq!(cl.cursor_pos, 0);
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
}
