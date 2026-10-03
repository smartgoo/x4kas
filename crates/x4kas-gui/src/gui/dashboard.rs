use eframe::egui::{self, RichText, Ui};

use super::theme;
use super::widgets::{CARD_GAP, card, kv, kv_grid, kv_with, or_dash, placeholder, yes_no};
use x4kas_core::app::App;
use x4kas_core::format::{format_hashrate, format_kas, format_number, format_usd};

pub fn show(ui: &mut Ui, app: &App) {
    egui::ScrollArea::vertical().show(ui, |ui| {
        ui.columns(2, |cols| {
            card(&mut cols[0], "Node Info", |ui| node_info(ui, app));
            card(&mut cols[1], "Markets", |ui| markets(ui, app));
        });
        ui.add_space(CARD_GAP);
        ui.columns(2, |cols| {
            card(&mut cols[0], "Network Stats", |ui| network_stats(ui, app));
            card(&mut cols[1], "Mempool & Fees", |ui| {
                mempool_summary(ui, app)
            });
        });
    });
}

fn node_info(ui: &mut Ui, app: &App) {
    let Some(ref info) = app.node.server_info else {
        placeholder(ui, "Waiting for data…");
        return;
    };
    kv_grid(ui, "node_info", |ui| {
        kv(ui, "Version", &info.server_version);
        kv(ui, "Network", &info.network_id);
        let synced_color = if info.is_synced {
            theme::OK
        } else {
            theme::ERROR
        };
        kv(
            ui,
            "Synced",
            RichText::new(yes_no(info.is_synced)).color(synced_color),
        );
        kv(ui, "UTXO Index", yes_no(info.has_utxo_index));
        if let Some(ref dag) = app.node.dag_info {
            kv(ui, "Block Count", format_number(dag.block_count));
            kv(ui, "Header Count", format_number(dag.header_count));
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
    let Some(ref dag) = app.node.dag_info else {
        placeholder(ui, "Waiting for data…");
        return;
    };
    kv_grid(ui, "network_stats", |ui| {
        kv(ui, "Difficulty", format_number(dag.difficulty as u64));
        kv(ui, "Hashrate", or_dash(app.node.hashrate, format_hashrate));
        kv(ui, "DAA Score", format_number(dag.virtual_daa_score));
        kv(ui, "Tips", dag.tip_hashes.len().to_string());

        if let Some(ref supply) = app.node.coin_supply {
            let (max, circ) = (supply.max_sompi as f64, supply.circulating_sompi as f64);
            let pct = if max > 0.0 { circ / max * 100.0 } else { 0.0 };
            kv(ui, "Max Supply", format!("{} KAS", format_kas(max, 0)));
            kv(ui, "Circulating", format!("{} KAS", format_kas(circ, 0)));
            kv(ui, "% Circulating", format!("{pct:.2}%"));
        }
    });
}

fn markets(ui: &mut Ui, app: &App) {
    let Some(ref market) = app.market_data else {
        placeholder(ui, "Fetching market data…");
        return;
    };
    kv_grid(ui, "markets", |ui| {
        kv_with(ui, "Price (USD)", |ui| {
            // Right to left: the change first, so it ends up after the price.
            if let Some(change) = market.price_change_24h_pct {
                let color = if change >= 0.0 {
                    theme::OK
                } else {
                    theme::ERROR
                };
                ui.label(RichText::new(format!("(24h {change:+.2}%)")).color(color));
            }
            ui.label(
                RichText::new(format!("${:.6}", market.price_usd)).color(theme::ACCENT_BRIGHT),
            );
        });
        kv(ui, "Price (BTC)", format!("{:.10}", market.price_btc));
        kv(ui, "Market Cap", format_usd(market.market_cap));
        kv(ui, "24h Volume", format_usd(market.volume_24h));
    });
}

fn mempool_summary(ui: &mut Ui, app: &App) {
    let Some(ref mempool) = app.node.mempool_state else {
        placeholder(ui, "Waiting for data…");
        return;
    };
    kv_grid(ui, "mempool_summary", |ui| {
        kv(
            ui,
            "Transactions",
            format_number(mempool.entries.len() as u64),
        );
        kv(
            ui,
            "Total Fees",
            format!("{} KAS", format_kas(mempool.total_fees as f64, 8)),
        );
        if let Some(ref fee) = app.node.fee_estimate {
            let rate = |r: f64| format!("{r:.2} sompi/gram");
            kv(ui, "Priority Fee", rate(fee.priority_feerate));
            if let Some(normal) = fee.normal_feerate {
                kv(ui, "Normal Fee", rate(normal));
            }
            if let Some(low) = fee.low_feerate {
                kv(ui, "Low Fee", rate(low));
            }
        }
    });
}
