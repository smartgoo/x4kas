use eframe::egui::{self, RichText, Sense, Ui};
use egui_extras::{Column, TableBuilder};

use super::widgets::{card, kv, kv_grid, placeholder, syncing_guard};
use crate::app::App;
use crate::rpc::types::sompi_to_kas;

pub fn show(ui: &mut Ui, app: &mut App) {
    if syncing_guard(ui, app, "Mempool") {
        return;
    }

    card(ui, "Mempool Summary", |ui| summary(ui, app));
    ui.add_space(8.0);

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
        .header(22.0, |mut header| {
            header.col(|ui| {
                ui.strong("Transaction ID");
            });
            header.col(|ui| {
                ui.strong("Fee (KAS)");
            });
            header.col(|ui| {
                ui.strong("Orphan");
            });
        })
        .body(|body| {
            body.rows(20.0, mempool.entries.len(), |mut row| {
                let i = row.index();
                let entry = &mempool.entries[i];
                row.set_selected(i == app.mempool_selected);
                row.col(|ui| {
                    ui.label(RichText::new(&entry.transaction_id).monospace());
                });
                row.col(|ui| {
                    ui.label(format!("{:.8}", sompi_to_kas(entry.fee)));
                });
                row.col(|ui| {
                    ui.label(if entry.is_orphan { "Yes" } else { "No" });
                });
                if row.response().clicked() {
                    clicked = Some(i);
                }
            });
        });

    if let Some(i) = clicked {
        app.mempool_selected = i;
        app.open_mempool_detail(i);
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
            RichText::new(mempool.entry_count.to_string()).strong(),
        );
        kv(ui, "Orphans", orphan_count.to_string());
        kv(
            ui,
            "Total Fees",
            format!("{:.8} KAS", sompi_to_kas(mempool.total_fees)),
        );
    });
}

fn detail_window(ctx: &egui::Context, app: &mut App) {
    let Some(ref detail) = app.mempool_detail else {
        return;
    };
    let mut open = true;
    egui::Window::new("Transaction Detail")
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_width(560.0)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.add(
                egui::TextEdit::multiline(&mut detail.as_str())
                    .code_editor()
                    .desired_width(f32::INFINITY),
            );
        });
    if !open || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        app.mempool_detail = None;
    }
}
