//! Keyboard shortcut / usage help window.

use eframe::egui::{self, RichText};

use super::theme;
use super::widgets::{command_key, modal_window, section_title};

/// The shortcuts, with the command modifier named for the platform (`Cmd` on macOS,
/// `Ctrl` elsewhere) where the app checks `Modifiers::COMMAND`.
fn shortcuts(ctx: &egui::Context) -> Vec<(String, &'static str)> {
    let cmd = command_key(ctx);
    vec![
        ("1 – 6".to_string(), "Switch tab"),
        (
            "Ctrl+Tab / Ctrl+Shift+Tab".to_string(),
            "Next / previous tab",
        ),
        (
            format!("{cmd}+T / {cmd}+W"),
            "Explorer: Home to open a new sub tab / close the sub tab",
        ),
        (format!("{cmd}+L"), "Explorer: focus the search field"),
        (format!("{cmd}+Enter"), "Query: run the query"),
        (
            format!("{cmd}+[ / {cmd}+]"),
            "Back / forward (the info pane when open, else the Explorer tab; mouse back/forward buttons too)",
        ),
        ("Middle-click".to_string(), "Explorer: close a sub tab"),
        (
            format!("{cmd}+click"),
            "Open an address, block or transaction link in a new Explorer tab",
        ),
        ("P".to_string(), "Pause / resume polling"),
        ("Ctrl+`".to_string(), "Show / hide the terminal"),
        ("? / F1".to_string(), "Toggle this help"),
        (
            "Esc".to_string(),
            "Close one thing: a menu or field, else the top window, else the info pane, else Settings",
        ),
        (
            format!("{cmd}+Q"),
            "Quit (finishes writing the index first)",
        ),
    ]
}

/// `close`: this frame's Esc is for this window.
pub fn show(ctx: &egui::Context, open: &mut bool, close: bool) {
    if !*open {
        return;
    }
    *open = modal_window(
        ctx,
        egui::Window::new("Help").resizable(false),
        close,
        |ui| {
            section(ui, "Shortcuts", &shortcuts(ctx));
            ui.add_space(8.0);
            ui.label(
                RichText::new(
                    "Click the connection status in the status bar to switch nodes. \
                     Click any address for its info, history and watch settings; \
                     right-click it to label it, open it in the Explorer tab or in a web \
                     explorer, or click its label to edit it. A transaction id opens its \
                     Explorer page; inside the Explorer, every address and block hash \
                     navigates the current sub tab (right-click for a new one). \
                     The ⚙ button opens Settings. \
                     Shortcuts are ignored while a text field or the terminal has focus; \
                     click outside the terminal to leave it.",
                )
                .weak(),
            );
        },
    );
}

fn section(ui: &mut egui::Ui, title: &str, rows: &[(String, &str)]) {
    section_title(ui, title);
    egui::Grid::new(title)
        .num_columns(2)
        .spacing([24.0, 1.0])
        .min_row_height(theme::ROW_HEIGHT)
        .show(ui, |ui| {
            for (keys, action) in rows {
                ui.label(RichText::new(keys).color(theme::ACCENT_BRIGHT));
                ui.label(*action);
                ui.end_row();
            }
        });
}
