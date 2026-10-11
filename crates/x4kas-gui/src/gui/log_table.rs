//! What the Logs page's tables share: selecting rows (a drag across them, Cmd/Ctrl+click
//! to add or drop one, Shift+click for the rows from the last one) to copy them, and
//! the controls above them (the level, the search line, pause, copy).

use std::collections::BTreeSet;

use eframe::egui::{self, Response, RichText, TextEdit, Ui};
use egui_extras::TableBuilder;

use super::theme;
use super::widgets::page_table;
use x4kas_core::logging::LogLevel;

/// The rows picked in a table, by key: anything that grows in the order the table
/// shows the rows (a record's sequence number, a line number, a hit's index), so the
/// selection lists them in that order.
#[derive(Default)]
pub struct RowSelection {
    selected: BTreeSet<u64>,
    /// The row a Shift+click extends from.
    anchor: Option<u64>,
    /// Where a drag across rows started (its place among the rows shown).
    drag_from: Option<usize>,
}

impl RowSelection {
    pub fn contains(&self, key: u64) -> bool {
        self.selected.contains(&key)
    }

    pub fn len(&self) -> usize {
        self.selected.len()
    }

    pub fn is_empty(&self) -> bool {
        self.selected.is_empty()
    }

    pub fn clear(&mut self) {
        self.selected.clear();
        self.anchor = None;
    }

    /// Just this row (a jump to it, a plain click).
    pub fn set_single(&mut self, key: u64) {
        self.selected = BTreeSet::from([key]);
        self.anchor = Some(key);
    }

    /// A plain click: the row alone, or nothing when it was the row alone already.
    pub fn click(&mut self, key: u64) {
        if self.selected.len() == 1 && self.selected.contains(&key) {
            self.clear();
        } else {
            self.set_single(key);
        }
    }

    /// The keys picked, in the table's order.
    pub fn keys(&self) -> impl Iterator<Item = u64> + '_ {
        self.selected.iter().copied()
    }

    /// Handle row `index` of the `count` shown (`key_at` gives each one's key): a drag
    /// across rows picks them, Cmd/Ctrl+click adds or drops this one, Shift+click picks
    /// the rows from the last one picked. Returns whether it was a plain click, which
    /// the caller gives its own meaning (selecting the row, opening it).
    pub fn interact(
        &mut self,
        row: &Response,
        index: usize,
        count: usize,
        key_at: impl Fn(usize) -> u64,
    ) -> bool {
        let key = key_at(index);
        if row.drag_started() {
            self.drag_from = Some(index);
            self.set_single(key);
        }
        if let Some(from) = self.drag_from {
            if !row.ctx.input(|i| i.pointer.primary_down()) {
                self.drag_from = None;
            } else if row
                .ctx
                .input(|i| i.pointer.interact_pos())
                .is_some_and(|p| row.rect.y_range().contains(p.y))
            {
                let (a, b) = (from.min(index), from.max(index).min(count - 1));
                self.selected = (a..=b).map(&key_at).collect();
            }
        }
        if !row.clicked() {
            return false;
        }
        let modifiers = row.ctx.input(|i| i.modifiers);
        if modifiers.command {
            if !self.selected.remove(&key) {
                self.selected.insert(key);
            }
            self.anchor = Some(key);
        } else if modifiers.shift {
            // From the anchor's row (looked for among those shown) to this one.
            let from = self
                .anchor
                .and_then(|anchor| (0..count).find(|i| key_at(*i) == anchor))
                .unwrap_or(index);
            let (a, b) = (from.min(index), from.max(index));
            self.selected = (a..=b).map(&key_at).collect();
        } else {
            return true;
        }
        false
    }
}

/// A table whose rows can be clicked and dragged across (for [`RowSelection`]).
pub fn selectable_table(ui: &mut Ui, max_height: f32) -> TableBuilder<'_> {
    page_table(ui, max_height).sense(egui::Sense::click_and_drag())
}

/// A cell's text: cut short to the column (whole on hover), and not selectable, so
/// the row keeps its hover and click wherever the pointer is.
pub fn cell(ui: &mut Ui, text: impl Into<egui::WidgetText>) -> Response {
    ui.add(egui::Label::new(text).truncate().selectable(false))
}

/// Cmd/Ctrl+C with no text field taking it.
pub fn copy_requested(ui: &Ui) -> bool {
    !ui.ctx().wants_keyboard_input()
        && ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Copy)))
}

/// Copy: the rows picked, or every row shown when none is. Returns whether clicked.
pub fn copy_button(ui: &mut Ui, selected: usize, enabled: bool) -> bool {
    let text = if selected > 0 {
        format!("Copy {selected} selected")
    } else {
        "Copy all".to_string()
    };
    ui.add_enabled(enabled || selected > 0, egui::Button::new(text))
        .on_hover_text(
            "Copy the selected rows (Cmd/Ctrl+C), or every row shown when none is. Select \
             by dragging across rows, Cmd/Ctrl+click to add or drop a row, Shift+click for \
             a range.",
        )
        .clicked()
}

/// Play or pause: a button showing what a click does.
pub fn pause_button(ui: &mut Ui, paused: &mut bool) {
    let (icon, hover) = if *paused {
        ("▶", "Resume: show the records logged meanwhile and go on")
    } else {
        (
            "⏸",
            "Pause: stop taking new records (they wait until resumed)",
        )
    };
    if ui.button(icon).on_hover_text(hover).clicked() {
        *paused = !*paused;
    }
}

/// The level filter: every level, or a level and the more severe ones.
pub fn level_combo(ui: &mut Ui, id: &str, level: &mut Option<LogLevel>) {
    ui.label(RichText::new("Show:").color(theme::LABEL));
    egui::ComboBox::from_id_salt(id)
        .selected_text(level_text(*level))
        .show_ui(ui, |ui| {
            ui.selectable_value(level, None, level_text(None));
            for l in LogLevel::ALL {
                ui.selectable_value(level, Some(l), level_text(Some(l)));
            }
        });
}

fn level_text(level: Option<LogLevel>) -> &'static str {
    match level {
        None => "All log levels",
        Some(LogLevel::Error) => "Errors",
        Some(LogLevel::Warn) => "Warnings and errors",
        Some(LogLevel::Info) => "Info and above",
        Some(LogLevel::Debug) => "Debug and above",
        Some(LogLevel::Trace) => "Trace and above",
    }
}

/// A search field across the whole width, `trailing` widgets at its right end (added
/// right to left: the first one rightmost). Returns the field's response.
pub fn search_line(
    ui: &mut Ui,
    text: &mut String,
    hint: &str,
    trailing: impl FnOnce(&mut Ui),
) -> Response {
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            trailing(ui);
            ui.add(
                TextEdit::singleline(text)
                    .hint_text(hint)
                    .desired_width(ui.available_width()),
            )
        })
        .inner
    })
    .inner
}

/// The regex and case toggles at the end of a search line (right to left: `Aa`, then
/// `.*` before it). Their flags apart from the text, which the field borrows.
pub fn regex_and_case(ui: &mut Ui, regex: &mut bool, case_sensitive: &mut bool) {
    ui.toggle_value(case_sensitive, "Aa")
        .on_hover_text("Match case");
    ui.toggle_value(regex, ".*")
        .on_hover_text("Regular expression");
}

/// Enter pressed in a field that had focus.
pub fn submitted(field: &Response) -> bool {
    field.lost_focus() && field.ctx.input(|i| i.key_pressed(egui::Key::Enter))
}

pub fn level_color(level: LogLevel) -> egui::Color32 {
    match level {
        LogLevel::Error => theme::ERROR,
        LogLevel::Warn => theme::WARN,
        LogLevel::Info => theme::ACCENT,
        LogLevel::Debug => theme::LABEL,
        LogLevel::Trace => theme::TEXT_DIM,
    }
}

/// The height of the line of text under a table (a count), with its gap.
pub fn footer_height(ui: &Ui) -> f32 {
    ui.text_style_height(&egui::TextStyle::Body) + ui.spacing().item_spacing.y + 4.0
}

/// A table header's height, as `table_header` draws it.
pub fn header_height(ui: &Ui) -> f32 {
    theme::ROW_HEIGHT + 4.0 + ui.spacing().item_spacing.y
}
