//! Alert toasts: an alert the watchlist raises pops up bottom-right over whatever tab is
//! shown and fades after a few seconds (the Alerts card on the Monitoring tab keeps
//! them). Its address opens Address Info like any other.

use std::time::{Duration, Instant};

use eframe::egui::{self, Align2, RichText, Stroke, vec2};

use super::theme;
use super::widgets::{address, request_address};
use x4kas_core::app::App;
use x4kas_core::watch::Alert;

const LIFETIME: Duration = Duration::from_secs(12);
const MAX_SHOWN: usize = 4;
const WIDTH: f32 = 360.0;

#[derive(Default)]
pub struct Toasts {
    /// Oldest first, with when each appeared.
    items: Vec<(Alert, Instant)>,
    /// `WatchState::alerts_raised` as of the last frame.
    seen: u64,
}

impl Toasts {
    /// Pick up the alerts raised since the last frame (newest first in `App`, so the
    /// oldest new one is queued first) and drop the ones that have expired.
    pub fn collect(&mut self, app: &App) {
        let raised = app.watch.alerts_raised;
        let new = raised.saturating_sub(self.seen) as usize;
        self.seen = raised;
        let now = Instant::now();
        for alert in app.watch.alerts.iter().take(new).rev() {
            self.items.push((alert.clone(), now));
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
                for (i, (alert, _)) in self.items.iter().enumerate() {
                    let who = alert
                        .name
                        .clone()
                        .or_else(|| app.labels.name(&alert.address).map(str::to_string));
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
                                    .on_hover_text("Open Address Info")
                                    .clicked()
                                {
                                    request_address(ui.ctx(), &alert.address);
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
                            address(ui, &alert.address);
                            ui.label(&alert.message);
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
