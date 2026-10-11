//! The Health page's Live view: the records the logger keeps in memory
//! (`logging::LogBuffer`, what the filter let through since launch), filtered by level
//! and text, newest at the bottom, following new ones as they come; rows can be
//! selected and copied (`RowSelection`).

use std::collections::VecDeque;
use std::sync::Arc;

use eframe::egui::{self, RichText, Ui};
use egui_extras::Column;

use super::log_table::{
    RowSelection, cell, copy_button, copy_requested, footer_height, header_height, level_color,
    level_combo, pause_button, search_line, selectable_table,
};
use super::widgets::{placeholder, table_header, table_row_height};
use x4kas_core::format::{format_clock_ms, format_number};
use x4kas_core::logging::{BUFFER_RECORDS, LogBuffer, LogLevel, LogRecord};

pub struct LogViewer {
    buffer: Option<Arc<LogBuffer>>,
    /// A copy of the buffer's records, topped up each frame with the new ones.
    records: VecDeque<LogRecord>,
    /// The sequence number to ask the buffer from next.
    next_seq: u64,
    /// Show only records this severe or more; `None` for every level.
    min_level: Option<LogLevel>,
    /// Show only records whose target, spans or message contain it (any case).
    search: String,
    /// Keep the newest record in view.
    follow: bool,
    /// Take no new records (they wait in the buffer).
    paused: bool,
    /// The rows picked, by record sequence number.
    selection: RowSelection,
    /// The last [`Self::shown`] and what it was worked out from, so a frame with
    /// nothing new doesn't filter every record again.
    shown_cache: Option<(ShownKey, Arc<Vec<usize>>)>,
}

/// What [`LogViewer::shown`] depends on: the records kept (by their first and last
/// sequence numbers), the level and the search.
#[derive(PartialEq)]
struct ShownKey {
    first: Option<u64>,
    last: Option<u64>,
    min_level: Option<LogLevel>,
    search: String,
}

impl Default for LogViewer {
    fn default() -> Self {
        Self {
            buffer: None,
            records: VecDeque::new(),
            next_seq: 0,
            min_level: None,
            search: String::new(),
            follow: true,
            paused: false,
            selection: RowSelection::default(),
            shown_cache: None,
        }
    }
}

impl LogViewer {
    pub fn new(buffer: Option<Arc<LogBuffer>>) -> Self {
        Self {
            buffer,
            ..Self::default()
        }
    }

    /// Take the records logged since the last frame.
    fn pull(&mut self) {
        let Some(buffer) = &self.buffer else {
            return;
        };
        if self.paused {
            return;
        }
        let new = buffer.since(self.next_seq);
        if let Some(last) = new.last() {
            self.next_seq = last.seq + 1;
        }
        self.records.extend(new);
        while self.records.len() > BUFFER_RECORDS {
            self.records.pop_front();
        }
    }

    /// The indices of the records the level and search let through.
    fn shown(&mut self) -> Arc<Vec<usize>> {
        let key = ShownKey {
            first: self.records.front().map(|r| r.seq),
            last: self.records.back().map(|r| r.seq),
            min_level: self.min_level,
            search: self.search.clone(),
        };
        if let Some((cached, shown)) = &self.shown_cache
            && *cached == key
        {
            return shown.clone();
        }
        let needle = self.search.trim().to_lowercase();
        let shown: Arc<Vec<usize>> = Arc::new(
            self.records
                .iter()
                .enumerate()
                .filter(|(_, r)| self.min_level.is_none_or(|min| r.level <= min))
                .filter(|(_, r)| {
                    needle.is_empty()
                        || [&r.message, &r.target, &r.spans]
                            .iter()
                            .any(|text| text.to_lowercase().contains(&needle))
                })
                .map(|(i, _)| i)
                .collect(),
        );
        self.shown_cache = Some((key, shown.clone()));
        shown
    }

    /// The rows to copy: the selected ones still kept, else every one shown.
    fn copy_text(&self, shown: &[usize]) -> String {
        let lines: Vec<String> = if self.selection.is_empty() {
            shown.iter().map(|i| self.records[*i].line()).collect()
        } else {
            // By sequence number, not an offset from the first: a pause longer than
            // the buffer leaves a gap in them.
            self.selection
                .keys()
                .filter_map(|seq| {
                    let i = self.records.binary_search_by_key(&seq, |r| r.seq).ok()?;
                    Some(self.records[i].line())
                })
                .collect()
        };
        lines.join("\n")
    }

    /// The controls, the search line, the table and a count, `height` tall in all.
    pub fn show(&mut self, ui: &mut Ui, height: f32) {
        self.pull();
        if self.buffer.is_none() {
            placeholder(ui, "Logging didn't start, so nothing is kept to show here.");
            return;
        }
        let top = ui.cursor().top();
        let shown = self.shown();
        let mut copy = copy_requested(ui) && !self.selection.is_empty();
        ui.horizontal(|ui| {
            level_combo(ui, "log_view_level", &mut self.min_level);
            ui.add_space(8.0);
            pause_button(ui, &mut self.paused);
            ui.checkbox(&mut self.follow, "Follow")
                .on_hover_text("Keep the newest record in view");
            copy |= copy_button(ui, self.selection.len(), !shown.is_empty());
        });
        let has_text = !self.search.is_empty();
        let mut clear = false;
        let field = search_line(ui, &mut self.search, "Search source or message", |ui| {
            clear = has_text && ui.small_button("×").clicked();
        });
        if clear || (field.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape))) {
            self.search.clear();
        }
        if copy {
            ui.ctx().copy_text(self.copy_text(&shown));
        }
        ui.add_space(4.0);

        // A fixed height, header and body, so the card doesn't grow as records come.
        let table_height = height - (ui.cursor().top() - top) - footer_height(ui);
        let size = egui::vec2(ui.available_width(), table_height);
        ui.allocate_ui_with_layout(size, egui::Layout::top_down(egui::Align::Min), |ui| {
            ui.set_min_height(table_height);
            self.table(ui, &shown);
        });
        ui.add_space(4.0);
        ui.label(
            RichText::new(format!(
                "{} of max {} records shown here",
                format_number(shown.len() as u64),
                format_number(BUFFER_RECORDS as u64),
            ))
            .weak(),
        );
    }

    fn table(&mut self, ui: &mut Ui, shown: &[usize]) {
        ui.push_id("log_view_table", |ui| {
            let row_height = table_row_height(ui);
            let table = selectable_table(ui, ui.available_height() - header_height(ui))
                .stick_to_bottom(self.follow)
                .column(Column::auto().at_least(100.0))
                .column(Column::auto().at_least(50.0))
                .column(Column::initial(260.0).at_least(120.0).clip(true))
                .column(Column::remainder().at_least(200.0).clip(true));
            table_header(table, &["Time (UTC)", "Level", "Source", "Message"]).body(|body| {
                if shown.is_empty() {
                    body.rows(row_height, 1, |mut row| {
                        row.col(|_| {});
                        row.col(|_| {});
                        row.col(|_| {});
                        row.col(|ui| placeholder(ui, "Nothing logged that matches."));
                    });
                    return;
                }
                let records = &self.records;
                let key_at = |i: usize| records[shown[i]].seq;
                body.rows(row_height, shown.len(), |mut row| {
                    let index = row.index();
                    let record = &records[shown[index]];
                    row.set_selected(self.selection.contains(record.seq));
                    row.col(|ui| {
                        cell(ui, RichText::new(format_clock_ms(record.time_ms)).weak());
                    });
                    row.col(|ui| {
                        cell(
                            ui,
                            RichText::new(record.level.as_str().to_uppercase())
                                .color(level_color(record.level)),
                        );
                    });
                    row.col(|ui| {
                        let source = if record.spans.is_empty() {
                            record.target.clone()
                        } else {
                            format!("{}: {}", record.spans, record.target)
                        };
                        cell(ui, RichText::new(source).weak());
                    });
                    row.col(|ui| {
                        cell(ui, record.message.as_str());
                    });
                    let response = row.response();
                    if self
                        .selection
                        .interact(&response, index, shown.len(), key_at)
                    {
                        self.selection.click(record.seq);
                    }
                });
            });
        });
    }
}
