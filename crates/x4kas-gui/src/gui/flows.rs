//! The flow graph window: money followed hop by hop from an address (data in
//! `App.address.flows`, from `UiCommand::AddressFlows`). Nodes are addresses sized by
//! the volume over their edges, laid out by a small force simulation in graph space
//! (positions relative to the graph's center, so the window can be resized freely);
//! the view pans by dragging the background and zooms with the wheel or a pinch around
//! the pointer. A click on a node expands it by one hop (once; expanded nodes wear a
//! ring), a right-click opens its menu (info pane, label, watchlist), a drag moves it: a dragged node is pinned
//! where it was dropped (the simulation leaves it alone from then on), and the other
//! nodes don't move while one is dragged. Hovering an edge shows its amount in a
//! tooltip in the canvas's top right corner (never over the edge); "Show amounts" writes
//! it over every edge instead.

use std::collections::{HashMap, HashSet};

use eframe::egui::{self, Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2, pos2, vec2};

use super::address::export_status;
use super::dialogs;
use super::theme;
use super::widgets::{address, modal_window, placeholder, request_address};
use x4kas_core::app::{App, ExportOrigin};
use x4kas_core::controller::{CommandSender, ExportRequest, UiCommand};
use x4kas_core::format::{format_kas, shorten_middle};
use x4kas_core::index::export::ExportFormat;
use x4kas_core::index::query::FlowGraph;
use x4kas_core::index::records::AddrId;

/// Layout state between frames.
pub struct FlowWindowUi {
    /// Node positions in graph space: relative to the graph's center, unzoomed.
    positions: HashMap<AddrId, Pos2>,
    /// The graph these positions belong to (node count and roots), to reseed on change.
    laid_out_for: (usize, Vec<String>),
    /// Simulation steps left; the layout settles and then stops repainting.
    steps_left: u32,
    dragging: Option<Drag>,
    /// The view: where the graph's origin sits relative to the canvas center (in graph
    /// units) and the zoom factor.
    pan: Vec2,
    zoom: f32,
    /// Nodes whose counterparties were asked for (roots count), so a click doesn't ask
    /// again and the ring says so.
    expanded: HashSet<AddrId>,
    /// Nodes the user placed: the simulation never moves them again (until a new graph).
    pinned: HashSet<AddrId>,
    /// Write every edge's amount and transaction count over it (a `Prefs` setting).
    pub show_amounts: bool,
    /// The node whose right-click menu is open (its id and address).
    menu_node: Option<(AddrId, String)>,
}

impl Default for FlowWindowUi {
    fn default() -> Self {
        Self {
            positions: HashMap::new(),
            laid_out_for: (0, Vec::new()),
            steps_left: 0,
            dragging: None,
            pan: Vec2::ZERO,
            zoom: 1.0,
            expanded: HashSet::new(),
            pinned: HashSet::new(),
            show_amounts: false,
            menu_node: None,
        }
    }
}

/// What a drag on the canvas moves.
#[derive(Clone, Copy, PartialEq)]
enum Drag {
    Node(AddrId),
    View,
}

/// The view's zoom range.
const ZOOM_RANGE: std::ops::RangeInclusive<f32> = 0.25..=4.0;

/// Graph space ↔ canvas, for one frame's canvas `rect`.
#[derive(Clone, Copy)]
struct View {
    center: Pos2,
    pan: Vec2,
    zoom: f32,
}

impl View {
    fn to_screen(self, p: Pos2) -> Pos2 {
        self.center + (p.to_vec2() + self.pan) * self.zoom
    }

    fn to_graph(self, p: Pos2) -> Pos2 {
        ((p - self.center) / self.zoom - self.pan).to_pos2()
    }
}

const NODE_MIN_RADIUS: f32 = 6.0;
const NODE_MAX_RADIUS: f32 = 22.0;
const SETTLE_STEPS: u32 = 240;
/// Settling after an expansion: shorter and gentler, so the graph the user is looking
/// at keeps its shape while the new nodes find their place.
const EXPAND_STEPS: u32 = 90;
/// Where a new node is seeded from its neighbour: the springs' rest length, so it has
/// little reason to move.
const SEED_DIST: f32 = 110.0;

impl FlowWindowUi {
    /// `close`: this frame's Esc is for this window.
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        app: &mut App,
        cmd_tx: &CommandSender,
        close: bool,
    ) {
        if !app.address.flows.open {
            return;
        }
        let title = match app.address.flows.roots.first() {
            Some(root) => format!("Flows from {}", shorten_middle(root, 24)),
            None => "Flows".to_string(),
        };
        let window = egui::Window::new(title)
            .id(egui::Id::new("flows_window"))
            .default_size([900.0, 620.0])
            .resizable(true);
        let open = modal_window(ctx, window, close, |ui| self.contents(ui, app, cmd_tx));
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
                        "{} addresses, {} flows",
                        flows.shown().nodes.len(),
                        flows.shown().edges.len()
                    ))
                    .weak(),
                )
                .on_hover_text(
                    "Click a node to expand it by one hop, right-click for its menu (info, \
                     label, watchlist), drag it to arrange. Drag the background to pan, \
                     scroll or pinch to zoom.",
                );
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let (response, _) = egui::containers::menu::MenuButton::from_button(
                    egui::Button::new("Export ▾").small(),
                )
                .ui(ui, |ui| {
                    for (format, item) in [
                        (ExportFormat::Json, "To JSON"),
                        (ExportFormat::Csv, "To CSV"),
                    ] {
                        if ui
                            .add_enabled(!flows.shown().edges.is_empty(), egui::Button::new(item))
                            .clicked()
                        {
                            let _ = cmd_tx.send(UiCommand::Export(ExportRequest::Flows {
                                graph: flows.shown().clone(),
                                roots: flows.roots.clone(),
                                format,
                            }));
                            ui.close();
                        }
                    }
                });
                response.on_hover_text("Export the graph as shown to ~/.x4kas/exports");
                ui.separator();
                ui.checkbox(&mut self.show_amounts, "Show amounts")
                    .on_hover_text("Write the KAS amount and transaction count over every flow");
                ui.separator();
                ui.checkbox(&mut collapse, "Collapse chains").on_hover_text(
                    "Fold addresses that only pass money on (one flow in, one out), such \
                         as peel chains, into a single dashed arrow with a hop count",
                );
                ui.separator();
                let fitted = self.zoom == 1.0 && self.pan == Vec2::ZERO;
                if ui
                    .add_enabled(!fitted, egui::Button::new("Reset view").small())
                    .on_hover_text(
                        "Back to the whole graph, centered (also double-click the background)",
                    )
                    .clicked()
                {
                    self.reset_view();
                }
            });
        });
        if let Some(ref err) = flows.error {
            ui.label(RichText::new(err).color(theme::ERROR));
        }
        if let Some(status) = app.address.export.of(&ExportOrigin::Flows) {
            export_status(ui, status);
        }
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
        self.reseed_if_needed(&graph, &roots);
        // Nothing moves while the user is placing a node.
        if !matches!(self.dragging, Some(Drag::Node(_))) {
            self.step(&graph);
        }

        // Zoom around the pointer (wheel or pinch), so what is under it stays put.
        if response.hovered() {
            let (wheel, pinch) = ui.input(|i| (i.smooth_scroll_delta.y, i.zoom_delta()));
            let factor = pinch * (wheel * 0.0025).exp();
            if factor != 1.0
                && let Some(p) = response.hover_pos()
            {
                let before = self.view(rect);
                let under = before.to_graph(p);
                self.zoom = (self.zoom * factor).clamp(*ZOOM_RANGE.start(), *ZOOM_RANGE.end());
                let after = View {
                    zoom: self.zoom,
                    ..before
                };
                // pan' such that `under` maps back onto `p`.
                self.pan = (p - after.center) / after.zoom - under.to_vec2();
            }
        }
        if response.double_clicked() && !self.node_at(&graph, rect, response.hover_pos()) {
            self.reset_view();
        }
        let view = self.view(rect);

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

        // Edges with arrowheads; the amount over each when asked for, else the hovered
        // edge's in the corner tooltip below.
        let radius_on_screen = |id: &AddrId| node_radius[id] * self.zoom.clamp(0.5, 2.0);
        let amount_of = |edge: &x4kas_core::index::query::FlowEdge| {
            format!(
                "{} KAS in {} txs",
                format_kas(edge.amount as f64, 2),
                edge.tx_count
            )
        };
        let mut hovered_edge = None;
        for edge in &graph.edges {
            let (Some(&from), Some(&to)) =
                (self.positions.get(&edge.from), self.positions.get(&edge.to))
            else {
                continue;
            };
            let (from, to) = (view.to_screen(from), view.to_screen(to));
            if from == to {
                continue;
            }
            let dir = (to - from).normalized();
            let start = from + dir * radius_on_screen(&edge.from);
            let end = to - dir * radius_on_screen(&edge.to);
            let hovered = hovered_edge.is_none()
                && pointer.is_some_and(|p| distance_to_segment(p, start, end) < 5.0);
            if hovered {
                hovered_edge = Some(edge);
            }
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
            if self.show_amounts {
                painter.text(
                    (start + end.to_vec2()) / 2.0 + vec2(0.0, -4.0),
                    egui::Align2::CENTER_BOTTOM,
                    amount_of(edge),
                    egui::FontId::monospace(theme::SMALL_FONT_SIZE),
                    if hovered {
                        theme::TEXT_BRIGHT
                    } else {
                        theme::TEXT_DIM
                    },
                );
            }
        }
        if let Some(edge) = hovered_edge {
            let (from, to) = (
                graph.nodes.iter().find(|n| n.id == edge.from),
                graph.nodes.iter().find(|n| n.id == edge.to),
            );
            let name = |node: Option<&x4kas_core::index::query::FlowNode>| {
                node.map(|n| {
                    labels
                        .name(&n.address)
                        .map(str::to_string)
                        .unwrap_or_else(|| shorten_middle(&n.address, 16))
                })
                .unwrap_or_default()
            };
            // Anchored to the canvas's top right corner, out of the way of the edge.
            egui::Area::new(ui.id().with("flow_edge_tooltip"))
                .order(egui::Order::Tooltip)
                .fixed_pos(rect.right_top() + vec2(-8.0, 8.0))
                .pivot(egui::Align2::RIGHT_TOP)
                .interactable(false)
                .show(ui.ctx(), |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.set_max_width(ui.spacing().tooltip_width);
                        ui.label(RichText::new(amount_of(edge)).color(theme::TEXT_BRIGHT));
                        ui.label(RichText::new(format!("{} → {}", name(from), name(to))).weak());
                        if !edge.via.is_empty() {
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
                        }
                    });
                });
        }

        // Nodes.
        let mut clicked: Option<(AddrId, String)> = None;
        let mut hovered_node = None;
        for node in &graph.nodes {
            let Some(&pos) = self.positions.get(&node.id) else {
                continue;
            };
            let pos = view.to_screen(pos);
            let r = radius_on_screen(&node.id);
            let is_root = node.hop == 0;
            let expanded = is_root || self.expanded.contains(&node.id);
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
            if expanded && !is_root {
                // A ring: this node's counterparties are all on the graph.
                painter.circle_stroke(pos, r + 3.0, Stroke::new(1.0_f32, theme::ACCENT_DIM));
            }
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
                hovered_node = Some(node.id);
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
                    ui.label(
                        RichText::new(if expanded {
                            "Expanded · right-click for menu · drag to move"
                        } else {
                            "Click to expand · right-click for menu · drag to move"
                        })
                        .weak(),
                    );
                });
                if response.clicked() {
                    clicked = Some((node.id, node.address.clone()));
                }
                if response.secondary_clicked() {
                    self.menu_node = Some((node.id, node.address.clone()));
                }
            }
        }
        self.node_menu(ui, app, &response, cmd_tx);

        // A drag moves the node under the pointer, else the view.
        if response.drag_started() {
            self.dragging = Some(hovered_node.map_or(Drag::View, Drag::Node));
        }
        match self.dragging {
            Some(Drag::Node(id)) if response.dragged() => {
                // The node keeps its offset under the pointer and stays where it is
                // dropped: pinned, so the simulation never pulls it back.
                if let Some(pos) = self.positions.get_mut(&id) {
                    *pos += response.drag_delta() / self.zoom;
                }
                self.pinned.insert(id);
            }
            Some(Drag::View) if response.dragged() => {
                self.pan += response.drag_delta() / self.zoom;
            }
            Some(_) => self.dragging = None,
            None => {}
        }
        if response.dragged() {
            ui.ctx().set_cursor_icon(match self.dragging {
                Some(Drag::View) => egui::CursorIcon::Grabbing,
                _ => egui::CursorIcon::Move,
            });
        } else if hovered_node.is_some() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }

        if let Some((id, address)) = clicked
            && self.expanded.insert(id)
            && !graph.nodes.iter().any(|n| n.id == id && n.hop == 0)
        {
            // The node stays where the user clicked it; its counterparties settle
            // around it.
            self.pinned.insert(id);
            app.address.flows.loading = true;
            let _ = cmd_tx.send(UiCommand::AddressFlows { address, hops: 1 });
        }
        painter.text(
            rect.left_bottom() + vec2(8.0, -6.0),
            egui::Align2::LEFT_BOTTOM,
            format!("{:.0}%", self.zoom * 100.0),
            egui::FontId::monospace(theme::SMALL_FONT_SIZE),
            theme::TEXT_DIM,
        );
        if self.steps_left > 0 {
            ui.ctx().request_repaint();
        }
    }

    /// The right-click menu of a node (opened by a secondary click on one, at the
    /// pointer): its info pane, its label and its watchlist entry, each worded by what
    /// the address already has.
    fn node_menu(
        &mut self,
        ui: &mut Ui,
        app: &mut App,
        canvas: &egui::Response,
        cmd_tx: &CommandSender,
    ) {
        let open = if canvas.secondary_clicked() && self.menu_node.is_some() {
            Some(egui::SetOpenCommand::Bool(true))
        } else if canvas.clicked() || canvas.secondary_clicked() {
            Some(egui::SetOpenCommand::Bool(false))
        } else {
            None
        };
        let Some((_, addr)) = self.menu_node.clone() else {
            return;
        };
        let labelled = app.labels.user_labels().contains_key(&addr);
        let watched = app.watch.entry(&addr).is_some();
        let mut unwatch = false;
        let shown = egui::Popup::menu(canvas)
            .id(ui.id().with("flow_node_menu"))
            .open_memory(open)
            .at_pointer_fixed()
            .show(|ui| {
                ui.label(RichText::new(shorten_middle(&addr, 24)).weak());
                if ui.button("Show info").clicked() {
                    request_address(ui.ctx(), &addr);
                    ui.close();
                }
                ui.separator();
                if ui
                    .button(if labelled { "Edit label" } else { "Add label" })
                    .on_hover_text("Your own name for this address, shown wherever it appears")
                    .clicked()
                {
                    dialogs::request_label(ui.ctx(), &addr);
                    ui.close();
                }
                ui.separator();
                if watched {
                    if ui
                        .button("Edit watchlist settings")
                        .on_hover_text("Alerts on or off, and the rules that raise them")
                        .clicked()
                    {
                        dialogs::request_watch(ui.ctx(), &addr);
                        ui.close();
                    }
                    if ui
                        .button("Remove from watchlist")
                        .on_hover_text("Stop following this address; its events stay listed")
                        .clicked()
                    {
                        unwatch = true;
                        ui.close();
                    }
                } else if ui
                    .button("Add to watchlist")
                    .on_hover_text("Follow its balance and get alerts on activity")
                    .clicked()
                {
                    dialogs::request_watch(ui.ctx(), &addr);
                    ui.close();
                }
            });
        if shown.is_none() {
            self.menu_node = None;
        }
        if unwatch {
            let mut list = app.watch.list.clone();
            list.entries.retain(|e| e.address != addr);
            let _ = cmd_tx.send(UiCommand::WatchSet(list));
        }
    }

    fn view(&self, rect: Rect) -> View {
        View {
            center: rect.center(),
            pan: self.pan,
            zoom: self.zoom,
        }
    }

    fn reset_view(&mut self) {
        self.pan = Vec2::ZERO;
        self.zoom = 1.0;
    }

    /// Whether a node is under `pointer`.
    fn node_at(&self, graph: &FlowGraph, rect: Rect, pointer: Option<Pos2>) -> bool {
        let Some(p) = pointer else {
            return false;
        };
        let view = self.view(rect);
        graph.nodes.iter().any(|n| {
            self.positions
                .get(&n.id)
                .is_some_and(|pos| view.to_screen(*pos).distance(p) <= NODE_MAX_RADIUS + 2.0)
        })
    }

    /// Place new nodes around their first placed neighbour (or the center) and restart
    /// settling when the graph changed. The new nodes of one neighbour are fanned out
    /// on its least crowded side: the direction the nodes already placed push it away
    /// from, so an expansion grows outward into empty canvas as far as it can.
    fn reseed_if_needed(&mut self, graph: &FlowGraph, roots: &[String]) {
        let key = (graph.nodes.len(), roots.to_vec());
        if self.laid_out_for == key && !self.positions.is_empty() {
            return;
        }
        let fresh = self.laid_out_for.1 != key.1 || self.positions.is_empty();
        if fresh {
            // A new graph: start over, view included.
            self.positions.clear();
            self.expanded.clear();
            self.pinned.clear();
            self.reset_view();
        }
        // New nodes by the neighbour they hang off; `None` for one without a placed
        // neighbour (the roots of a new graph), which hangs off the center.
        let mut groups: Vec<(Option<AddrId>, Vec<AddrId>)> = Vec::new();
        for node in &graph.nodes {
            if self.positions.contains_key(&node.id) {
                continue;
            }
            let anchor = graph.edges.iter().find_map(|e| {
                let other = if e.from == node.id {
                    e.to
                } else if e.to == node.id {
                    e.from
                } else {
                    return None;
                };
                self.positions.contains_key(&other).then_some(other)
            });
            match groups.iter_mut().find(|(a, _)| *a == anchor) {
                Some((_, ids)) => ids.push(node.id),
                None => groups.push((anchor, vec![node.id])),
            }
        }
        let mut seed = 0.618_f32;
        for (anchor, ids) in groups {
            let origin = anchor
                .and_then(|a| self.positions.get(&a).copied())
                .unwrap_or(Pos2::ZERO);
            // The open side: where everything already placed pushes the anchor to,
            // weighted by closeness. With nothing around, away from the center, or any
            // direction for a lone root.
            let mut open = Vec2::ZERO;
            for (id, pos) in &self.positions {
                if Some(*id) == anchor {
                    continue;
                }
                let d = origin - *pos;
                let dist = d.length().max(8.0);
                open += d / (dist * dist);
            }
            if open.length_sq() < 1e-9 {
                open = origin - Pos2::ZERO;
            }
            let base = if open.length_sq() < 1e-9 {
                seed = (seed * 9.73 + 0.37).fract();
                seed * std::f32::consts::TAU
            } else {
                open.angle()
            };
            // Fan the group over an arc centered on the open side, wider the more
            // nodes there are, and onto a second ring once the first is full.
            let n = ids.len();
            let per_ring = 10;
            let on_first = n.min(per_ring);
            let span = (on_first as f32 * 0.45).min(std::f32::consts::TAU * 0.7);
            for (i, id) in ids.into_iter().enumerate() {
                let ring = i / per_ring;
                let slot = i % per_ring;
                let in_ring = if ring == 0 {
                    on_first
                } else {
                    (n - ring * per_ring).min(per_ring)
                };
                let t = (slot as f32 + 0.5) / in_ring as f32 - 0.5;
                let angle = base + t * span;
                let dist = SEED_DIST * (1.0 + 0.5 * ring as f32);
                self.positions
                    .insert(id, origin + vec2(angle.cos(), angle.sin()) * dist);
            }
        }
        self.laid_out_for = key;
        self.steps_left = if fresh { SETTLE_STEPS } else { EXPAND_STEPS };
    }

    /// One frame of a spring/repulsion layout; roots are pulled to the center.
    fn step(&mut self, graph: &FlowGraph) {
        if self.steps_left == 0 {
            return;
        }
        self.steps_left -= 1;
        let ids: Vec<AddrId> = graph.nodes.iter().map(|n| n.id).collect();
        let mut forces: HashMap<AddrId, Vec2> = ids.iter().map(|id| (*id, Vec2::ZERO)).collect();
        let center = Pos2::ZERO;
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
            if self.pinned.contains(&node.id) {
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
