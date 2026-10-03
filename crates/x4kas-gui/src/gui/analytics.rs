use eframe::egui::{self, RichText, Ui};
use egui_extras::{Column, TableBuilder};

use super::theme;
use super::widgets::{
    CARD_GAP, address, card, card_with_header, direct_node_placeholder, fit_label, kv, kv_grid,
    or_dash, placeholder, section_title,
};
use x4kas_core::analytics::AggregatedView;
use x4kas_core::app::{AnalyticsPanel, AnalyticsPhase, App, TimeWindow};
use x4kas_core::format::{format_hashrate, format_kas, format_number};
use x4kas_core::tx_inspect::TransactionProtocol;

/// Tables taller than this scroll.
const TABLE_MAX_HEIGHT: f32 = 200.0;

pub fn show(ui: &mut Ui, app: &mut App) {
    if !app.connection.is_direct() {
        placeholder(ui, direct_node_placeholder(app, ""));
        return;
    }

    // While catching up, grey out the tab under a centered progress overlay.
    let syncing = sync_fraction(app);
    let rect = ui.max_rect();
    ui.add_enabled_ui(syncing.is_none(), |ui| {
        banners(ui, app);
        tab_contents(ui, app);
    });
    if let Some(fraction) = syncing {
        sync_overlay(ui, rect, fraction);
    }
}

fn tab_contents(ui: &mut Ui, app: &mut App) {
    let hashrate = app.node.hashrate;

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
            panel_card(
                &mut cols[1],
                app,
                "Mining Analysis",
                AnalyticsPanel::Miners,
                |ui, view| mining_analysis(ui, hashrate, view),
            );
        });
        ui.add_space(CARD_GAP);
        ui.columns(2, |cols| {
            panel_card(
                &mut cols[0],
                app,
                "Top Senders",
                AnalyticsPanel::TopSenders,
                |ui, view| addresses(ui, &view.top_senders, "sender"),
            );
            panel_card(
                &mut cols[1],
                app,
                "Top Receivers",
                AnalyticsPanel::TopReceivers,
                |ui, view| addresses(ui, &view.top_receivers, "receiver"),
            );
        });
    });
}

/// Catch-up progress in `0.0..=1.0` while analytics is syncing to the tip.
fn sync_fraction(app: &App) -> Option<f32> {
    let status = &app.analytics.status;
    let tip = app.node.server_info.as_ref()?.virtual_daa_score;
    (status.phase == AnalyticsPhase::CatchingUp).then(|| status.fraction(tip).unwrap_or(0.0))
}

/// Covers `rect` with a near-opaque wash and shows "Analytics Syncing (n%)" in its middle.
fn sync_overlay(ui: &Ui, rect: egui::Rect, fraction: f32) {
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, theme::BG.gamma_multiply(0.85));
    let galley = painter.layout_no_wrap(
        format!("Analytics Syncing ({:.1}%)", fraction * 100.0),
        egui::FontId::monospace(theme::FONT_SIZE),
        theme::WARN,
    );
    let text = egui::Align2::CENTER_CENTER.anchor_size(rect.center(), galley.size());
    let frame = text.expand2(egui::vec2(16.0, 10.0));
    painter.rect(
        frame,
        3.0,
        theme::SURFACE,
        egui::Stroke::new(1.0_f32, theme::BORDER_HI),
        egui::StrokeKind::Inside,
    );
    painter.galley(text.min, galley, theme::WARN);
}

/// A card for an analytics panel, with its time window dropdown after the title. The
/// contents get the view for that window, or a placeholder shows until there is one.
fn panel_card(
    ui: &mut Ui,
    app: &mut App,
    title: &str,
    panel: AnalyticsPanel,
    add_contents: impl FnOnce(&mut Ui, &AggregatedView),
) {
    card_with_header(
        ui,
        title,
        app,
        |ui, app| {
            let window = app.analytics.window_mut(panel);
            egui::ComboBox::from_id_salt(("time_window", format!("{panel:?}")))
                .selected_text(window.label())
                .width(0.0)
                .show_ui(ui, |ui| {
                    for w in TimeWindow::ALL {
                        ui.selectable_value(window, w, w.label());
                    }
                });
        },
        |ui, app| match app.analytics.view(app.analytics.window(panel)) {
            Some(view) => add_contents(ui, view),
            None => placeholder(ui, "Collecting data…"),
        },
    );
}

fn banners(ui: &mut Ui, app: &mut App) {
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

// ── Transaction Summary ──

fn tx_summary(ui: &mut Ui, view: &AggregatedView) {
    let t = &view.totals;
    section_title(ui, "Unique Transactions");
    kv_grid(ui, "tx_summary", |ui| {
        kv(ui, "Tx Count", format_number(t.tx_count));
        kv(ui, "TPS", or_dash(view.tps(), |tps| format!("{tps:.2}")));
        kv(ui, "Chain Blocks", format_number(t.chain_blocks));
    });
    ui.add_space(4.0);
    section_title(ui, "Output Script Classes");
    let c = &t.script_classes;
    kv_grid(ui, "script_classes", |ui| {
        kv(ui, "P2PK", format_number(c.pubkey));
        kv(ui, "P2PK ECDSA", format_number(c.pubkey_ecdsa));
        kv(ui, "P2SH", format_number(c.script_hash));
        kv(ui, "Non-standard", format_number(c.nonstandard));
    });
}

// ── Fees ──

fn fees(ui: &mut Ui, app: &App) {
    section_title(ui, "Fee Rates (sompi/gram)");
    match app.node.fee_estimate {
        Some(ref fee) => {
            let rate = |r: Option<f64>| or_dash(r, |r| format!("{r:.2}"));
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
                or_dash(view.avg_fee(), |f| format_kas(f, 6)),
                or_dash((t.fee_tx_count > 0).then_some(t.total_fees), |f| {
                    format_kas(f as f64, 3)
                }),
                format_number(t.fee_tx_count),
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

fn inspection(ui: &mut Ui, view: &AggregatedView) {
    let i = &view.totals.inspection;
    ui.columns(3, |cols| {
        section_title(&mut cols[0], "Opcodes");
        kv_grid(&mut cols[0], "inspect_opcodes", |ui| {
            kv(ui, "Introspection Txs", format_number(i.introspection_txs));
            kv(ui, "OpZkPrecompile Txs", format_number(i.zk_precompile_txs));
            kv(ui, "└ Groth16", format_number(i.zk_groth16_txs));
            kv(ui, "└ R0Succinct", format_number(i.zk_r0succinct_txs));
            kv(
                ui,
                "OpChainblockSeqCommit Txs",
                format_number(i.chainblock_seqcommit_txs),
            );
        });

        section_title(&mut cols[1], "Covenants");
        kv_grid(&mut cols[1], "inspect_covenants", |ui| {
            kv(
                ui,
                "Covenant-Creating Txs",
                format_number(i.covenant_creating_txs),
            );
            kv(
                ui,
                "Outputs Created",
                format_number(i.covenant_outputs_created),
            );
            kv(ui, "Outputs Spent", format_number(i.covenant_outputs_spent));
        });

        section_title(&mut cols[2], "Protocols");
        kv_grid(&mut cols[2], "inspect_protocols", |ui| {
            for p in TransactionProtocol::ALL {
                kv(ui, p.label(), format_number(view.protocol_count(p)));
            }
        });
    });
}

// ── Mining Share by Node Version ──

fn node_versions(ui: &mut Ui, view: &AggregatedView) {
    let total = view.node_version_total();
    if total == 0 {
        placeholder(ui, "No coinbase data yet");
        return;
    }
    let share = |n: u64| n as f64 / total as f64 * 100.0;
    let name = |v: &str| if v.is_empty() { "Unknown" } else { v }.to_string();

    let rows = view
        .node_versions
        .iter()
        .map(|(v, n)| [name(v), format_number(*n), format!("{:.2}%", share(*n))])
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
    ui.add_space(2.0);
    ui.label(
        RichText::new(format!(
            "From {} accepted coinbase transactions.",
            format_number(total)
        ))
        .weak()
        .small(),
    );
}

// ── Top Senders / Receivers ──

fn addresses(ui: &mut Ui, entries: &[(String, u64)], kind: &str) {
    if entries.is_empty() {
        placeholder(ui, &format!("No {kind} data yet"));
        return;
    }
    let rows = entries
        .iter()
        .map(|(addr, n)| [addr.clone(), format_number(*n)])
        .collect();
    wide_table(ui, &format!("{kind}s"), ["Address", "Txs"], rows, address);
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
        // Room for clickable cells such as [`address`]: a selectable label is at least
        // `interact_size.y` tall and grows by `expansion` on hover, and the next row paints
        // over anything that spills past this one.
        let row_height =
            ui.spacing().interact_size.y + 2.0 * ui.visuals().widgets.hovered.expansion;
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
            .max_scroll_height(TABLE_MAX_HEIGHT)
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::exact(first));
        for w in widths {
            table = table.column(Column::exact(w));
        }
        table
            .header(theme::ROW_HEIGHT, |mut header| {
                for (i, h) in headers.into_iter().enumerate() {
                    header.col(|ui| {
                        right_after_first(ui, i, |ui| section_title(ui, h));
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

// ── Mining ──

fn mining_analysis(ui: &mut Ui, hashrate: Option<f64>, view: &AggregatedView) {
    let blocks = view.totals.mined_blocks;
    kv_grid(ui, "mining_info", |ui| {
        kv(
            ui,
            "Hashrate",
            RichText::new(or_dash(hashrate, format_hashrate)).color(theme::ACCENT_BRIGHT),
        );
        kv(
            ui,
            "Unique Miners",
            format_number(view.unique_miners as u64),
        );
        kv(ui, "Blocks Mined", format_number(blocks));
    });
    if view.top_miners.is_empty() {
        return;
    }
    ui.add_space(4.0);
    let share = |n: u64| n as f64 / blocks.max(1) as f64 * 100.0;
    let rows = view
        .top_miners
        .iter()
        .map(|(addr, n)| {
            [
                addr.clone(),
                format_number(*n),
                format!("{:.2}%", share(*n)),
            ]
        })
        .collect();
    wide_table(
        ui,
        "top_miners",
        ["Top Miners", "Blocks", "Share"],
        rows,
        address,
    );
}
