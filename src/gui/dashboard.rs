use eframe::egui::{self, RichText, Ui};

use super::theme;
use super::widgets::{card, direct_node_placeholder, kv, kv_grid, placeholder};
use crate::app::App;
use crate::format::{format_hashrate, format_usd};
use crate::rpc::types::{format_number, sompi_to_kas};

pub fn show(ui: &mut Ui, app: &App) {
    egui::ScrollArea::vertical().show(ui, |ui| {
        ui.columns(2, |cols| {
            card(&mut cols[0], "Node Info", |ui| node_info(ui, app));
            cols[0].add_space(8.0);
            card(&mut cols[0], "Markets", |ui| markets(ui, app));

            card(&mut cols[1], "Network Stats", |ui| network_stats(ui, app));
            cols[1].add_space(8.0);
            card(&mut cols[1], "Mempool & Fees", |ui| mempool_summary(ui, app));
        });
        ui.add_space(8.0);
        card(ui, "Mining Info", |ui| mining_info(ui, app));
    });
}

fn syncing(ui: &mut Ui) {
    ui.label(RichText::new("Node is syncing…").color(theme::WARN));
}

fn yes_no(value: bool) -> &'static str {
    if value { "Yes" } else { "No" }
}

fn node_info(ui: &mut Ui, app: &App) {
    let Some(ref info) = app.node.server_info else {
        placeholder(ui, "Waiting for data…");
        return;
    };
    kv_grid(ui, "node_info", |ui| {
        kv(ui, "Version", &info.server_version);
        kv(ui, "Network", &info.network_id);
        let synced_color = if info.is_synced { theme::OK } else { theme::ERROR };
        kv(
            ui,
            "Synced",
            RichText::new(yes_no(info.is_synced)).color(synced_color),
        );
        kv(ui, "UTXO Index", yes_no(info.has_utxo_index));
        if app.is_daemon_active() {
            kv(ui, "Mode", RichText::new("Embedded Node").color(theme::OK));
        }
        if let Some(ref url) = app.node.node_url {
            kv(ui, "URL", url);
        }
        if let Some(ref uid) = app.node.node_uid {
            kv(ui, "Node ID", uid);
        }
    });
}

fn network_stats(ui: &mut Ui, app: &App) {
    if app.is_node_syncing() {
        syncing(ui);
        if let Some(ref info) = app.node.server_info {
            kv_grid(ui, "network_syncing", |ui| {
                kv(ui, "DAA Score", format_number(info.virtual_daa_score));
            });
        }
        return;
    }

    let Some(ref dag) = app.node.dag_info else {
        placeholder(ui, "Waiting for data…");
        return;
    };
    kv_grid(ui, "network_stats", |ui| {
        kv(ui, "Block Count", format_number(dag.block_count));
        kv(ui, "Header Count", format_number(dag.header_count));
        kv(ui, "Difficulty", format!("{:.0}", dag.difficulty));
        kv(ui, "DAA Score", format_number(dag.virtual_daa_score));
        kv(ui, "Tips", dag.tip_hashes.len().to_string());

        if let Some(ref supply) = app.node.coin_supply {
            let max_kas = sompi_to_kas(supply.max_sompi);
            let circ_kas = sompi_to_kas(supply.circulating_sompi);
            let pct = if max_kas > 0.0 {
                (circ_kas / max_kas) * 100.0
            } else {
                0.0
            };
            kv(ui, "Max Supply", format!("{} KAS", format_number(max_kas as u64)));
            kv(ui, "Circulating", format!("{} KAS", format_number(circ_kas as u64)));
            kv(ui, "% Circulating", format!("{pct:.2}%"));
        }
    });
}

fn markets(ui: &mut Ui, app: &App) {
    let Some(ref market) = app.market_data else {
        placeholder(ui, "Fetching market data…");
        return;
    };
    let change = market.price_change_24h_pct;
    let (change_text, change_color) = if change >= 0.0 {
        (format!("+{change:.2}%"), theme::OK)
    } else {
        (format!("{change:.2}%"), theme::ERROR)
    };
    kv_grid(ui, "markets", |ui| {
        ui.label(RichText::new("Price (USD)").weak());
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("${:.6}", market.price_usd)).strong());
            ui.label(RichText::new(change_text).color(change_color));
        });
        ui.end_row();
        kv(ui, "Price (BTC)", format!("{:.10}", market.price_btc));
        kv(ui, "Market Cap", format_usd(market.market_cap));
        kv(ui, "24h Volume", format_usd(market.volume_24h));
    });
}

fn mempool_summary(ui: &mut Ui, app: &App) {
    if app.is_node_syncing() {
        syncing(ui);
        return;
    }
    let Some(ref mempool) = app.node.mempool_state else {
        placeholder(ui, "Waiting for data…");
        return;
    };
    kv_grid(ui, "mempool_summary", |ui| {
        kv(ui, "Transactions", format_number(mempool.entry_count as u64));
        kv(
            ui,
            "Total Fees",
            format!("{:.8} KAS", sompi_to_kas(mempool.total_fees)),
        );
        if let Some(ref fee) = app.node.fee_estimate {
            kv(ui, "Priority Fee", &fee.priority_bucket);
            if let Some(normal) = fee.normal_buckets.first() {
                kv(ui, "Normal Fee", normal);
            }
            if let Some(low) = fee.low_buckets.first() {
                kv(ui, "Low Fee", low);
            }
        }
    });
}

fn mining_info(ui: &mut Ui, app: &App) {
    if app.is_node_syncing() {
        syncing(ui);
        return;
    }
    let Some(ref mining) = app.node.mining_info else {
        placeholder(ui, direct_node_placeholder(app, "Collecting mining data…"));
        return;
    };
    kv_grid(ui, "mining_info", |ui| {
        kv(
            ui,
            "Hashrate",
            RichText::new(format_hashrate(mining.hashrate)).strong(),
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
        ui.label(RichText::new("Top Miners").weak());
        kv_grid(ui, "top_miners", |ui| {
            for (addr, count) in &mining.top_miners {
                ui.label(RichText::new(addr).monospace());
                ui.label(format!("{count} blocks"));
                ui.end_row();
            }
        });
    }
}
