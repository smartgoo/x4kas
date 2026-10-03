//! Small building blocks shared by the tab views.

use eframe::egui::{self, RichText, Ui, WidgetText};

use super::theme;
use crate::app::{ActiveConnection, App};

/// A titled, framed panel.
pub fn card(ui: &mut Ui, title: &str, add_contents: impl FnOnce(&mut Ui)) {
    egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::same(12))
        .corner_radius(6)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(title).strong().color(theme::ACCENT));
            ui.add_space(6.0);
            add_contents(ui);
        });
}

/// A two-column label/value grid. Fill it with [`kv`].
pub fn kv_grid(ui: &mut Ui, id: &str, add_rows: impl FnOnce(&mut Ui)) {
    egui::Grid::new(id)
        .num_columns(2)
        .spacing([16.0, 4.0])
        .show(ui, add_rows);
}

/// One row of a [`kv_grid`].
pub fn kv(ui: &mut Ui, label: &str, value: impl Into<WidgetText>) {
    ui.label(RichText::new(label).weak());
    ui.label(value);
    ui.end_row();
}

/// Greyed-out placeholder text, e.g. "Waiting for data…".
pub fn placeholder(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).weak().italics());
}

/// Show a "Node is syncing…" notice for tabs that need a synced node.
/// Returns `true` if the notice was shown (the caller should skip its content).
pub fn syncing_guard(ui: &mut Ui, app: &App, title: &str) -> bool {
    if !app.is_node_syncing() {
        return false;
    }
    ui.vertical_centered(|ui| {
        ui.add_space(ui.available_height() / 3.0);
        ui.spinner();
        ui.label(
            RichText::new(format!(
                "Node is syncing… {title} data will be available once synced."
            ))
            .color(theme::WARN),
        );
    });
    true
}

/// Placeholder text for data that needs a direct node (URL or embedded), not the resolver.
pub fn direct_node_placeholder<'a>(app: &App, waiting: &'a str) -> &'a str {
    match app.connection {
        _ if app.has_direct_node => waiting,
        ActiveConnection::Resolver => "Disabled when using the public resolver",
        _ => "Not connected",
    }
}
