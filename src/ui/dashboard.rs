use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::App;
use crate::format::{format_hashrate, format_usd};
use crate::rpc::types::{format_number, sompi_to_kas};

pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(35),
            Constraint::Percentage(35),
            Constraint::Percentage(30),
        ])
        .split(area);

    let top = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[0]);

    let middle = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[1]);

    render_node_info(frame, top[0], app);
    render_network_stats(frame, top[1], app);
    render_markets(frame, middle[0], app);
    render_mempool_summary(frame, middle[1], app);
    render_mining_info(frame, rows[2], app);
}

fn render_node_info(frame: &mut Frame, area: Rect, app: &App) {
    let lines = if let Some(ref info) = app.node.server_info {
        let synced_style = if info.is_synced {
            Style::default().fg(Color::Green)
        } else {
            Style::default().fg(Color::Red)
        };

        let mut lines = vec![
            Line::from(vec![
                Span::styled(" Version:     ", Style::default().fg(Color::DarkGray)),
                Span::raw(&info.server_version),
            ]),
            Line::from(vec![
                Span::styled(" Network:     ", Style::default().fg(Color::DarkGray)),
                Span::raw(&info.network_id),
            ]),
            Line::from(vec![
                Span::styled(" Synced:      ", Style::default().fg(Color::DarkGray)),
                Span::styled(if info.is_synced { "Yes" } else { "No" }, synced_style),
            ]),
            Line::from(vec![
                Span::styled(" UTXO Index:  ", Style::default().fg(Color::DarkGray)),
                Span::raw(if info.has_utxo_index { "Yes" } else { "No" }),
            ]),
        ];

        if app.is_daemon_active() {
            lines.push(Line::from(vec![
                Span::styled(" Mode:        ", Style::default().fg(Color::DarkGray)),
                Span::styled("Embedded Node", Style::default().fg(Color::Green)),
            ]));
        }

        if let Some(ref url) = app.node.node_url {
            lines.push(Line::from(vec![
                Span::styled(" URL:         ", Style::default().fg(Color::DarkGray)),
                Span::raw(url),
            ]));
        }
        if let Some(ref uid) = app.node.node_uid {
            lines.push(Line::from(vec![
                Span::styled(" Node ID:     ", Style::default().fg(Color::DarkGray)),
                Span::raw(uid),
            ]));
        }

        lines
    } else {
        vec![Line::from(Span::styled(
            " Waiting for data...",
            Style::default().fg(Color::DarkGray),
        ))]
    };

    let block = Block::default().borders(Borders::ALL).title(Span::styled(
        " Node Info ",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ));

    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, area);
}

fn render_network_stats(frame: &mut Frame, area: Rect, app: &App) {
    if app.is_node_syncing() {
        let daa = app
            .node.server_info
            .as_ref()
            .map(|s| format_number(s.virtual_daa_score))
            .unwrap_or_default();
        let lines = vec![
            Line::from(Span::styled(
                " Node is syncing...",
                Style::default().fg(Color::Yellow),
            )),
            Line::from(vec![
                Span::styled(" DAA Score:      ", Style::default().fg(Color::DarkGray)),
                Span::raw(daa),
            ]),
        ];
        let block = Block::default().borders(Borders::ALL).title(Span::styled(
            " Network Stats ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
        frame.render_widget(Paragraph::new(lines).block(block), area);
        return;
    }

    let mut lines = if let Some(ref dag) = app.node.dag_info {
        vec![
            Line::from(vec![
                Span::styled(" Block Count:    ", Style::default().fg(Color::DarkGray)),
                Span::raw(format_number(dag.block_count)),
            ]),
            Line::from(vec![
                Span::styled(" Header Count:   ", Style::default().fg(Color::DarkGray)),
                Span::raw(format_number(dag.header_count)),
            ]),
            Line::from(vec![
                Span::styled(" Difficulty:     ", Style::default().fg(Color::DarkGray)),
                Span::raw(format!("{:.0}", dag.difficulty)),
            ]),
            Line::from(vec![
                Span::styled(" DAA Score:      ", Style::default().fg(Color::DarkGray)),
                Span::raw(format_number(dag.virtual_daa_score)),
            ]),
            Line::from(vec![
                Span::styled(" Tips:           ", Style::default().fg(Color::DarkGray)),
                Span::raw(dag.tip_hashes.len().to_string()),
            ]),
        ]
    } else {
        vec![Line::from(Span::styled(
            " Waiting for data...",
            Style::default().fg(Color::DarkGray),
        ))]
    };

    if let Some(ref supply) = app.node.coin_supply {
        let max_kas = sompi_to_kas(supply.max_sompi);
        let circ_kas = sompi_to_kas(supply.circulating_sompi);
        let pct = if max_kas > 0.0 {
            (circ_kas / max_kas) * 100.0
        } else {
            0.0
        };

        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled(" Max Supply:     ", Style::default().fg(Color::DarkGray)),
            Span::raw(format!("{} KAS", format_number(max_kas as u64))),
        ]));
        lines.push(Line::from(vec![
            Span::styled(" Circulating:    ", Style::default().fg(Color::DarkGray)),
            Span::raw(format!("{} KAS", format_number(circ_kas as u64))),
        ]));
        lines.push(Line::from(vec![
            Span::styled(" % Circulating:  ", Style::default().fg(Color::DarkGray)),
            Span::raw(format!("{:.2}%", pct)),
        ]));
    }

    let block = Block::default().borders(Borders::ALL).title(Span::styled(
        " Network Stats ",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ));

    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_markets(frame: &mut Frame, area: Rect, app: &App) {
    let lines = if let Some(ref market) = app.market_data {
        let change_color = if market.price_change_24h_pct >= 0.0 {
            Color::Green
        } else {
            Color::Red
        };
        let change_prefix = if market.price_change_24h_pct >= 0.0 {
            "+"
        } else {
            ""
        };

        vec![
            Line::from(vec![
                Span::styled(" Price (USD):   ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("${:.6}", market.price_usd),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled(
                    format!("{}{:.2}%", change_prefix, market.price_change_24h_pct),
                    Style::default().fg(change_color),
                ),
            ]),
            Line::from(vec![
                Span::styled(" Price (BTC):   ", Style::default().fg(Color::DarkGray)),
                Span::raw(format!("{:.10}", market.price_btc)),
            ]),
            Line::from(vec![
                Span::styled(" Market Cap:    ", Style::default().fg(Color::DarkGray)),
                Span::raw(format_usd(market.market_cap)),
            ]),
            Line::from(vec![
                Span::styled(" 24h Volume:    ", Style::default().fg(Color::DarkGray)),
                Span::raw(format_usd(market.volume_24h)),
            ]),
        ]
    } else {
        vec![Line::from(Span::styled(
            " Fetching market data...",
            Style::default().fg(Color::DarkGray),
        ))]
    };

    let block = Block::default().borders(Borders::ALL).title(Span::styled(
        " Markets ",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ));

    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_mining_info(frame: &mut Frame, area: Rect, app: &App) {
    if app.is_node_syncing() {
        let block = Block::default().borders(Borders::ALL).title(Span::styled(
            " Mining Info ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
        let msg = Paragraph::new(Line::from(Span::styled(
            " Node is syncing...",
            Style::default().fg(Color::Yellow),
        )))
        .block(block);
        frame.render_widget(msg, area);
        return;
    }

    let lines = if let Some(ref mining) = app.node.mining_info {
        let hashrate_str = format_hashrate(mining.hashrate);
        let mut lines = vec![
            Line::from(vec![
                Span::styled(" Hashrate:        ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    hashrate_str,
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                Span::styled(" Unique Miners:   ", Style::default().fg(Color::DarkGray)),
                Span::raw(format!(
                    "{} (last {} blocks)",
                    mining.unique_miners, mining.blocks_analyzed
                )),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                " Top Miners:",
                Style::default().fg(Color::DarkGray),
            )),
        ];

        for (addr, count) in &mining.top_miners {
            lines.push(Line::from(vec![
                Span::raw("   "),
                Span::styled(addr, Style::default().fg(Color::White)),
                Span::raw(format!("  ({} blocks)", count)),
            ]));
        }

        lines
    } else if !app.has_direct_node {
        vec![Line::from(Span::styled(
            " Disabled when using Kaspa PNN via Resolver",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        vec![Line::from(Span::styled(
            " Collecting mining data...",
            Style::default().fg(Color::DarkGray),
        ))]
    };

    let block = Block::default().borders(Borders::ALL).title(Span::styled(
        " Mining Info ",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ));

    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_mempool_summary(frame: &mut Frame, area: Rect, app: &App) {
    if app.is_node_syncing() {
        let block = Block::default().borders(Borders::ALL).title(Span::styled(
            " Mempool & Fees ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
        let msg = Paragraph::new(Line::from(Span::styled(
            " Node is syncing...",
            Style::default().fg(Color::Yellow),
        )))
        .block(block);
        frame.render_widget(msg, area);
        return;
    }

    let mut lines = Vec::new();

    if let Some(ref mempool) = app.node.mempool_state {
        lines.push(Line::from(vec![
            Span::styled(" Transactions: ", Style::default().fg(Color::DarkGray)),
            Span::raw(format_number(mempool.entry_count as u64)),
        ]));
        lines.push(Line::from(vec![
            Span::styled(" Total Fees:   ", Style::default().fg(Color::DarkGray)),
            Span::raw(format!("{:.8} KAS", sompi_to_kas(mempool.total_fees))),
        ]));
    } else {
        lines.push(Line::from(Span::styled(
            " Waiting for data...",
            Style::default().fg(Color::DarkGray),
        )));
    }

    if let Some(ref fee) = app.node.fee_estimate {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled(" Priority Fee: ", Style::default().fg(Color::DarkGray)),
            Span::raw(&fee.priority_bucket),
        ]));
        if let Some(normal) = fee.normal_buckets.first() {
            lines.push(Line::from(vec![
                Span::styled(" Normal Fee:   ", Style::default().fg(Color::DarkGray)),
                Span::raw(normal),
            ]));
        }
        if let Some(low) = fee.low_buckets.first() {
            lines.push(Line::from(vec![
                Span::styled(" Low Fee:      ", Style::default().fg(Color::DarkGray)),
                Span::raw(low),
            ]));
        }
    }

    let block = Block::default().borders(Borders::ALL).title(Span::styled(
        " Mempool & Fees ",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ));

    frame.render_widget(Paragraph::new(lines).block(block), area);
}
