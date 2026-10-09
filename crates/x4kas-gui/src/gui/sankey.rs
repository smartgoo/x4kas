//! The transaction flow window: one transaction as a Sankey diagram (data in
//! `App.sankey`, from `UiCommand::TxSankey`). Its inputs stand on the left, its
//! outputs (and the fee) on the right, the transaction as a bar between them, and a
//! ribbon as wide as its amount joins each to the bar. A click on an input follows it
//! to the transaction that created it, a click on an output to the one that spent it
//! (when the index knows one); the window keeps a back/forward history of its own.
//! A right-click on an input or output shows its address's info pane.

use eframe::egui::{self, Color32, Pos2, Rect, RichText, Sense, Stroke, Ui, pos2, vec2};

use super::theme;
use super::widgets::{modal_window, placeholder, request_address, transaction_id};
use x4kas_core::app::App;
use x4kas_core::controller::{CommandSender, UiCommand};
use x4kas_core::explorer::{TxStatus, TxView};
use x4kas_core::format::{format_kas, format_utc, shorten_middle};

/// View state between frames.
#[derive(Default)]
pub struct SankeyWindowUi {
    /// The address whose right-click menu is open.
    menu_addr: Option<String>,
}

/// One side's node: an input (the output it spends) or an output of the transaction.
struct Node {
    /// What the bar says: the address's label or the address, shortened.
    name: String,
    /// The full address, for the tooltip and the menu.
    address: Option<String>,
    /// Sompi; `None` for an input whose spent output the index doesn't know.
    amount: Option<u64>,
    kind: NodeKind,
    /// Where a click goes: the creating or the spending transaction.
    follow: Option<String>,
    /// The bar's rect, once laid out.
    rect: Rect,
}

#[derive(Clone, Copy, PartialEq)]
enum NodeKind {
    Input,
    Coinbase,
    Output,
    Change,
    Fee,
}

const BAR_W: f32 = 12.0;
const TX_BAR_W: f32 = 18.0;
const GAP: f32 = 6.0;
const MIN_H: f32 = 4.0;
const PAD: f32 = 12.0;
/// Samples along a ribbon's length; each pair of neighbours is one quad of the mesh.
const RIBBON_STEPS: usize = 48;
/// The least width the header keeps for the transaction id before dropping its hint.
const TXID_MIN_WIDTH: f32 = 240.0;

impl SankeyWindowUi {
    /// `close`: this frame's Esc is for this window.
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        app: &mut App,
        cmd_tx: &CommandSender,
        close: bool,
    ) {
        if !app.sankey.open {
            return;
        }
        let title = match &app.sankey.txid {
            Some(txid) => format!("Transaction flow {}", shorten_middle(txid, 20)),
            None => "Transaction flow".to_string(),
        };
        let window = egui::Window::new(title)
            .id(egui::Id::new("sankey_window"))
            .default_size([960.0, 600.0])
            .resizable(true);
        let open = modal_window(ctx, window, close, |ui| self.contents(ui, app, cmd_tx));
        if !open {
            app.sankey.close();
        }
    }

    fn contents(&mut self, ui: &mut Ui, app: &mut App, cmd_tx: &CommandSender) {
        // Back and forward: the buttons, Cmd+[ / Cmd+] (Alt+arrows) and the mouse's
        // buttons, as everywhere else; the window is modal, so they are its own.
        let (mut back, mut forward) = ui.ctx().input_mut(|i| {
            (
                i.consume_key(egui::Modifiers::COMMAND, egui::Key::OpenBracket)
                    || i.consume_key(egui::Modifiers::ALT, egui::Key::ArrowLeft)
                    || i.pointer.button_pressed(egui::PointerButton::Extra1),
                i.consume_key(egui::Modifiers::COMMAND, egui::Key::CloseBracket)
                    || i.consume_key(egui::Modifiers::ALT, egui::Key::ArrowRight)
                    || i.pointer.button_pressed(egui::PointerButton::Extra2),
            )
        });
        ui.horizontal(|ui| {
            let sankey = &app.sankey;
            if ui
                .add_enabled(!sankey.back.is_empty(), egui::Button::new("◀").small())
                .on_hover_text("Back (Cmd+[)")
                .clicked()
            {
                back = true;
            }
            if ui
                .add_enabled(!sankey.forward.is_empty(), egui::Button::new("▶").small())
                .on_hover_text("Forward (Cmd+])")
                .clicked()
            {
                forward = true;
            }
            ui.separator();
            // Right to left, so the status and the hint keep their room and the
            // transaction id fits what is left (shortened in the middle if it must).
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let hint = "Click an input for the transaction that created it, an \
                            output for the one that spent it";
                let status = sankey.view.as_ref().map(status_text);
                let width = |text: &str| {
                    ui.fonts_mut(|f| {
                        f.layout_no_wrap(
                            text.to_string(),
                            egui::TextStyle::Body.resolve(ui.style()),
                            Color32::PLACEHOLDER,
                        )
                        .size()
                        .x
                    })
                };
                // The hint only while the id keeps a readable length beside it.
                let taken = status.as_deref().map_or(0.0, width) + width("Loading…") + 40.0;
                if ui.available_width() - taken - width(hint) > TXID_MIN_WIDTH {
                    ui.label(RichText::new(hint).weak().small());
                    ui.add_space(8.0);
                }
                if sankey.loading {
                    ui.label(RichText::new("Loading…").weak());
                    ui.spinner();
                }
                if let Some(status) = status {
                    ui.label(RichText::new(status).weak());
                }
                if let Some(txid) = &sankey.txid {
                    transaction_id(ui, txid);
                }
            });
        });
        let moved = if back {
            app.sankey.go_back()
        } else if forward {
            app.sankey.go_forward()
        } else {
            false
        };
        if moved && let Some(txid) = app.sankey.txid.clone() {
            let _ = cmd_tx.send(UiCommand::TxSankey {
                txid,
                block_hint: None,
            });
        }
        if let Some(err) = &app.sankey.error {
            ui.label(RichText::new(err).color(theme::ERROR));
        }
        let Some(view) = app.sankey.view.clone() else {
            if app.sankey.error.is_none() {
                // Keep the window its size while the transaction loads.
                ui.allocate_space(ui.available_size());
            }
            return;
        };
        if view.inputs.is_empty() && !view.is_coinbase && view.outputs.is_empty() {
            placeholder(ui, "Nothing to draw");
            return;
        }
        let labels = app.labels.clone();
        let name_of = |address: Option<&String>| -> String {
            match address {
                Some(a) => labels
                    .name(a)
                    .map(str::to_string)
                    .unwrap_or_else(|| shorten_middle(a, 20)),
                None => "unknown".to_string(),
            }
        };
        let mut inputs: Vec<Node> = if view.is_coinbase {
            vec![Node {
                name: "Block reward".to_string(),
                address: None,
                amount: Some(view.output_total()),
                kind: NodeKind::Coinbase,
                follow: None,
                rect: Rect::NOTHING,
            }]
        } else {
            view.inputs
                .iter()
                .map(|input| Node {
                    name: name_of(input.address.as_ref()),
                    address: input.address.clone(),
                    amount: input.amount,
                    kind: NodeKind::Input,
                    follow: Some(input.prev_txid.clone()),
                    rect: Rect::NOTHING,
                })
                .collect()
        };
        let mut outputs: Vec<Node> = view
            .outputs
            .iter()
            .enumerate()
            .map(|(i, output)| Node {
                name: name_of(output.address.as_ref()),
                address: output.address.clone(),
                amount: Some(output.amount),
                kind: if output.change >= x4kas_core::index::cluster::CHANGE_THRESHOLD {
                    NodeKind::Change
                } else {
                    NodeKind::Output
                },
                follow: app.sankey.spenders.get(i).cloned().flatten(),
                rect: Rect::NOTHING,
            })
            .collect();
        if let Some(fee) = view.fee.filter(|f| *f > 0) {
            outputs.push(Node {
                name: "Fee".to_string(),
                address: None,
                amount: Some(fee),
                kind: NodeKind::Fee,
                follow: None,
                rect: Rect::NOTHING,
            });
        }

        let (rect, response) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 3.0, theme::BG_DEEP);

        // Heights: the taller side fills the height, bars proportional to amounts with
        // a floor so every one can be seen and hit.
        let sum = |nodes: &[Node]| nodes.iter().filter_map(|n| n.amount).sum::<u64>();
        let total = sum(&inputs).max(sum(&outputs)).max(1) as f64;
        let usable = |count: usize| {
            (rect.height() - 2.0 * PAD - GAP * count.saturating_sub(1) as f32).max(1.0)
        };
        let scale = (usable(inputs.len()).min(usable(outputs.len())) as f64 / total).max(0.0);
        let height = |amount: Option<u64>| -> f32 {
            match amount {
                Some(a) => ((a as f64 * scale) as f32).max(MIN_H),
                None => MIN_H * 2.0,
            }
        };
        let lay_out = |nodes: &mut [Node], x: f32| {
            let stack: f32 = nodes.iter().map(|n| height(n.amount)).sum::<f32>()
                + GAP * nodes.len().saturating_sub(1) as f32;
            let mut y = rect.center().y - stack / 2.0;
            for node in nodes.iter_mut() {
                let h = height(node.amount);
                node.rect = Rect::from_min_size(pos2(x, y), vec2(BAR_W, h));
                y += h + GAP;
            }
        };
        lay_out(&mut inputs, rect.left() + PAD);
        lay_out(&mut outputs, rect.right() - PAD - BAR_W);
        let tx_h: f32 = (total * scale) as f32;
        let tx_rect = Rect::from_center_size(rect.center(), vec2(TX_BAR_W, tx_h.max(MIN_H)));
        let pointer = response.hover_pos();

        // Ribbons: inputs into the transaction, stacked in order; the transaction
        // into its outputs. The hovered one is brighter.
        let mut hovered_ribbon: Option<(bool, usize)> = None;
        let mut y_in = tx_rect.top();
        for (i, node) in inputs.iter().enumerate() {
            let h = ((node.amount.unwrap_or(0) as f64 * scale) as f32).max(1.0);
            let ribbon = Ribbon {
                x0: node.rect.right(),
                top0: node.rect.top(),
                bottom0: node.rect.bottom(),
                x1: tx_rect.left(),
                top1: y_in,
                bottom1: y_in + h,
            };
            let hovered = pointer.is_some_and(|p| ribbon.contains(p));
            if hovered {
                hovered_ribbon = Some((true, i));
            }
            ribbon.paint(&painter, ribbon_color(node.kind, hovered));
            y_in += h;
        }
        let mut y_out = tx_rect.top();
        for (i, node) in outputs.iter().enumerate() {
            let h = ((node.amount.unwrap_or(0) as f64 * scale) as f32).max(1.0);
            let ribbon = Ribbon {
                x0: tx_rect.right(),
                top0: y_out,
                bottom0: y_out + h,
                x1: node.rect.left(),
                top1: node.rect.top(),
                bottom1: node.rect.bottom(),
            };
            let hovered = pointer.is_some_and(|p| ribbon.contains(p));
            if hovered {
                hovered_ribbon = Some((false, i));
            }
            ribbon.paint(&painter, ribbon_color(node.kind, hovered));
            y_out += h;
        }

        // The transaction bar.
        painter.rect(
            tx_rect,
            2.0,
            theme::ACCENT_DIM,
            Stroke::new(1.0_f32, theme::ACCENT),
            egui::StrokeKind::Inside,
        );
        painter.text(
            tx_rect.center_top() + vec2(0.0, -4.0),
            egui::Align2::CENTER_BOTTOM,
            if view.is_coinbase {
                format!("{} minted", format_kas(view.output_total() as f64, 2))
            } else {
                format!(
                    "{} in · {} out",
                    view.input_total()
                        .map_or("?".to_string(), |t| format_kas(t as f64, 2)),
                    format_kas(view.output_total() as f64, 2)
                )
            },
            egui::FontId::monospace(theme::SMALL_FONT_SIZE),
            theme::TEXT_DIM,
        );

        // The bars with their names and amounts; a tooltip and a click on the hovered.
        let mut hovered_node: Option<(bool, usize)> = None;
        for (is_input, nodes) in [(true, &inputs), (false, &outputs)] {
            for (i, node) in nodes.iter().enumerate() {
                let hovered = pointer.is_some_and(|p| node.rect.expand(2.0).contains(p))
                    || hovered_ribbon == Some((is_input, i));
                if hovered {
                    hovered_node = Some((is_input, i));
                }
                let stroke = if hovered {
                    theme::ACCENT_BRIGHT
                } else {
                    theme::BORDER_HI
                };
                painter.rect(
                    node.rect,
                    2.0,
                    bar_color(node.kind),
                    Stroke::new(1.0_f32, stroke),
                    egui::StrokeKind::Inside,
                );
                // The name beside the bar, on the ribbon's side, with the amount on a
                // second line when the bar is tall enough, else after it.
                let amount = match node.amount {
                    Some(a) => format_kas(a as f64, 2),
                    None => "?".to_string(),
                };
                let (anchor, align) = if is_input {
                    (
                        node.rect.right_center() + vec2(6.0, 0.0),
                        egui::Align2::LEFT_CENTER,
                    )
                } else {
                    (
                        node.rect.left_center() - vec2(6.0, 0.0),
                        egui::Align2::RIGHT_CENTER,
                    )
                };
                let color = if hovered {
                    theme::TEXT_BRIGHT
                } else if node
                    .address
                    .as_ref()
                    .is_some_and(|a| labels.name(a).is_some())
                {
                    theme::ACCENT_BRIGHT
                } else {
                    theme::TEXT
                };
                let font = egui::FontId::monospace(theme::SMALL_FONT_SIZE);
                if node.rect.height() >= 30.0 {
                    painter.text(
                        anchor - vec2(0.0, 7.0),
                        align,
                        &node.name,
                        font.clone(),
                        color,
                    );
                    painter.text(
                        anchor + vec2(0.0, 7.0),
                        align,
                        amount,
                        font,
                        theme::TEXT_DIM,
                    );
                } else if node.rect.height() >= 12.0 || hovered {
                    painter.text(
                        anchor,
                        align,
                        format!("{} · {}", node.name, amount),
                        font,
                        color,
                    );
                }
            }
        }

        if let Some((is_input, i)) = hovered_node {
            let node = if is_input { &inputs[i] } else { &outputs[i] };
            let follow = node.follow.clone();
            response.clone().on_hover_ui_at_pointer(|ui| {
                if let Some(address) = &node.address {
                    ui.label(address);
                }
                let amount = match node.amount {
                    Some(a) => format!("{} KAS", format_kas(a as f64, 8)),
                    None => "Amount unknown: the spent output isn't in the index".to_string(),
                };
                ui.label(RichText::new(amount).weak());
                let hint = match (node.kind, &follow) {
                    (NodeKind::Coinbase, _) => "Newly minted by this block".to_string(),
                    (NodeKind::Fee, _) => "Paid to the miner".to_string(),
                    (NodeKind::Input, Some(txid)) => {
                        format!("Created by {} · click to follow", shorten_middle(txid, 20))
                    }
                    (NodeKind::Output | NodeKind::Change, Some(txid)) => {
                        format!("Spent by {} · click to follow", shorten_middle(txid, 20))
                    }
                    (NodeKind::Change, None) => {
                        "Likely change · unspent in the indexed window".to_string()
                    }
                    (_, None) => "Unspent in the indexed window".to_string(),
                };
                ui.label(RichText::new(hint).weak());
                if node.address.is_some() {
                    ui.label(RichText::new("Right-click for the address's info").weak());
                }
            });
            if follow.is_some() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            if response.clicked()
                && let Some(txid) = follow
            {
                app.sankey.navigate(txid.clone());
                let _ = cmd_tx.send(UiCommand::TxSankey {
                    txid,
                    block_hint: None,
                });
            }
            if response.secondary_clicked() {
                self.menu_addr = node.address.clone();
            }
        }
        self.node_menu(ui, &response);
    }

    /// The right-click menu of an input or output: its address's info pane.
    fn node_menu(&mut self, ui: &mut Ui, canvas: &egui::Response) {
        let open = if canvas.secondary_clicked() && self.menu_addr.is_some() {
            Some(egui::SetOpenCommand::Bool(true))
        } else if canvas.clicked() || canvas.secondary_clicked() {
            Some(egui::SetOpenCommand::Bool(false))
        } else {
            None
        };
        let Some(addr) = self.menu_addr.clone() else {
            return;
        };
        let shown = egui::Popup::menu(canvas)
            .id(ui.id().with("sankey_node_menu"))
            .open_memory(open)
            .at_pointer_fixed()
            .show(|ui| {
                ui.label(RichText::new(shorten_middle(&addr, 24)).weak());
                if ui.button("Show address info").clicked() {
                    request_address(ui.ctx(), &addr);
                    ui.close();
                }
            });
        if shown.is_none() {
            self.menu_addr = None;
        }
    }
}

fn status_text(view: &TxView) -> String {
    match &view.status {
        TxStatus::Accepted { time_ms, .. } => format!("accepted {}", format_utc(*time_ms)),
        TxStatus::Mempool { is_orphan: true } => "in the mempool (orphan)".to_string(),
        TxStatus::Mempool { is_orphan: false } => "in the mempool".to_string(),
        TxStatus::InBlock { time_ms, .. } => format!("in a block, {}", format_utc(*time_ms)),
    }
}

fn bar_color(kind: NodeKind) -> Color32 {
    match kind {
        NodeKind::Coinbase => theme::ACCENT_DIM,
        NodeKind::Fee => theme::SURFACE,
        NodeKind::Change => theme::SURFACE_HI,
        NodeKind::Input | NodeKind::Output => theme::SURFACE_HI,
    }
}

fn ribbon_color(kind: NodeKind, hovered: bool) -> Color32 {
    let base = match kind {
        NodeKind::Fee => theme::TEXT_DIM,
        NodeKind::Change => theme::ACCENT_DIM,
        NodeKind::Input | NodeKind::Output | NodeKind::Coinbase => theme::ACCENT,
    };
    base.gamma_multiply(if hovered { 0.75 } else { 0.4 })
}

/// A band from the vertical span `top0..bottom0` at `x0` to `top1..bottom1` at `x1`,
/// its edges easing from one to the other.
struct Ribbon {
    x0: f32,
    top0: f32,
    bottom0: f32,
    x1: f32,
    top1: f32,
    bottom1: f32,
}

impl Ribbon {
    fn edges_at(&self, t: f32) -> (f32, f32) {
        let s = t * t * (3.0 - 2.0 * t);
        (
            self.top0 + (self.top1 - self.top0) * s,
            self.bottom0 + (self.bottom1 - self.bottom0) * s,
        )
    }

    fn contains(&self, p: Pos2) -> bool {
        if self.x1 <= self.x0 || p.x < self.x0 || p.x > self.x1 {
            return false;
        }
        let t = (p.x - self.x0) / (self.x1 - self.x0);
        let (top, bottom) = self.edges_at(t);
        p.y >= top && p.y <= bottom
    }

    fn paint(&self, painter: &egui::Painter, color: Color32) {
        if self.x1 <= self.x0 {
            return;
        }
        let mut mesh = egui::Mesh::default();
        let mut tops = Vec::with_capacity(RIBBON_STEPS + 1);
        let mut bottoms = Vec::with_capacity(RIBBON_STEPS + 1);
        for step in 0..=RIBBON_STEPS {
            let t = step as f32 / RIBBON_STEPS as f32;
            let x = self.x0 + (self.x1 - self.x0) * t;
            let (top, bottom) = self.edges_at(t);
            tops.push(pos2(x, top));
            bottoms.push(pos2(x, bottom));
            mesh.colored_vertex(pos2(x, top), color);
            mesh.colored_vertex(pos2(x, bottom), color);
            if step > 0 {
                let i = (step * 2) as u32;
                mesh.add_triangle(i - 2, i - 1, i);
                mesh.add_triangle(i - 1, i + 1, i);
            }
        }
        painter.add(egui::Shape::mesh(mesh));
        // A mesh has no anti-aliasing, so where the gap to the next ribbon narrows to
        // under a pixel its edge breaks into dashes; a feathered line along each edge
        // smooths it.
        let edge = Stroke::new(1.0_f32, color);
        painter.add(egui::Shape::line(tops, edge));
        painter.add(egui::Shape::line(bottoms, edge));
    }
}
