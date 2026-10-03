use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};

use crate::app::{App, DagFocus};
use crate::format::truncate_hash;
use crate::rpc::types::format_number;

pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    if super::common::render_syncing_guard(frame, area, app, "BlockDAG") {
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(6),  // visualizer
            Constraint::Length(19), // metrics + GHOSTDAG stats
            Constraint::Min(0),     // tips/parents
        ])
        .split(area);

    render_visualizer(frame, chunks[0], app);
    render_metrics(frame, chunks[1], app);
    render_tips(frame, chunks[2], app);

    if app.dag_selection.block_loading {
        render_loading_popup(frame, area);
    } else if let Some(ref detail) = app.dag_selection.block_detail {
        render_block_popup(frame, area, detail);
    }
}

fn render_visualizer(frame: &mut Frame, area: Rect, app: &App) {
    let vis = &app.node.dag_visualizer;
    let inner_width = area.width.saturating_sub(2) as usize;
    let inner_height = area.height.saturating_sub(2) as usize;

    let mut lines: Vec<Line> = Vec::new();

    if vis.columns.is_empty() {
        lines.push(Line::from(Span::styled(
            " Collecting DAG data...",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        // Each column takes 4 chars: block + spacing (e.g. "██ ─")
        let col_width = 4;
        let max_cols = inner_width / col_width;
        let skip = vis.columns.len().saturating_sub(max_cols);
        let visible_cols: Vec<&crate::app::DagVisualizerColumn> =
            vis.columns.iter().skip(skip).collect();

        let max_blocks = visible_cols
            .iter()
            .map(|c| c.blocks.len())
            .max()
            .unwrap_or(0);
        let display_rows = max_blocks.min(inner_height.saturating_sub(1));

        // Draw blocks as small filled/outlined squares
        for row in 0..display_rows {
            let mut spans: Vec<Span> = Vec::new();
            spans.push(Span::raw(" "));
            for (ci, col) in visible_cols.iter().enumerate() {
                if row < col.blocks.len() {
                    let block = &col.blocks[row];
                    let (symbol, style) = if block.is_selected_parent {
                        (
                            "██",
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD),
                        )
                    } else {
                        ("▒▒", Style::default().fg(Color::White))
                    };
                    spans.push(Span::styled(symbol, style));
                } else {
                    spans.push(Span::raw("  "));
                }
                // Connection between columns
                if ci < visible_cols.len() - 1 {
                    spans.push(Span::styled("──", Style::default().fg(Color::DarkGray)));
                }
            }
            lines.push(Line::from(spans));
        }

        // Flow arrow line at the bottom
        if display_rows > 0 && visible_cols.len() > 1 {
            let mut conn_spans: Vec<Span> = Vec::new();
            conn_spans.push(Span::raw(" "));
            for ci in 0..visible_cols.len() {
                conn_spans.push(Span::styled("  ", Style::default().fg(Color::DarkGray)));
                if ci < visible_cols.len() - 1 {
                    conn_spans.push(Span::styled("─→", Style::default().fg(Color::DarkGray)));
                }
            }
            lines.push(Line::from(conn_spans));
        }
    }

    let block = Block::default().borders(Borders::ALL).title(Span::styled(
        " DAG Visualizer (██ = selected parent) ",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ));

    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_metrics(frame: &mut Frame, area: Rect, app: &App) {
    let label = Style::default().fg(Color::DarkGray);
    let lines = if let Some(ref dag) = app.node.dag_info {
        let stats = &app.node.dag_stats;
        let mut lines = vec![
            Line::from(vec![
                Span::styled(" Network:          ", label),
                Span::raw(&dag.network),
            ]),
            Line::from(vec![
                Span::styled(" Block Count:      ", label),
                Span::raw(format_number(dag.block_count)),
            ]),
            Line::from(vec![
                Span::styled(" Header Count:     ", label),
                Span::raw(format_number(dag.header_count)),
            ]),
            Line::from(vec![
                Span::styled(" Difficulty:       ", label),
                Span::raw(format!("{:.4}", dag.difficulty)),
            ]),
            Line::from(vec![
                Span::styled(" DAA Score:        ", label),
                Span::raw(format_number(dag.virtual_daa_score)),
            ]),
            Line::from(vec![
                Span::styled(" Past Median Time: ", label),
                Span::raw(dag.past_median_time.to_string()),
            ]),
            Line::from(vec![
                Span::styled(" Pruning Point:    ", label),
                Span::raw(truncate_hash(&dag.pruning_point_hash)),
            ]),
            Line::from(vec![
                Span::styled(" Sink:             ", label),
                Span::raw(truncate_hash(&dag.sink)),
            ]),
            Line::from(vec![
                Span::styled(" Tips Count:       ", label),
                Span::raw(dag.tip_hashes.len().to_string()),
            ]),
            Line::from(vec![
                Span::styled(" Virtual Parents:  ", label),
                Span::raw(dag.virtual_parent_hashes.len().to_string()),
            ]),
        ];

        // GHOSTDAG separator and stats
        lines.push(Line::from(Span::styled(
            " ── GHOSTDAG ─────────────────────────",
            Style::default().fg(Color::DarkGray),
        )));
        lines.push(Line::from(vec![
            Span::styled(" Blue Score:       ", label),
            Span::raw(
                stats
                    .sink_blue_score
                    .map(format_number)
                    .unwrap_or_else(|| "---".into()),
            ),
        ]));
        lines.push(Line::from(vec![
            Span::styled(" Blue/Red Tips:    ", label),
            Span::raw(match stats.blue_red_ratio() {
                Some((blue, red)) => format!("{} blue / {} red", blue, red),
                None => "---".into(),
            }),
        ]));
        lines.push(Line::from(vec![
            Span::styled(" DAG Width:        ", label),
            Span::raw(match (stats.samples.back(), stats.avg_dag_width()) {
                (Some(s), Some(avg)) => format!("{} tips (avg: {:.1})", s.tip_count, avg),
                _ => "---".into(),
            }),
        ]));
        lines.push(Line::from(vec![
            Span::styled(" Block Interval:   ", label),
            Span::raw(match stats.block_interval_ms() {
                Some(ms) => format!("{:.0}ms", ms),
                None => "---".into(),
            }),
        ]));
        lines.push(Line::from(vec![
            Span::styled(" Blue Block Rate:  ", label),
            Span::raw(match stats.blue_block_rate() {
                Some(rate) => format!("{:.2} blocks/s", rate),
                None => "---".into(),
            }),
        ]));
        lines.push(Line::from(vec![
            Span::styled(" Unvalidated:      ", label),
            Span::raw(match stats.headers_blocks_delta() {
                Some(delta) => format!("{} headers ahead", format_number(delta)),
                None => "---".into(),
            }),
        ]));

        lines
    } else {
        vec![Line::from(Span::styled(
            " Waiting for DAG data...",
            Style::default().fg(Color::DarkGray),
        ))]
    };

    let block = Block::default().borders(Borders::ALL).title(Span::styled(
        " BlockDAG Metrics ",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ));

    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_tips(frame: &mut Frame, area: Rect, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    let tips_focused = app.dag_selection.focus == DagFocus::Tips;
    let parents_focused = app.dag_selection.focus == DagFocus::Parents;

    // Tip hashes
    let tip_items: Vec<ListItem> = if let Some(ref dag) = app.node.dag_info {
        dag.tip_hashes
            .iter()
            .enumerate()
            .map(|(i, h)| {
                let style = if tips_focused && i == app.dag_selection.tip_selected {
                    Style::default().bg(Color::DarkGray).fg(Color::White)
                } else {
                    Style::default().fg(Color::White)
                };
                ListItem::new(Line::from(Span::styled(h.as_str(), style)))
            })
            .collect()
    } else {
        vec![]
    };

    let tips_border = if tips_focused {
        Color::Cyan
    } else {
        Color::White
    };
    let tips_list = List::new(tip_items).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(tips_border))
            .title(Span::styled(
                " Tip Hashes (←→ switch, ↑↓ select, Enter info) ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
    );

    frame.render_widget(tips_list, chunks[0]);

    // Virtual parent hashes
    let parent_items: Vec<ListItem> = if let Some(ref dag) = app.node.dag_info {
        dag.virtual_parent_hashes
            .iter()
            .enumerate()
            .map(|(i, h)| {
                let style = if parents_focused && i == app.dag_selection.parent_selected {
                    Style::default().bg(Color::DarkGray).fg(Color::White)
                } else {
                    Style::default().fg(Color::White)
                };
                ListItem::new(Line::from(Span::styled(h.as_str(), style)))
            })
            .collect()
    } else {
        vec![]
    };

    let parents_border = if parents_focused {
        Color::Cyan
    } else {
        Color::White
    };
    let parents_list = List::new(parent_items).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(parents_border))
            .title(Span::styled(
                " Virtual Parent Hashes ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
    );

    frame.render_widget(parents_list, chunks[1]);
}

fn render_loading_popup(frame: &mut Frame, area: Rect) {
    let popup_width = 30;
    let popup_height = 3;
    let x = (area.width.saturating_sub(popup_width)) / 2 + area.x;
    let y = (area.height.saturating_sub(popup_height)) / 2 + area.y;
    let popup_area = Rect::new(x, y, popup_width, popup_height);

    frame.render_widget(Clear, popup_area);
    let popup = Paragraph::new(" Loading block info...").block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow)),
    );
    frame.render_widget(popup, popup_area);
}

fn render_block_popup(frame: &mut Frame, area: Rect, detail: &str) {
    let content_lines = detail.lines().count() as u16;
    let popup_width = area.width.saturating_sub(10).clamp(40, 80);
    let popup_height = (content_lines + 4).clamp(8, area.height.saturating_sub(6));
    let x = (area.width.saturating_sub(popup_width)) / 2 + area.x;
    let y = (area.height.saturating_sub(popup_height)) / 2 + area.y;
    let popup_area = Rect::new(x, y, popup_width, popup_height);

    frame.render_widget(Clear, popup_area);

    let popup = Paragraph::new(detail.to_string())
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(Span::styled(
                    " Block Info (Esc to close) ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ))
                .border_style(Style::default().fg(Color::Yellow)),
        )
        .wrap(Wrap { trim: false });

    frame.render_widget(popup, popup_area);
}
