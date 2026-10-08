use eframe::egui::{self, RichText, Sense, Ui};
use egui_extras::{Column, TableBuilder};

use super::theme;
use super::widgets::{
    CARD_GAP, card, kv, kv_grid, placeholder, request_pane, section_title, transaction_id, yes_no,
};
use x4kas_core::app::App;
use x4kas_core::explorer::ExplorerPage;
use x4kas_core::format::{format_kas, format_number};

pub fn show(ui: &mut Ui, app: &App) {
    card(ui, "Mempool Summary", |ui| summary(ui, app));
    ui.add_space(CARD_GAP);

    let Some(ref mempool) = app.node.mempool_state else {
        return;
    };
    if mempool.entries.is_empty() {
        placeholder(ui, "Mempool is empty.");
        return;
    }

    let mut clicked = None;
    TableBuilder::new(ui)
        .striped(true)
        .sense(Sense::click())
        .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
        .column(Column::remainder().at_least(200.0))
        .column(Column::auto().at_least(140.0))
        .column(Column::auto().at_least(70.0))
        .header(18.0, |mut header| {
            header.col(|ui| {
                section_title(ui, "Transaction ID");
            });
            header.col(|ui| {
                section_title(ui, "Fee (KAS)");
            });
            header.col(|ui| {
                section_title(ui, "Orphan");
            });
        })
        .body(|body| {
            // The transaction shown in the info pane is highlighted.
            let open_tx = match app.explorer.pane_page() {
                Some(ExplorerPage::Transaction { txid, .. }) => Some(txid),
                _ => None,
            };
            body.rows(16.0, mempool.entries.len(), |mut row| {
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
