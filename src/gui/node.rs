use eframe::egui::{self, Color32, ComboBox, DragValue, RichText, TextEdit, Ui};

use super::theme;
use super::widgets::{
    CARD_GAP, card, danger_button, field_label, kv, kv_grid, placeholder, primary_button,
};
use crate::app::{App, DaemonStatus, IntegratedNodeState};
use crate::config::DaemonConfig;
use crate::controller::{CommandSender, UiCommand};
use crate::daemon::DaemonHandle;

pub fn show(ui: &mut Ui, app: &mut App, cmd_tx: &CommandSender) {
    match app.integrated_node.status {
        DaemonStatus::Stopped | DaemonStatus::Error(_) => {
            settings(ui, &mut app.integrated_node, cmd_tx)
        }
        _ => running(ui, app, cmd_tx),
    }
}

// ── Settings (node stopped) ──

fn settings(ui: &mut Ui, state: &mut IntegratedNodeState, cmd_tx: &CommandSender) {
    ui.horizontal(|ui| {
        let can_start = !state.config.app_dir.trim().is_empty();
        let start = ui
            .add_enabled(can_start, primary_button("▶ Start node"))
            .on_disabled_hover_text("App Dir must not be empty");
        if start.clicked() {
            start_daemon(state, cmd_tx);
        }
        if ui
            .button("Reload from disk")
            .on_hover_text(DaemonConfig::config_path().display().to_string())
            .clicked()
        {
            match DaemonConfig::load() {
                Ok(c) => {
                    state.config = c;
                    state.status_message = Some(("Config reloaded".to_string(), false));
                }
                Err(e) => state.status_message = Some((format!("Load failed: {e}"), true)),
            }
        }
        ui.label(
            RichText::new("Changes are saved automatically.")
                .weak()
                .small(),
        );
    });

    if let DaemonStatus::Error(ref msg) = state.status {
        ui.label(RichText::new(format!("Error: {msg}")).color(theme::ERROR));
    }
    if let Some((ref msg, is_error)) = state.status_message {
        let color = if is_error { theme::ERROR } else { theme::OK };
        ui.label(RichText::new(msg).color(color));
    }
    ui.add_space(6.0);

    let mut changed = false;
    egui::ScrollArea::vertical().show(ui, |ui| {
        changed |= config_form(ui, &mut state.config);
    });

    if changed {
        state.status_message = None;
        if let Err(e) = state.config.save() {
            state.status_message = Some((format!("Auto-save failed: {e}"), true));
        }
    }
}

pub(super) fn start_daemon(state: &mut IntegratedNodeState, cmd_tx: &CommandSender) {
    state.log_lines.clear();
    state.status_message = None;
    match cmd_tx.send(UiCommand::StartDaemon(Box::new(state.config.clone()))) {
        Ok(()) => state.status = DaemonStatus::Starting,
        Err(_) => {
            state.status_message = Some(("Controller is not running".to_string(), true));
        }
    }
}

/// Render the config form. Returns `true` if any value changed.
fn config_form(ui: &mut Ui, cfg: &mut DaemonConfig) -> bool {
    let mut changed = false;

    section(ui, "General", |ui| {
        changed |= combo(
            ui,
            "Network",
            &mut cfg.network,
            DaemonConfig::valid_networks(),
        );
        changed |= check(ui, "UTXO Index", &mut cfg.utxo_index);
        changed |= check(ui, "Archival", &mut cfg.archival);
        changed |= field(ui, "RAM Scale", |ui| {
            ui.add(
                DragValue::new(&mut cfg.ram_scale)
                    .range(0.1..=10.0)
                    .speed(0.1)
                    .fixed_decimals(1),
            )
        });
        changed |= combo(
            ui,
            "Log Level",
            &mut cfg.log_level,
            DaemonConfig::valid_log_levels(),
        );
        changed |= field(ui, "Async Threads", |ui| {
            ui.add(DragValue::new(&mut cfg.async_threads).range(1..=256))
        });
        changed |= check(ui, "Auto Start", &mut cfg.auto_start_daemon);
    });

    section(ui, "Networking", |ui| {
        changed |= opt_text(ui, "Listen", &mut cfg.listen, "0.0.0.0:16111");
        changed |= opt_text(ui, "External IP", &mut cfg.externalip, "");
        changed |= field(ui, "Outbound Peers", |ui| {
            ui.add(DragValue::new(&mut cfg.outbound_target).range(0..=1024))
        });
        changed |= field(ui, "Max Inbound", |ui| {
            ui.add(DragValue::new(&mut cfg.inbound_limit).range(0..=4096))
        });
        changed |= text(
            ui,
            "Connect Peers",
            &mut cfg.connect_peers,
            "host:port, host:port",
        );
        changed |= text(ui, "Add Peers", &mut cfg.add_peers, "host:port, host:port");
        changed |= check(ui, "Disable UPnP", &mut cfg.disable_upnp);
        changed |= check(ui, "Disable DNS Seed", &mut cfg.disable_dns_seed);
    });

    section(ui, "Storage", |ui| {
        changed |= text(ui, "App Dir", &mut cfg.app_dir, "");
        changed |= combo(
            ui,
            "RocksDB Preset",
            &mut cfg.rocksdb_preset,
            DaemonConfig::valid_rocksdb_presets(),
        );
        changed |= opt_text(ui, "RocksDB WAL Dir", &mut cfg.rocksdb_wal_dir, "default");
        changed |= field(ui, "RocksDB Cache MB", |ui| {
            opt_number(ui, &mut cfg.rocksdb_cache_size, 1.0)
        });
        changed |= field(ui, "Retention Days", |ui| {
            opt_number(ui, &mut cfg.retention_period_days, 0.1)
        });
        changed |= check(ui, "Reset DB", &mut cfg.reset_db);
        changed |= field(ui, "RPC Max Clients", |ui| {
            ui.add(DragValue::new(&mut cfg.rpc_max_clients).range(1..=4096))
        });
    });

    section(ui, "Performance", |ui| {
        changed |= check(ui, "Perf Metrics", &mut cfg.perf_metrics);
    });

    changed
}

fn section(ui: &mut Ui, title: &str, add_rows: impl FnOnce(&mut Ui)) {
    egui::CollapsingHeader::new(RichText::new(title).color(theme::ACCENT))
        .default_open(true)
        .show(ui, |ui| {
            egui::Grid::new(title)
                .num_columns(2)
                .spacing([24.0, 6.0])
                .min_col_width(140.0)
                .show(ui, add_rows);
        });
}

fn field(ui: &mut Ui, label: &str, add: impl FnOnce(&mut Ui) -> egui::Response) -> bool {
    field_label(ui, label);
    let changed = add(ui).changed();
    ui.end_row();
    changed
}

fn check(ui: &mut Ui, label: &str, value: &mut bool) -> bool {
    field(ui, label, |ui| ui.checkbox(value, ""))
}

fn combo(ui: &mut Ui, label: &str, value: &mut String, options: &[&str]) -> bool {
    field(ui, label, |ui| {
        let mut changed = false;
        let mut response = ComboBox::from_id_salt(label)
            .selected_text(value.as_str())
            .show_ui(ui, |ui| {
                for option in options {
                    changed |= ui
                        .selectable_value(value, option.to_string(), *option)
                        .changed();
                }
            })
            .response;
        if changed {
            response.mark_changed();
        }
        response
    })
}

fn text(ui: &mut Ui, label: &str, value: &mut String, hint: &str) -> bool {
    field(ui, label, |ui| {
        ui.add(
            TextEdit::singleline(value)
                .hint_text(hint)
                .desired_width(360.0),
        )
    })
}

/// Text field for an optional value: empty means `None`.
fn opt_text(ui: &mut Ui, label: &str, value: &mut Option<String>, hint: &str) -> bool {
    let mut buf = value.clone().unwrap_or_default();
    let changed = text(ui, label, &mut buf, hint);
    if changed {
        *value = (!buf.trim().is_empty()).then_some(buf);
    }
    changed
}

/// Optional number with an "enabled" checkbox; unchecked means `None` (use the node default).
fn opt_number<T>(ui: &mut Ui, value: &mut Option<T>, speed: f64) -> egui::Response
where
    T: egui::emath::Numeric + Default,
{
    ui.horizontal(|ui| {
        let mut enabled = value.is_some();
        let mut response = ui.checkbox(&mut enabled, "");
        if response.changed() {
            *value = enabled.then(T::default);
        }
        match value {
            Some(v) => response |= ui.add(DragValue::new(v).speed(speed)),
            None => {
                ui.label(RichText::new("node default").weak());
            }
        }
        response
    })
    .inner
}

// ── Running ──

fn running(ui: &mut Ui, app: &mut App, cmd_tx: &CommandSender) {
    card(ui, "Node Status", |ui| {
        ui.horizontal(|ui| {
            kv_grid(ui, "node_status", |ui| status_rows(ui, app));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
                let state = &mut app.integrated_node;
                let can_stop = state.status == DaemonStatus::Running;
                if ui
                    .add_enabled(can_stop, danger_button("■ Stop node"))
                    .clicked()
                    && cmd_tx.send(UiCommand::StopDaemon).is_ok()
                {
                    state.status = DaemonStatus::Stopping;
                }
            });
        });
    });
    ui.add_space(CARD_GAP);
    card(ui, "Node Logs", |ui| logs(ui, &app.integrated_node));
}

fn status_rows(ui: &mut Ui, app: &App) {
    let state = &app.integrated_node;
    let (status, status_color) = theme::daemon_status(&state.status);
    let (sync, sync_color) = match app.node.server_info {
        Some(ref info) if info.is_synced => ("Synced", theme::OK),
        Some(_) => ("Syncing…", theme::WARN),
        None => ("Waiting…", theme::TEXT_DIM),
    };
    field_label(ui, "Status");
    ui.horizontal(|ui| {
        ui.label(RichText::new(status).color(status_color));
        ui.label(RichText::new(sync).color(sync_color));
    });
    ui.end_row();
    kv(ui, "Network", &state.config.network);
    kv(
        ui,
        "wRPC",
        DaemonHandle::wrpc_borsh_url(&state.config.network),
    );
    kv(ui, "Uptime", uptime(state));
}

fn uptime(state: &IntegratedNodeState) -> String {
    let Some(started) = state.started_at else {
        return "—".to_string();
    };
    let secs = started.elapsed().as_secs();
    if secs >= 3600 {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

fn logs(ui: &mut Ui, state: &IntegratedNodeState) {
    if state.log_lines.is_empty() {
        placeholder(ui, "Waiting for log output…");
        return;
    }
    let row_height = ui.text_style_height(&egui::TextStyle::Monospace);
    egui::ScrollArea::both()
        .auto_shrink(false)
        .stick_to_bottom(true)
        .show_rows(ui, row_height, state.log_lines.len(), |ui, range| {
            for line in state.log_lines.range(range) {
                ui.label(RichText::new(line).color(log_color(line)));
            }
        });
}

fn log_color(line: &str) -> Color32 {
    if line.contains("ERROR") {
        theme::ERROR
    } else if line.contains("WARN") {
        theme::WARN
    } else if line.contains("DEBUG") || line.contains("TRACE") {
        theme::TEXT_DIM
    } else {
        theme::TEXT
    }
}
