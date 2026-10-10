//! Mempool tab: the summary card and the entries as a page table in a card that fills
//! the rest of the tab (scrolling inside). A row click shows the transaction's info
//! pane, which highlights the row.

use eframe::egui::{RichText, Ui};
use egui_extras::Column;

use super::theme;
use super::widgets::{
    CARD_GAP, card, kv, kv_grid, page_table, placeholder, request_pane, table_header,
    table_row_height, transaction_id, yes_no,
};
use x4kas_core::app::App;
use x4kas_core::explorer::ExplorerPage;
use x4kas_core::format::{format_kas, format_number};

pub fn show(ui: &mut Ui, app: &App) {
    card(ui, "Mempool Summary", |ui| summary(ui, app));
    ui.add_space(CARD_GAP);
    let count = app
        .node
        .mempool_state
        .as_ref()
        .map_or(0, |m| m.entries.len());
    let title = if count > 0 {
        format!("Transactions ({})", format_number(count as u64))
    } else {
        "Transactions".to_string()
    };
    card(ui, &title, |ui| transactions(ui, app));
}

/// The entries, in a table that takes the rest of the card (the card takes the rest of
/// the tab), or why there are none.
fn transactions(ui: &mut Ui, app: &App) {
    let Some(ref mempool) = app.node.mempool_state else {
        placeholder(ui, "Waiting for mempool data…");
        return;
    };
    if mempool.entries.is_empty() {
        placeholder(ui, "Mempool is empty.");
        return;
    }

    let mut clicked = None;
    ui.push_id("mempool_entries", |ui| {
        let row_height = table_row_height(ui);
        // The header row sits above the body, which gets the rest of the tab.
        let header_height = theme::ROW_HEIGHT + 4.0 + ui.spacing().item_spacing.y;
        let body_height = (ui.available_height() - header_height).max(3.0 * row_height);
        let table = page_table(ui, body_height)
            .column(Column::remainder().at_least(200.0))
            .column(Column::auto().at_least(140.0))
            .column(Column::auto().at_least(70.0));
        table_header(table, &["Transaction ID", "Fee (KAS)", "Orphan"]).body(|body| {
            // The transaction shown in the info pane is highlighted.
            let open_tx = match app.explorer.pane_page() {
                Some(ExplorerPage::Transaction { txid, .. }) => Some(txid),
                _ => None,
            };
            body.rows(row_height, mempool.entries.len(), |mut row| {
                let i = row.index();
                let entry = &mempool.entries[i];
                row.set_selected(open_tx == Some(&entry.transaction_id));
                row.col(|ui| {
                    transaction_id(ui, &entry.transaction_id);
                });
                row.col(|ui| {
                    ui.label(format_kas(entry.fee as f64, 8));
                });
                row.col(|ui| {
                    orphan_label(ui, entry.is_orphan);
                });
                if row.response().clicked() {
                    clicked = Some(i);
                }
            });
        });
    });

    // A row click shows the transaction's info pane, like a click on its id.
    if let Some(entry) = clicked.and_then(|i| mempool.entries.get(i)) {
        request_pane(ui.ctx(), ExplorerPage::transaction(&entry.transaction_id));
    }
}

fn summary(ui: &mut Ui, app: &App) {
    let Some(ref mempool) = app.node.mempool_state else {
        placeholder(ui, "Waiting for mempool data…");
        return;
    };
    let orphan_count = mempool.entries.iter().filter(|e| e.is_orphan).count();
    kv_grid(ui, "mempool_summary_tab", |ui| {
        kv(
            ui,
            "Total Entries",
            RichText::new(format_number(mempool.entries.len() as u64)).color(theme::ACCENT_BRIGHT),
        );
        kv(ui, "Orphans", orphan_count.to_string());
        kv(
            ui,
            "Total Fees",
            format!("{} KAS", format_kas(mempool.total_fees as f64, 8)),
        );
    });
}

fn orphan_label(ui: &mut Ui, is_orphan: bool) {
    let color = if is_orphan {
        theme::WARN
    } else {
        theme::TEXT_DIM
    };
    ui.label(RichText::new(yes_no(is_orphan)).color(color));
}
