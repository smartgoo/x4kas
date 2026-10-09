//! The action bar over an address, block or transaction page: a row of flat buttons,
//! like an application's menu bar, at the top of the Explorer's page and of the info
//! pane. The web explorers (one menu) first, then what can be done with an address: its label and
//! watchlist settings (dialogs, see `gui/dialogs.rs`) and its flow graph. It needs only
//! the page (not its data), so it is there while the page loads or when it wasn't found.

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
        MenuButton::from_button(action_button("Open Explorer ▾")).ui(ui, |ui| {
            menu_link(ui, "Kaspa Explorer ↗", &explorer);
            if let Some(stream) = &stream {
                menu_link(ui, "Kaspa Stream ↗", stream);
            }
        });
        if let ExplorerPage::Address(addr) = page {
            ui.separator();
            address_actions(ui, app, addr, cmd_tx);
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

/// The address's own actions: the Add/Edit menu (label, watchlist), flow graph and
/// export. Their wording follows the address's state (a label to add or edit, a
/// watchlist to join or settings to change).
fn address_actions(ui: &mut Ui, app: &mut App, addr: &str, cmd_tx: &CommandSender) {
    // One menu for what the address can be given: a label and a watchlist entry. Its
    // items say add or edit by what it has; the button says "Add…" until it has both.
    let labelled = app.labels.user_labels().contains_key(addr);
    let watched = app.watch.entry(addr).is_some();
    let menu = if labelled && watched {
        "Edit ▾"
    } else {
        "Add ▾"
    };
    let label = if labelled { "Edit label" } else { "Add label" };
    let watch = if watched {
        "Watch settings"
    } else {
        "Add to watchlist"
    };
    let watch_hint = if watched {
        "Alerts on or off, the rules, or remove it from the watchlist"
    } else {
        "Follow its balance and get alerts on activity"
    };
    let (response, _) = MenuButton::from_button(action_button(menu)).ui(ui, |ui| {
        if ui
            .button(label)
            .on_hover_text("Your own name for this address, shown wherever it appears")
            .clicked()
        {
            dialogs::request_label(ui.ctx(), addr);
            ui.close();
        }
        if ui.button(watch).on_hover_text(watch_hint).clicked() {
            dialogs::request_watch(ui.ctx(), addr);
            ui.close();
        }
    });
    response.on_hover_text("A label of your own, or a place on the watchlist");
    // The flow graph reads the index, which only a direct node fills.
    let direct = app.connection.is_direct();
    if ui
        .add_enabled(direct, action_button("Flow graph"))
        .on_hover_text("Follow the money: counterparties of counterparties")
        .on_disabled_hover_text("Needs a direct node: the flow graph reads the address index")
        .clicked()
    {
        open_flow_graph(app, addr, cmd_tx);
    }
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
