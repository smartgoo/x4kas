//! The action bar over an address, block or transaction page: a row of flat buttons,
//! like an application's menu bar, at the top of the Explorer's page and of the info
//! pane. An Open menu (the web explorers, an address's flow graph) first, then what
//! an address can be given (its label, and the watchlist until it is on it: dialogs,
//! see `gui/dialogs.rs`), a watched address's Watchlist menu (settings, removal) and
//! its export, or a block's export. It needs only the page (not its data), so it is
//! there while the page loads or when it wasn't found.

use eframe::egui::containers::menu::MenuButton;
use eframe::egui::{self, Button, OpenUrl, Ui};

use super::address::open_flow_graph;
use super::dialogs;
use super::theme;
use super::widgets::{CARD_GAP, is_testnet};
use x4kas_core::app::App;
use x4kas_core::controller::{CommandSender, ExportRequest, UiCommand};
use x4kas_core::explorer::{ExplorerPage, PageData, PageLoad};
use x4kas_core::format::{
    explorer_address_url, explorer_block_url, explorer_tx_url, kaspa_stream_address_url,
    kaspa_stream_block_url, kaspa_stream_tx_url,
};
use x4kas_core::index::export::ExportFormat;

/// Draw the bar for `page`, if it is a page with actions (an address, a block or a
/// transaction), with a rule under it and the gap to the first card.
pub fn bar(ui: &mut Ui, app: &mut App, page: &ExplorerPage, cmd_tx: &CommandSender) {
    let testnet = is_testnet(ui.ctx());
    // Kaspa Stream only covers mainnet.
    let (explorer, stream) = match page {
        ExplorerPage::Address(addr) => (
            explorer_address_url(addr),
            (!testnet).then(|| kaspa_stream_address_url(addr)),
        ),
        ExplorerPage::Block(hash) => (
            explorer_block_url(hash, testnet),
            (!testnet).then(|| kaspa_stream_block_url(hash)),
        ),
        ExplorerPage::Transaction { txid, .. } => (
            explorer_tx_url(txid, testnet),
            (!testnet).then(|| kaspa_stream_tx_url(txid)),
        ),
        ExplorerPage::Home | ExplorerPage::Lookup(_) => return,
    };
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        // Open: the web explorers, and for an address its flow graph.
        let mut flows = false;
        MenuButton::from_button(action_button("Open ▾")).ui(ui, |ui| {
            menu_link(ui, "Kaspa Explorer ↗", &explorer);
            if let Some(stream) = &stream {
                menu_link(ui, "Kaspa Stream ↗", stream);
            }
            if matches!(page, ExplorerPage::Address(_)) {
                ui.separator();
                // The flow graph reads the index, which only a direct node fills.
                let direct = app.connection.is_direct();
                if ui
                    .add_enabled(direct, Button::new("Flow graph"))
                    .on_hover_text("Follow the money: counterparties of counterparties")
                    .on_disabled_hover_text(
                        "Needs a direct node: the flow graph reads the address index",
                    )
                    .clicked()
                {
                    flows = true;
                    ui.close();
                }
            }
        });
        match page {
            ExplorerPage::Address(addr) => {
                if flows {
                    open_flow_graph(app, addr, cmd_tx);
                }
                ui.separator();
                address_actions(ui, app, addr, cmd_tx);
            }
            ExplorerPage::Block(_) => {
                ui.separator();
                block_export(ui, app, page, cmd_tx);
            }
            _ => {}
        }
    });
    let rule_y = ui.cursor().top() + 2.0;
    ui.painter().hline(
        ui.max_rect().x_range(),
        rule_y,
        egui::Stroke::new(1.0_f32, theme::BORDER),
    );
    ui.add_space(CARD_GAP);
}

/// The address's own actions: the Add menu (a label; the watchlist while it isn't on
/// it), the Watchlist menu once it is (its settings, removal) and export.
fn address_actions(ui: &mut Ui, app: &mut App, addr: &str, cmd_tx: &CommandSender) {
    // What the address can be given: a label, and a watchlist entry while it has none
    // (once watched, the Watchlist menu takes over). The items say add or edit by what
    // it has; the button says "Add…" until everything in it is an edit.
    let labelled = app.labels.user_labels().contains_key(addr);
    let watched = app.watch.entry(addr).is_some();
    let menu = if labelled && watched {
        "Edit ▾"
    } else {
        "Add ▾"
    };
    let label = if labelled { "Edit label" } else { "Add label" };
    let (response, _) = MenuButton::from_button(action_button(menu)).ui(ui, |ui| {
        if ui
            .button(label)
            .on_hover_text("Your own name for this address, shown wherever it appears")
            .clicked()
        {
            dialogs::request_label(ui.ctx(), addr);
            ui.close();
        }
        if !watched
            && ui
                .button("Add to watchlist")
                .on_hover_text("Follow its balance and get alerts on activity")
                .clicked()
        {
            dialogs::request_watch(ui.ctx(), addr);
            ui.close();
        }
    });
    response.on_hover_text(if watched {
        "A label of your own"
    } else {
        "A label of your own, or a place on the watchlist"
    });
    if watched {
        ui.separator();
        let mut remove = false;
        let (response, _) = MenuButton::from_button(action_button("Watchlist ▾")).ui(ui, |ui| {
            if ui
                .button("Edit settings")
                .on_hover_text("Alerts on or off, and the rules that raise them")
                .clicked()
            {
                dialogs::request_watch(ui.ctx(), addr);
                ui.close();
            }
            if ui
                .button("Remove")
                .on_hover_text("Stop following this address; its events stay listed")
                .clicked()
            {
                remove = true;
                ui.close();
            }
        });
        response.on_hover_text("This address is on the watchlist");
        if remove {
            let mut list = app.watch.list.clone();
            list.entries.retain(|e| e.address != addr);
            let _ = cmd_tx.send(UiCommand::WatchSet(list));
        }
    }
    ui.separator();
    // Export needs the page: its transactions come from the index, so there is
    // nothing to export until it is loaded with some.
    let page = ExplorerPage::Address(addr.to_string());
    let exportable = matches!(
        app.explorer.load(&page),
        Some(PageLoad::Ready(data))
            if matches!(&**data, PageData::Address(d) if !d.view.page.items.is_empty())
    );
    ui.add_enabled_ui(exportable, |ui| {
        let (response, _) = MenuButton::from_button(action_button("Export ▾")).ui(ui, |ui| {
            for format in [ExportFormat::Csv, ExportFormat::Json] {
                let ext = format.extension().to_ascii_uppercase();
                if ui
                    .button(format!("Transactions as {ext}"))
                    .on_hover_text(format!(
                        "Every indexed transaction of this address as {ext}, to ~/.x4kas/exports"
                    ))
                    .clicked()
                {
                    let _ = cmd_tx.send(UiCommand::Export(ExportRequest::Transactions {
                        address: addr.to_string(),
                        format,
                    }));
                    ui.close();
                }
            }
        });
        response
            .on_hover_text("The indexed transactions, to ~/.x4kas/exports")
            .on_disabled_hover_text("Nothing to export until the page shows transactions");
    });
}

/// A block's Export menu: the block as the page shows it, as JSON, once it is loaded.
fn block_export(ui: &mut Ui, app: &App, page: &ExplorerPage, cmd_tx: &CommandSender) {
    let view = match app.explorer.load(page) {
        Some(PageLoad::Ready(data)) => match &**data {
            PageData::Block(view) => Some(view),
            _ => None,
        },
        _ => None,
    };
    ui.add_enabled_ui(view.is_some(), |ui| {
        let (response, _) = MenuButton::from_button(action_button("Export ▾")).ui(ui, |ui| {
            if ui
                .button("Block as JSON")
                .on_hover_text(
                    "The block as this page shows it (header, DAG standing, miner, \
                     transactions with acceptance and fees), to ~/.x4kas/exports",
                )
                .clicked()
            {
                if let Some(view) = view {
                    let _ = cmd_tx.send(UiCommand::Export(ExportRequest::Block(Box::new(
                        view.clone(),
                    ))));
                }
                ui.close();
            }
        });
        response
            .on_hover_text("The block as JSON, to ~/.x4kas/exports")
            .on_disabled_hover_text("Nothing to export until the block is loaded");
    });
}

/// A web link as a menu item: opens in the browser, the URL on hover.
fn menu_link(ui: &mut Ui, text: &str, url: &str) {
    if ui.button(text).on_hover_text(url).clicked() {
        ui.ctx().open_url(OpenUrl::new_tab(url));
        ui.close();
    }
}

/// A menu-like button: flat until hovered.
fn action_button(text: &str) -> Button<'static> {
    Button::new(text).frame_when_inactive(false)
}
