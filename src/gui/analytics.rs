use std::time::{SystemTime, UNIX_EPOCH};

use eframe::egui::{self, RichText, Ui};
use egui_plot::{Bar, BarChart, GridMark, Plot};

use super::theme;
use super::widgets::{
    CARD_GAP, card, column_header, direct_node_placeholder, field_label, kv, kv_grid, placeholder,
    section_title, syncing_guard,
};
use crate::analytics::AggregatedView;
use crate::app::{AnalyticsPanel, App, PanelState, TimeWindow, ViewMode};
use crate::format::{format_hashrate, format_kas};
use crate::rpc::types::format_number;
use crate::tx_inspect::TransactionProtocol;

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

    egui::ScrollArea::vertical().show(ui, |ui| {
        ui.columns(2, |cols| {
            card(&mut cols[0], "Transaction Summary", |ui| {
                tx_summary(ui, app)
            });
            card(&mut cols[1], "Fees", |ui| fees(ui, app));
        });
        ui.add_space(CARD_GAP);
        card(ui, "Transaction Inspection", |ui| inspection(ui, app));
        ui.add_space(CARD_GAP);
        ui.columns(2, |cols| {
            card(&mut cols[0], "Mining Share by Node Version", |ui| {
                node_versions(ui, app)
            });
            card(&mut cols[1], "Mining Analysis", |ui| {
                mining_analysis(ui, app)
            });
        });
        ui.add_space(CARD_GAP);
        ui.columns(2, |cols| {
            card(&mut cols[0], "Top Senders", |ui| {
                addresses(ui, app, AnalyticsPanel::TopSenders)
            });
            card(&mut cols[1], "Top Receivers", |ui| {
                addresses(ui, app, AnalyticsPanel::TopReceivers)
            });
        });
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

/// Window selector (and Table/Chart toggle if `chart`) for a panel; returns its state.
fn controls(ui: &mut Ui, app: &mut App, panel: AnalyticsPanel, chart: bool) -> PanelState {
    let state = app.analytics.panel(panel);
    ui.horizontal(|ui| {
        for w in TimeWindow::ALL {
            ui.selectable_value(&mut state.window, w, w.label());
        }
        if chart {
            ui.separator();
            for m in [ViewMode::Table, ViewMode::Chart] {
                ui.selectable_value(&mut state.mode, m, m.label());
            }
        }
    });
    ui.add_space(4.0);
    *state
}

/// The view for `window`, or a placeholder if analytics hasn't produced one yet.
fn view_or_placeholder<'a>(
    ui: &mut Ui,
    app: &'a App,
    window: TimeWindow,
) -> Option<&'a AggregatedView> {
    let view = app.analytics.view(window);
    if view.is_none() {
        placeholder(ui, "Collecting data…");
    }
    view
}

fn count(n: u64) -> String {
    format_number(n)
}

// ── Transaction Summary ──

fn tx_summary(ui: &mut Ui, app: &mut App) {
    let state = controls(ui, app, AnalyticsPanel::TxSummary, true);
    let Some(view) = view_or_placeholder(ui, app, state.window) else {
        return;
    };
    if state.mode == ViewMode::Chart {
        tx_chart(ui, view);
        return;
    }

    let t = &view.totals;
    section_title(ui, "Unique Transactions");
    kv_grid(ui, "tx_summary", |ui| {
        kv(ui, "Tx Count", count(t.tx_count));
        kv(
            ui,
            "TPS",
            view.tps().map_or("—".into(), |tps| format!("{tps:.2}")),
        );
        kv(ui, "Chain Blocks", count(t.chain_blocks));
    });
    ui.add_space(6.0);
    section_title(ui, "Output Script Classes");
    let c = &t.script_classes;
    kv_grid(ui, "script_classes", |ui| {
        kv(ui, "P2PK", count(c.pubkey));
        kv(ui, "P2PK ECDSA", count(c.pubkey_ecdsa));
        kv(ui, "P2SH", count(c.script_hash));
        kv(ui, "Non-standard", count(c.nonstandard));
    });
    ui.add_space(4.0);
    ui.label(
        RichText::new("Excludes coinbase. Script classes count outputs.")
            .weak()
            .small(),
    );
}

/// Bars of transactions per bin, x in minutes relative to now.
fn tx_chart(ui: &mut Ui, view: &AggregatedView) {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or_default();
    let bin_min = view.series_bin_ms as f64 / 60_000.0;
    let last = view.tx_series.len().saturating_sub(1);
    let bars = view
        .tx_series
        .iter()
        .enumerate()
        .map(|(i, &(start, n))| {
            let center = (start as f64 - now_ms) / 60_000.0 + bin_min / 2.0;
            // The current bin is still filling
            let fill = if i == last {
                theme::ACCENT_DIM
            } else {
                theme::ACCENT
            };
            Bar::new(center, n as f64).width(bin_min * 0.8).fill(fill)
        })
        .collect();

    Plot::new("tx_series")
        .height(CHART_HEIGHT)
        .allow_zoom(false)
        .allow_drag(false)
        .allow_scroll(false)
        .show_grid([false, true])
        .x_axis_formatter(|mark: GridMark, _| relative_time_label(mark.value))
        .show(ui, |plot_ui| {
            plot_ui.bar_chart(
                BarChart::new("Transactions", bars).element_formatter(Box::new(|bar, _| {
                    format!(
                        "{} ago\n{} txs",
                        relative_time_label(bar.argument),
                        format_number(bar.value as u64)
                    )
                })),
            );
        });
    let per = match view.window {
        TimeWindow::OneMin => "5 seconds",
        TimeWindow::OneHour => "minute",
        TimeWindow::TwentyFourHour => "hour",
    };
    ui.label(
        RichText::new(format!(
            "Transactions per {per}. The last bar is still filling."
        ))
        .weak()
        .small(),
    );
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

// ── Fees ──

fn fees(ui: &mut Ui, app: &App) {
    section_title(ui, "Fee Rates (sompi/gram)");
    match app.node.fee_estimate {
        Some(ref fee) => {
            let rate = |r: Option<f64>| r.map_or("—".into(), |r| format!("{r:.2}"));
            kv_grid(ui, "fee_rates", |ui| {
                kv(ui, "Low", rate(fee.low_feerate));
                kv(ui, "Normal", rate(fee.normal_feerate));
                kv(ui, "Priority", rate(Some(fee.priority_feerate)));
            });
        }
        None => placeholder(ui, "Waiting for fee estimate…"),
    }

    ui.add_space(6.0);
    section_title(ui, "Accepted Fees (KAS)");
    if app.analytics.cached_views.is_none() {
        placeholder(ui, "Collecting data…");
        return;
    }
    egui::Grid::new("fee_windows")
        .num_columns(4)
        .striped(true)
        .spacing([20.0, 4.0])
        .show(ui, |ui| {
            for header in ["Prior", "Average", "Total", "Txs"] {
                column_header(ui, header);
            }
            ui.end_row();
            for w in TimeWindow::ALL {
                let Some(view) = app.analytics.view(w) else {
                    continue;
                };
                let t = &view.totals;
                ui.label(RichText::new(w.label()).weak());
                ui.label(view.avg_fee().map_or("—".into(), |f| format_kas(f, 6)));
                ui.label(if t.fee_tx_count > 0 {
                    format_kas(t.total_fees as f64, 3)
                } else {
                    "—".into()
                });
                ui.label(count(t.fee_tx_count));
                ui.end_row();
            }
        });
    ui.add_space(4.0);
    ui.label(
        RichText::new("Fee = inputs − outputs. Average is per transaction.")
            .weak()
            .small(),
    );
}

// ── Transaction Inspection ──

fn inspection(ui: &mut Ui, app: &mut App) {
    let state = controls(ui, app, AnalyticsPanel::Inspection, false);
    let Some(view) = view_or_placeholder(ui, app, state.window) else {
        return;
    };
    let i = &view.totals.inspection;
    ui.columns(3, |cols| {
        section_title(&mut cols[0], "Opcodes");
        kv_grid(&mut cols[0], "inspect_opcodes", |ui| {
            kv(ui, "Introspection Txs", count(i.introspection_txs));
            kv(ui, "OpZkPrecompile Txs", count(i.zk_precompile_txs));
            kv(ui, "└ Groth16", count(i.zk_groth16_txs));
            kv(ui, "└ R0Succinct", count(i.zk_r0succinct_txs));
            kv(
                ui,
                "OpChainblockSeqCommit Txs",
                count(i.chainblock_seqcommit_txs),
            );
        });

        section_title(&mut cols[1], "Covenants");
        kv_grid(&mut cols[1], "inspect_covenants", |ui| {
            kv(ui, "Covenant-Creating Txs", count(i.covenant_creating_txs));
            kv(ui, "Outputs Created", count(i.covenant_outputs_created));
            kv(ui, "Outputs Spent", count(i.covenant_outputs_spent));
        });

        section_title(&mut cols[2], "Protocols");
        kv_grid(&mut cols[2], "inspect_protocols", |ui| {
            for p in TransactionProtocol::ALL {
                kv(ui, p.label(), count(view.protocol_count(p)));
            }
        });
    });
}

// ── Mining Share by Node Version ──

fn node_versions(ui: &mut Ui, app: &mut App) {
    let state = controls(ui, app, AnalyticsPanel::NodeVersions, true);
    let Some(view) = view_or_placeholder(ui, app, state.window) else {
        return;
    };
    let total = view.node_version_total();
    if total == 0 {
        placeholder(ui, "No coinbase data yet");
        return;
    }
    let share = |n: u64| n as f64 / total as f64 * 100.0;
    let name = |v: &str| if v.is_empty() { "Unknown" } else { v }.to_string();

    if state.mode == ViewMode::Chart {
        let labels = view.node_versions.iter().map(|(v, _)| name(v)).collect();
        let bars = view
            .node_versions
            .iter()
            .enumerate()
            .map(|(i, (v, n))| {
                Bar::new(i as f64, share(*n))
                    .name(name(v))
                    .fill(theme::SERIES[i % theme::SERIES.len()])
            })
            .collect();
        bar_chart(ui, "node_versions_chart", bars, labels, "%");
    } else {
        egui::ScrollArea::vertical()
            .id_salt("node_versions")
            .max_height(CHART_HEIGHT)
            .show(ui, |ui| {
                egui::Grid::new("node_versions")
                    .num_columns(3)
                    .striped(true)
                    .spacing([24.0, 4.0])
                    .show(ui, |ui| {
                        for header in ["Version", "Blocks", "Share"] {
                            column_header(ui, header);
                        }
                        ui.end_row();
                        for (v, n) in &view.node_versions {
                            ui.label(name(v));
                            ui.label(count(*n));
                            ui.label(format!("{:.2}%", share(*n)));
                            ui.end_row();
                        }
                    });
            });
    }
    ui.add_space(4.0);
    ui.label(
        RichText::new(format!(
            "From {} accepted coinbase transactions.",
            count(total)
        ))
        .weak()
        .small(),
    );
}

// ── Top Senders / Receivers ──

fn addresses(ui: &mut Ui, app: &mut App, panel: AnalyticsPanel) {
    let state = controls(ui, app, panel, true);
    let Some(view) = view_or_placeholder(ui, app, state.window) else {
        return;
    };
    let (entries, kind) = match panel {
        AnalyticsPanel::TopSenders => (&view.top_senders, "sender"),
        _ => (&view.top_receivers, "receiver"),
    };
    if entries.is_empty() {
        placeholder(ui, &format!("No {kind} data yet"));
        return;
    }
    let id = format!("{kind}s");
    match state.mode {
        ViewMode::Table => address_table(ui, &id, entries),
        ViewMode::Chart => address_chart(ui, &id, entries),
    }
}

fn address_table(ui: &mut Ui, id: &str, entries: &[(String, u64)]) {
    egui::ScrollArea::vertical()
        .id_salt(id)
        .max_height(CHART_HEIGHT)
        .show(ui, |ui| {
            egui::Grid::new(id)
                .num_columns(2)
                .striped(true)
                .spacing([24.0, 4.0])
                .show(ui, |ui| {
                    column_header(ui, "Address");
                    column_header(ui, "Txs");
                    ui.end_row();
                    for (addr, n) in entries {
                        ui.label(addr);
                        ui.label(count(*n));
                        ui.end_row();
                    }
                });
        });
}

fn address_chart(ui: &mut Ui, id: &str, entries: &[(String, u64)]) {
    let top: Vec<_> = entries.iter().take(10).collect();
    // Full addresses don't fit under the bars: label by rank and list them below.
    let labels = (1..=top.len()).map(|rank| format!("#{rank}")).collect();
    let bars = top
        .iter()
        .enumerate()
        .map(|(i, (addr, n))| Bar::new(i as f64, *n as f64).name(addr).fill(theme::ACCENT))
        .collect();
    bar_chart(ui, id, bars, labels, "");

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

/// Bars at x = 0, 1, … labelled with `labels`; hover shows name and value + `unit`.
fn bar_chart(ui: &mut Ui, id: &str, bars: Vec<Bar>, labels: Vec<String>, unit: &'static str) {
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
            plot_ui.bar_chart(BarChart::new("counts", bars).width(0.7).element_formatter(
                Box::new(move |bar, _| {
                    if unit == "%" {
                        format!("{}\n{:.2}%", bar.name, bar.value)
                    } else {
                        format!("{}\n{}", bar.name, format_number(bar.value as u64))
                    }
                }),
            ));
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
