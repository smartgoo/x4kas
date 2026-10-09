//! Dashboard tab, modeled on the Kaspalytics home page: the live DAG visualizer and
//! BlockDAG card from [`blockdag`], node-backed cards (markets, supply, mining and mempool, node info, fee
//! rates) and the chain analytics cards from [`analytics`].

use eframe::egui::{self, RichText, Ui};

use super::analytics::{self, panel_card};
use super::blockdag;
use super::theme;
use super::widgets::{
    CARD_GAP, card, card_with_header, copy_value, kv, kv_columns, kv_grid, kv_with, or_dash,
    subheader, weighted_columns, yes_no,
};
use x4kas_core::app::{AnalyticsPanel, App};
use x4kas_core::format::{format_duration, format_hashrate, format_kas, format_number, format_usd};

/// Narrowest a card gets before its row wraps (see [`weighted_columns`]).
const SMALL_CARD: f32 = 320.0;
/// Narrowest a card with a list table gets before its row wraps.
const TABLE_CARD: f32 = 420.0;

pub fn show(ui: &mut Ui, app: &mut App) {
    analytics::banners(ui, app);

    egui::ScrollArea::vertical().show(ui, |ui| {
        blockdag::band(ui, app);
        ui.add_space(CARD_GAP);
        weighted_columns(ui, [2.0, 1.0], SMALL_CARD, |[left, right]| {
            card(left, "BlockDAG", |ui| blockdag::stats(ui, app));
            card(right, "Markets", |ui| markets(ui, app));
        });
        ui.add_space(CARD_GAP);
        weighted_columns(
            ui,
            [1.0; 3],
            SMALL_CARD,
            |[supply_col, mining_col, node_col]| {
                card(supply_col, "Supply", |ui| supply(ui, app));
                card(mining_col, "Mining", |ui| mining(ui, app));
                card(node_col, "Node Info", |ui| node_info(ui, app));
            },
        );
        ui.add_space(CARD_GAP);
        card_with_header(
            ui,
            "Transactions per 10 Minutes (24h)",
            app,
            |ui, app| analytics::sync_dot(ui, app),
            |ui, app| analytics::tx_chart(ui, app),
        );
        ui.add_space(CARD_GAP);
        weighted_columns(ui, [2.0, 1.0], SMALL_CARD, |[left, right]| {
            panel_card(
                left,
                app,
                "Transaction Summary",
                AnalyticsPanel::TxSummary,
                analytics::tx_summary,
            );
            card(right, "Mempool", |ui| mempool(ui, app));
        });
        ui.add_space(CARD_GAP);
        panel_card(
            ui,
            app,
            "Transaction Inspection",
            AnalyticsPanel::Inspection,
            analytics::inspection,
        );
        ui.add_space(CARD_GAP);
        card_with_header(
            ui,
            "Fees",
            app,
            |ui, app| analytics::sync_dot(ui, app),
            |ui, app| {
                kv_columns(ui, 220.0, |[rates, avg, total]| {
                    fee_rates(rates, app);
                    analytics::fee_windows(avg, total, app);
                });
            },
        );
        ui.add_space(CARD_GAP);
        weighted_columns(ui, [1.0; 2], TABLE_CARD, |[left, right]| {
            panel_card(
                left,
                app,
                "Mining Share by Node Version",
                AnalyticsPanel::NodeVersions,
                analytics::node_versions,
            );
            panel_card(
                right,
                app,
                "Top Miners",
                AnalyticsPanel::Miners,
                analytics::top_miners,
            );
        });
    });
}

// Node-backed cards always draw every row, with dashes until the data arrives, so the
// layout doesn't jump as it fills in.

/// Market data older than this (the API failing since) is marked stale.
const MARKET_STALE: std::time::Duration = std::time::Duration::from_secs(180);

fn markets(ui: &mut Ui, app: &App) {
    let market = app.market_data.as_ref();
    let age = market.and_then(|m| m.fetched_at).map(|at| at.elapsed());
    kv_grid(ui, "markets", |ui| {
        kv_with(ui, "Price (USD)", |ui| {
            // Right to left: the change first, so it ends up after the price.
            if let Some(change) = market.and_then(|m| m.price_change_24h_pct) {
                let color = if change >= 0.0 {
                    theme::OK
                } else {
                    theme::ERROR
                };
                ui.label(RichText::new(format!("(24h {change:+.2}%)")).color(color));
            }
            ui.label(
                RichText::new(or_dash(market, |m| format!("${:.6}", m.price_usd)))
                    .color(theme::ACCENT_BRIGHT),
            );
        });
        kv(
            ui,
            "Price (BTC)",
            or_dash(market, |m| format!("{:.10}", m.price_btc)),
        );
        kv(
            ui,
            "Market Cap",
            or_dash(market, |m| format_usd(m.market_cap)),
        );
        kv(
            ui,
            "24h Volume",
            or_dash(market, |m| format_usd(m.volume_24h)),
        );
    });
    // Say so when the numbers stopped updating, or never arrived.
    match (age, &app.market_error) {
        (Some(age), _) if age > MARKET_STALE => {
            ui.label(
                RichText::new(format!("Last updated {} ago", format_duration(age)))
                    .color(theme::WARN),
            )
            .on_hover_text(
                app.market_error
                    .as_deref()
                    .unwrap_or("The market data source hasn't answered since"),
            );
        }
        (None, Some(error)) => {
            ui.label(RichText::new("Market data unavailable").weak())
                .on_hover_text(error);
        }
        _ => {}
    }
}

/// Coin supply and the block reward schedule.
fn supply(ui: &mut Ui, app: &App) {
    let node = &app.node;
    let supply = node
        .coin_supply
        .as_ref()
        .map(|s| (s.max_sompi as f64, s.circulating_sompi as f64));
    let reward = node.block_reward();
    kv_grid(ui, "supply", |ui| {
        let kas = |v: f64| format!("{} KAS", format_kas(v, 0));
        kv(ui, "Circulating", or_dash(supply, |(_, circ)| kas(circ)));
        kv(ui, "Max Supply", or_dash(supply, |(max, _)| kas(max)));
        let pct = supply.map(|(max, circ)| if max > 0.0 { circ / max * 100.0 } else { 0.0 });
        kv(ui, "% Issued", or_dash(pct, |pct| format!("{pct:.2}%")));
        kv(
            ui,
            "Unspendable",
            or_dash(node.burn_balance, |b| kas(b as f64)),
        );
        kv(
            ui,
            "Block Reward",
            or_dash(reward, |r| format!("{} KAS", format_kas(r.sompi as f64, 4))),
        );
        kv(
            ui,
            "Next Reward",
            or_dash(reward, |r| {
                format!(
                    "{} KAS in {}",
                    format_kas(r.next_sompi as f64, 4),
                    format_duration(r.next_in)
                )
            }),
        );
    });
}

fn mining(ui: &mut Ui, app: &App) {
    let node = &app.node;
    kv_grid(ui, "mining", |ui| {
        let difficulty = node.dag_info.as_ref().map(|d| d.difficulty as u64);
        kv(ui, "Difficulty", or_dash(difficulty, format_number));
        kv(ui, "Hashrate", or_dash(node.hashrate, format_hashrate));
        analytics::miner_counts(ui, app);
    });
}

fn mempool(ui: &mut Ui, app: &App) {
    let mempool = app.node.mempool_state.as_ref();
    kv_grid(ui, "mempool", |ui| {
        kv(
            ui,
            "Entries",
            or_dash(mempool, |m| format_number(m.entries.len() as u64)),
        );
        kv(
            ui,
            "Total Fees",
            or_dash(mempool, |m| {
                format!("{} KAS", format_kas(m.total_fees as f64, 8))
            }),
        );
    });
}

fn node_info(ui: &mut Ui, app: &App) {
    let info = app.node.server_info.as_ref();
    kv_grid(ui, "node_info", |ui| {
        kv(ui, "Version", or_dash(info, |i| i.server_version.clone()));
        kv(ui, "Network", or_dash(info, |i| i.network_id.clone()));
        let synced = match info {
            Some(i) if i.is_synced => RichText::new(yes_no(true)).color(theme::OK),
            Some(_) => RichText::new(yes_no(false)).color(theme::ERROR),
            None => RichText::new("—"),
        };
        kv(ui, "Synced", synced);
        kv_with(ui, "UTXO Index", |ui| match info {
            Some(i) if i.has_utxo_index => {
                ui.label(yes_no(true));
            }
            Some(_) => {
                ui.label(RichText::new(yes_no(false)).color(theme::WARN))
                    .on_hover_text("The watchlist needs the node's UTXO index (--utxoindex)");
            }
            None => {
                ui.label("—");
            }
        });
        let dag = app.node.dag_info.as_ref();
        kv(
            ui,
            "Block Count",
            or_dash(dag, |d| format_number(d.block_count)),
        );
        kv(
            ui,
            "Header Count",
            or_dash(dag, |d| format_number(d.header_count)),
        );
        kv_with(ui, "URL", |ui| match app.node.node_url.as_deref() {
            Some(url) => copy_value(ui, url, "Copy URL"),
            None => {
                ui.label("—");
            }
        });
        kv_with(ui, "Node ID", |ui| match app.node.node_uid.as_deref() {
            Some(id) => copy_value(ui, id, "Copy node id"),
            None => {
                ui.label("—");
            }
        });
    });
}

/// The node's fee-rate estimate: the first column of the Fees card.
fn fee_rates(ui: &mut Ui, app: &App) {
    subheader(ui, "Fee Rates (sompi/gram)");
    let fee = app.node.fee_estimate.as_ref();
    let rate = |r: Option<f64>| or_dash(r, |r| format!("{r:.2}"));
    kv_grid(ui, "fee_rates", |ui| {
        kv(ui, "Low", rate(fee.and_then(|f| f.low_feerate)));
        kv(ui, "Normal", rate(fee.and_then(|f| f.normal_feerate)));
        kv(ui, "Priority", rate(fee.map(|f| f.priority_feerate)));
    });
}
