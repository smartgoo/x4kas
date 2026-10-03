//! Small building blocks shared by the tab views.

use eframe::egui::{self, Button, FontId, Margin, RichText, Stroke, Ui, WidgetText, pos2};

use super::theme;
use crate::app::{ActiveConnection, App};

/// Vertical space between stacked cards.
pub const CARD_GAP: f32 = 4.0;

/// A bordered pane with its title set into the top border, like a TUI block:
/// `┌─ Title ───────┐`.
pub fn card(ui: &mut Ui, title: &str, add_contents: impl FnOnce(&mut Ui)) {
    let font = FontId::monospace(theme::FONT_SIZE);
    let galley = ui
        .painter()
        .layout_no_wrap(format!(" {title} "), font, theme::ACCENT);
    let title_height = galley.size().y;

    // Room above the border for the top half of the title.
    ui.add_space(title_height / 2.0);
    let rect = egui::Frame::new()
        .fill(theme::BG)
        .stroke(Stroke::new(1.0_f32, theme::BORDER_HI))
        .corner_radius(3)
        .inner_margin(Margin {
            left: 10,
            right: 10,
            top: 11,
            bottom: 6,
        })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add_contents(ui);
        })
        .response
        .rect;

    // Cut the border behind the title, then draw it.
    let pos = pos2(rect.left() + 10.0, rect.top() - title_height / 2.0);
    let painter = ui.painter();
    painter.rect_filled(
        egui::Rect::from_min_size(pos, galley.size()),
        0.0,
        theme::BG,
    );
    painter.galley(pos, galley, theme::ACCENT);
}

/// Accent-colored heading for sections that aren't cards (side panels, help).
pub fn section_title(ui: &mut Ui, title: &str) {
    ui.label(RichText::new(title).color(theme::ACCENT));
}

/// Header cell for tables and striped grids.
pub fn column_header(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).color(theme::ACCENT));
}

/// A two-column label/value grid. Fill it with [`kv`].
pub fn kv_grid(ui: &mut Ui, id: &str, add_rows: impl FnOnce(&mut Ui)) {
    egui::Grid::new(id)
        .num_columns(2)
        .spacing([16.0, 1.0])
        .min_row_height(theme::ROW_HEIGHT)
        .show(ui, add_rows);
}

/// A dim field label with a trailing colon, e.g. `Network:`.
pub fn field_label(ui: &mut Ui, label: &str) -> egui::Response {
    ui.label(RichText::new(format!("{label}:")).weak())
}

/// One row of a [`kv_grid`].
pub fn kv(ui: &mut Ui, label: &str, value: impl Into<WidgetText>) {
    field_label(ui, label);
    ui.label(value);
    ui.end_row();
}

/// Greyed-out placeholder text, e.g. "Waiting for data…".
pub fn placeholder(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).weak());
}

pub fn yes_no(value: bool) -> &'static str {
    if value { "Yes" } else { "No" }
}

/// The main action of a form: filled with the accent color.
pub fn primary_button(text: &str) -> Button<'static> {
    Button::new(RichText::new(text).color(theme::BG_DEEP)).fill(theme::ACCENT)
}

/// A thin vertical divider between status bar segments.
pub fn divider(ui: &mut Ui) {
    ui.label(RichText::new("│").color(theme::BORDER_HI));
}

/// Placeholder text for data that needs a direct node (a URL), not the resolver.
pub fn direct_node_placeholder<'a>(app: &App, waiting: &'a str) -> &'a str {
    match app.connection {
        _ if app.has_direct_node => waiting,
        ActiveConnection::Resolver => "Disabled when using the public resolver",
        _ => "Not connected",
    }
}
