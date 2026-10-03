use eframe::egui::{self, Pos2, Rect, RichText, Sense, Stroke, Ui, vec2};

use super::theme;
use super::widgets::{CARD_GAP, card, field_label, kv, kv_grid, placeholder, syncing_guard};
use crate::app::{App, DagFocus, DagVisualizer};
use crate::controller::{CommandSender, UiCommand};
use crate::rpc::types::format_number;

const CANVAS_HEIGHT: f32 = 170.0;
const COL_WIDTH: f32 = 40.0;
const ROW_HEIGHT: f32 = 24.0;
const BLOCK_SIZE: f32 = 18.0;

pub fn show(ui: &mut Ui, app: &mut App, cmd_tx: &CommandSender) {
    if syncing_guard(ui, app, "BlockDAG") {
        return;
    }

    let mut lookup: Option<String> = None;

    egui::ScrollArea::vertical().show(ui, |ui| {
        card(ui, "DAG Visualizer", |ui| {
            legend(ui);
            if let Some(hash) = visualizer(ui, &app.node.dag_visualizer) {
                lookup = Some(hash);
            }
        });
        ui.add_space(CARD_GAP);

        ui.columns(2, |cols| {
            card(&mut cols[0], "BlockDAG Metrics", |ui| metrics(ui, app));
            card(&mut cols[1], "GHOSTDAG", |ui| ghostdag(ui, app));
        });
        ui.add_space(CARD_GAP);

        if let Some(hash) = hash_lists(ui, app) {
            lookup = Some(hash);
        }
    });

    if let Some(hash) = lookup {
        app.dag_selection.block_detail = None;
        app.dag_selection.block_loading = true;
        let _ = cmd_tx.send(UiCommand::LookupBlock(hash));
    }

    block_window(ui.ctx(), app);
}

fn legend(ui: &mut Ui) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("■").color(theme::ACCENT));
        ui.label(RichText::new("selected parent").weak());
        ui.label(RichText::new("■").color(theme::SLATE));
        ui.label(RichText::new("other tip").weak());
        ui.label(RichText::new("│ older → newer │ click a block for details").weak());
    });
}

/// Draw tip snapshots as columns of blocks. Returns the hash of a clicked block.
fn visualizer(ui: &mut Ui, vis: &DagVisualizer) -> Option<String> {
    if vis.columns.is_empty() {
        placeholder(ui, "Collecting DAG data…");
        return None;
    }

    let (response, painter) =
        ui.allocate_painter(vec2(ui.available_width(), CANVAS_HEIGHT), Sense::click());
    let rect = response.rect;

    let max_cols = ((rect.width() - 8.0) / COL_WIDTH).floor().max(1.0) as usize;
    let max_rows = ((rect.height() - 8.0) / ROW_HEIGHT).floor().max(1.0) as usize;
    let skip = vis.columns.len().saturating_sub(max_cols);

    // Lay out block rects per visible column (right-aligned so the newest is at the edge).
    let visible: Vec<_> = vis.columns.iter().skip(skip).collect();
    let x_offset = rect.right() - visible.len() as f32 * COL_WIDTH;
    let columns: Vec<Vec<(Rect, &crate::app::DagVisualizerBlock)>> = visible
        .iter()
        .enumerate()
        .map(|(ci, col)| {
            let x = x_offset + ci as f32 * COL_WIDTH + COL_WIDTH / 2.0;
            let n = col.blocks.len().min(max_rows);
            let y0 = rect.center().y - (n as f32 * ROW_HEIGHT) / 2.0 + ROW_HEIGHT / 2.0;
            col.blocks
                .iter()
                .take(n)
                .enumerate()
                .map(|(ri, b)| {
                    let center = Pos2::new(x, y0 + ri as f32 * ROW_HEIGHT);
                    (
                        Rect::from_center_size(center, vec2(BLOCK_SIZE, BLOCK_SIZE)),
                        b,
                    )
                })
                .collect()
        })
        .collect();

    // Edges: each new snapshot builds on the previous snapshot's selected parents.
    let edge = Stroke::new(1.0_f32, ui.visuals().weak_text_color().gamma_multiply(0.5));
    for pair in columns.windows(2) {
        for (prev, block) in &pair[0] {
            if !block.is_selected_parent {
                continue;
            }
            for (cur, _) in &pair[1] {
                painter.line_segment([prev.right_center(), cur.left_center()], edge);
            }
        }
    }

    let pointer = response.hover_pos();
    let mut hovered = None;
    for (r, block) in columns.iter().flatten() {
        let is_hovered = pointer.is_some_and(|p| r.expand(3.0).contains(p));
        let fill = if block.is_selected_parent {
            theme::ACCENT
        } else {
            theme::SLATE
        };
        painter.rect_filled(*r, 3.0, fill);
        if is_hovered {
            painter.rect_stroke(
                r.expand(2.0),
                4.0,
                Stroke::new(1.5_f32, ui.visuals().strong_text_color()),
                egui::StrokeKind::Outside,
            );
            hovered = Some(block.hash_full.clone());
        }
    }

    let clicked = response.clicked();
    if let Some(ref hash) = hovered {
        response.on_hover_text_at_pointer(hash);
    }
    if clicked { hovered } else { None }
}

fn metrics(ui: &mut Ui, app: &App) {
    let Some(ref dag) = app.node.dag_info else {
        placeholder(ui, "Waiting for DAG data…");
        return;
    };
    kv_grid(ui, "dag_metrics", |ui| {
        kv(ui, "Network", &dag.network);
        kv(ui, "Block Count", format_number(dag.block_count));
        kv(ui, "Header Count", format_number(dag.header_count));
        kv(ui, "Difficulty", format_number(dag.difficulty as u64));
        kv(ui, "DAA Score", format_number(dag.virtual_daa_score));
        kv(ui, "Past Median Time", dag.past_median_time.to_string());
        hash_kv(ui, "Pruning Point", &dag.pruning_point_hash);
        hash_kv(ui, "Sink", &dag.sink);
        kv(ui, "Tips Count", dag.tip_hashes.len().to_string());
        kv(
            ui,
            "Virtual Parents",
            dag.virtual_parent_hashes.len().to_string(),
        );
    });
}

fn hash_kv(ui: &mut Ui, label: &str, hash: &str) {
    field_label(ui, label);
    ui.label(hash);
    ui.end_row();
}

fn ghostdag(ui: &mut Ui, app: &App) {
    if app.node.dag_info.is_none() {
        placeholder(ui, "Waiting for DAG data…");
        return;
    }
    let stats = &app.node.dag_stats;
    let dash = || "—".to_string();
    kv_grid(ui, "ghostdag", |ui| {
        kv(
            ui,
            "Blue Score",
            stats
                .sink_blue_score
                .map(format_number)
                .unwrap_or_else(dash),
        );
        kv(
            ui,
            "Blue/Red Tips",
            stats
                .blue_red_ratio()
                .map(|(blue, red)| format!("{blue} blue / {red} red"))
                .unwrap_or_else(dash),
        );
        kv(
            ui,
            "DAG Width",
            match (stats.samples.back(), stats.avg_dag_width()) {
                (Some(s), Some(avg)) => format!("{} tips (avg: {avg:.1})", s.tip_count),
                _ => dash(),
            },
        );
        kv(
            ui,
            "Block Interval",
            stats
                .block_interval_ms()
                .map(|ms| format!("{ms:.0} ms"))
                .unwrap_or_else(dash),
        );
        kv(
            ui,
            "Blue Block Rate",
            stats
                .blue_block_rate()
                .map(|r| format!("{r:.2} blocks/s"))
                .unwrap_or_else(dash),
        );
        kv(
            ui,
            "Unvalidated",
            stats
                .headers_blocks_delta()
                .map(|d| format!("{} headers ahead", format_number(d)))
                .unwrap_or_else(dash),
        );
    });
}

/// Tip and virtual-parent hash lists. Returns the hash of a clicked entry.
fn hash_lists(ui: &mut Ui, app: &mut App) -> Option<String> {
    let dag = app.node.dag_info.as_ref()?;
    let sel = &mut app.dag_selection;
    let mut clicked = None;

    ui.columns(2, |cols| {
        card(&mut cols[0], "Tip Hashes", |ui| {
            for (i, hash) in dag.tip_hashes.iter().enumerate() {
                let selected = sel.focus == DagFocus::Tips && sel.tip_selected == i;
                if ui
                    .selectable_label(selected, hash.as_str())
                    .on_hover_text("Click for block info")
                    .clicked()
                {
                    sel.focus = DagFocus::Tips;
                    sel.tip_selected = i;
                    clicked = Some(hash.clone());
                }
            }
        });
        card(&mut cols[1], "Virtual Parent Hashes", |ui| {
            for (i, hash) in dag.virtual_parent_hashes.iter().enumerate() {
                let selected = sel.focus == DagFocus::Parents && sel.parent_selected == i;
                if ui
                    .selectable_label(selected, hash.as_str())
                    .on_hover_text("Click for block info")
                    .clicked()
                {
                    sel.focus = DagFocus::Parents;
                    sel.parent_selected = i;
                    clicked = Some(hash.clone());
                }
            }
        });
    });
    clicked
}

fn block_window(ctx: &egui::Context, app: &mut App) {
    let sel = &mut app.dag_selection;
    if !sel.block_loading && sel.block_detail.is_none() {
        return;
    }
    let mut open = true;
    egui::Window::new("Block Info")
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_width(640.0)
        .default_height(480.0)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            if sel.block_loading {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Loading block info…");
                });
            } else if let Some(ref detail) = sel.block_detail {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::multiline(&mut detail.as_str())
                            .code_editor()
                            .desired_width(f32::INFINITY),
                    );
                });
            }
        });
    if !open || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        sel.block_detail = None;
        sel.block_loading = false;
    }
}
