//! Explorer tab: browser-like sub tabs over block, address and transaction pages
//! (`x4kas_core::explorer`), each with a search field and back/forward history. The
//! pages are modeled on the classic Kaspa explorers, with the index's extras (acceptance,
//! fees, resolved inputs, clusters) folded in. While the tab draws, a click on any
//! address, block hash or transaction id navigates the active sub tab
//! (`widgets::set_in_explorer`); the right-click menu opens it in a new one or in the
//! info pane (`gui/pane.rs`, which shares the pages' core pieces: `block_overview`,
//! `block_transactions`, `tx_overview`, `tx_inputs`, `tx_outputs`).

use std::collections::HashMap;

use eframe::egui::{self, Button, RichText, TextEdit, Ui};
use egui_extras::Column;

use super::address::{self, AddressForms};
use super::tab_button;
use super::theme;
use super::widgets::{
    CARD_GAP, address as address_widget, block_hash, card, copy_value, is_testnet, kv, kv_columns,
    kv_grid, kv_with, label_search_popup, link_table, or_dash, page_table, placeholder,
    primary_button, set_in_explorer, subheader, table_header, table_row_height, transaction_id,
    transaction_id_in_block, weighted_columns, yes_no,
};
use x4kas_core::app::{App, ConnectionStatus};
use x4kas_core::controller::{CommandSender, UiCommand};
use x4kas_core::explorer::{
    AddressPageData, BlockView, ExplorerPage, PageData, PageLoad, TxStatus, TxView, parse_query,
};
use x4kas_core::format::{
    explorer_block_url, explorer_tx_url, format_duration, format_kas, format_number, format_utc,
    kaspa_stream_block_url, kaspa_stream_tx_url, now_ms,
};
use x4kas_core::index::cluster::CHANGE_THRESHOLD;

/// Blocks listed on the Home page.
const LATEST_BLOCKS: usize = 25;
/// Rows a page's table shows before scrolling.
const TABLE_MAX_HEIGHT: f32 = 420.0;
/// The height of a block's parents, children and merge set lists before scrolling.
const HASH_LIST_HEIGHT: f32 = 160.0;

/// The tab's own state: the search drafts and the address page's forms.
#[derive(Default)]
pub struct ExplorerUi {
    /// Per sub tab id: the page the draft was set from, and the draft.
    drafts: HashMap<u64, (ExplorerPage, String)>,
    /// The label matches popup is showing under the search field.
    search_open: bool,
    /// Focus the search field on the next frame (Ctrl+L).
    pub focus_search: bool,
    forms: AddressForms,
}

impl ExplorerUi {
    pub fn show(&mut self, ui: &mut Ui, app: &mut App, cmd_tx: &CommandSender) {
        set_in_explorer(ui.ctx(), true);
        self.tab_strip(ui, app);
        self.nav_bar(ui, app, cmd_tx);
        ui.add_space(CARD_GAP);

        // Ask for the active page once connected; a failed load waits for a reload.
        let page = app.explorer.active_tab().page.clone();
        let connected = matches!(app.node.connection_status, ConnectionStatus::Connected);
        if connected && app.explorer.needs_load(&page) {
            app.explorer.start_loading(page.clone());
            let _ = cmd_tx.send(UiCommand::ExplorerLoad(page.clone()));
        }

        let mut open_flows = false;
        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .show(ui, |ui| {
                self.page(ui, app, &page, connected, cmd_tx, &mut open_flows);
            });
        if open_flows && let ExplorerPage::Address(addr) = &page {
            address::open_flow_graph(app, addr, cmd_tx);
        }
        set_in_explorer(ui.ctx(), false);
    }

    /// New tab (Ctrl+T), close tab (Ctrl+W) and focus the search field (Ctrl+L).
    pub fn handle_shortcuts(&mut self, ctx: &egui::Context, app: &mut App) {
        let m = egui::Modifiers::COMMAND;
        if ctx.input_mut(|i| i.consume_key(m, egui::Key::T)) {
            app.explorer.open_tab(ExplorerPage::Home);
            self.focus_search = true;
        }
        if ctx.input_mut(|i| i.consume_key(m, egui::Key::W)) {
            let active = app.explorer.active;
            app.explorer.close_tab(active);
        }
        if ctx.input_mut(|i| i.consume_key(m, egui::Key::L)) {
            self.focus_search = true;
        }
    }

    /// The sub tabs, like a browser's: a click activates, × or a middle click closes,
    /// + opens a new Home tab.
    fn tab_strip(&mut self, ui: &mut Ui, app: &mut App) {
        let mut activate = None;
        let mut close = None;
        let mut open = false;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            for (i, tab) in app.explorer.tabs.iter().enumerate() {
                let title = tab_title(app, &tab.page);
                let selected = i == app.explorer.active;
                let response = tab_button(ui, &title, selected).on_hover_text(match &tab.page {
                    ExplorerPage::Home => "Home".to_string(),
                    page => page.query().to_string(),
                });
                if response.clicked() {
                    activate = Some(i);
                }
                if response.middle_clicked() {
                    close = Some(i);
                }
                if ui
                    .add(Button::new(RichText::new("×").color(theme::TEXT_DIM)).frame(false))
                    .on_hover_text("Close tab (Ctrl+W)")
                    .clicked()
                {
                    close = Some(i);
                }
                ui.add_space(6.0);
            }
            if ui
                .add(Button::new(RichText::new("+").color(theme::ACCENT)).frame(false))
                .on_hover_text("New tab (Ctrl+T)")
                .clicked()
            {
                open = true;
            }
        });
        if let Some(i) = activate {
            app.explorer.active = i;
        }
        if let Some(i) = close {
            self.drafts.remove(&app.explorer.tabs[i].id);
            app.explorer.close_tab(i);
        }
        if open {
            app.explorer.open_tab(ExplorerPage::Home);
            self.focus_search = true;
        }
    }

    /// Back, forward, reload and the search field, like a browser's address bar.
    fn nav_bar(&mut self, ui: &mut Ui, app: &mut App, cmd_tx: &CommandSender) {
        let tab = app.explorer.active_tab();
        let (id, page) = (tab.id, tab.page.clone());
        let can_back = !tab.back.is_empty();
        let can_forward = !tab.forward.is_empty();
        let loading = matches!(app.explorer.load(&page), Some(PageLoad::Loading));
        let can_reload = page.is_loadable() && !loading;
        // The draft follows the page, like a URL bar, until the user types.
        let draft = match self.drafts.get_mut(&id) {
            Some((from, draft)) if *from == page => draft,
            _ => {
                self.drafts
                    .insert(id, (page.clone(), page.query().to_string()));
                &mut self.drafts.get_mut(&id).expect("just inserted").1
            }
        };

        let mut go = None;
        let mut back = false;
        let mut forward = false;
        let mut reload = false;
        let mut submitted = false;
        let mut field = None;
        ui.horizontal(|ui| {
            back = ui
                .add_enabled(can_back, Button::new("◀"))
                .on_hover_text("Back")
                .clicked();
            forward = ui
                .add_enabled(can_forward, Button::new("▶"))
                .on_hover_text("Forward")
                .clicked();
            reload = ui
                .add_enabled(can_reload, Button::new("⟳"))
                .on_hover_text("Reload this page from the node")
                .clicked();
            let go_width = 60.0;
            let response = ui.add(
                TextEdit::singleline(draft)
                    .hint_text(
                        "Address, block hash or transaction id, or a label such as \"Bybit\"",
                    )
                    .desired_width(ui.available_width() - go_width),
            );
            if self.focus_search {
                response.request_focus();
                self.focus_search = false;
            }
            if response.changed() {
                self.search_open = true;
            }
            submitted = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            field = Some(response);
            if ui.add(primary_button("Go")).clicked() {
                submitted = true;
            }
        });
        if submitted {
            match parse_query(draft) {
                Some(target) => go = Some(target),
                None => self.search_open = true,
            }
        }
        // Label matches, when the field isn't an address or an id.
        if let Some(field) = field
            && parse_query(draft).is_none()
            && let Some(addr) =
                label_search_popup(ui, &field, &mut self.search_open, draft, &app.labels)
        {
            go = Some(ExplorerPage::Address(addr));
        }

        if back {
            app.explorer.go_back();
        }
        if forward {
            app.explorer.go_forward();
        }
        if reload {
            app.explorer.start_loading(page.clone());
            let _ = cmd_tx.send(UiCommand::ExplorerLoad(page));
        }
        if let Some(target) = go {
            app.explorer.navigate(target);
            self.search_open = false;
        }
    }

    fn page(
        &mut self,
        ui: &mut Ui,
        app: &App,
        page: &ExplorerPage,
        connected: bool,
        cmd_tx: &CommandSender,
        open_flows: &mut bool,
    ) {
        let mut load = app.explorer.load(page);
        // A lookup already resolved: show what it resolved to (the tab was moved on,
        // but the lookup can come back through the history).
        if let Some(PageLoad::Ready(data)) = load
            && let PageData::Redirect(target) = &**data
        {
            load = app.explorer.load(target);
        }
        match (page, load) {
            (ExplorerPage::Home, _) => home(ui, app),
            (_, None) if !connected => card(ui, "Explorer", |ui| {
                placeholder(ui, "Connect to a node to load this page.");
            }),
            (_, None | Some(PageLoad::Loading)) => card(ui, page.title().as_str(), |ui| {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Loading…");
                });
            }),
            (_, Some(PageLoad::Failed(error))) => card(ui, "Not found", |ui| {
                copy_value(ui, page.query(), "Copy");
                ui.add_space(4.0);
                ui.label(RichText::new(error).color(theme::ERROR));
                ui.add_space(4.0);
                ui.label(
                    RichText::new("Reload with ⟳ once the node or the index has it.")
                        .weak()
                        .small(),
                );
            }),
            (_, Some(PageLoad::Ready(data))) => match &**data {
                PageData::Block(view) => block_page(ui, app, view),
                PageData::Address(data) => {
                    let addr = page.query();
                    *open_flows = self.address_page(ui, app, addr, data, cmd_tx);
                }
                PageData::Transaction(view) => tx_page(ui, app, view),
                PageData::Redirect(_) => placeholder(ui, "Resolving…"),
            },
        }
    }

    /// The address page's cards: the header, the body's (`address::body`) and the
    /// watch settings last. The info pane draws the same.
    fn address_page(
        &mut self,
        ui: &mut Ui,
        app: &App,
        addr: &str,
        data: &AddressPageData,
        cmd_tx: &CommandSender,
    ) -> bool {
        self.forms.sync(app, addr);
        card(ui, "Address", |ui| {
            self.forms
                .header(ui, app, addr, data.online_result.as_deref(), cmd_tx);
            if let Some(error) = &data.error {
                ui.label(RichText::new(error).color(theme::ERROR));
            }
        });
        ui.add_space(CARD_GAP);
        let open_flows = address::body(ui, app, addr, &data.view, data.loading_more, cmd_tx);
        ui.add_space(CARD_GAP);
        card(ui, "Watch", |ui| {
            self.forms.watch_settings(ui, app, addr, cmd_tx);
        });
        open_flows
    }
}

/// A sub tab's title: a labelled address shows its label.
fn tab_title(app: &App, page: &ExplorerPage) -> String {
    match page {
        ExplorerPage::Address(addr) => app
            .labels
            .name(addr)
            .map(str::to_string)
            .unwrap_or_else(|| page.title()),
        _ => page.title(),
    }
}

fn ago(ms: u64) -> String {
    format!(
        "{} ago",
        format_duration(std::time::Duration::from_millis(
            now_ms().saturating_sub(ms)
        ))
    )
}

/// A timestamp as UTC and how long ago.
fn when(ms: u64) -> String {
    format!("{} ({})", format_utc(ms), ago(ms))
}

fn kas(sompi: u64) -> String {
    format!("{} KAS", format_kas(sompi as f64, 8))
}

// --- Home ---

/// Search hints, the newest blocks from the node and the pages viewed recently.
fn home(ui: &mut Ui, app: &App) {
    card(ui, "Explorer", |ui| {
        ui.label("Search for a Kaspa address, a block hash or a transaction id above.");
    });
    ui.add_space(CARD_GAP);
    weighted_columns(ui, [1.0, 1.0], 360.0, |[left, right]| {
        card(left, "Latest blocks", |ui| latest_blocks(ui, app));
        card(right, "Recently viewed", |ui| recent(ui, app));
    });
}

/// The newest blocks the DAG visualizer has seen, newest DAA score first.
fn latest_blocks(ui: &mut Ui, app: &App) {
    let blocks: Vec<_> = app
        .node
        .dag_visualizer
        .columns
        .iter()
        .rev()
        .flat_map(|(_, blocks)| blocks.iter().rev())
        .take(LATEST_BLOCKS)
        .collect();
    if blocks.is_empty() {
        placeholder(ui, "Waiting for blocks from the node…");
        return;
    }
    ui.push_id("latest_blocks", |ui| {
        let row_height = table_row_height(ui);
        let table = page_table(ui, TABLE_MAX_HEIGHT)
            .column(Column::remainder().at_least(140.0))
            .column(Column::auto().at_least(90.0))
            .column(Column::auto().at_least(60.0));
        table_header(table, &["Block", "DAA score", "Parents"]).body(|body| {
            body.rows(row_height, blocks.len(), |mut row| {
                let block = blocks[row.index()];
                row.col(|ui| {
                    block_hash(ui, &block.hash, false);
                });
                row.col(|ui| {
                    ui.label(format_number(block.daa_score));
                });
                row.col(|ui| {
                    ui.label(block.parents.len().to_string());
                });
            });
        });
    });
}

fn recent(ui: &mut Ui, app: &App) {
    let mut any = false;
    kv_grid(ui, "recent_pages", |ui| {
        for page in app.explorer.recent() {
            any = true;
            match page {
                ExplorerPage::Block(hash) => kv_with(ui, "Block", |ui| {
                    block_hash(ui, hash, false);
                }),
                ExplorerPage::Address(addr) => kv_with(ui, "Address", |ui| {
                    address_widget(ui, addr);
                }),
                ExplorerPage::Transaction { txid, block } => kv_with(ui, "Transaction", |ui| {
                    transaction_id_in_block(ui, txid, block.as_deref());
                }),
                ExplorerPage::Home | ExplorerPage::Lookup(_) => {}
            }
        }
    });
    if !any {
        placeholder(ui, "Pages you open show up here.");
    }
}

// --- Block ---

fn block_page(ui: &mut Ui, app: &App, view: &BlockView) {
    card(ui, "Block", |ui| block_overview(ui, view));
    ui.add_space(CARD_GAP);

    weighted_columns(ui, [1.0, 1.0], 360.0, |[left, right]| {
        card(
            left,
            &format!(
                "Parents ({}, {} levels)",
                view.parents.len(),
                view.parent_levels
            ),
            |ui| hash_list(ui, &view.parents, "No parents (genesis)"),
        );
        card(
            right,
            &format!("Children ({})", view.children.len()),
            |ui| hash_list(ui, &view.children, "No children known yet"),
        );
    });
    ui.add_space(CARD_GAP);
    weighted_columns(ui, [1.0, 1.0], 360.0, |[left, right]| {
        card(
            left,
            &format!("Merge set blues ({})", view.merge_set_blues.len()),
            |ui| hash_list(ui, &view.merge_set_blues, "None"),
        );
        card(
            right,
            &format!("Merge set reds ({})", view.merge_set_reds.len()),
            |ui| hash_list(ui, &view.merge_set_reds, "None"),
        );
    });
    ui.add_space(CARD_GAP);
    card(
        ui,
        &format!("Transactions ({})", view.transactions.len()),
        |ui| block_transactions(ui, app, view),
    );
}

/// The block's core: hash and explorer links, header fields, DAG standing (chain
/// block, color, confirmations, reward, merging block, selected parent), miner and
/// merkle roots. The page's first card; the info pane shows it too.
pub(super) fn block_overview(ui: &mut Ui, view: &BlockView) {
    let testnet = is_testnet(ui.ctx());
    {
        kv_grid(ui, "block_ids", |ui| {
            kv_with(ui, "Hash", |ui| copy_value(ui, &view.hash, "Copy hash"));
            kv_with(ui, "View on", |ui| {
                ui.hyperlink_to("Kaspa Stream", kaspa_stream_block_url(&view.hash));
                ui.label(RichText::new("·").weak());
                ui.hyperlink_to("Kaspa Explorer", explorer_block_url(&view.hash, testnet));
            });
        });
        ui.add_space(6.0);
        kv_columns(ui, 320.0, |[left, right]| {
            subheader(left, "Header");
            kv_grid(left, "block_header", |ui| {
                kv(ui, "Timestamp", when(view.timestamp_ms));
                kv(ui, "DAA score", format_number(view.daa_score));
                kv(ui, "Blue score", format_number(view.blue_score));
                kv_with(ui, "Blue work", |ui| {
                    copy_value(ui, &view.blue_work, "Copy blue work");
                });
                kv(
                    ui,
                    "Difficulty",
                    or_dash(view.difficulty, |d| format!("{d:.0}")),
                );
                kv(ui, "Bits", format!("{:#010x}", view.bits));
                kv(ui, "Nonce", format!("{:#018x}", view.nonce));
                kv(ui, "Version", view.version.to_string());
            });
            subheader(right, "DAG standing");
            kv_grid(right, "block_dag", |ui| {
                kv(
                    ui,
                    "Chain block",
                    or_dash(view.is_chain_block, |c| yes_no(c).to_string()),
                );
                let reward = view.reward.as_ref();
                kv_with(ui, "Color", |ui| match reward {
                    Some(r) => {
                        let color = match r.color {
                            x4kas_core::rpc::types::BlockColor::Blue => theme::ACCENT_BRIGHT,
                            x4kas_core::rpc::types::BlockColor::Red => theme::ERROR,
                            x4kas_core::rpc::types::BlockColor::Unknown => theme::TEXT_DIM,
                        };
                        ui.label(RichText::new(r.color.label()).color(color));
                    }
                    None => {
                        ui.label("—");
                    }
                });
                kv(
                    ui,
                    "Confirmations",
                    or_dash(reward.and_then(|r| r.confirmations), format_number),
                );
                kv(ui, "Reward", or_dash(reward.and_then(|r| r.reward), kas));
                hash_row(
                    ui,
                    "Merged by",
                    reward.and_then(|r| r.merging_chain_block.as_deref()),
                );
                hash_row(ui, "Selected parent", view.selected_parent.as_deref());
                hash_row(ui, "Pruning point", Some(&view.pruning_point));
                kv(
                    ui,
                    "Transactions",
                    format!(
                        "{} ({} accepted per the index)",
                        view.transactions.len(),
                        view.accepted_count()
                    ),
                );
            });
        });
        ui.add_space(6.0);
        subheader(ui, "Miner and commitments");
        kv_grid(ui, "block_miner", |ui| {
            match &view.miner {
                Some(miner) => {
                    kv_with(ui, "Miner", |ui| match &miner.address {
                        Some(addr) => address_widget(ui, addr),
                        None => {
                            ui.label("—");
                        }
                    });
                    let software = match (&miner.node_version, &miner.tag) {
                        (Some(v), Some(tag)) => format!("{v} / {tag}"),
                        (Some(v), None) => v.clone(),
                        (None, Some(tag)) => tag.clone(),
                        (None, None) => "—".to_string(),
                    };
                    kv(ui, "Miner software", software);
                }
                None => kv(ui, "Miner", "— (header only)"),
            }
            kv_with(ui, "Hash merkle root", |ui| {
                copy_value(ui, &view.hash_merkle_root, "Copy");
            });
            kv_with(ui, "Accepted ID merkle root", |ui| {
                copy_value(ui, &view.accepted_id_merkle_root, "Copy");
            });
            kv_with(ui, "UTXO commitment", |ui| {
                copy_value(ui, &view.utxo_commitment, "Copy");
            });
        });
    }
}

/// A [`kv`] row with a [`block_hash`], or a dash as tall as one until there is a hash.
fn hash_row(ui: &mut Ui, label: &str, hash: Option<&str>) {
    kv_with(ui, label, |ui| match hash {
        Some(hash) => {
            block_hash(ui, hash, false);
        }
        None => {
            ui.set_min_height(ui.spacing().interact_size.y);
            ui.label("—");
        }
    });
}

/// A card's list of block hashes as numbered striped rows.
fn hash_list(ui: &mut Ui, hashes: &[String], empty: &str) {
    if hashes.is_empty() {
        placeholder(ui, empty);
        return;
    }
    link_table(ui, empty, "Block", hashes, HASH_LIST_HEIGHT, |ui, hash| {
        block_hash(ui, hash, false);
    });
}

/// The block's transactions table (acceptance and fees from the index).
pub(super) fn block_transactions(ui: &mut Ui, app: &App, view: &BlockView) {
    if view.transactions.is_empty() {
        placeholder(
            ui,
            if view.is_header_only {
                "Header only: the node pruned this block's transactions."
            } else {
                "No transactions"
            },
        );
        return;
    }
    let indexed = app.connection.is_direct();
    ui.push_id("block_txs", |ui| {
        let row_height = table_row_height(ui);
        let table = page_table(ui, TABLE_MAX_HEIGHT)
            .column(Column::remainder().at_least(140.0))
            .column(Column::auto().at_least(90.0))
            .column(Column::auto().at_least(60.0))
            .column(Column::auto().at_least(120.0))
            .column(Column::auto().at_least(80.0))
            .column(Column::remainder().at_least(140.0))
            .column(Column::auto().at_least(70.0));
        table_header(
            table,
            &[
                "Transaction",
                "Type",
                "In → Out",
                "Amount (KAS)",
                "Fee (KAS)",
                "To",
                "Accepted",
            ],
        )
        .body(|body| {
            body.rows(row_height, view.transactions.len(), |mut row| {
                let tx = &view.transactions[row.index()];
                row.col(|ui| {
                    transaction_id_in_block(ui, &tx.txid, Some(&view.hash));
                });
                row.col(|ui| {
                    ui.label(tx_type(tx.is_coinbase, tx.protocol));
                });
                row.col(|ui| {
                    ui.label(format!("{} → {}", tx.input_count, tx.output_count));
                });
                row.col(|ui| {
                    ui.label(format_kas(tx.output_total as f64, 8));
                });
                row.col(|ui| {
                    ui.label(or_dash(tx.accepted.as_ref().and_then(|a| a.fee), |f| {
                        format_kas(f as f64, 8)
                    }));
                });
                row.col(|ui| match &tx.recipient {
                    Some(addr) => address_widget(ui, addr),
                    None => {
                        ui.label("—");
                    }
                });
                row.col(|ui| match &tx.accepted {
                    Some(accepted) => {
                        ui.label(RichText::new("✓").color(theme::OK))
                            .on_hover_text(format!("Accepted by {}", accepted.block));
                    }
                    None if indexed => {
                        ui.label(RichText::new("—").weak())
                            .on_hover_text("Not accepted within the indexed window");
                    }
                    None => {
                        ui.label(RichText::new("?").weak())
                            .on_hover_text("Acceptance needs the address index (a direct node)");
                    }
                });
            });
        });
    });
}

fn tx_type(
    is_coinbase: bool,
    protocol: Option<x4kas_core::tx_inspect::TransactionProtocol>,
) -> String {
    if is_coinbase {
        "coinbase".to_string()
    } else {
        protocol.map_or_else(|| "standard".to_string(), |p| p.label().to_string())
    }
}

// --- Transaction ---

fn tx_page(ui: &mut Ui, app: &App, view: &TxView) {
    card(ui, "Transaction", |ui| tx_overview(ui, app, view));
    ui.add_space(CARD_GAP);
    weighted_columns(ui, [1.0, 1.0], 400.0, |[left, right]| {
        card(left, &format!("Inputs ({})", view.inputs.len()), |ui| {
            tx_inputs(ui, view)
        });
        card(right, &format!("Outputs ({})", view.outputs.len()), |ui| {
            tx_outputs(ui, view)
        });
    });
}

/// The transaction's core: id and explorer links, status and accepting block,
/// confirmations, type, fee, mass, totals, version, lock time, subnetwork and payload.
/// The page's first card; the info pane shows it too.
pub(super) fn tx_overview(ui: &mut Ui, app: &App, view: &TxView) {
    let testnet = is_testnet(ui.ctx());
    {
        kv_grid(ui, "tx_ids", |ui| {
            kv_with(ui, "Transaction id", |ui| {
                copy_value(ui, &view.txid, "Copy transaction id");
            });
            kv_with(ui, "View on", |ui| {
                ui.hyperlink_to("Kaspa Stream", kaspa_stream_tx_url(&view.txid));
                ui.label(RichText::new("·").weak());
                ui.hyperlink_to("Kaspa Explorer", explorer_tx_url(&view.txid, testnet));
            });
        });
        ui.add_space(6.0);
        kv_columns(ui, 320.0, |[left, right]| {
            subheader(left, "Status");
            kv_grid(left, "tx_status", |ui| {
                let (status, color) = match &view.status {
                    TxStatus::Accepted { .. } => ("Accepted", theme::OK),
                    TxStatus::Mempool { is_orphan: false } => ("In the mempool", theme::WARN),
                    TxStatus::Mempool { is_orphan: true } => {
                        ("In the mempool (orphan)", theme::WARN)
                    }
                    TxStatus::InBlock { .. } => ("In a block, not known accepted", theme::WARN),
                };
                kv(ui, "Status", RichText::new(status).color(color));
                match &view.status {
                    TxStatus::Accepted {
                        block,
                        daa_score,
                        time_ms,
                    } => {
                        hash_row(ui, "Accepting block", Some(block));
                        kv(ui, "Accepted at", when(*time_ms));
                        kv(ui, "DAA score", format_number(*daa_score));
                        let tip = app.node.server_info.as_ref().map(|s| s.virtual_daa_score);
                        kv_with(ui, "Confirmations", |ui| {
                            ui.label(or_dash(tip, |tip| {
                                format_number(tip.saturating_sub(*daa_score))
                            }))
                            .on_hover_text(
                                "DAA score of the virtual block minus the accepting block's",
                            );
                        });
                    }
                    TxStatus::Mempool { .. } => {
                        kv(ui, "Block", "— (waiting to be mined)");
                    }
                    TxStatus::InBlock { block, time_ms } => {
                        hash_row(ui, "Block", Some(block));
                        kv(ui, "Block time", when(*time_ms));
                    }
                }
                kv(ui, "Type", tx_type(view.is_coinbase, view.protocol));
                kv(
                    ui,
                    "Fee",
                    or_dash(view.fee, |f| {
                        format!("{} ({} sompi)", kas(f), format_number(f))
                    }),
                );
                kv(ui, "Mass", format_number(view.mass));
            });
            subheader(right, "Details");
            kv_grid(right, "tx_amounts", |ui| {
                kv(
                    ui,
                    "Inputs",
                    format!(
                        "{} ({})",
                        view.inputs.len(),
                        or_dash(view.input_total(), kas)
                    ),
                );
                kv(
                    ui,
                    "Outputs",
                    format!("{} ({})", view.outputs.len(), kas(view.output_total())),
                );
                kv(ui, "Version", or_dash(view.version, |v| v.to_string()));
                kv(ui, "Lock time", or_dash(view.lock_time, format_number));
                kv_with(ui, "Subnetwork", |ui| match &view.subnetwork_id {
                    Some(id) => copy_value(ui, id, "Copy subnetwork id"),
                    None => {
                        ui.label("—");
                    }
                });
                kv_with(ui, "Payload", |ui| match &view.payload {
                    Some(payload) if payload.is_empty() => {
                        ui.label("empty");
                    }
                    Some(payload) => match view.payload_text() {
                        Some(text) => copy_value(ui, &text, "Copy payload"),
                        None => {
                            let hex: String = payload.iter().map(|b| format!("{b:02x}")).collect();
                            copy_value(ui, &hex, "Copy payload (hex)");
                        }
                    },
                    None => {
                        ui.label(RichText::new("— (not kept by the index)").weak());
                    }
                });
            });
        });
    }
}

/// The inputs table: spent outpoint, address and amount (resolved from the index).
pub(super) fn tx_inputs(ui: &mut Ui, view: &TxView) {
    if view.is_coinbase {
        placeholder(ui, "Coinbase: no inputs, the reward is newly minted.");
        return;
    }
    if view.inputs.is_empty() {
        placeholder(ui, "No inputs");
        return;
    }
    ui.push_id("tx_inputs", |ui| {
        let row_height = table_row_height(ui);
        let table = page_table(ui, TABLE_MAX_HEIGHT)
            .column(Column::auto().at_least(24.0))
            .column(Column::remainder().at_least(120.0))
            .column(Column::remainder().at_least(120.0))
            .column(Column::auto().at_least(100.0));
        table_header(table, &["#", "Spends output", "Address", "Amount (KAS)"]).body(|body| {
            body.rows(row_height, view.inputs.len(), |mut row| {
                let i = row.index();
                let input = &view.inputs[i];
                row.col(|ui| {
                    ui.label(RichText::new(i.to_string()).weak());
                });
                row.col(|ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    transaction_id(ui, &input.prev_txid);
                    ui.label(RichText::new(format!(":{}", input.prev_index)).weak());
                });
                row.col(|ui| match &input.address {
                    Some(addr) => address_widget(ui, addr),
                    None => {
                        ui.label(RichText::new("—").weak())
                            .on_hover_text("The spent output isn't in the index");
                    }
                });
                row.col(|ui| {
                    ui.label(or_dash(input.amount, |a| format_kas(a as f64, 8)));
                });
            });
        });
    });
}

/// The outputs table: address, amount and a change or script-class note.
pub(super) fn tx_outputs(ui: &mut Ui, view: &TxView) {
    if view.outputs.is_empty() {
        placeholder(ui, "No outputs");
        return;
    }
    ui.push_id("tx_outputs", |ui| {
        let row_height = table_row_height(ui);
        let table = page_table(ui, TABLE_MAX_HEIGHT)
            .column(Column::auto().at_least(24.0))
            .column(Column::remainder().at_least(160.0))
            .column(Column::auto().at_least(100.0))
            .column(Column::auto().at_least(70.0));
        table_header(table, &["#", "Address", "Amount (KAS)", "Note"]).body(|body| {
            body.rows(row_height, view.outputs.len(), |mut row| {
                let i = row.index();
                let output = &view.outputs[i];
                row.col(|ui| {
                    ui.label(RichText::new(i.to_string()).weak());
                });
                row.col(|ui| match &output.address {
                    Some(addr) => address_widget(ui, addr),
                    None => {
                        ui.label(RichText::new("non-standard script").weak());
                    }
                });
                row.col(|ui| {
                    ui.label(format_kas(output.amount as f64, 8));
                });
                row.col(|ui| {
                    if output.change >= CHANGE_THRESHOLD {
                        ui.label(RichText::new("change?").color(theme::WARN))
                            .on_hover_text(format!(
                                "Likely the sender's change ({}% by the index's heuristics)",
                                output.change
                            ));
                    } else if let Some(class) = output.script_class {
                        ui.label(RichText::new(class).weak());
                    }
                });
            });
        });
    });
}
