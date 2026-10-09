//! The pieces of an address page, shared by the info pane (opened from any address, see
//! `widgets::request_address`) and the Explorer tab: `header` (the address and its
//! label), `body` (the cards: summary with balance and indexed totals,
//! balance history, transactions, counterparties, cluster and peel chain) and
//! `watch_card` (the watchlist entry, for a watched address). The actions on an address
//! (label, watchlist, flow graph, export) are the action bar's (`gui/actions.rs`).

use eframe::egui::{self, RichText, Ui};
use egui_extras::Column;

use super::dialogs;
use super::theme;
use super::widgets::{
    CARD_GAP, address, block_hash, card, card_with_header, copy_value, kv, kv_columns, kv_grid,
    kv_with, link_table, or_dash, page_table, placeholder, subheader, table_header,
    table_row_height, transaction_id, weighted_columns,
};
use x4kas_core::app::{AddressView, App, ExportOrigin, ExportStatus};
use x4kas_core::controller::{CommandSender, UiCommand};
use x4kas_core::format::{format_duration, format_kas, format_number, now_ms};
use x4kas_core::index::query::TxRow;

/// Rows of the transactions table before it scrolls.
const TXS_HEIGHT: f32 = 260.0;
/// Height of the cluster members and peel chain lists before they scroll.
const LIST_HEIGHT: f32 = 120.0;

/// The address card's rows: the address and its label (the user's over a public one,
/// edited through the action bar's label dialog).
pub(super) fn header(ui: &mut Ui, app: &App, addr: &str) {
    kv_grid(ui, "address_header", |ui| {
        kv_with(ui, "Address", |ui| copy_value(ui, addr, "Copy address"));
        kv_with(ui, "Label", |ui| {
            // Right to left: the source's link and categories after the name.
            match app.labels.get(addr) {
                Some(known) => {
                    if let Some(link) = &known.link {
                        ui.hyperlink_to("↗", link).on_hover_text(link);
                    }
                    if !known.categories.is_empty() {
                        ui.label(RichText::new(known.categories.join(", ")).weak());
                    }
                    ui.label(
                        RichText::new(format!("{} ({})", known.name, known.source.label()))
                            .color(theme::ACCENT_BRIGHT),
                    );
                }
                None => {
                    ui.label(RichText::new("none (Add label above)").weak());
                }
            }
        });
    });
}

/// The "Watch" card of an address on the watchlist: whether alerts are on, the rules
/// and any pending activity, with the settings dialog in its header. An address that
/// isn't watched has no card: the action bar's "Add to watchlist" is the way in.
pub(super) fn watch_card(ui: &mut Ui, app: &App, addr: &str) {
    let Some(entry) = app.watch.entry(addr) else {
        return;
    };
    ui.add_space(CARD_GAP);
    card_with_header(
        ui,
        "Watch",
        &mut (),
        |ui, _| {
            if ui
                .small_button("Settings…")
                .on_hover_text("Alerts on or off, the rules, or remove it from the watchlist")
                .clicked()
            {
                dialogs::request_watch(ui.ctx(), addr);
            }
        },
        |ui, _| {
            let kas = |v: u64| format!("{} KAS", format_kas(v as f64, 8));
            let rules = &entry.rules;
            kv_grid(ui, "watch_card", |ui| {
                kv(
                    ui,
                    "Alerts",
                    if entry.enabled {
                        RichText::new("On").color(theme::OK)
                    } else {
                        RichText::new("Off").color(theme::TEXT_DIM)
                    },
                );
                let mut set: Vec<String> = Vec::new();
                if rules.any_activity {
                    set.push("any activity".to_string());
                }
                if let Some(v) = rules.received_min {
                    set.push(format!("received ≥ {}", kas(v)));
                }
                if let Some(v) = rules.sent_min {
                    set.push(format!("sent ≥ {}", kas(v)));
                }
                if let Some(v) = rules.balance_below {
                    set.push(format!("balance < {}", kas(v)));
                }
                if let Some(v) = rules.balance_above {
                    set.push(format!("balance > {}", kas(v)));
                }
                if let Some(h) = rules.idle_hours {
                    set.push(format!("active after ≥ {h}h idle"));
                }
                kv(
                    ui,
                    "Rules",
                    if set.is_empty() {
                        "none".to_string()
                    } else {
                        set.join(", ")
                    },
                );
                let pending = app.watch.pending.get(addr).copied().unwrap_or((0, 0));
                kv(
                    ui,
                    "Pending",
                    if pending == (0, 0) {
                        "—".to_string()
                    } else {
                        format!("+{} / −{}", kas(pending.0), kas(pending.1))
                    },
                );
            });
        },
    );
}

/// The cards below the header for a loaded address: summary, balance history,
/// transactions (the export's status under them) beside counterparties, cluster and,
/// when it is a link of one, the peel chain. The actions (export, flow graph) are in
/// the action bar (`gui/actions.rs`).
pub(super) fn body(
    ui: &mut Ui,
    app: &App,
    addr: &str,
    view: &AddressView,
    loading_more: bool,
    cmd_tx: &CommandSender,
) {
    card(ui, "Summary", |ui| {
        summary(ui, view);
        if let Some(note) = &view.index_note {
            ui.add_space(4.0);
            ui.label(RichText::new(note).color(theme::WARN));
        }
    });
    if view.index_note.is_some() {
        return;
    }
    ui.add_space(CARD_GAP);
    card(ui, "Balance history", |ui| balance_chart(ui, view));
    ui.add_space(CARD_GAP);
    weighted_columns(ui, [1.0, 1.0], 360.0, |[left, right]| {
        card(left, "Transactions", |ui| {
            transactions(ui, addr, view, loading_more, cmd_tx);
            if let Some(status) = app.address.export.of(&ExportOrigin::Address(addr.into())) {
                export_status(ui, status);
            }
        });
        card(right, "Top counterparties", |ui| counterparties(ui, view));
    });
    ui.add_space(CARD_GAP);
    card(ui, "Likely owner cluster", |ui| cluster(ui, view));
    if view.profile.peel_chain.is_some() {
        ui.add_space(CARD_GAP);
        card(ui, "Peel chain", |ui| peel_chain(ui, view));
    }
}

/// Start the flow graph window from `addr`.
pub(super) fn open_flow_graph(app: &mut App, addr: &str, cmd_tx: &CommandSender) {
    app.address.flows.start(addr.to_string());
    let _ = cmd_tx.send(UiCommand::AddressFlows {
        address: addr.to_string(),
        hops: 2,
    });
}

fn summary(ui: &mut Ui, view: &AddressView) {
    let stats = &view.profile.stats;
    let seen = |ms: u64| {
        if ms == 0 {
            return "—".to_string();
        }
        format!(
            "{} ago",
            format_duration(std::time::Duration::from_millis(
                now_ms().saturating_sub(ms)
            ))
        )
    };
    kv_columns(ui, 300.0, |[left, right]| {
        subheader(left, "Balance");
        kv_grid(left, "address_balance", |ui| {
            kv(
                ui,
                "Balance",
                RichText::new(or_dash(view.balance, |b| {
                    format!("{} KAS", format_kas(b as f64, 8))
                }))
                .color(theme::ACCENT_BRIGHT),
            );
            kv(
                ui,
                "Received (indexed)",
                format!("{} KAS", format_kas(stats.received as f64, 8)),
            );
            kv(
                ui,
                "Sent (indexed)",
                format!("{} KAS", format_kas(stats.sent as f64, 8)),
            );
            kv(ui, "Transactions (indexed)", format_number(stats.tx_count));
        });
        subheader(right, "Activity");
        kv_grid(right, "address_seen", |ui| {
            kv(ui, "First seen", seen(stats.first_seen_ms));
            kv(ui, "Last seen", seen(stats.last_seen_ms));
            kv(
                ui,
                "Index covers",
                or_dash(view.profile.coverage, |(from, to)| {
                    format!(
                        "{} → {}",
                        seen(from),
                        format_duration(std::time::Duration::from_millis(
                            now_ms().saturating_sub(to.min(now_ms()))
                        ))
                    )
                }),
            );
            kv(ui, "Counterparties", format_number(view.peers.len() as u64));
        });
    });
    ui.label(
        RichText::new(
            "Indexed totals cover the node's retention window, not the address's lifetime.",
        )
        .weak()
        .small(),
    );
}

/// Balance over the indexed window as a step line.
fn balance_chart(ui: &mut Ui, view: &AddressView) {
    if view.curve.len() < 2 {
        placeholder(ui, "Not enough indexed activity for a chart");
        return;
    }
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 90.0), egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 2.0, theme::BG_DEEP);
    let (t0, t1) = (
        view.curve[0].0,
        view.curve[view.curve.len() - 1].0.max(view.curve[0].0 + 1),
    );
    let (mut lo, mut hi) = (i64::MAX, i64::MIN);
    for &(_, b) in &view.curve {
        lo = lo.min(b);
        hi = hi.max(b);
    }
    let span = (hi - lo).max(1) as f32;
    let x = |t: u64| rect.left() + (t - t0) as f32 / (t1 - t0) as f32 * rect.width();
    let y = |b: i64| rect.bottom() - (b - lo) as f32 / span * (rect.height() - 8.0) - 4.0;
    let mut points = Vec::with_capacity(view.curve.len() * 2);
    let mut last_y = y(view.curve[0].1);
    for &(t, b) in &view.curve {
        points.push(egui::pos2(x(t), last_y));
        last_y = y(b);
        points.push(egui::pos2(x(t), last_y));
    }
    points.push(egui::pos2(rect.right(), last_y));
    painter.add(egui::Shape::line(
        points,
        egui::Stroke::new(1.5_f32, theme::ACCENT),
    ));
    painter.text(
        rect.left_top() + egui::vec2(4.0, 2.0),
        egui::Align2::LEFT_TOP,
        format!("{} KAS", format_kas(hi as f64, 2)),
        egui::FontId::monospace(theme::SMALL_FONT_SIZE),
        theme::TEXT_DIM,
    );
    painter.text(
        rect.left_bottom() + egui::vec2(4.0, -2.0),
        egui::Align2::LEFT_BOTTOM,
        format!("{} KAS", format_kas(lo as f64, 2)),
        egui::FontId::monospace(theme::SMALL_FONT_SIZE),
        theme::TEXT_DIM,
    );
}

/// The CSV and JSON export buttons in the Transactions card's header, once there are
/// transactions to export.
/// The transactions table, newest first, with "Load older" under it while there are
/// more.
fn transactions(
    ui: &mut Ui,
    addr: &str,
    view: &AddressView,
    loading_more: bool,
    cmd_tx: &CommandSender,
) {
    let rows: &[TxRow] = &view.page.items;
    if rows.is_empty() {
        placeholder(ui, "No indexed transactions");
        return;
    }
    ui.push_id("address_txs", |ui| {
        let row_height = table_row_height(ui);
        let table = page_table(ui, TXS_HEIGHT)
            .column(Column::remainder().at_least(110.0))
            .column(Column::auto().at_least(80.0))
            .column(Column::auto().at_least(120.0))
            .column(Column::auto().at_least(60.0))
            .column(Column::remainder().at_least(110.0));
        table_header(
            table,
            &[
                "Transaction",
                "When",
                "Change (KAS)",
                "Fee",
                "Accepting block",
            ],
        )
        .body(|body| {
            body.rows(row_height, rows.len(), |mut row| {
                let tx = &rows[row.index()];
                row.col(|ui| {
                    transaction_id(ui, &tx.txid);
                });
                row.col(|ui| {
                    ui.label(
                        format_duration(std::time::Duration::from_millis(
                            now_ms().saturating_sub(tx.time_ms),
                        )) + " ago",
                    );
                });
                row.col(|ui| {
                    let (sign, color) = if tx.delta >= 0 {
                        ("+", theme::OK)
                    } else {
                        ("-", theme::ERROR)
                    };
                    let text = format!("{sign}{}", format_kas(tx.delta.unsigned_abs() as f64, 8));
                    let text = if tx.is_coinbase {
                        format!("{text} ⛏")
                    } else {
                        text
                    };
                    ui.label(RichText::new(text).color(color));
                });
                row.col(|ui| {
                    ui.label(or_dash(tx.fee, |f| format_kas(f as f64, 8)));
                });
                row.col(|ui| {
                    block_hash(ui, &tx.accepting_block, false);
                });
            });
        });
    });
    if let Some(next) = view.page.next {
        ui.horizontal(|ui| {
            if loading_more {
                ui.spinner();
            } else if ui.button("Load older").clicked() {
                let _ = cmd_tx.send(UiCommand::AddressPage {
                    address: addr.to_string(),
                    before: next,
                });
            }
        });
    }
}

/// The likely-owner cluster: size, the most common label among members, and a sample.
fn cluster(ui: &mut Ui, view: &AddressView) {
    let Some(ref cluster) = view.cluster else {
        placeholder(ui, "Not indexed");
        return;
    };
    if cluster.size <= 1 {
        ui.label(
            RichText::new(
                "No other addresses linked yet (common-input ownership and change detection).",
            )
            .weak(),
        );
        return;
    }
    ui.horizontal(|ui| {
        ui.label(format!("{} addresses", format_number(cluster.size as u64)));
        if let Some(name) = &cluster.label {
            ui.label(RichText::new(format!("likely {name}")).color(theme::ACCENT_BRIGHT));
        }
        ui.label(
            RichText::new("Heuristic: inputs spent together and probable change share an owner.")
                .weak()
                .small(),
        );
    });
    let members: Vec<String> = cluster
        .members
        .iter()
        .filter(|m| **m != view.profile.address)
        .cloned()
        .collect();
    link_table(
        ui,
        "cluster_members",
        "Address",
        &members,
        LIST_HEIGHT,
        address,
    );
    if cluster.size as usize > cluster.members.len() {
        ui.label(
            RichText::new(format!(
                "Showing {} of {}",
                cluster.members.len(),
                cluster.size
            ))
            .weak()
            .small(),
        );
    }
}

/// The outcome of the last export, if any: a spinner, the file written or the error.
pub(super) fn export_status(ui: &mut Ui, status: &ExportStatus) {
    if status.running {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(RichText::new("Exporting…").weak());
        });
        return;
    }
    match &status.last {
        Some(Ok(path)) => {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Saved").color(theme::OK));
                copy_value(ui, &path.display().to_string(), "Copy path");
            });
        }
        Some(Err(e)) => {
            ui.label(RichText::new(format!("Export failed: {e}")).color(theme::ERROR));
        }
        None => {}
    }
}

/// The peel chain the address is a link of: its position, and what each spend peeled
/// off and carried on.
fn peel_chain(ui: &mut Ui, view: &AddressView) {
    let Some(chain) = &view.profile.peel_chain else {
        return;
    };
    let carried = chain.links.last().map(|l| l.carried).unwrap_or(0);
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format!("Link {} of {}", chain.position, chain.len() + 1))
                .color(theme::WARN),
        )
        .on_hover_text(
            "A run of single-input, two-output spends, each spending the previous one's \
             remainder and paying a slice off to the side. The index follows it as far as \
             it holds both ends.",
        );
        ui.label(
            RichText::new(format!(
                "{} spends · {} KAS peeled off · {} KAS carried at the end",
                chain.len(),
                format_kas(chain.peeled_total as f64, 2),
                format_kas(carried as f64, 2)
            ))
            .weak()
            .small(),
        );
    });
    egui::ScrollArea::vertical()
        .id_salt("peel_links")
        .max_height(LIST_HEIGHT)
        .show(ui, |ui| {
            kv_grid(ui, "peel_links", |ui| {
                for (i, link) in chain.links.iter().enumerate() {
                    let label = format!(
                        "{}. {} KAS peeled to",
                        i + 1,
                        format_kas(link.peeled as f64, 2)
                    );
                    kv_with(ui, &label, |ui| {
                        address(ui, &link.peeled_to);
                        ui.label(
                            RichText::new(format!("{} KAS on", format_kas(link.carried as f64, 2)))
                                .weak()
                                .small(),
                        )
                        .on_hover_text(format!("txid {}", link.txid));
                    });
                }
            });
        });
}

fn counterparties(ui: &mut Ui, view: &AddressView) {
    if view.peers.is_empty() {
        placeholder(ui, "No counterparties indexed");
        return;
    }
    kv_grid(ui, "address_peers", |ui| {
        for peer in &view.peers {
            let flows = format!(
                "+{} / -{}",
                format_kas(peer.stats.in_amount as f64, 2),
                format_kas(peer.stats.out_amount as f64, 2)
            );
            kv_with(ui, &flows, |ui| address(ui, &peer.address));
        }
    });
}
