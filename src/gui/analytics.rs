use std::time::{SystemTime, UNIX_EPOCH};

use eframe::egui::{self, RichText, Ui};
use egui_extras::{Column, TableBuilder};
use egui_plot::{Bar, BarChart, GridMark, Plot};

use super::theme;
use super::widgets::{
    CARD_GAP, address, card, card_with_header, column_header, direct_node_placeholder, fit_label,
    kv, kv_grid, placeholder, section_title,
};
use crate::analytics::AggregatedView;
use crate::app::{AnalyticsPanel, AnalyticsPhase, App, PanelState, TimeWindow, ViewMode};
use crate::format::{format_duration, format_hashrate, format_kas};
use crate::rpc::types::format_number;
use crate::tx_inspect::TransactionProtocol;

const CHART_HEIGHT: f32 = 200.0;
/// Charts are temporarily hidden: no Table/Chart toggle, every panel shows its table.
/// Set to true to bring them back.
const CHARTS_ENABLED: bool = false;

pub fn show(ui: &mut Ui, app: &mut App) {
    if !app.has_direct_node {
        placeholder(ui, direct_node_placeholder(app, ""));
        return;
    }

    banners(ui, app);

    egui::ScrollArea::vertical().show(ui, |ui| {
        ui.columns(2, |cols| {
            panel_card(
                &mut cols[0],
                app,
                "Transaction Summary",
                AnalyticsPanel::TxSummary,
                tx_summary,
            );
            card(&mut cols[1], "Fees", |ui| fees(ui, app));
        });
        ui.add_space(CARD_GAP);
        panel_card(
            ui,
            app,
            "Transaction Inspection",
            AnalyticsPanel::Inspection,
            inspection,
        );
        ui.add_space(CARD_GAP);
        ui.columns(2, |cols| {
            panel_card(
                &mut cols[0],
                app,
                "Mining Share by Node Version",
                AnalyticsPanel::NodeVersions,
                node_versions,
            );
            card(&mut cols[1], "Mining Analysis", |ui| {
                mining_analysis(ui, app)
            });
        });
        ui.add_space(CARD_GAP);
        ui.columns(2, |cols| {
            panel_card(
                &mut cols[0],
                app,
                "Top Senders",
                AnalyticsPanel::TopSenders,
                |ui, app| addresses(ui, app, AnalyticsPanel::TopSenders),
            );
            panel_card(
                &mut cols[1],
                app,
                "Top Receivers",
                AnalyticsPanel::TopReceivers,
                |ui, app| addresses(ui, app, AnalyticsPanel::TopReceivers),
            );
        });
    });
}

/// A card for an analytics panel, with its time window dropdown after the title.
fn panel_card(
    ui: &mut Ui,
    app: &mut App,
    title: &str,
    panel: AnalyticsPanel,
    add_contents: impl FnOnce(&mut Ui, &mut App),
) {
    card_with_header(
        ui,
        title,
        app,
        |ui, app| {
            let window = &mut app.analytics.panel(panel).window;
            egui::ComboBox::from_id_salt(("time_window", format!("{panel:?}")))
                .selected_text(window.label())
                .width(0.0)
                .show_ui(ui, |ui| {
                    for w in TimeWindow::ALL {
                        ui.selectable_value(window, w, w.label());
                    }
                });
        },
        add_contents,
    );
}

fn banners(ui: &mut Ui, app: &mut App) {
    let status = &app.analytics.status;
    let tip = app.node.server_info.as_ref().map(|s| s.virtual_daa_score);
    if status.phase == AnalyticsPhase::CatchingUp
        && let Some(tip) = tip
    {
        let fraction = status.fraction(tip).unwrap_or(0.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Syncing analytics…").color(theme::WARN));
            ui.add(
                egui::ProgressBar::new(fraction)
                    .desired_width(300.0)
                    .text(format!(
                        "DAA {}/{} ({:.1}%)",
                        format_number(status.current_daa.unwrap_or(0)),
                        format_number(tip),
                        fraction * 100.0
                    )),
            );
            if let Some(eta) = status.eta(tip) {
                ui.label(RichText::new(format!("~{} left", format_duration(eta))).weak());
            }
        });
        ui.add_space(2.0);
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
        ui.add_space(2.0);
    }
    if dismiss {
        app.analytics.reorg_notification = None;
    }
}

/// The panel's state, after the Table/Chart toggle if `chart` (the time window is in the
/// card header, see [`panel_card`]).
fn controls(ui: &mut Ui, app: &mut App, panel: AnalyticsPanel, chart: bool) -> PanelState {
    let state = app.analytics.panel(panel);
    if chart && CHARTS_ENABLED {
        ui.horizontal(|ui| {
            for m in [ViewMode::Table, ViewMode::Chart] {
                ui.selectable_value(&mut state.mode, m, m.label());
            }
        });
        ui.add_space(2.0);
    }
    if !CHARTS_ENABLED {
        state.mode = ViewMode::Table;
    }
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
    ui.add_space(4.0);
    section_title(ui, "Output Script Classes");
    let c = &t.script_classes;
    kv_grid(ui, "script_classes", |ui| {
        kv(ui, "P2PK", count(c.pubkey));
        kv(ui, "P2PK ECDSA", count(c.pubkey_ecdsa));
        kv(ui, "P2SH", count(c.script_hash));
        kv(ui, "Non-standard", count(c.nonstandard));
    });
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

    ui.add_space(4.0);
    if app.analytics.cached_views.is_none() {
        placeholder(ui, "Collecting data…");
        return;
    }
    let rows = TimeWindow::ALL
        .into_iter()
        .filter_map(|w| {
            let view = app.analytics.view(w)?;
            let t = &view.totals;
            Some([
                format!("Prior {}", w.label()),
                view.avg_fee().map_or("—".into(), |f| format_kas(f, 6)),
                if t.fee_tx_count > 0 {
                    format_kas(t.total_fees as f64, 3)
                } else {
                    "—".into()
                },
                count(t.fee_tx_count),
            ])
        })
        .collect();
    wide_table(
        ui,
        "fee_windows",
        ["Accepted Fees (KAS)", "Average", "Total", "Txs"],
        rows,
        |ui, w| {
            ui.label(RichText::new(w).weak());
        },
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
        let rows = view
            .node_versions
            .iter()
            .map(|(v, n)| [name(v), count(*n), format!("{:.2}%", share(*n))])
            .collect();
        wide_table(
            ui,
            "node_versions",
            ["Version", "Blocks", "Share"],
            rows,
            |ui, v| {
                fit_label(ui, v);
            },
        );
    }
    ui.add_space(2.0);
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
    let rows = entries
        .iter()
        .map(|(addr, n)| [addr.clone(), count(*n)])
        .collect();
    wide_table(ui, id, ["Address", "Txs"], rows, address);
}

/// Full-width table: the first column takes the remaining width and is drawn by
/// `first_cell`, which fits it to that width (e.g. [`fit_label`], [`address`]). The other
/// columns are right-aligned, so values sit against the right edge of the card.
fn wide_table<const N: usize>(
    ui: &mut Ui,
    id: &str,
    headers: [&str; N],
    rows: Vec<[String; N]>,
    first_cell: fn(&mut Ui, &str),
) {
    ui.push_id(id, |ui| {
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
        // Room for the padding of clickable cells such as [`address`].
        let row_height = theme::ROW_HEIGHT + 2.0 * ui.spacing().button_padding.y;
        // Exact widths, recomputed every frame: a `Column::remainder` never shrinks below
        // what its content used last frame, so a fitted first column would only ever grow
        // and never shorten its values when the window narrows. All text is monospace, so
        // the other columns are sized by their longest cell.
        let font = egui::TextStyle::Body.resolve(ui.style());
        let glyph = ui.fonts_mut(|f| f.glyph_width(&font, '0'));
        let spacing = ui.spacing().item_spacing.x;
        let widths: Vec<f32> = (1..N)
            .map(|i| {
                let chars = rows
                    .iter()
                    .map(|r| r[i].chars().count())
                    .chain([headers[i].chars().count()])
                    .max()
                    .unwrap_or(0);
                (chars as f32 * glyph).max(60.0)
            })
            .collect();
        let rest: f32 = widths.iter().map(|w| w + spacing).sum();
        let first = (ui.available_width() - rest).max(0.0);
        let mut table = TableBuilder::new(ui)
            .striped(true)
            .max_scroll_height(CHART_HEIGHT)
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::exact(first));
        for w in widths {
            table = table.column(Column::exact(w));
        }
        table
            .header(theme::ROW_HEIGHT, |mut header| {
                for (i, h) in headers.into_iter().enumerate() {
                    header.col(|ui| {
                        right_after_first(ui, i, |ui| column_header(ui, h));
                    });
                }
            })
            .body(|body| {
                body.rows(row_height, rows.len(), |mut row| {
                    for (i, cell) in rows[row.index()].iter().enumerate() {
                        row.col(|ui| {
                            if i == 0 {
                                first_cell(ui, cell);
                            } else {
                                right_after_first(ui, i, |ui| {
                                    ui.label(cell);
                                });
                            }
                        });
                    }
                });
            });
    });
}

/// Cells after the first are pinned to the right edge of their column, so values line up
/// with the right edge of the card.
fn right_after_first(ui: &mut Ui, column: usize, add: impl FnOnce(&mut Ui)) {
    if column == 0 {
        add(ui);
    } else {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), add);
    }
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

    ui.add_space(2.0);
    // Rows rather than a grid so each address is fitted to the card width.
    for (rank, (addr, _)) in top.iter().enumerate() {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            ui.label(RichText::new(format!("#{:<2}", rank + 1)).weak());
            address(ui, addr);
        });
    }
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
        ui.add_space(4.0);
        let rows = mining
            .top_miners
            .iter()
            .map(|(addr, n)| [addr.clone(), count(*n as u64)])
            .collect();
        wide_table(ui, "top_miners", ["Top Miners", "Blocks"], rows, address);
    }
}
