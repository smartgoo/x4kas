//! The pieces of an address page, shared by the info pane (opened from any address, see
//! `widgets::request_address`) and the Explorer tab: `AddressForms` (the label and watch
//! settings forms) and `body` (the cards: summary with balance and indexed totals,
//! balance history, transactions, counterparties with the flow graph button, cluster
//! and peel chain).

use eframe::egui::{self, RichText, TextEdit, Ui};
use egui_extras::Column;

use super::monitoring::network;
use super::theme;
use super::widgets::{
    CARD_GAP, address, block_hash, card, card_with_header, copy_value, is_testnet, kv, kv_columns,
    kv_grid, kv_with, link_table, or_dash, page_table, placeholder, primary_button, subheader,
    table_header, table_row_height, transaction_id, weighted_columns,
};
use x4kas_core::app::{AddressView, App, ExportStatus};
use x4kas_core::controller::{CommandSender, ExportRequest, UiCommand};
use x4kas_core::format::{
    explorer_address_url, format_duration, format_kas, format_number, kaspa_stream_address_url,
    now_ms,
};
use x4kas_core::index::export::ExportFormat;
use x4kas_core::index::query::TxRow;
use x4kas_core::labels::OnlineEntry;
use x4kas_core::watch::{AlertRules, WatchEntry};

/// Rows of the transactions table before it scrolls.
const TXS_HEIGHT: f32 = 260.0;
/// Height of the cluster members and peel chain lists before they scroll.
const LIST_HEIGHT: f32 = 120.0;

/// The editable state of an address's forms (its label and watch settings), kept
/// across frames and refilled when the address changes.
#[derive(Default)]
pub struct AddressForms {
    /// Which address the fields below were filled for.
    for_address: String,
    label: String,
    /// The user's saved label when `label` was last filled, to notice edits made
    /// elsewhere (a chip, the Settings page) and refill.
    saved_label: Option<String>,
    watch: WatchEntry,
    /// Threshold fields in KAS, as typed.
    received_min: String,
    sent_min: String,
    balance_below: String,
    balance_above: String,
    /// Hours, as typed.
    idle_hours: String,
}

impl AddressForms {
    /// Fill the fields for `addr` if they aren't already, and pick up a label saved
    /// elsewhere.
    pub fn sync(&mut self, app: &App, addr: &str) {
        let saved = app.labels.user_labels().get(addr).cloned();
        if self.for_address != addr {
            self.fill(app, addr);
        } else if saved != self.saved_label {
            self.label = saved.clone().unwrap_or_default();
            self.saved_label = saved;
        }
    }

    fn fill(&mut self, app: &App, addr: &str) {
        self.for_address = addr.to_string();
        self.saved_label = app.labels.user_labels().get(addr).cloned();
        self.label = self.saved_label.clone().unwrap_or_default();
        self.watch = app
            .watch
            .entry(addr)
            .cloned()
            .unwrap_or_else(|| WatchEntry::new(addr, &network(app)));
        let kas = |v: Option<u64>| v.map(|s| format_kas(s as f64, 8)).unwrap_or_default();
        self.received_min = kas(self.watch.rules.received_min);
        self.sent_min = kas(self.watch.rules.sent_min);
        self.balance_below = kas(self.watch.rules.balance_below);
        self.balance_above = kas(self.watch.rules.balance_above);
        self.idle_hours = self
            .watch
            .rules
            .idle_hours
            .map(|h| h.to_string())
            .unwrap_or_default();
    }

    /// Address, explorer links, the online lookup and the label (public one shown,
    /// user's editable).
    pub fn header(
        &mut self,
        ui: &mut Ui,
        app: &App,
        addr: &str,
        online_result: Option<&[OnlineEntry]>,
        cmd_tx: &CommandSender,
    ) {
        let label = &mut self.label;
        let _ = is_testnet(ui.ctx());
        kv_grid(ui, "address_header", |ui| {
            kv_with(ui, "Address", |ui| copy_value(ui, addr, "Copy address"));
            kv_with(ui, "View on", |ui| {
                ui.hyperlink_to("Kaspa Stream", kaspa_stream_address_url(addr));
                ui.label(RichText::new("·").weak());
                ui.hyperlink_to("Kaspa Explorer", explorer_address_url(addr));
            });
            kv_with(ui, "Online", |ui| {
                // Right to left: the button, then what was learned.
                let enabled = app.label_settings.any_enabled();
                if ui
                    .add_enabled(enabled, egui::Button::new("Look up"))
                    .on_hover_text(if enabled {
                        "Ask KNS for this address's .kas name (sends it the address)"
                    } else {
                        "Enable KNS lookups first: x4kas-cli labels kns on"
                    })
                    .on_disabled_hover_text("Enable KNS lookups first: x4kas-cli labels kns on")
                    .clicked()
                {
                    let _ = cmd_tx.send(UiCommand::LookupLabelOnline(addr.to_string()));
                }
                match online_result {
                    None => {
                        ui.label(RichText::new("not asked").weak());
                    }
                    Some([]) => {
                        ui.label(RichText::new("no online source enabled").weak());
                    }
                    Some(entries) => {
                        for entry in entries {
                            let text = match &entry.name {
                                Some(name) => format!("{}: {name}", entry.source.label()),
                                None => format!("{}: no label", entry.source.label()),
                            };
                            ui.label(RichText::new(text).color(theme::TEXT));
                        }
                    }
                }
            });
            kv_with(ui, "Label", |ui| {
                // Right to left: the button, then the field, then the known label.
                if ui.button("Save").clicked() {
                    let _ = cmd_tx.send(UiCommand::SetLabel {
                        address: addr.to_string(),
                        name: Some(label.clone()).filter(|l| !l.trim().is_empty()),
                    });
                }
                ui.add(
                    TextEdit::singleline(label)
                        .hint_text("your label")
                        .desired_width(180.0),
                );
                if let Some(known) = app.labels.get(addr) {
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
            });
        });
    }

    /// The watchlist entry for this address: on/off and alert rules (the body of a
    /// "Watch" card).
    pub fn watch_settings(&mut self, ui: &mut Ui, app: &App, addr: &str, cmd_tx: &CommandSender) {
        let watched = app.watch.entry(addr).is_some();
        let mut changed = false;
        ui.horizontal(|ui| {
            if !watched {
                if ui.add(primary_button("Add to watchlist")).clicked() {
                    changed = true;
                }
                return;
            }
            changed |= ui.checkbox(&mut self.watch.enabled, "Alerts on").changed();
            if ui.button("Remove").clicked() {
                let mut list = app.watch.list.clone();
                list.entries.retain(|e| e.address != addr);
                let _ = cmd_tx.send(UiCommand::WatchSet(list));
            }
        });
        if watched {
            ui.horizontal_wrapped(|ui| {
                changed |= ui
                    .checkbox(&mut self.watch.rules.any_activity, "Any activity")
                    .changed();
                for (label, field, hint) in [
                    ("Received ≥", &mut self.received_min, "KAS"),
                    ("Sent ≥", &mut self.sent_min, "KAS"),
                    ("Balance <", &mut self.balance_below, "KAS"),
                    ("Balance >", &mut self.balance_above, "KAS"),
                    ("Active after ≥", &mut self.idle_hours, "hours idle"),
                ] {
                    ui.label(RichText::new(label).color(theme::LABEL));
                    if ui
                        .add(
                            TextEdit::singleline(field)
                                .hint_text(hint)
                                .desired_width(90.0),
                        )
                        .lost_focus()
                    {
                        changed = true;
                    }
                }
            });
        }
        if changed {
            self.watch.rules = AlertRules {
                any_activity: self.watch.rules.any_activity,
                received_min: parse_kas(&self.received_min),
                sent_min: parse_kas(&self.sent_min),
                balance_below: parse_kas(&self.balance_below),
                balance_above: parse_kas(&self.balance_above),
                idle_hours: self.idle_hours.trim().parse().ok().filter(|h| *h > 0),
            };
            let mut list = app.watch.list.clone();
            match list.entries.iter_mut().find(|e| e.address == addr) {
                Some(entry) => *entry = self.watch.clone(),
                None => list.entries.push(self.watch.clone()),
            }
            let _ = cmd_tx.send(UiCommand::WatchSet(list));
        }
    }
}

/// The cards below the header for a loaded address: summary, balance history,
/// transactions (export buttons in the header) beside counterparties (the flow graph
/// button in the header), cluster and, when it is a link of one, the peel chain.
/// Returns whether "Open flow graph" was clicked (see [`open_flow_graph`]).
pub(super) fn body(
    ui: &mut Ui,
    app: &App,
    addr: &str,
    view: &AddressView,
    loading_more: bool,
    cmd_tx: &CommandSender,
) -> bool {
    card(ui, "Summary", |ui| {
        summary(ui, view);
        if let Some(note) = &view.index_note {
            ui.add_space(4.0);
            ui.label(RichText::new(note).color(theme::WARN));
        }
    });
    if view.index_note.is_some() {
        return false;
    }
    ui.add_space(CARD_GAP);
    card(ui, "Balance history", |ui| balance_chart(ui, view));
    ui.add_space(CARD_GAP);
    let mut open_flows = false;
    weighted_columns(ui, [1.0, 1.0], 360.0, |[left, right]| {
        card_with_header(
            left,
            "Transactions",
            &mut (),
            |ui, _| export_buttons(ui, addr, view, cmd_tx),
            |ui, _| {
                transactions(ui, addr, view, loading_more, cmd_tx);
                export_status(ui, &app.address.export);
            },
        );
        card_with_header(
            right,
            "Top counterparties",
            &mut open_flows,
            |ui, open| {
                *open = ui
                    .small_button("Open flow graph")
                    .on_hover_text("Follow the money: counterparties of counterparties")
                    .clicked();
            },
            |ui, _| counterparties(ui, view),
        );
    });
    ui.add_space(CARD_GAP);
    card(ui, "Likely owner cluster", |ui| cluster(ui, view));
    if view.profile.peel_chain.is_some() {
        ui.add_space(CARD_GAP);
        card(ui, "Peel chain", |ui| peel_chain(ui, view));
    }
    open_flows
}

/// Start the flow graph window from `addr`.
pub(super) fn open_flow_graph(app: &mut App, addr: &str, cmd_tx: &CommandSender) {
    app.address.flows.start(addr.to_string());
    let _ = cmd_tx.send(UiCommand::AddressFlows {
        address: addr.to_string(),
        hops: 2,
    });
}

/// KAS as typed to sompi; empty or invalid is no threshold.
fn parse_kas(s: &str) -> Option<u64> {
    let v: f64 = s.trim().replace(',', "").parse().ok()?;
    (v > 0.0).then(|| (v * 100_000_000.0).round() as u64)
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
fn export_buttons(ui: &mut Ui, addr: &str, view: &AddressView, cmd_tx: &CommandSender) {
    if view.page.items.is_empty() {
        return;
    }
    ui.label(RichText::new("Export").weak().small());
    for format in [ExportFormat::Csv, ExportFormat::Json] {
        if ui
            .small_button(format.extension().to_ascii_uppercase())
            .on_hover_text(format!(
                "Export every indexed transaction as {} to ~/.x4kas/exports",
                format.extension().to_ascii_uppercase()
            ))
            .clicked()
        {
            let _ = cmd_tx.send(UiCommand::Export(ExportRequest::Transactions {
                address: addr.to_string(),
                format,
            }));
        }
    }
}

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
