//! Alert toasts: a watchlist event that trips an alert rule, or a row a watched query
//! found, pops up bottom-right over whatever tab is shown and fades after a few seconds
//! (the Activity card on the Monitoring tab keeps the former, the Query tab's sidebar
//! the latter). Its address, block or transaction opens the info pane like any other.

use std::time::{Duration, Instant};

use eframe::egui::{self, Align2, RichText, Stroke, vec2};

use super::theme;
use super::widgets::{address, block_hash, request_address, request_saved_query, transaction_id};
use x4kas_core::app::App;
use x4kas_core::index::hex;
use x4kas_core::query::exec::Cell;
use x4kas_core::query::watch::QueryEvent;
use x4kas_core::watch::AddressEvent;

const LIFETIME: Duration = Duration::from_secs(12);
const MAX_SHOWN: usize = 4;
const WIDTH: f32 = 360.0;

enum Toast {
    Watch(AddressEvent),
    Query(QueryEvent),
}

#[derive(Default)]
pub struct Toasts {
    /// Oldest first, with when each appeared.
    items: Vec<(Toast, Instant)>,
    /// `WatchState::events_raised` as of the last frame.
    seen: u64,
    /// `QueryState::events_raised` as of the last frame.
    seen_query: u64,
}

impl Toasts {
    /// Pick up the events with alerts since the last frame (newest first in `App`, so
    /// the oldest new one is queued first) and drop the ones that have expired.
    pub fn collect(&mut self, app: &App) {
        let raised = app.watch.events_raised;
        let new = raised.saturating_sub(self.seen) as usize;
        self.seen = raised;
        let now = Instant::now();
        for event in app.watch.events.iter().take(new).rev() {
            if !event.alerts.is_empty() {
                self.items.push((Toast::Watch(event.clone()), now));
            }
        }
        let raised = app.query.events_raised;
        let new = raised.saturating_sub(self.seen_query) as usize;
        self.seen_query = raised;
        for event in app.query.events.iter().take(new).rev() {
            // A watched query may be silent (its rows still land in the sidebar).
            let notify = app
                .query
                .saved
                .get(&event.query_id)
                .and_then(|q| q.watch.as_ref())
                .is_none_or(|w| w.notify);
            if notify {
                self.items.push((Toast::Query(event.clone()), now));
            }
        }
        self.items
            .retain(|(_, at)| now.duration_since(*at) < LIFETIME);
        if self.items.len() > MAX_SHOWN {
            self.items.drain(..self.items.len() - MAX_SHOWN);
        }
    }

    pub fn show(&mut self, ctx: &egui::Context, app: &App) {
        if self.items.is_empty() {
            return;
        }
        // Above the status bar (and the terminal, when open), not the screen edge.
        let corner = ctx.available_rect().right_bottom() - vec2(12.0, 12.0);
        let mut dismissed = Vec::new();
        egui::Area::new(egui::Id::new("alert_toasts"))
            .order(egui::Order::Foreground)
            .pivot(Align2::RIGHT_BOTTOM)
            .fixed_pos(corner)
            .show(ctx, |ui| {
                ui.set_width(WIDTH);
                for (i, (toast, _)) in self.items.iter().enumerate() {
                    egui::Frame::new()
                        .fill(theme::SURFACE_HI)
                        .stroke(Stroke::new(1.0_f32, theme::WARN))
                        .corner_radius(4)
                        .inner_margin(8)
                        .show(ui, |ui| {
                            ui.set_width(WIDTH - 16.0);
                            match toast {
                                Toast::Watch(event) => {
                                    let who = app.labels.name(&event.address);
                                    ui.horizontal(|ui| {
                                        ui.label(RichText::new("⚠").color(theme::WARN));
                                        let title = who.unwrap_or("Watched address");
                                        if ui
                                            .link(RichText::new(title).color(theme::TEXT_BRIGHT))
                                            .on_hover_text("Show address info")
                                            .clicked()
                                        {
                                            request_address(ui.ctx(), &event.address);
                                        }
                                        dismiss_button(ui, i, &mut dismissed);
                                    });
                                    address(ui, &event.address);
                                    for message in &event.alerts {
                                        ui.label(message);
                                    }
                                }
                                Toast::Query(event) => {
                                    ui.horizontal(|ui| {
                                        ui.label(RichText::new("◉").color(theme::WARN));
                                        let title = RichText::new(&event.query_name)
                                            .color(theme::TEXT_BRIGHT);
                                        let query = app
                                            .query
                                            .saved
                                            .get(&event.query_id)
                                            .and_then(|q| q.query().ok());
                                        match query {
                                            Some(query) => {
                                                if ui
                                                    .link(title)
                                                    .on_hover_text(
                                                        "A watched query found a new row; \
                                                         click to run the query",
                                                    )
                                                    .clicked()
                                                {
                                                    request_saved_query(
                                                        ui.ctx(),
                                                        query,
                                                        &event.query_id,
                                                    );
                                                }
                                            }
                                            None => {
                                                ui.label(title).on_hover_text(
                                                    "A watched query found a new row",
                                                );
                                            }
                                        }
                                        dismiss_button(ui, i, &mut dismissed);
                                    });
                                    primary_link(ui, event);
                                    ui.label(RichText::new(event.summary()).weak().small());
                                }
                            }
                        });
                    ui.add_space(6.0);
                }
            });
        for i in dismissed.into_iter().rev() {
            self.items.remove(i);
        }
        // The frame loop repaints every second, which is enough for the expiry.
    }
}

fn dismiss_button(ui: &mut egui::Ui, i: usize, dismissed: &mut Vec<usize>) {
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        if ui.small_button("×").on_hover_text("Dismiss").clicked() {
            dismissed.push(i);
        }
    });
}

/// A query event's row id as the link it is: an address, a block hash or a
/// transaction id (the cell the event's `primary` text came from).
pub fn primary_link(ui: &mut egui::Ui, event: &QueryEvent) {
    let primary = event.row.iter().find(|cell| {
        matches!(cell, Cell::Address(_) | Cell::Txid(_) | Cell::Hash(_))
            && cell.text() == event.primary
    });
    match primary {
        Some(Cell::Address(a)) => address(ui, a),
        Some(Cell::Txid(h)) => transaction_id(ui, &hex(h)),
        Some(Cell::Hash(h)) => {
            block_hash(ui, &hex(h), false);
        }
        _ => {
            ui.label(&event.primary);
        }
    }
}
