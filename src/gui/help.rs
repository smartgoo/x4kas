//! Keyboard shortcut / usage help window.

use eframe::egui::{self, RichText};

use super::theme;

const SHORTCUTS: &[(&str, &str)] = &[
    ("1 – 6", "Switch tab"),
    ("Ctrl+Tab / Ctrl+Shift+Tab", "Next / previous tab"),
    ("P", "Pause / resume polling"),
    (": or ⌘K / Ctrl+K", "Open command palette"),
    ("?", "Toggle this help"),
    ("Esc", "Close popup, palette or help"),
];

const PALETTE: &[(&str, &str)] = &[
    ("Enter", "Run command"),
    ("Tab", "Complete command name"),
    ("↑ / ↓", "Command history"),
];

pub fn show(ctx: &egui::Context, open: &mut bool) {
    if !*open {
        return;
    }
    egui::Window::new("Help")
        .open(open)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            section(ui, "Shortcuts", SHORTCUTS);
            ui.add_space(8.0);
            section(ui, "Command palette", PALETTE);
            ui.add_space(8.0);
            ui.label(
                RichText::new(
                    "Shortcuts are ignored while a text field has focus. \
                     Most actions are also available with the mouse.",
                )
                .weak(),
            );
        });
    if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        *open = false;
    }
}

fn section(ui: &mut egui::Ui, title: &str, rows: &[(&str, &str)]) {
    ui.label(RichText::new(title).strong().color(theme::ACCENT));
    egui::Grid::new(title)
        .num_columns(2)
        .spacing([24.0, 4.0])
        .show(ui, |ui| {
            for (keys, action) in rows {
                ui.label(RichText::new(*keys).monospace());
                ui.label(*action);
                ui.end_row();
            }
        });
}
