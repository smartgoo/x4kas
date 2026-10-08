//! The info pane: an overlay that slides in from the right edge over the active tab,
//! with the core of an address, block or transaction page (`x4kas_core::explorer`). A
//! click on any address, block hash or transaction id outside the Explorer tab opens it
//! (see `widgets::request_pane`); a click inside the pane, or while it is open, moves it
//! on, and its ◀ ▶ buttons walk that history. The page shown is `App.explorer.pane`,
//! loaded through the Explorer's cache like a sub tab's page; "Open in Explorer" moves
//! it to a page there. Being an overlay (a foreground `Area`, not a side panel), it
//! never relayouts the content behind it. The page is laid out in cards, as in the
//! Explorer, stacked for the pane's width.

use eframe::egui::{self, Button, CursorIcon, Id, Rect, RichText, Sense, Stroke, Ui, pos2, vec2};

use super::address::{self, AddressForms};
use super::explorer;
use super::theme;
use super::widgets::{CARD_GAP, card, copy_value, placeholder, request_explorer, section_title};
use x4kas_core::app::{App, ConnectionStatus};
use x4kas_core::controller::{CommandSender, UiCommand};
use x4kas_core::explorer::{AddressPageData, ExplorerPage, PageData, PageLoad};

/// The pane's width until the user resizes it (by dragging its left edge).
const DEFAULT_WIDTH: f32 = 675.0;
const MIN_WIDTH: f32 = 380.0;
/// The frame's margin around the contents.
const MARGIN: f32 = 10.0;
/// How long the slide in or out takes (the same both ways).
const SLIDE_SECS: f32 = 0.135;
/// Width of the resize handle on the left edge.
const HANDLE: f32 = 6.0;

pub struct InfoPane {
    forms: AddressForms,
    /// The page drawn: the open page, kept while the pane slides out.
    shown: Option<ExplorerPage>,
    /// The pane's width, frame included.
    width: f32,
}

impl Default for InfoPane {
    fn default() -> Self {
        Self {
            forms: AddressForms::default(),
            shown: None,
            width: DEFAULT_WIDTH,
        }
    }
}

/// What the title row asked for.
#[derive(Default)]
struct Nav {
    back: bool,
    forward: bool,
    close: bool,
}

impl InfoPane {
    /// Draw the pane, sliding in or out as `app.explorer.pane` opens or closes, over
    /// the area left between the panels (so call this after the bars and the terminal;
    /// it draws above the central panel whatever the order). Asks the controller for
    /// the page when it isn't loaded.
    pub fn show(&mut self, ctx: &egui::Context, app: &mut App, cmd_tx: &CommandSender) {
        if let Some(page) = app.explorer.pane_page() {
            self.shown = Some(page.clone());
        }
        let open = app.explorer.pane.is_some();
        let slide = ctx.animate_bool_with_time_and_easing(
            Id::new("info_pane_slide"),
            open,
            SLIDE_SECS,
            egui::emath::easing::cubic_out,
        );
        let Some(page) = self.shown.clone() else {
            return;
        };
        if slide <= 0.0 {
            self.shown = None;
            return;
        }

        let connected = matches!(app.node.connection_status, ConnectionStatus::Connected);
        if open && connected && app.explorer.needs_load(&page) {
            app.explorer.start_loading(page.clone());
            let _ = cmd_tx.send(UiCommand::ExplorerLoad(page.clone()));
        }

        // The area between the bars, above the terminal; the pane covers its right
        // part and slides in from beyond its right edge.
        let screen = ctx.available_rect();
        let max_width = (screen.width() - 40.0).max(MIN_WIDTH);
        self.width = self.width.clamp(MIN_WIDTH, max_width);
        let pane = Rect::from_min_size(
            pos2(screen.right() - self.width * slide, screen.top()),
            vec2(self.width, screen.height()),
        );
        let frame = egui::Frame::new()
            .fill(theme::SURFACE)
            .stroke(Stroke::new(1.0_f32, theme::BORDER))
            .inner_margin(egui::Margin::same(MARGIN as i8))
            .shadow(ctx.style().visuals.window_shadow);
        let mut nav = Nav::default();
        egui::Area::new(Id::new("info_pane"))
            .order(egui::Order::Foreground)
            .fixed_pos(pane.min)
            .constrain(false)
            .show(ctx, |ui| {
                ui.set_clip_rect(screen);
                frame.show(ui, |ui| {
                    ui.set_min_size(pane.size() - vec2(2.0 * MARGIN, 2.0 * MARGIN));
                    ui.set_max_size(pane.size() - vec2(2.0 * MARGIN, 2.0 * MARGIN));
                    self.contents(ui, app, &page, connected, cmd_tx, &mut nav);
                });
                if slide >= 1.0 {
                    self.resize_handle(ui, pane, max_width);
                }
            });
        if nav.back {
            app.explorer.pane_back();
        }
        if nav.forward {
            app.explorer.pane_forward();
        }
        if nav.close {
            app.explorer.close_pane();
        }
    }

    /// Dragging the pane's left edge resizes it, as a side panel's would.
    fn resize_handle(&mut self, ui: &mut Ui, pane: Rect, max_width: f32) {
        let handle = Rect::from_min_max(
            pos2(pane.left() - HANDLE / 2.0, pane.top()),
            pos2(pane.left() + HANDLE / 2.0, pane.bottom()),
        );
        let response = ui
            .interact(handle, ui.id().with("resize"), Sense::drag())
            .on_hover_cursor(CursorIcon::ResizeHorizontal);
        if response.dragged() {
            self.width = (self.width - response.drag_delta().x).clamp(MIN_WIDTH, max_width);
        }
        if response.hovered() || response.dragged() {
            ui.painter().vline(
                pane.left(),
                pane.y_range(),
                Stroke::new(2.0_f32, theme::ACCENT),
            );
        }
    }

    /// The title row (back, forward, what the page is, "Open in Explorer", close) and
    /// the page.
    fn contents(
        &mut self,
        ui: &mut Ui,
        app: &mut App,
        page: &ExplorerPage,
        connected: bool,
        cmd_tx: &CommandSender,
        nav: &mut Nav,
    ) {
        let title = match page {
            ExplorerPage::Address(_) => "Address",
            ExplorerPage::Block(_) => "Block",
            ExplorerPage::Transaction { .. } => "Transaction",
            ExplorerPage::Lookup(_) | ExplorerPage::Home => "Info",
        };
        let (can_back, can_forward) = app.explorer.pane.as_ref().map_or((false, false), |pane| {
            (!pane.back.is_empty(), !pane.forward.is_empty())
        });
        ui.horizontal(|ui| {
            nav.back = ui
                .add_enabled(can_back, Button::new("◀"))
                .on_hover_text("Back")
                .clicked();
            nav.forward = ui
                .add_enabled(can_forward, Button::new("▶"))
                .on_hover_text("Forward")
                .clicked();
            section_title(ui, title);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("×").on_hover_text("Close (Esc)").clicked() {
                    nav.close = true;
                }
                if ui
                    .button("Open in Explorer")
                    .on_hover_text("Show this as a page in the Explorer tab")
                    .clicked()
                {
                    request_explorer(ui.ctx(), page.clone(), false);
                }
            });
        });
        ui.add_space(4.0);

        let mut open_flows = false;
        let mut retry = false;
        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .show(ui, |ui| {
                self.page(
                    ui,
                    app,
                    page,
                    connected,
                    cmd_tx,
                    &mut open_flows,
                    &mut retry,
                );
                // Room under the last card for its shadow and the scroll bar's end.
                ui.add_space(CARD_GAP);
            });
        if open_flows && let ExplorerPage::Address(addr) = page {
            address::open_flow_graph(app, addr, cmd_tx);
        }
        if retry {
            app.explorer.start_loading(page.clone());
            let _ = cmd_tx.send(UiCommand::ExplorerLoad(page.clone()));
        }
    }

    /// The page as stacked cards, or its state in one card.
    #[allow(clippy::too_many_arguments)]
    fn page(
        &mut self,
        ui: &mut Ui,
        app: &App,
        page: &ExplorerPage,
        connected: bool,
        cmd_tx: &CommandSender,
        open_flows: &mut bool,
        retry: &mut bool,
    ) {
        match app.explorer.load(page) {
            None if !connected => card(ui, page.title().as_str(), |ui| {
                placeholder(ui, "Connect to a node to load this page.");
            }),
            None | Some(PageLoad::Loading) => card(ui, page.title().as_str(), |ui| {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Loading…");
                });
            }),
            Some(PageLoad::Failed(error)) => card(ui, "Not found", |ui| {
                copy_value(ui, page.query(), "Copy");
                ui.add_space(4.0);
                ui.label(RichText::new(error).color(theme::ERROR));
                ui.add_space(4.0);
                *retry = ui
                    .button("Retry")
                    .on_hover_text("Ask again, e.g. once the node or the index has it")
                    .clicked();
            }),
            Some(PageLoad::Ready(data)) => match &**data {
                PageData::Block(view) => {
                    card(ui, "Block", |ui| explorer::block_overview(ui, view));
                    ui.add_space(CARD_GAP);
                    card(
                        ui,
                        &format!("Transactions ({})", view.transactions.len()),
                        |ui| explorer::block_transactions(ui, app, view),
                    );
                }
                PageData::Address(data) => {
                    *open_flows = self.address_page(ui, app, page.query(), data, cmd_tx);
                }
                PageData::Transaction(view) => {
                    card(ui, "Transaction", |ui| explorer::tx_overview(ui, app, view));
                    ui.add_space(CARD_GAP);
                    card(ui, &format!("Inputs ({})", view.inputs.len()), |ui| {
                        explorer::tx_inputs(ui, view)
                    });
                    ui.add_space(CARD_GAP);
                    card(ui, &format!("Outputs ({})", view.outputs.len()), |ui| {
                        explorer::tx_outputs(ui, view)
                    });
                }
                PageData::Redirect(_) => card(ui, "Info", |ui| placeholder(ui, "Resolving…")),
            },
        }
    }

    /// The address page's cards: header (address, links, label), the body's and the
    /// watch settings, as in the Explorer.
    fn address_page(
        &mut self,
        ui: &mut Ui,
        app: &App,
        addr: &str,
        data: &AddressPageData,
        cmd_tx: &CommandSender,
    ) -> bool {
        self.forms.sync(app, addr);
        card(ui, "Address", |ui| {
            self.forms
                .header(ui, app, addr, data.online_result.as_deref(), cmd_tx);
            if let Some(error) = &data.error {
                ui.label(RichText::new(error).color(theme::ERROR));
            }
        });
        ui.add_space(CARD_GAP);
        let open_flows = address::body(ui, app, addr, &data.view, data.loading_more, cmd_tx);
        ui.add_space(CARD_GAP);
        card(ui, "Watch", |ui| {
            self.forms.watch_settings(ui, app, addr, cmd_tx);
        });
        open_flows
    }
}
