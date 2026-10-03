//! Keyboard shortcut / usage help window.

use eframe::egui::{self, RichText};

use super::theme;
use super::widgets::{modal_window, section_title};

const SHORTCUTS: &[(&str, &str)] = &[
    ("1 – 5", "Switch tab"),
    ("Ctrl+Tab / Ctrl+Shift+Tab", "Next / previous tab"),
    ("P", "Pause / resume polling"),
    ("Ctrl+`", "Show / hide the terminal"),
    ("? / F1", "Toggle this help"),
    ("Esc", "Close popup or help"),
];

pub fn show(ctx: &egui::Context, open: &mut bool) {
    if !*open {
        return;
    }
    *open = modal_window(ctx, egui::Window::new("Help").resizable(false), |ui| {
        section(ui, "Shortcuts", SHORTCUTS);
        ui.add_space(8.0);
        ui.label(
            RichText::new(
                "Click the connection status in the status bar to switch nodes. \
                     Shortcuts are ignored while a text field or the terminal has focus; \
                     click outside the terminal to leave it.",
            )
            .weak(),
        );
    });
}

fn section(ui: &mut egui::Ui, title: &str, rows: &[(&str, &str)]) {
    section_title(ui, title);
    egui::Grid::new(title)
        .num_columns(2)
        .spacing([24.0, 1.0])
        .min_row_height(theme::ROW_HEIGHT)
        .show(ui, |ui| {
            for (keys, action) in rows {
                ui.label(RichText::new(*keys).color(theme::ACCENT_BRIGHT));
                ui.label(*action);
                ui.end_row();
            }
        });
}
