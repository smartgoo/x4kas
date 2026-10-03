use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::app::{App, DaemonStatus, IntegratedNodeState};
use crate::format::{bool_str, opt_f64, opt_str, opt_usize};

pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    match &app.integrated_node.status {
        DaemonStatus::Stopped | DaemonStatus::Error(_) => render_settings(frame, area, app),
        _ => render_running(frame, area, app),
    }
}

fn render_settings(frame: &mut Frame, area: Rect, app: &App) {
    let state = &app.integrated_node;
    let cfg = &state.config;

    let mut lines: Vec<Line<'static>> = Vec::new();

    // ── General ──
    lines.push(section_header("General"));
    append_fields(state, &mut lines, &[
        (0,  "Network",       cfg.network.clone()),
        (1,  "UTXO Index",    bool_str(cfg.utxo_index)),
        (2,  "Archival",      bool_str(cfg.archival)),
        (3,  "RAM Scale",     format!("{:.1}", cfg.ram_scale)),
        (4,  "Log Level",     cfg.log_level.clone()),
        (5,  "Async Threads", cfg.async_threads.to_string()),
        (6,  "Auto Start",    bool_str(cfg.auto_start_daemon)),
    ]);

    // ── Networking ──
    lines.push(Line::from(""));
    lines.push(section_header("Networking"));
    append_fields(state, &mut lines, &[
        (7,  "Listen",        opt_str(&cfg.listen).to_string()),
        (8,  "External IP",   opt_str(&cfg.externalip).to_string()),
        (9,  "Outbound Peers", cfg.outbound_target.to_string()),
        (10, "Max Inbound",   cfg.inbound_limit.to_string()),
        (11, "Connect Peers", if cfg.connect_peers.is_empty() { "—".to_string() } else { cfg.connect_peers.clone() }),
        (12, "Add Peers",     if cfg.add_peers.is_empty() { "—".to_string() } else { cfg.add_peers.clone() }),
        (13, "Disable UPnP",  bool_str(cfg.disable_upnp)),
        (14, "Disable DNS Seed", bool_str(cfg.disable_dns_seed)),
    ]);

    // ── Storage ──
    lines.push(Line::from(""));
    lines.push(section_header("Storage"));
    append_fields(state, &mut lines, &[
        (15, "App Dir",        cfg.app_dir.clone()),
        (16, "RocksDB Preset", cfg.rocksdb_preset.clone()),
        (17, "RocksDB WAL Dir", opt_str(&cfg.rocksdb_wal_dir).to_string()),
        (18, "RocksDB Cache MB", opt_usize(&cfg.rocksdb_cache_size)),
        (19, "Retention Days", opt_f64(&cfg.retention_period_days)),
        (20, "Reset DB",       bool_str(cfg.reset_db)),
        (21, "RPC Max Clients", cfg.rpc_max_clients.to_string()),
    ]);

    // ── Performance ──
    lines.push(Line::from(""));
    lines.push(section_header("Performance"));
    append_fields(state, &mut lines, &[
        (22, "Perf Metrics", bool_str(cfg.perf_metrics)),
    ]);

    lines.push(Line::from(""));

    // Start action
    let start_style = if state.selected_field == 23 {
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Green)
    };
    let prefix = if state.selected_field == 23 {
        " > "
    } else {
        "   "
    };
    lines.push(Line::from(Span::styled(
        format!("{}[ Start Daemon ]", prefix),
        start_style,
    )));

    lines.push(Line::from(""));

    if let DaemonStatus::Error(ref msg) = state.status {
        lines.push(Line::from(Span::styled(
            format!(" Error: {}", msg),
            Style::default().fg(Color::Red),
        )));
        lines.push(Line::from(""));
    }

    if let Some((ref msg, is_error)) = state.status_message {
        let color = if is_error { Color::Red } else { Color::Green };
        lines.push(Line::from(Span::styled(
            format!(" {}", msg),
            Style::default().fg(color),
        )));
        lines.push(Line::from(""));
    }

    lines.push(Line::from(vec![
        Span::styled(" [s]", Style::default().fg(Color::Cyan)),
        Span::styled(" Save  ", Style::default().fg(Color::DarkGray)),
        Span::styled("[r]", Style::default().fg(Color::Cyan)),
        Span::styled(" Reload  ", Style::default().fg(Color::DarkGray)),
        Span::styled("[←/→]", Style::default().fg(Color::Cyan)),
        Span::styled(" Cycle  ", Style::default().fg(Color::DarkGray)),
        Span::styled("[Enter]", Style::default().fg(Color::Cyan)),
        Span::styled(" Edit/Toggle", Style::default().fg(Color::DarkGray)),
    ]));

    // Calculate scroll offset to keep selected field visible
    let total_content_lines = lines.len();
    let inner_height = area.height.saturating_sub(2) as usize;
    let scroll = if total_content_lines > inner_height {
        // Estimate which line the selected field is on
        let field_line = estimate_field_line(state.selected_field);
        if field_line >= inner_height {
            (field_line - inner_height / 2).min(total_content_lines.saturating_sub(inner_height))
        } else {
            0
        }
    } else {
        0
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Node Settings ");
    let para = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((scroll as u16, 0));
    frame.render_widget(para, area);
}

/// Estimate which line number a field index corresponds to (for scrolling)
fn estimate_field_line(field: usize) -> usize {
    // section header + fields, with blank lines between sections
    match field {
        0..=6 => 1 + field,             // General: header at 0, fields 1-7
        7..=14 => 10 + (field - 7),     // Networking: blank+header at 8-9, fields 10-17
        15..=21 => 20 + (field - 15),   // Storage: blank+header at 18-19, fields 20-26
        22 => 29,                        // Performance: blank+header at 27-28, field 29
        23 => 32,                        // Start button
        _ => 0,
    }
}

fn section_header(title: &str) -> Line<'static> {
    Line::from(Span::styled(
        format!(" ── {} ──", title),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ))
}

fn append_fields(
    state: &IntegratedNodeState,
    lines: &mut Vec<Line<'static>>,
    fields: &[(usize, &str, String)],
) {
    for (idx, label, value) in fields {
        let selected = state.selected_field == *idx;
        let prefix = if selected { " > " } else { "   " };
        let label_style = Style::default().fg(Color::DarkGray);
        let value_style = if selected && state.editing {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else if selected {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };

        let display_value = if selected && state.editing {
            format!("{}_", state.edit_buffer)
        } else {
            value.to_string()
        };

        lines.push(Line::from(vec![
            Span::raw(prefix.to_string()),
            Span::styled(format!("{:<18}", label), label_style),
            Span::styled(display_value, value_style),
        ]));
    }
}

fn render_running(frame: &mut Frame, area: Rect, app: &App) {
    let state = &app.integrated_node;

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(7), Constraint::Min(0)])
        .split(area);

    // Status panel
    let status_text = match &state.status {
        DaemonStatus::Starting => ("Starting...", Color::Yellow),
        DaemonStatus::Running => ("Running", Color::Green),
        DaemonStatus::Stopping => ("Stopping...", Color::Yellow),
        _ => ("Unknown", Color::DarkGray),
    };

    let sync_status = if let Some(ref info) = app.node.server_info {
        if info.is_synced {
            ("Synced".to_string(), Color::Green)
        } else {
            ("Syncing...".to_string(), Color::Yellow)
        }
    } else {
        ("Waiting...".to_string(), Color::DarkGray)
    };

    let uptime = state
        .started_at
        .map(|t| {
            let secs = t.elapsed().as_secs();
            if secs >= 3600 {
                format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
            } else if secs >= 60 {
                format!("{}m {}s", secs / 60, secs % 60)
            } else {
                format!("{}s", secs)
            }
        })
        .unwrap_or_else(|| "—".to_string());

    let port = crate::daemon::DaemonHandle::wrpc_borsh_url(&state.config.network);

    let mut status_lines = vec![
        Line::from(vec![
            Span::styled(" Status:   ", Style::default().fg(Color::DarkGray)),
            Span::styled(status_text.0, Style::default().fg(status_text.1)),
            Span::raw("  "),
            Span::styled(sync_status.0, Style::default().fg(sync_status.1)),
        ]),
        Line::from(vec![
            Span::styled(" Network:  ", Style::default().fg(Color::DarkGray)),
            Span::raw(&state.config.network),
        ]),
        Line::from(vec![
            Span::styled(" wRPC:     ", Style::default().fg(Color::DarkGray)),
            Span::raw(&port),
        ]),
        Line::from(vec![
            Span::styled(" Uptime:   ", Style::default().fg(Color::DarkGray)),
            Span::raw(uptime),
        ]),
    ];

    if matches!(state.status, DaemonStatus::Running) {
        status_lines.push(Line::from(vec![
            Span::styled(" Press ", Style::default().fg(Color::DarkGray)),
            Span::styled("[Enter]", Style::default().fg(Color::Cyan)),
            Span::styled(" to stop daemon", Style::default().fg(Color::DarkGray)),
        ]));
    }

    let status_block = Block::default()
        .borders(Borders::ALL)
        .title(" Daemon Status ");
    let status_para = Paragraph::new(status_lines).block(status_block);
    frame.render_widget(status_para, rows[0]);

    // Log panel
    let log_block = Block::default()
        .borders(Borders::ALL)
        .title(" Daemon Logs (j/k scroll) ");

    let inner_height = rows[1].height.saturating_sub(2) as usize;
    let total_lines = state.log_lines.len();
    let max_scroll = total_lines.saturating_sub(inner_height);
    let scroll = state.log_scroll.min(max_scroll);

    let visible_lines: Vec<Line> = state
        .log_lines
        .iter()
        .skip(scroll)
        .take(inner_height)
        .map(|line| {
            let color = if line.contains("ERROR") {
                Color::Red
            } else if line.contains("WARN") {
                Color::Yellow
            } else if line.contains("INFO") {
                Color::Green
            } else {
                Color::DarkGray
            };
            Line::from(Span::styled(
                format!(" {}", line),
                Style::default().fg(color),
            ))
        })
        .collect();

    let log_content = if visible_lines.is_empty() {
        Paragraph::new(Line::from(Span::styled(
            " Waiting for log output...",
            Style::default().fg(Color::DarkGray),
        )))
    } else {
        Paragraph::new(visible_lines)
    };

    frame.render_widget(log_content.block(log_block), rows[1]);
}
