//! The flow graph window: money followed hop by hop from an address (data in
//! `App.address.flows`, from `UiCommand::AddressFlows`). Nodes are addresses sized by
//! the volume over their edges, laid out by a small force simulation; a click on a node
//! expands it by one hop, a right-click shows its info pane.

use std::collections::HashMap;

use eframe::egui::{self, Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2, pos2, vec2};

use super::address::export_status;
use super::theme;
use super::widgets::{address, modal_window, placeholder, request_address};
use x4kas_core::app::App;
use x4kas_core::controller::{CommandSender, ExportRequest, UiCommand};
use x4kas_core::format::{format_kas, shorten_middle};
use x4kas_core::index::export::ExportFormat;
use x4kas_core::index::query::FlowGraph;
use x4kas_core::index::records::AddrId;

/// Layout state between frames.
#[derive(Default)]
pub struct FlowWindowUi {
    positions: HashMap<AddrId, Pos2>,
    /// The graph these positions belong to (node count and roots), to reseed on change.
    laid_out_for: (usize, Vec<String>),
    /// Simulation steps left; the layout settles and then stops repainting.
    steps_left: u32,
    dragging: Option<AddrId>,
}

const NODE_MIN_RADIUS: f32 = 6.0;
const NODE_MAX_RADIUS: f32 = 22.0;
const SETTLE_STEPS: u32 = 240;

impl FlowWindowUi {
    pub fn show(&mut self, ctx: &egui::Context, app: &mut App, cmd_tx: &CommandSender) {
        if !app.address.flows.open {
            return;
        }
        let title = match app.address.flows.roots.first() {
            Some(root) => format!("Flows from {}", shorten_middle(root, 24)),
            None => "Flows".to_string(),
        };
        let window = egui::Window::new(title)
            .default_size([900.0, 620.0])
            .resizable(true);
        let open = modal_window(ctx, window, |ui| self.contents(ui, app, cmd_tx));
        if !open {
            app.address.flows.close();
        }
    }

    fn contents(&mut self, ui: &mut Ui, app: &mut App, cmd_tx: &CommandSender) {
        let mut collapse = app.address.flows.collapse;
        let flows = &app.address.flows;
        ui.horizontal(|ui| {
            if flows.loading {
                ui.spinner();
                ui.label("Following the money…");
            } else {
                ui.label(
                    RichText::new(format!(
                        "{} addresses, {} flows. Click a node to expand it, right-click for its info, drag to arrange.",
                        flows.shown().nodes.len(),
                        flows.shown().edges.len()
                    ))
                    .weak(),
                );
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                for format in [ExportFormat::Json, ExportFormat::Csv] {
                    if ui
                        .add_enabled(
                            !flows.shown().edges.is_empty(),
                            egui::Button::new(format.extension().to_ascii_uppercase()).small(),
                        )
                        .on_hover_text(format!(
                            "Export the graph as shown as {} to ~/.x4kas/exports",
                            format.extension().to_ascii_uppercase()
                        ))
                        .clicked()
                    {
                        let _ = cmd_tx.send(UiCommand::Export(ExportRequest::Flows {
                            graph: flows.shown().clone(),
                            roots: flows.roots.clone(),
                            format,
                        }));
                    }
                }
                ui.label(RichText::new("Export").weak().small());
                ui.separator();
                ui.checkbox(&mut collapse, "Collapse chains")
                    .on_hover_text(
                        "Fold addresses that only pass money on (one flow in, one out), such \
                         as peel chains, into a single dashed arrow with a hop count",
                    );
            });
        });
        if let Some(ref err) = flows.error {
            ui.label(RichText::new(err).color(theme::ERROR));
        }
        export_status(ui, &app.address.export);
        let flows = &mut app.address.flows;
        flows.collapse = collapse;
        if flows.shown().nodes.is_empty() {
            if !flows.loading {
                placeholder(ui, "Nothing indexed for this address yet");
            }
            return;
        }

        let graph = flows.shown().clone();
        let roots = flows.roots.clone();
        let labels = app.labels.clone();
        let (rect, response) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
        self.reseed_if_needed(&graph, &roots, rect);
        self.step(&graph, rect);

        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 3.0, theme::BG_DEEP);
        let max_volume = graph
            .nodes
            .iter()
            .map(|n| n.volume)
            .max()
            .unwrap_or(1)
            .max(1);
        let radius = |volume: u64| {
            let t = (volume as f64 / max_volume as f64).sqrt() as f32;
            NODE_MIN_RADIUS + t * (NODE_MAX_RADIUS - NODE_MIN_RADIUS)
        };
        let node_radius: HashMap<AddrId, f32> = graph
            .nodes
            .iter()
            .map(|n| (n.id, radius(n.volume)))
            .collect();
        let pointer = response.hover_pos();

        // Edges with arrowheads, amounts on hover.
        for edge in &graph.edges {
            let (Some(&from), Some(&to)) =
                (self.positions.get(&edge.from), self.positions.get(&edge.to))
            else {
                continue;
            };
            let dir = (to - from).normalized();
            let start = from + dir * node_radius[&edge.from];
            let end = to - dir * node_radius[&edge.to];
            let hovered = pointer.is_some_and(|p| distance_to_segment(p, start, end) < 5.0);
            let color = if hovered {
                theme::ACCENT_BRIGHT
            } else {
                theme::DAG_EDGE
            };
            let width = if hovered { 2.0_f32 } else { 1.0_f32 };
            let stroke = Stroke::new(width, color);
            if edge.via.is_empty() {
                painter.line_segment([start, end], stroke);
            } else {
                painter.add(egui::Shape::dashed_line(&[start, end], stroke, 6.0, 4.0));
                painter.text(
                    (start + end.to_vec2()) / 2.0 + vec2(0.0, 4.0),
                    egui::Align2::CENTER_TOP,
                    format!("{} hops", edge.hops()),
                    egui::FontId::monospace(theme::SMALL_FONT_SIZE),
                    theme::TEXT_DIM,
                );
            }
            let side = dir.rot90();
            painter.add(egui::Shape::convex_polygon(
                vec![
                    end,
                    end - dir * 7.0 + side * 3.5,
                    end - dir * 7.0 - side * 3.5,
                ],
                color,
                Stroke::NONE,
            ));
            if hovered {
                painter.text(
                    (start + end.to_vec2()) / 2.0 + vec2(0.0, -8.0),
                    egui::Align2::CENTER_BOTTOM,
                    format!(
                        "{} KAS in {} txs",
                        format_kas(edge.amount as f64, 2),
                        edge.tx_count
                    ),
                    egui::FontId::monospace(theme::SMALL_FONT_SIZE),
                    theme::TEXT_BRIGHT,
                );
                if !edge.via.is_empty() {
                    response.clone().on_hover_ui_at_pointer(|ui| {
                        ui.label(
                            RichText::new(format!(
                                "Passes through {} address{}",
                                edge.via.len(),
                                if edge.via.len() == 1 { "" } else { "es" }
                            ))
                            .weak(),
                        );
                        for via in &edge.via {
                            address(ui, via);
                        }
                    });
                }
            }
        }

        // Nodes.
        let mut clicked: Option<(AddrId, String, bool)> = None;
        for node in &graph.nodes {
            let Some(&pos) = self.positions.get(&node.id) else {
                continue;
            };
            let r = node_radius[&node.id];
            let is_root = node.hop == 0;
            let hovered = pointer.is_some_and(|p| p.distance(pos) <= r + 2.0);
            let fill = if is_root {
                theme::ACCENT_DIM
            } else if hovered {
                theme::SURFACE_HI
            } else {
                theme::SURFACE
            };
            let stroke_color = if hovered {
                theme::ACCENT_BRIGHT
            } else if is_root {
                theme::ACCENT
            } else {
                theme::BORDER_HI
            };
            painter.circle(pos, r, fill, Stroke::new(1.5_f32, stroke_color));
            let name = labels
                .name(&node.address)
                .map(str::to_string)
                .unwrap_or_else(|| shorten_middle(&node.address, 16));
            painter.text(
                pos + vec2(0.0, r + 2.0),
                egui::Align2::CENTER_TOP,
                name,
                egui::FontId::monospace(theme::SMALL_FONT_SIZE),
                if labels.name(&node.address).is_some() {
                    theme::ACCENT_BRIGHT
                } else {
                    theme::TEXT_DIM
                },
            );
            if hovered {
                response.clone().on_hover_ui_at_pointer(|ui| {
                    ui.label(&node.address);
                    ui.label(
                        RichText::new(format!(
                            "{} KAS over {} flows · hop {}",
                            format_kas(node.volume as f64, 2),
                            graph
                                .edges
                                .iter()
                                .filter(|e| e.from == node.id || e.to == node.id)
                                .count(),
                            node.hop
                        ))
                        .weak(),
                    );
                });
                if response.drag_started() {
                    self.dragging = Some(node.id);
                }
                if response.clicked() {
                    clicked = Some((node.id, node.address.clone(), false));
                }
                if response.secondary_clicked() {
                    clicked = Some((node.id, node.address.clone(), true));
                }
            }
        }

        if let Some(id) = self.dragging {
            if response.dragged() {
                if let (Some(p), Some(pos)) = (pointer, self.positions.get_mut(&id)) {
                    *pos = p;
                }
                self.steps_left = self.steps_left.max(30);
            } else {
                self.dragging = None;
            }
        }

        if let Some((_, address, info)) = clicked {
            if info {
                request_address(ui.ctx(), &address);
            } else {
                app.address.flows.loading = true;
                let _ = cmd_tx.send(UiCommand::AddressFlows { address, hops: 1 });
            }
        }
        if self.steps_left > 0 {
            ui.ctx().request_repaint();
        }
    }

    /// Place new nodes near their first neighbour (or the center) and restart settling
    /// when the graph changed.
    fn reseed_if_needed(&mut self, graph: &FlowGraph, roots: &[String], rect: Rect) {
        let key = (graph.nodes.len(), roots.to_vec());
        if self.laid_out_for == key && !self.positions.is_empty() {
            return;
        }
        if self.laid_out_for.1 != key.1 {
            self.positions.clear();
        }
        let center = rect.center();
        let mut seed = 0.618_f32;
        for node in &graph.nodes {
            if self.positions.contains_key(&node.id) {
                continue;
            }
            let anchor = graph
                .edges
                .iter()
                .filter_map(|e| {
                    let other = if e.from == node.id {
                        e.to
                    } else if e.to == node.id {
                        e.from
                    } else {
                        return None;
                    };
                    self.positions.get(&other).copied()
                })
                .next()
                .unwrap_or(center);
            seed = (seed * 9.73 + 0.37).fract();
            let angle = seed * std::f32::consts::TAU;
            let dist = 40.0 + 60.0 * node.hop as f32;
            self.positions
                .insert(node.id, anchor + vec2(angle.cos(), angle.sin()) * dist);
        }
        self.laid_out_for = key;
        self.steps_left = SETTLE_STEPS;
    }

    /// One frame of a spring/repulsion layout; roots are pulled to the center.
    fn step(&mut self, graph: &FlowGraph, rect: Rect) {
        if self.steps_left == 0 {
            return;
        }
        self.steps_left -= 1;
        let ids: Vec<AddrId> = graph.nodes.iter().map(|n| n.id).collect();
        let mut forces: HashMap<AddrId, Vec2> = ids.iter().map(|id| (*id, Vec2::ZERO)).collect();
        let center = rect.center();
        for (i, &a) in ids.iter().enumerate() {
            for &b in &ids[i + 1..] {
                let (pa, pb) = (self.positions[&a], self.positions[&b]);
                let d = pb - pa;
                let dist = d.length().max(8.0);
                let push = 2600.0 / (dist * dist);
                let f = d / dist * push;
                *forces.get_mut(&a).unwrap() -= f;
                *forces.get_mut(&b).unwrap() += f;
            }
        }
        for edge in &graph.edges {
            let (Some(&pa), Some(&pb)) =
                (self.positions.get(&edge.from), self.positions.get(&edge.to))
            else {
                continue;
            };
            let d = pb - pa;
            let dist = d.length().max(1.0);
            let pull = (dist - 110.0) * 0.02;
            let f = d / dist * pull;
            if let Some(fa) = forces.get_mut(&edge.from) {
                *fa += f;
            }
            if let Some(fb) = forces.get_mut(&edge.to) {
                *fb -= f;
            }
        }
        for node in &graph.nodes {
            let Some(pos) = self.positions.get_mut(&node.id) else {
                continue;
            };
            if Some(node.id) == self.dragging {
                continue;
            }
            let mut f = forces[&node.id];
            if node.hop == 0 {
                f += (center - *pos) * 0.05;
            } else {
                f += (center - *pos) * 0.002;
            }
            let damping = 0.08 + 0.3 * (self.steps_left as f32 / SETTLE_STEPS as f32);
            *pos += f * damping;
            pos.x = pos.x.clamp(rect.left() + 12.0, rect.right() - 12.0);
            pos.y = pos.y.clamp(rect.top() + 12.0, rect.bottom() - 18.0);
        }
    }
}

fn distance_to_segment(p: Pos2, a: Pos2, b: Pos2) -> f32 {
    let ab = b - a;
    let len2 = ab.length_sq();
    if len2 == 0.0 {
        return p.distance(a);
    }
    let t = ((p - a).dot(ab) / len2).clamp(0.0, 1.0);
    p.distance(pos2(a.x + ab.x * t, a.y + ab.y * t))
}
