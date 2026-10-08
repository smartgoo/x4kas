//! The Dashboard's BlockDAG section: the live DAG visualizer band and the BlockDAG card.

use std::collections::{HashMap, HashSet};

use eframe::egui::{
    self, Align2, Color32, CursorIcon, FontId, Painter, Pos2, Rect, Sense, Stroke, Ui, Vec2, vec2,
};

use super::theme;
use super::widgets::{block_hash, kv, kv_columns, kv_grid, kv_with, or_dash, request_block};
use x4kas_core::app::{App, DAG_MAX_DAA_SCORES, DagBlock, DagVisualizer};
use x4kas_core::format::{format_number, shorten_middle};

// The visualizer mirrors the one on the Kaspalytics home page: a band of the newest DAA
// scores, one column each, blocks spread evenly down their column and joined to their
// parents. Hovering for a while pauses it so a block can be picked out.
const CANVAS_HEIGHT: f32 = 175.0;
const PADDING: f32 = 20.0;
const BLOCK_SIZE: f32 = 5.0;
/// Narrower canvases show half as many DAA scores, so columns stay apart.
const WIDE_CANVAS: f32 = 1024.0;
const NARROW_DAA_SCORES: usize = 50;
const EDGE_WIDTH: f32 = 0.25;
const EDGE_ALPHA: f32 = 0.25;
const EDGE_HIGHLIGHT_WIDTH: f32 = 1.0;
const EDGE_HIGHLIGHT_ALPHA: f32 = 0.6;
/// Fraction of the remaining distance a block moves per 60 Hz frame.
const EASE: f32 = 0.4;
/// Base fade-in time, varied per block so arrivals don't pop in lockstep.
const FADE_SECS: f64 = 0.125;
/// Blocks that left the window slide here (off the left edge), then are dropped.
const EXIT_X: f32 = -BLOCK_SIZE - 100.0;
/// Pointer distance at which a block counts as hovered (blocks are tiny).
const HIT_RADIUS: f32 = 6.0;

/// The live DAG visualizer band across the top of the Dashboard. A click on a block shows
/// its info pane.
pub(super) fn band(ui: &mut Ui, app: &App) {
    let waiting = if app.node.server_info.as_ref().is_some_and(|s| s.is_synced) {
        "Waiting for blocks…"
    } else {
        "Waiting for the node to sync…"
    };
    if let Some(hash) = visualizer(ui, &app.node.dag_visualizer, waiting) {
        request_block(ui.ctx(), &hash);
    }
}

/// Animation and hover state of the visualizer, kept in egui memory between frames.
#[derive(Clone, Default)]
struct DagView {
    /// Drawn blocks by hash, positioned relative to the canvas' top-left corner.
    nodes: HashMap<String, Node>,
    /// The blocks as they were when the hover pause began.
    frozen: Option<DagVisualizer>,
}

#[derive(Clone)]
struct Node {
    pos: Vec2,
    born: f64,
    fade_secs: f64,
}

/// Draw the live BlockDAG. Returns the hash of a clicked block.
fn visualizer(ui: &mut Ui, live: &DagVisualizer, waiting: &str) -> Option<String> {
    let id = ui.id().with("dag_visualizer");
    let mut view: DagView = ui.data_mut(|d| d.remove_temp(id)).unwrap_or_default();
    let (response, painter) =
        ui.allocate_painter(vec2(ui.available_width(), CANVAS_HEIGHT), Sense::click());
    let rect = response.rect;

    // Hovering anywhere on the canvas freezes it, so a block can be picked out.
    let paused = ui.rect_contains_pointer(rect) && !live.is_empty();
    // Taken out while drawing so `view` stays free to animate; put back before storing.
    let frozen = match view.frozen.take() {
        Some(f) if paused => Some(f),
        _ if paused => Some(live.clone()),
        _ => None,
    };
    let vis = frozen.as_ref().unwrap_or(live);

    let clicked = if vis.is_empty() {
        view.nodes.clear();
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            waiting,
            FontId::monospace(theme::FONT_SIZE),
            theme::TEXT_DIM,
        );
        title_chip(ui, rect, NARROW_DAA_SCORES);
        None
    } else {
        draw_blocks(ui, &response, &painter, &mut view, vis, paused)
    };
    view.frozen = frozen;
    ui.data_mut(|d| d.insert_temp(id, view));
    clicked
}

/// Lay out, animate and paint the blocks of `vis`, with the hover pill and title.
/// Returns the hash of a clicked block.
fn draw_blocks(
    ui: &Ui,
    response: &egui::Response,
    painter: &Painter,
    view: &mut DagView,
    vis: &DagVisualizer,
    paused: bool,
) -> Option<String> {
    let rect = response.rect;
    let (now, dt) = ui.input(|i| (i.time, i.stable_dt.min(0.1)));
    let max_scores = if rect.width() >= WIDE_CANVAS {
        DAG_MAX_DAA_SCORES
    } else {
        NARROW_DAA_SCORES
    };
    let targets = layout(vis, rect.size(), max_scores);
    let animating = animate(view, &targets, now, dt);

    // Blocks only respond while paused: picking one out of a sliding DAG is futile.
    let hovered = paused
        .then(|| response.hover_pos())
        .flatten()
        .and_then(|p| {
            view.nodes
                .iter()
                .filter(|(hash, _)| targets.contains_key(hash.as_str()))
                .map(|(hash, n)| (hash, (rect.min + n.pos).distance(p)))
                .filter(|(_, d)| *d <= HIT_RADIUS)
                .min_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(hash, _)| hash.clone())
        });
    let hovered_block = hovered
        .as_deref()
        .and_then(|h| vis.blocks().find(|b| b.hash == h));

    let tips = vis.tips();
    paint(painter, rect, view, vis, &tips, hovered_block, now);

    if paused {
        pill(
            painter,
            rect.center_top() + vec2(0.0, 10.0),
            Align2::CENTER_TOP,
            "Updates paused during hover",
        );
    }
    if let Some(block) = hovered_block {
        let tip = if tips.contains(block.hash.as_str()) {
            " (DAG tip)"
        } else {
            ""
        };
        let text = format!(
            "{}{tip}\nDAA score {} · {} parents\nClick for block info",
            shorten_middle(&block.hash, 11),
            format_number(block.daa_score),
            block.parents.len(),
        );
        pill(
            painter,
            rect.left_bottom() + vec2(10.0, -10.0),
            Align2::LEFT_BOTTOM,
            &text,
        );
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    title_chip(ui, rect, max_scores);

    if animating {
        ui.ctx().request_repaint();
    }
    response.clicked().then_some(hovered).flatten()
}

/// Target position of every block in the newest `max_scores` DAA scores, relative to the
/// canvas: columns evenly across the width (newest at the right), blocks evenly down each.
fn layout(vis: &DagVisualizer, size: Vec2, max_scores: usize) -> HashMap<&str, Vec2> {
    let shown: Vec<&Vec<DagBlock>> = vis.columns.values().rev().take(max_scores).collect();
    let count = shown.len() as f32;
    let column_width = (size.x - 2.0 * PADDING) / count;
    let inner_height = size.y - 2.0 * PADDING;
    let mut targets = HashMap::new();
    for (i, blocks) in shown.iter().rev().enumerate() {
        let x = size.x - PADDING - (count - i as f32 - 0.5) * column_width;
        let spacing = inner_height / (blocks.len() + 1) as f32;
        for (j, block) in blocks.iter().enumerate() {
            let y = PADDING + (j + 1) as f32 * spacing;
            targets.insert(block.hash.as_str(), vec2(x, y));
        }
    }
    targets
}

/// Move blocks toward their targets. New blocks appear in place and fade in; blocks that
/// left the window slide off to the left and are dropped. Returns whether anything moves.
fn animate(view: &mut DagView, targets: &HashMap<&str, Vec2>, now: f64, dt: f32) -> bool {
    let ease = 1.0 - (1.0 - EASE).powf(dt * 60.0);
    let mut animating = false;
    for (&hash, &target) in targets {
        match view.nodes.get_mut(hash) {
            Some(node) => {
                node.pos += (target - node.pos) * ease;
                if (target - node.pos).length() < 0.1 {
                    node.pos = target;
                } else {
                    animating = true;
                }
            }
            None => {
                let jitter = hash.bytes().next().map_or(0.5, |b| f64::from(b) / 255.0);
                let fade_secs = FADE_SECS * (0.7 + 0.8 * jitter);
                view.nodes.insert(
                    hash.to_string(),
                    Node {
                        pos: target,
                        born: now,
                        fade_secs,
                    },
                );
            }
        }
    }
    view.nodes.retain(|hash, node| {
        if !targets.contains_key(hash.as_str()) {
            node.pos.x += (EXIT_X - node.pos.x) * ease;
            animating = true;
            return node.pos.x > EXIT_X / 2.0;
        }
        if now - node.born < node.fade_secs {
            animating = true;
        }
        true
    });
    animating
}

/// Edges, then blocks: tips and the hovered block's parents in their own colors.
fn paint(
    painter: &Painter,
    rect: Rect,
    view: &DagView,
    vis: &DagVisualizer,
    tips: &HashSet<&str>,
    hovered: Option<&DagBlock>,
    now: f64,
) {
    let painter = painter.with_clip_rect(rect);
    let at = |hash: &str| view.nodes.get(hash).map(|n| rect.min + n.pos);

    let edge = Stroke::new(EDGE_WIDTH, theme::DAG_EDGE.gamma_multiply(EDGE_ALPHA));
    for block in vis.blocks() {
        let Some(child) = at(&block.hash) else {
            continue;
        };
        for parent in block.parents.iter().filter_map(|p| at(p)) {
            painter.line_segment([parent, child], edge);
        }
    }

    let parents: HashSet<&str> = hovered
        .map(|b| b.parents.iter().map(String::as_str).collect())
        .unwrap_or_default();
    if let Some(block) = hovered
        && let Some(child) = at(&block.hash)
    {
        let edge = Stroke::new(
            EDGE_HIGHLIGHT_WIDTH,
            theme::DAG_PARENT.gamma_multiply(EDGE_HIGHLIGHT_ALPHA),
        );
        for parent in parents.iter().filter_map(|p| at(p)) {
            painter.line_segment([parent, child], edge);
        }
    }

    for (hash, node) in &view.nodes {
        let color = if hovered.is_some_and(|b| &b.hash == hash) {
            theme::DAG_HOVER
        } else if parents.contains(hash.as_str()) {
            theme::DAG_PARENT
        } else if tips.contains(hash.as_str()) {
            theme::DAG_TIP
        } else {
            theme::DAG_BLOCK
        };
        let alpha = ((now - node.born) / node.fade_secs).clamp(0.0, 1.0) as f32;
        let block = Rect::from_center_size(rect.min + node.pos, Vec2::splat(BLOCK_SIZE));
        painter.rect_filled(block, 1.0, color.gamma_multiply(alpha));
    }
}

/// Text on a dark rounded backing, anchored at `pos`.
fn pill(painter: &Painter, pos: Pos2, align: Align2, text: &str) {
    let galley = painter.layout_no_wrap(
        text.to_string(),
        FontId::monospace(theme::FONT_SIZE),
        theme::TEXT,
    );
    let padding = vec2(8.0, 5.0);
    let rect = align.anchor_size(pos, galley.size() + 2.0 * padding);
    painter.rect(
        rect,
        4.0,
        theme::BG_DEEP.gamma_multiply(0.9),
        Stroke::new(1.0_f32, theme::BORDER),
        egui::StrokeKind::Inside,
    );
    painter.galley(rect.min + padding, galley, Color32::PLACEHOLDER);
}

/// The band's title in its top-left corner, with an explanation on hover.
fn title_chip(ui: &Ui, rect: Rect, max_scores: usize) {
    let painter = ui.painter();
    let galley = painter.layout_no_wrap(
        "KASPA'S BLOCKDAG, IN REAL-TIME".to_string(),
        FontId::monospace(theme::SMALL_FONT_SIZE),
        theme::TEXT_DIM,
    );
    let padding = vec2(6.0, 3.0);
    let chip = Rect::from_min_size(rect.min + vec2(4.0, 4.0), galley.size() + 2.0 * padding);
    painter.rect_filled(chip, 4.0, theme::SURFACE.gamma_multiply(0.8));
    painter.galley(chip.min + padding, galley, Color32::PLACEHOLDER);
    ui.interact(chip, ui.id().with("dag_title"), Sense::hover())
        .on_hover_text(format!(
            "The {max_scores} most recent DAA scores of mined blocks, grouped by DAA score.\n\
             Tips (blocks nothing references yet) are highlighted.\n\
             Hover to pause, then point at a block to see its parents and click it \
             for block info."
        ));
}

/// The BlockDAG card under the visualizer: DAG counts, GHOSTDAG stats and the selected
/// chain's key blocks, in three columns. Every row shows from the start, with dashes until
/// the data arrives, so the layout doesn't jump.
pub(super) fn stats(ui: &mut Ui, app: &App) {
    let dag = app.node.dag_info.as_ref();
    let stats = &app.node.dag_stats;
    kv_columns(ui, 260.0, |[chain, ghostdag]| {
        kv_grid(chain, "dag_chain", |ui| {
            kv(
                ui,
                "DAA Score",
                or_dash(dag, |d| format_number(d.virtual_daa_score)),
            );
            kv(
                ui,
                "Blue Score",
                or_dash(app.node.sink_blue_score, format_number),
            );
            hash_row(ui, "Sink", dag.map(|d| d.sink.as_str()));
            hash_row(
                ui,
                "Pruning Point",
                dag.map(|d| d.pruning_point_hash.as_str()),
            );
            kv(
                ui,
                "Past Median Time",
                or_dash(dag, |d| d.past_median_time.to_string()),
            );
        });

        kv_grid(ghostdag, "ghostdag", |ui| {
            kv(
                ui,
                "Tips",
                or_dash(dag, |d| format_number(d.tip_hashes.len() as u64)),
            );
            kv(
                ui,
                "DAG Width (60s avg)",
                or_dash(stats.avg_dag_width(), |avg| format!("{avg:.1}")),
            );
            kv(
                ui,
                "Block Interval",
                or_dash(stats.block_interval_ms(), |ms| format!("{ms:.0} ms")),
            );
            kv(
                ui,
                "Blue Block Rate",
                or_dash(stats.blue_block_rate(), |r| format!("{r:.2} blocks/s")),
            );
        });
    });
}

/// A [`kv`] row with a [`block_hash`], or a dash as tall as one until there is a hash.
fn hash_row(ui: &mut Ui, label: &str, hash: Option<&str>) {
    kv_with(ui, label, |ui| match hash {
        Some(hash) => {
            block_hash(ui, hash, false);
        }
        None => {
            ui.set_min_height(ui.spacing().interact_size.y);
            ui.label("—");
        }
    });
}
