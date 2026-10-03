use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use crate::app::{App, DagFocus, DaemonStatus, IntegratedNodeState, Tab};
use crate::config::DaemonConfig;
use crate::controller::{CommandSender, UiCommand};
use crate::rpc::types::sompi_to_kas;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigField {
    // General (0-6)
    Network,
    UtxoIndex,
    Archival,
    RamScale,
    LogLevel,
    AsyncThreads,
    AutoStart,
    // Networking (7-14)
    Listen,
    ExternalIp,
    OutboundTarget,
    InboundLimit,
    ConnectPeers,
    AddPeers,
    DisableUpnp,
    DisableDnsSeed,
    // Storage (15-21)
    AppDir,
    RocksdbPreset,
    RocksdbWalDir,
    RocksdbCacheSize,
    RetentionDays,
    ResetDb,
    RpcMaxClients,
    // Performance (22)
    PerfMetrics,
    // Action (23)
    StartDaemon,
}

impl ConfigField {
    pub const COUNT: usize = 24;

    pub fn from_index(i: usize) -> Option<Self> {
        use ConfigField::*;
        [
            Network, UtxoIndex, Archival, RamScale, LogLevel, AsyncThreads, AutoStart,
            Listen, ExternalIp, OutboundTarget, InboundLimit, ConnectPeers, AddPeers,
            DisableUpnp, DisableDnsSeed, AppDir, RocksdbPreset, RocksdbWalDir,
            RocksdbCacheSize, RetentionDays, ResetDb, RpcMaxClients, PerfMetrics, StartDaemon,
        ]
        .get(i)
        .copied()
    }
}

/// Handle command-mode key input. Returns `Some(cmd)` if a command was submitted
/// (caller must drop the app guard and dispatch the command).
pub fn handle_command_mode_keys(app: &mut App, code: KeyCode) -> Option<String> {
    match code {
        KeyCode::Esc => {
            app.command_line.deactivate();
            app.command_line.show_output = false;
            None
        }
        KeyCode::Enter => app.command_line.submit(),
        KeyCode::Backspace => { app.command_line.backspace(); None }
        KeyCode::Delete => { app.command_line.delete_char(); None }
        KeyCode::Left => { app.command_line.move_left(); None }
        KeyCode::Right => { app.command_line.move_right(); None }
        KeyCode::Home => { app.command_line.move_home(); None }
        KeyCode::End => { app.command_line.move_end(); None }
        KeyCode::Up => { app.command_line.history_up(); None }
        KeyCode::Down => { app.command_line.history_down(); None }
        KeyCode::Char(c) => { app.command_line.insert_char(c); None }
        _ => None,
    }
}

/// Handle normal-mode key input (not command mode). Returns `true` if the event
/// was consumed by an overlay (help, command output) and the caller should `continue`.
pub fn handle_normal_keys(
    app: &mut App,
    key: KeyEvent,
    cmd_tx: &CommandSender,
) -> bool {
    // Reset quit confirmation on any key that isn't 'q'
    if key.code != KeyCode::Char('q') {
        app.quit_confirm = false;
    }

    // Help overlay takes priority
    if app.show_help {
        match key.code {
            KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') => {
                app.show_help = false;
            }
            _ => {}
        }
        return true;
    }

    // Command output overlay: intercept scroll keys
    if app.command_line.show_output && !app.command_line.output.is_empty() {
        match key.code {
            KeyCode::Esc => { app.command_line.show_output = false; }
            KeyCode::Char('j') => {
                app.command_line.output_scroll = app.command_line.output_scroll.saturating_sub(1);
            }
            KeyCode::Char('k') => {
                app.command_line.output_scroll = app.command_line.output_scroll.saturating_add(1);
            }
            KeyCode::Char('J') => {
                app.command_line.output_scroll = app.command_line.output_scroll.saturating_sub(10);
            }
            KeyCode::Char('K') => {
                app.command_line.output_scroll = app.command_line.output_scroll.saturating_add(10);
            }
            KeyCode::Char('g') => { app.command_line.output_scroll = 0; }
            KeyCode::Char('G') => { app.command_line.output_scroll = usize::MAX; }
            _ => {}
        }
        return true;
    }

    // Normal mode
    match (key.code, key.modifiers) {
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => { app.should_quit = true; }
        (KeyCode::Char('q'), _) => {
            if app.quit_confirm { app.should_quit = true; } else { app.quit_confirm = true; }
        }
        (KeyCode::Esc, _) => { app.command_line.show_output = false; }
        (KeyCode::Char('?'), _) => { app.show_help = true; }
        (KeyCode::Char(':'), _) => { app.command_line.activate(); }
        (KeyCode::Char('p'), _) => { app.paused = !app.paused; }
        (KeyCode::Tab, _) => { app.next_tab(); }
        (KeyCode::BackTab, _) => { app.prev_tab(); }
        (KeyCode::Char('1'), _) => { app.active_tab = Tab::Dashboard; }
        (KeyCode::Char('2'), _) => { app.active_tab = Tab::Mempool; }
        (KeyCode::Char('3'), _) => { app.active_tab = Tab::BlockDag; }
        (KeyCode::Char('4'), _) => { app.active_tab = Tab::Analytics; }
        (KeyCode::Char('5'), _) => { app.active_tab = Tab::RpcExplorer; }
        (KeyCode::Char('6'), _) => { app.active_tab = Tab::IntegratedNode; }
        _ => match app.active_tab {
            Tab::RpcExplorer => handle_rpc_explorer_keys(app, key.code, cmd_tx),
            Tab::Mempool => handle_mempool_keys(app, key.code),
            Tab::BlockDag => handle_blockdag_keys(app, key.code, cmd_tx),
            Tab::Analytics => handle_analytics_keys(app, key.code),
            Tab::IntegratedNode => handle_integrated_node_keys(app, key.code, cmd_tx),
            _ => {}
        },
    }

    false
}

/// Handle mouse events.
pub fn handle_mouse(app: &mut App, mouse: MouseEvent) -> bool {
    // Dismiss help on any click
    if app.show_help {
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            app.show_help = false;
        }
        return true;
    }

    match mouse.kind {
        MouseEventKind::ScrollDown => match app.active_tab {
            Tab::Mempool if app.mempool_detail.is_none() => {
                let max = app.node.mempool_state.as_ref()
                    .map(|m| m.entries.len().saturating_sub(1))
                    .unwrap_or(0);
                if app.mempool_selected < max { app.mempool_selected += 1; }
            }
            Tab::BlockDag if app.dag_selection.block_detail.is_none() => {
                if let Some(ref dag) = app.node.dag_info {
                    match app.dag_selection.focus {
                        DagFocus::Tips => {
                            if app.dag_selection.tip_selected < dag.tip_hashes.len().saturating_sub(1) {
                                app.dag_selection.tip_selected += 1;
                            }
                        }
                        DagFocus::Parents => {
                            if app.dag_selection.parent_selected < dag.virtual_parent_hashes.len().saturating_sub(1) {
                                app.dag_selection.parent_selected += 1;
                            }
                        }
                    }
                }
            }
            Tab::RpcExplorer => {
                app.rpc_explorer.scroll_offset = app.rpc_explorer.scroll_offset.saturating_add(3);
            }
            Tab::IntegratedNode if app.integrated_node.is_running() => {
                let max = app.integrated_node.log_lines.len();
                app.integrated_node.log_scroll = app.integrated_node.log_scroll.saturating_add(3).min(max);
                app.integrated_node.log_auto_scroll = app.integrated_node.log_scroll >= max;
            }
            _ => {}
        },
        MouseEventKind::ScrollUp => match app.active_tab {
            Tab::Mempool if app.mempool_detail.is_none() => {
                app.mempool_selected = app.mempool_selected.saturating_sub(1);
            }
            Tab::BlockDag if app.dag_selection.block_detail.is_none() => {
                match app.dag_selection.focus {
                    DagFocus::Tips => {
                        app.dag_selection.tip_selected = app.dag_selection.tip_selected.saturating_sub(1);
                    }
                    DagFocus::Parents => {
                        app.dag_selection.parent_selected = app.dag_selection.parent_selected.saturating_sub(1);
                    }
                }
            }
            Tab::RpcExplorer => {
                app.rpc_explorer.scroll_offset = app.rpc_explorer.scroll_offset.saturating_sub(3);
            }
            Tab::IntegratedNode if app.integrated_node.is_running() => {
                app.integrated_node.log_scroll = app.integrated_node.log_scroll.saturating_sub(3);
                app.integrated_node.log_auto_scroll = false;
            }
            _ => {}
        },
        MouseEventKind::Down(MouseButton::Left) => {
            if mouse.row < 3 {
                let tabs = Tab::all();
                let mut x_pos: u16 = 2;
                for tab in tabs {
                    let title_len = tab.title().len() as u16;
                    if mouse.column >= x_pos && mouse.column < x_pos + title_len {
                        app.active_tab = *tab;
                        break;
                    }
                    x_pos += title_len + 3;
                }
            }
        }
        _ => {}
    }

    false
}

pub fn handle_rpc_explorer_keys(
    app: &mut App,
    key: KeyCode,
    cmd_tx: &CommandSender,
) {
    if app.rpc_explorer.available_methods.is_empty() {
        return;
    }

    match key {
        KeyCode::Up => {
            if app.rpc_explorer.selected_method > 0 {
                app.rpc_explorer.selected_method -= 1;
            }
        }
        KeyCode::Down => {
            let len = app.rpc_explorer.available_methods.len();
            if len > 0 && app.rpc_explorer.selected_method < len - 1 {
                app.rpc_explorer.selected_method += 1;
            }
        }
        KeyCode::Enter => {
            let method = app.rpc_explorer.available_methods[app.rpc_explorer.selected_method];
            app.rpc_explorer.is_loading = true;
            app.rpc_explorer.scroll_offset = 0;

            let _ = cmd_tx.send(UiCommand::ExecuteRpc(method.to_string()));
        }
        KeyCode::Char('j') | KeyCode::Char('J') => {
            let step = if key == KeyCode::Char('J') { 10 } else { 1 };
            app.rpc_explorer.scroll_offset = app.rpc_explorer.scroll_offset.saturating_add(step);
        }
        KeyCode::Char('k') | KeyCode::Char('K') => {
            let step = if key == KeyCode::Char('K') { 10 } else { 1 };
            app.rpc_explorer.scroll_offset = app.rpc_explorer.scroll_offset.saturating_sub(step);
        }
        KeyCode::PageDown => {
            app.rpc_explorer.scroll_offset = app.rpc_explorer.scroll_offset.saturating_add(20);
        }
        KeyCode::PageUp => {
            app.rpc_explorer.scroll_offset = app.rpc_explorer.scroll_offset.saturating_sub(20);
        }
        KeyCode::Home | KeyCode::Char('g') => {
            app.rpc_explorer.scroll_offset = 0;
        }
        KeyCode::End | KeyCode::Char('G') => {
            app.rpc_explorer.scroll_offset = usize::MAX;
        }
        _ => {}
    }
}

pub fn handle_mempool_keys(app: &mut App, key: KeyCode) {
    if app.mempool_detail.is_some() {
        if key == KeyCode::Esc {
            app.mempool_detail = None;
        }
        return;
    }

    let entry_count = app
        .node.mempool_state
        .as_ref()
        .map(|m| m.entries.len())
        .unwrap_or(0);

    match key {
        KeyCode::Up | KeyCode::Char('k') => {
            app.mempool_selected = app.mempool_selected.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if entry_count > 0 && app.mempool_selected < entry_count.saturating_sub(1) {
                app.mempool_selected += 1;
            }
        }
        KeyCode::Home | KeyCode::Char('g') => {
            app.mempool_selected = 0;
        }
        KeyCode::End | KeyCode::Char('G') => {
            if entry_count > 0 {
                app.mempool_selected = entry_count - 1;
            }
        }
        KeyCode::Enter => {
            if let Some(ref mempool) = app.node.mempool_state
                && app.mempool_selected < mempool.entries.len()
            {
                let entry = &mempool.entries[app.mempool_selected];
                let detail = format!(
                    "Transaction ID: {}\nFee: {:.8} KAS ({} sompi)\nOrphan: {}",
                    entry.transaction_id,
                    sompi_to_kas(entry.fee),
                    entry.fee,
                    if entry.is_orphan { "Yes" } else { "No" },
                );
                app.mempool_detail = Some(detail);
            }
        }
        _ => {}
    }
}

pub fn handle_blockdag_keys(
    app: &mut App,
    key: KeyCode,
    cmd_tx: &CommandSender,
) {
    if app.dag_selection.block_detail.is_some() {
        if key == KeyCode::Esc {
            app.dag_selection.block_detail = None;
        }
        return;
    }

    match key {
        KeyCode::Left | KeyCode::Right | KeyCode::Char('h') | KeyCode::Char('l') => {
            app.dag_selection.focus = match app.dag_selection.focus {
                DagFocus::Tips => DagFocus::Parents,
                DagFocus::Parents => DagFocus::Tips,
            };
        }
        KeyCode::Up | KeyCode::Char('k') => match app.dag_selection.focus {
            DagFocus::Tips => {
                app.dag_selection.tip_selected = app.dag_selection.tip_selected.saturating_sub(1);
            }
            DagFocus::Parents => {
                app.dag_selection.parent_selected = app.dag_selection.parent_selected.saturating_sub(1);
            }
        },
        KeyCode::Down | KeyCode::Char('j') => {
            if let Some(ref dag) = app.node.dag_info {
                match app.dag_selection.focus {
                    DagFocus::Tips => {
                        if app.dag_selection.tip_selected < dag.tip_hashes.len().saturating_sub(1) {
                            app.dag_selection.tip_selected += 1;
                        }
                    }
                    DagFocus::Parents => {
                        if app.dag_selection.parent_selected
                            < dag.virtual_parent_hashes.len().saturating_sub(1)
                        {
                            app.dag_selection.parent_selected += 1;
                        }
                    }
                }
            }
        }
        KeyCode::Home | KeyCode::Char('g') => match app.dag_selection.focus {
            DagFocus::Tips => {
                app.dag_selection.tip_selected = 0;
            }
            DagFocus::Parents => {
                app.dag_selection.parent_selected = 0;
            }
        },
        KeyCode::End | KeyCode::Char('G') => {
            if let Some(ref dag) = app.node.dag_info {
                match app.dag_selection.focus {
                    DagFocus::Tips => {
                        app.dag_selection.tip_selected = dag.tip_hashes.len().saturating_sub(1);
                    }
                    DagFocus::Parents => {
                        app.dag_selection.parent_selected =
                            dag.virtual_parent_hashes.len().saturating_sub(1);
                    }
                }
            }
        }
        KeyCode::Enter => {
            let hash = if let Some(ref dag) = app.node.dag_info {
                match app.dag_selection.focus {
                    DagFocus::Tips => dag.tip_hashes.get(app.dag_selection.tip_selected).cloned(),
                    DagFocus::Parents => dag
                        .virtual_parent_hashes
                        .get(app.dag_selection.parent_selected)
                        .cloned(),
                }
            } else {
                None
            };

            if let Some(hash) = hash {
                app.dag_selection.block_loading = true;
                let _ = cmd_tx.send(UiCommand::LookupBlock(hash));
            }
        }
        _ => {}
    }
}

pub fn handle_analytics_keys(app: &mut App, key: KeyCode) {
    // Dismiss reorg notification on Esc
    if app.analytics.reorg_notification.is_some() && key == KeyCode::Esc {
        app.analytics.reorg_notification = None;
        return;
    }

    match key {
        // Panel navigation (2x2 + 1 full-width bottom)
        // Grid:  0  1
        //        2  3
        //        4  4
        KeyCode::Left | KeyCode::Char('h') => {
            if app.analytics.focus % 2 == 1 && app.analytics.focus < 4 {
                app.analytics.focus -= 1;
            }
        }
        KeyCode::Right | KeyCode::Char('l') => {
            if app.analytics.focus.is_multiple_of(2) && app.analytics.focus < 4 {
                app.analytics.focus += 1;
            }
        }
        KeyCode::Up | KeyCode::Char('k') => {
            match app.analytics.focus {
                2 => app.analytics.focus = 0,
                3 => app.analytics.focus = 1,
                4 => app.analytics.focus = 2,
                _ => {}
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            match app.analytics.focus {
                0 => app.analytics.focus = 2,
                1 => app.analytics.focus = 3,
                2 | 3 => app.analytics.focus = 4,
                _ => {}
            }
        }
        // Toggle view mode for focused panel
        KeyCode::Char('v') => {
            app.analytics.view_modes[app.analytics.focus].toggle();
        }
        // Cycle time window for focused panel
        KeyCode::Char('t') => {
            app.analytics.time_windows[app.analytics.focus].cycle();
            // Refresh the cached view for this panel immediately
            if let Some(ref engine) = app.analytics.engine
                && let Ok(eng) = engine.try_read()
            {
                let focus = app.analytics.focus;
                let new_view = eng.get_view(app.analytics.time_windows[focus]);
                if let Some(ref mut views) = app.analytics.cached_views {
                    views[focus] = new_view;
                }
            }
        }
        _ => {}
    }
}

pub fn handle_integrated_node_keys(
    app: &mut App,
    key: KeyCode,
    cmd_tx: &CommandSender,
) {
    let state = &mut app.integrated_node;

    if state.is_running() {
        // Running mode
        match key {
            KeyCode::Enter => {
                if matches!(state.status, DaemonStatus::Running)
                    && cmd_tx.send(UiCommand::StopDaemon).is_ok()
                {
                    state.status = DaemonStatus::Stopping;
                }
            }
            KeyCode::Char('j') => {
                let max = state.log_lines.len();
                state.log_scroll = state.log_scroll.saturating_add(1).min(max);
                state.log_auto_scroll = state.log_scroll >= max;
            }
            KeyCode::Char('k') => {
                state.log_scroll = state.log_scroll.saturating_sub(1);
                state.log_auto_scroll = false;
            }
            KeyCode::Char('J') => {
                let max = state.log_lines.len();
                state.log_scroll = state.log_scroll.saturating_add(10).min(max);
                state.log_auto_scroll = state.log_scroll >= max;
            }
            KeyCode::Char('K') => {
                state.log_scroll = state.log_scroll.saturating_sub(10);
                state.log_auto_scroll = false;
            }
            KeyCode::Home | KeyCode::Char('g') => {
                state.log_scroll = 0;
                state.log_auto_scroll = false;
            }
            KeyCode::End | KeyCode::Char('G') => {
                state.log_scroll = state.log_lines.len();
                state.log_auto_scroll = true;
            }
            _ => {}
        }
    } else if state.editing {
        // Field editing mode
        match key {
            KeyCode::Esc => {
                state.editing = false;
                state.edit_buffer.clear();
            }
            KeyCode::Enter => {
                let val = state.edit_buffer.clone();
                state.editing = false;
                state.edit_buffer.clear();
                if let Some(field) = ConfigField::from_index(state.selected_field) {
                    apply_field_edit(&mut state.config, field, &val);
                    auto_save_config(state);
                }
            }
            KeyCode::Backspace => {
                state.edit_buffer.pop();
            }
            KeyCode::Char(c) => {
                state.edit_buffer.push(c);
            }
            _ => {}
        }
    } else {
        // Settings navigation mode
        state.status_message = None; // Clear transient messages on any navigation
        match key {
            KeyCode::Up => {
                state.selected_field = state.selected_field.saturating_sub(1);
            }
            KeyCode::Down => {
                let max = ConfigField::COUNT - 1;
                if state.selected_field < max {
                    state.selected_field += 1;
                }
            }
            KeyCode::Enter => {
                if let Some(field) = ConfigField::from_index(state.selected_field) {
                    use ConfigField::*;
                    match field {
                        // Cycle enum fields
                        Network => { state.config.cycle_network(); auto_save_config(state); }
                        LogLevel => { state.config.cycle_log_level(); auto_save_config(state); }
                        RocksdbPreset => { state.config.cycle_rocksdb_preset(); auto_save_config(state); }
                        // Toggle bool fields
                        UtxoIndex => { state.config.utxo_index = !state.config.utxo_index; auto_save_config(state); }
                        Archival => { state.config.archival = !state.config.archival; auto_save_config(state); }
                        AutoStart => { state.config.auto_start_daemon = !state.config.auto_start_daemon; auto_save_config(state); }
                        DisableUpnp => { state.config.disable_upnp = !state.config.disable_upnp; auto_save_config(state); }
                        DisableDnsSeed => { state.config.disable_dns_seed = !state.config.disable_dns_seed; auto_save_config(state); }
                        ResetDb => { state.config.reset_db = !state.config.reset_db; auto_save_config(state); }
                        PerfMetrics => { state.config.perf_metrics = !state.config.perf_metrics; auto_save_config(state); }
                        // Start daemon action
                        StartDaemon => {
                            if matches!(state.status, DaemonStatus::Stopped | DaemonStatus::Error(_)) {
                                state.log_lines.clear();
                                state.log_scroll = 0;
                                state.log_auto_scroll = true;
                                state.status_message = None;
                                match cmd_tx.send(UiCommand::StartDaemon(Box::new(state.config.clone()))) {
                                    Ok(()) => {
                                        state.status = DaemonStatus::Starting;
                                    }
                                    Err(_) => {
                                        state.status_message =
                                            Some(("Command channel full, try again".to_string(), true));
                                    }
                                }
                            }
                        }
                        // Editable fields
                        _ => {
                            state.editing = true;
                            state.edit_buffer = get_field_value(&state.config, field);
                        }
                    }
                }
            }
            KeyCode::Left | KeyCode::Right => {
                if let Some(field) = ConfigField::from_index(state.selected_field) {
                    match field {
                        ConfigField::Network => { state.config.cycle_network(); auto_save_config(state); }
                        ConfigField::LogLevel => { state.config.cycle_log_level(); auto_save_config(state); }
                        ConfigField::RocksdbPreset => { state.config.cycle_rocksdb_preset(); auto_save_config(state); }
                        _ => {}
                    }
                }
            }
            KeyCode::Char('r') => match DaemonConfig::load() {
                Ok(c) => {
                    state.config = c;
                    state.status_message = Some(("Config reloaded".to_string(), false));
                }
                Err(e) => {
                    state.status_message = Some((format!("Load failed: {}", e), true));
                }
            },
            _ => {}
        }
    }
}

fn auto_save_config(state: &mut IntegratedNodeState) {
    if let Err(e) = state.config.save() {
        state.status_message = Some((format!("Auto-save failed: {}", e), true));
    }
}

fn get_field_value(config: &DaemonConfig, field: ConfigField) -> String {
    use ConfigField::*;
    match field {
        RamScale => format!("{:.1}", config.ram_scale),
        AsyncThreads => config.async_threads.to_string(),
        Listen => config.listen.clone().unwrap_or_default(),
        ExternalIp => config.externalip.clone().unwrap_or_default(),
        OutboundTarget => config.outbound_target.to_string(),
        InboundLimit => config.inbound_limit.to_string(),
        ConnectPeers => config.connect_peers.clone(),
        AddPeers => config.add_peers.clone(),
        AppDir => config.app_dir.clone(),
        RocksdbWalDir => config.rocksdb_wal_dir.clone().unwrap_or_default(),
        RocksdbCacheSize => config.rocksdb_cache_size.map_or(String::new(), |v| v.to_string()),
        RetentionDays => config.retention_period_days.map_or(String::new(), |v| format!("{:.1}", v)),
        RpcMaxClients => config.rpc_max_clients.to_string(),
        _ => String::new(),
    }
}

fn apply_field_edit(config: &mut DaemonConfig, field: ConfigField, val: &str) {
    use ConfigField::*;
    let trimmed = val.trim();
    match field {
        RamScale => {
            if let Ok(v) = trimmed.parse::<f64>() {
                config.ram_scale = v.clamp(0.1, 10.0);
            }
        }
        AsyncThreads => {
            if let Ok(v) = trimmed.parse::<usize>()
                && v > 0
            {
                config.async_threads = v;
            }
        }
        Listen => {
            config.listen = if trimmed.is_empty() { None } else { Some(trimmed.to_string()) };
        }
        ExternalIp => {
            config.externalip = if trimmed.is_empty() { None } else { Some(trimmed.to_string()) };
        }
        OutboundTarget => {
            if let Ok(v) = trimmed.parse::<usize>() {
                config.outbound_target = v;
            }
        }
        InboundLimit => {
            if let Ok(v) = trimmed.parse::<usize>() {
                config.inbound_limit = v;
            }
        }
        ConnectPeers => {
            config.connect_peers = trimmed.to_string();
        }
        AddPeers => {
            config.add_peers = trimmed.to_string();
        }
        AppDir => {
            if !trimmed.is_empty() {
                config.app_dir = trimmed.to_string();
            }
        }
        RocksdbWalDir => {
            config.rocksdb_wal_dir = if trimmed.is_empty() { None } else { Some(trimmed.to_string()) };
        }
        RocksdbCacheSize => {
            config.rocksdb_cache_size = if trimmed.is_empty() { None } else { trimmed.parse().ok() };
        }
        RetentionDays => {
            config.retention_period_days = if trimmed.is_empty() { None } else { trimmed.parse().ok() };
        }
        RpcMaxClients => {
            if let Ok(v) = trimmed.parse::<usize>() {
                config.rpc_max_clients = v;
            }
        }
        _ => {}
    }
}
