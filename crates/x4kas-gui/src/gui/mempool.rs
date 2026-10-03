use eframe::egui::{self, RichText, Sense, Ui};
use egui_extras::{Column, TableBuilder};

use super::theme;
use super::widgets::{
    CARD_GAP, card, copy_value, kv, kv_grid, kv_with, modal_window, placeholder, section_title,
    yes_no,
};
use x4kas_core::app::App;
use x4kas_core::format::{format_kas, format_number};
use x4kas_core::rpc::types::MempoolEntryInfo;

pub fn show(ui: &mut Ui, app: &mut App) {
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
            let open_tx = app.mempool_open.as_ref().map(|e| &e.transaction_id);
            body.rows(16.0, mempool.entries.len(), |mut row| {
                let i = row.index();
                let entry = &mempool.entries[i];
                // Only the transaction open in the detail window is highlighted.
                row.set_selected(open_tx == Some(&entry.transaction_id));
                row.col(|ui| {
                    ui.label(&entry.transaction_id);
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

    if let Some(i) = clicked {
        app.open_mempool_entry(i);
    }

    detail_window(ui.ctx(), app);
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

fn detail_window(ctx: &egui::Context, app: &mut App) {
    let Some(ref entry) = app.mempool_open else {
        return;
    };
    let window = egui::Window::new("Transaction Detail").default_width(560.0);
    let open = modal_window(ctx, window, |ui| detail(ui, entry));
    if !open {
        app.mempool_open = None;
    }
}

fn detail(ui: &mut Ui, entry: &MempoolEntryInfo) {
    kv_grid(ui, "mempool_detail", |ui| {
        kv_with(ui, "Transaction ID", |ui| {
            copy_value(ui, &entry.transaction_id, "Copy transaction ID");
        });
        kv(
            ui,
            "Fee",
            format!(
                "{} KAS ({} sompi)",
                format_kas(entry.fee as f64, 8),
                format_number(entry.fee)
            ),
        );
        kv_with(ui, "Orphan", |ui| orphan_label(ui, entry.is_orphan));
    });
}
