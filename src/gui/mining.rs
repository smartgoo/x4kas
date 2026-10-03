use eframe::egui::{RichText, Ui};

use super::theme;
use super::widgets::{
    card, direct_node_placeholder, field_label, kv, kv_grid, placeholder, syncing_guard,
};
use crate::app::App;
use crate::format::format_hashrate;

pub fn show(ui: &mut Ui, app: &App) {
    if syncing_guard(ui, app, "Mining") {
        return;
    }
    card(ui, "Mining Info", |ui| mining_info(ui, app));
}

fn mining_info(ui: &mut Ui, app: &App) {
    let Some(ref mining) = app.node.mining_info else {
        placeholder(ui, direct_node_placeholder(app, "Collecting mining data…"));
        return;
    };
    kv_grid(ui, "mining_info", |ui| {
        kv(
            ui,
            "Hashrate",
            RichText::new(format_hashrate(mining.hashrate)).color(theme::ACCENT_BRIGHT),
        );
        kv(
            ui,
            "Unique Miners",
            format!(
                "{} (last {} blocks)",
                mining.unique_miners, mining.blocks_analyzed
            ),
        );
    });
    if !mining.top_miners.is_empty() {
        ui.add_space(6.0);
        field_label(ui, "Top Miners");
        kv_grid(ui, "top_miners", |ui| {
            for (addr, count) in &mining.top_miners {
                ui.label(addr);
                ui.label(format!("{count} blocks"));
                ui.end_row();
            }
        });
    }
}
