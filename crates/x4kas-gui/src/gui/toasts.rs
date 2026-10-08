//! Alert toasts: a watchlist event that trips an alert rule pops up bottom-right over
//! whatever tab is shown and fades after a few seconds (the Activity card on the
//! Monitoring tab keeps it, with its alert dot). Its address opens the info pane like any
//! other.

use std::time::{Duration, Instant};

use eframe::egui::{self, Align2, RichText, Stroke, vec2};

use super::theme;
use super::widgets::{address, request_address};
use x4kas_core::app::App;
use x4kas_core::watch::AddressEvent;

const LIFETIME: Duration = Duration::from_secs(12);
const MAX_SHOWN: usize = 4;
const WIDTH: f32 = 360.0;

#[derive(Default)]
pub struct Toasts {
    /// Oldest first, with when each appeared.
    items: Vec<(AddressEvent, Instant)>,
    /// `WatchState::events_raised` as of the last frame.
    seen: u64,
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
                self.items.push((event.clone(), now));
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
                for (i, (event, _)) in self.items.iter().enumerate() {
                    let who = app
                        .watch
                        .entry(&event.address)
                        .and_then(|w| w.name.clone())
                        .or_else(|| app.labels.name(&event.address).map(str::to_string));
                    egui::Frame::new()
                        .fill(theme::SURFACE_HI)
                        .stroke(Stroke::new(1.0_f32, theme::WARN))
                        .corner_radius(4)
                        .inner_margin(8)
                        .show(ui, |ui| {
                            ui.set_width(WIDTH - 16.0);
                            ui.horizontal(|ui| {
                                ui.label(RichText::new("⚠").color(theme::WARN));
                                let title = who.as_deref().unwrap_or("Watched address");
                                if ui
                                    .link(RichText::new(title).color(theme::TEXT_BRIGHT))
                                    .on_hover_text("Show address info")
                                    .clicked()
                                {
                                    request_address(ui.ctx(), &event.address);
                                }
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui.small_button("×").on_hover_text("Dismiss").clicked()
                                        {
                                            dismissed.push(i);
                                        }
                                    },
                                );
                            });
                            address(ui, &event.address);
                            for message in &event.alerts {
                                ui.label(message);
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
