use std::time::{SystemTime, UNIX_EPOCH};

use eframe::egui::{self, Color32, RichText, Ui};
use egui_plot::{Bar, BarChart, GridMark, Line, Plot, PlotPoints};

use super::theme;
use super::widgets::{
    CARD_GAP, card, column_header, direct_node_placeholder, field_label, kv, kv_grid, placeholder,
    syncing_guard,
};
use crate::analytics::AggregatedView;
use crate::app::{App, TimeWindow, ViewMode};
use crate::format::format_hashrate;
use crate::rpc::types::format_number;

const PANELS: [&str; 5] = [
    "Fee Analysis",
    "Tx Summary",
    "Protocol Activity",
    "Top Senders",
    "Top Receivers",
];
const CHART_HEIGHT: f32 = 200.0;

pub fn show(ui: &mut Ui, app: &mut App) {
    if syncing_guard(ui, app, "Analytics") {
        return;
    }
    if !app.has_direct_node {
        placeholder(ui, direct_node_placeholder(app, ""));
        return;
    }

    banners(ui, app);

    // Panel indices follow `PANELS`.
    egui::ScrollArea::vertical().show(ui, |ui| {
        ui.columns(3, |cols| {
            panel(&mut cols[0], app, 1);
            panel(&mut cols[1], app, 2);
            panel(&mut cols[2], app, 0);
        });
        ui.add_space(CARD_GAP);
        ui.columns(2, |cols| {
            panel(&mut cols[0], app, 3);
            panel(&mut cols[1], app, 4);
        });
        ui.add_space(CARD_GAP);
        card(ui, "Mining Analysis", |ui| mining_analysis(ui, app));
    });
}

fn banners(ui: &mut Ui, app: &mut App) {
    if let Some((current, tip)) = app.analytics.sync_progress {
        let fraction = if tip > 0 {
            (current as f32 / tip as f32).min(1.0)
        } else {
            0.0
        };
        ui.horizontal(|ui| {
            ui.label(RichText::new("Syncing analytics…").color(theme::WARN));
            ui.add(
                egui::ProgressBar::new(fraction)
                    .desired_width(300.0)
                    .text(format!(
                        "DAA {}/{} ({:.1}%)",
                        format_number(current),
                        format_number(tip),
                        fraction * 100.0
                    )),
            );
        });
        ui.add_space(4.0);
    }

    let mut dismiss = false;
    if let Some(ref msg) = app.analytics.reorg_notification {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!("⚠ {msg}"))
                    .strong()
                    .color(theme::WARN),
            );
            dismiss = ui.button("Dismiss").clicked();
        });
        ui.add_space(4.0);
    }
    if dismiss {
        app.analytics.reorg_notification = None;
    }
}

fn panel(ui: &mut Ui, app: &mut App, i: usize) {
    card(ui, PANELS[i], |ui| {
        ui.horizontal(|ui| {
            let mut window = app.analytics.time_windows[i];
            for w in [
                TimeWindow::OneMin,
                TimeWindow::OneHour,
                TimeWindow::TwentyFourHour,
            ] {
                ui.selectable_value(&mut window, w, w.label());
            }
            if window != app.analytics.time_windows[i] {
                app.set_analytics_window(i, window);
            }
            ui.separator();
            let mode = &mut app.analytics.view_modes[i];
            for m in [ViewMode::Table, ViewMode::Chart] {
                ui.selectable_value(mode, m, m.label());
            }
        });
        ui.add_space(4.0);

        let Some(ref views) = app.analytics.cached_views else {
            placeholder(ui, "Collecting data…");
            return;
        };
        let view = &views[i];
        let id = format!("analytics_{i}");
        match (i, app.analytics.view_modes[i]) {
            (0, ViewMode::Table) => fee_table(ui, &id, view),
            (0, ViewMode::Chart) => {
                time_chart(ui, &id, "Avg fee", &view.fee_over_time, theme::ACCENT)
            }
            (1, ViewMode::Table) => tx_table(ui, &id, view),
            (1, ViewMode::Chart) => {
                time_chart(ui, &id, "Transactions", &view.tx_over_time, theme::INFO)
            }
            (2, ViewMode::Table) => protocol_table(ui, &id, view),
            (2, ViewMode::Chart) => protocol_chart(ui, &id, view),
            (3, ViewMode::Table) => address_table(ui, &id, &view.top_senders, "sender"),
            (3, ViewMode::Chart) => address_chart(ui, &id, &view.top_senders, "sender"),
            (4, ViewMode::Table) => address_table(ui, &id, &view.top_receivers, "receiver"),
            (4, ViewMode::Chart) => address_chart(ui, &id, &view.top_receivers, "receiver"),
            _ => {}
        }
    });
}

// ── Tables ──

fn fee_table(ui: &mut Ui, id: &str, view: &AggregatedView) {
    let or_dash = |v: u64| {
        if view.fee_count > 0 {
            v.to_string()
        } else {
            "—".to_string()
        }
    };
    kv_grid(ui, id, |ui| {
        kv(ui, "Avg Fee (mass)", format!("{:.2}", view.avg_fee));
        kv(ui, "Total Fees", format_number(view.total_fees));
        kv(ui, "Min Fee (mass)", or_dash(view.min_fee));
        kv(ui, "Max Fee (mass)", or_dash(view.max_fee));
        kv(ui, "Tx w/ Fee Data", format_number(view.fee_count as u64));
    });
}

fn tx_table(ui: &mut Ui, id: &str, view: &AggregatedView) {
    kv_grid(ui, id, |ui| {
        kv(
            ui,
            "Time Periods",
            format_number(view.blocks_analyzed as u64),
        );
        kv(
            ui,
            "Total Transactions",
            format_number(view.tx_count as u64),
        );
        kv(ui, "Unique Senders", view.top_senders.len().to_string());
        kv(ui, "Unique Receivers", view.top_receivers.len().to_string());
    });
}

fn protocol_table(ui: &mut Ui, id: &str, view: &AggregatedView) {
    if view.protocol_counts.is_empty() {
        placeholder(ui, "No protocol activity yet");
        return;
    }
    count_grid(
        ui,
        id,
        "Protocol",
        view.protocol_counts.iter().map(|(p, c)| (p.label(), *c)),
    );
}

fn address_table(ui: &mut Ui, id: &str, entries: &[(String, usize)], kind: &str) {
    if entries.is_empty() {
        placeholder(ui, &format!("No {kind} data yet"));
        return;
    }
    egui::ScrollArea::vertical()
        .id_salt(id)
        .max_height(CHART_HEIGHT)
        .show(ui, |ui| {
            count_grid(
                ui,
                id,
                "Address",
                entries.iter().map(|(a, c)| (a.as_str(), *c)),
            );
        });
}

fn count_grid<'a>(
    ui: &mut Ui,
    id: &str,
    name_header: &str,
    rows: impl Iterator<Item = (&'a str, usize)>,
) {
    egui::Grid::new(id)
        .num_columns(2)
        .striped(true)
        .spacing([24.0, 4.0])
        .show(ui, |ui| {
            column_header(ui, name_header);
            column_header(ui, "Txs");
            ui.end_row();
            for (name, count) in rows {
                ui.label(name);
                ui.label(format_number(count as u64));
                ui.end_row();
            }
        });
}

// ── Charts ──

/// Line chart of `(timestamp_ms, value)` points, with the x axis shown relative to now.
fn time_chart(ui: &mut Ui, id: &str, name: &str, data: &[(f64, f64)], color: Color32) {
    if data.is_empty() {
        placeholder(ui, "No data yet");
        return;
    }
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or_default();
    // x in minutes relative to now (negative = in the past)
    let points: PlotPoints = data
        .iter()
        .map(|(ts, y)| [(ts - now_ms) / 60_000.0, *y])
        .collect();

    Plot::new(id)
        .height(CHART_HEIGHT)
        .allow_zoom(false)
        .allow_drag(false)
        .allow_scroll(false)
        .x_axis_formatter(|mark: GridMark, _| relative_time_label(mark.value))
        .label_formatter(move |_, p| format!("{} ago\n{:.2}", relative_time_label(-p.x.abs()), p.y))
        .show(ui, |plot_ui| {
            plot_ui.line(Line::new(name, points).color(color).width(2.0_f32));
        });
}

fn relative_time_label(minutes: f64) -> String {
    let m = minutes.abs();
    if m >= 120.0 {
        format!("{:.0}h", m / 60.0)
    } else if m >= 1.0 {
        format!("{m:.0}m")
    } else {
        format!("{:.0}s", m * 60.0)
    }
}

fn protocol_chart(ui: &mut Ui, id: &str, view: &AggregatedView) {
    if view.protocol_counts.is_empty() {
        placeholder(ui, "No protocol activity yet");
        return;
    }
    let labels: Vec<String> = view
        .protocol_counts
        .iter()
        .map(|(p, _)| p.label().to_string())
        .collect();
    let bars = view
        .protocol_counts
        .iter()
        .enumerate()
        .map(|(i, (p, count))| {
            Bar::new(i as f64, *count as f64)
                .name(p.label())
                .fill(theme::SERIES[i % theme::SERIES.len()])
        })
        .collect();
    bar_chart(ui, id, bars, labels);
}

fn address_chart(ui: &mut Ui, id: &str, entries: &[(String, usize)], kind: &str) {
    if entries.is_empty() {
        placeholder(ui, &format!("No {kind} data yet"));
        return;
    }
    let top: Vec<_> = entries.iter().take(10).collect();
    // Full addresses don't fit under the bars: label by rank and list them below.
    let labels = (1..=top.len()).map(|rank| format!("#{rank}")).collect();
    let bars = top
        .iter()
        .enumerate()
        .map(|(i, (addr, count))| {
            Bar::new(i as f64, *count as f64)
                .name(addr)
                .fill(theme::ACCENT)
        })
        .collect();
    bar_chart(ui, id, bars, labels);

    ui.add_space(4.0);
    egui::Grid::new((id, "legend"))
        .num_columns(2)
        .spacing([12.0, 2.0])
        .show(ui, |ui| {
            for (rank, (addr, _)) in top.iter().enumerate() {
                ui.label(RichText::new(format!("#{}", rank + 1)).weak());
                ui.label(addr.as_str());
                ui.end_row();
            }
        });
}

fn bar_chart(ui: &mut Ui, id: &str, bars: Vec<Bar>, labels: Vec<String>) {
    Plot::new(id)
        .height(CHART_HEIGHT)
        .allow_zoom(false)
        .allow_drag(false)
        .allow_scroll(false)
        .show_grid([false, true])
        .x_axis_formatter(move |mark: GridMark, _| {
            let i = mark.value.round();
            if (mark.value - i).abs() > f64::EPSILON || i < 0.0 {
                return String::new();
            }
            labels.get(i as usize).cloned().unwrap_or_default()
        })
        .show(ui, |plot_ui| {
            plot_ui.bar_chart(BarChart::new("counts", bars).width(0.7));
        });
}

// ── Mining ──

fn mining_analysis(ui: &mut Ui, app: &App) {
    let Some(ref mining) = app.node.mining_info else {
        placeholder(ui, direct_node_placeholder(app, "Collecting mining data…"));
        return;
    };
    kv_grid(ui, "mining_info", |ui| {
        kv(
            ui,
            "Hashrate",
            RichText::new(format_hashrate(mining.hashrate)).color(theme::ACCENT_BRIGHT),
        );
        kv(
            ui,
            "Unique Miners",
            format!(
                "{} (last {} blocks)",
                mining.unique_miners, mining.blocks_analyzed
            ),
        );
    });
    if !mining.top_miners.is_empty() {
        ui.add_space(6.0);
        field_label(ui, "Top Miners");
        kv_grid(ui, "top_miners", |ui| {
            for (addr, count) in &mining.top_miners {
                ui.label(addr);
                ui.label(format!("{count} blocks"));
                ui.end_row();
            }
        });
    }
}
