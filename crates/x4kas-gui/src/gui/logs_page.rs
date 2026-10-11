//! The Logs page (`LogsPage`, opened by the health button in the top bar): a left
//! navigation of the Live view (this session's records, `LogViewer`), a search across
//! the log files, and every file in the logs folder; the chosen one's view on the
//! right, in place of the active tab. A file is read through its line index
//! (`App::logs.open`, built by the controller), only the lines on screen, so a file of
//! any size opens at once and scrolls freely. Every table's rows can be selected and
//! copied (`log_table::RowSelection`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use eframe::egui::{self, RichText, TextEdit, Ui};
use egui_extras::Column;

use super::log_table::{
    RowSelection, cell, copy_button, copy_requested, footer_height, header_height, level_color,
    level_combo, regex_and_case, search_line, selectable_table, submitted,
};
use super::log_view::LogViewer;
use super::theme;
use super::widgets::{
    CARD_GAP, card, card_overhead, card_with_header, placeholder, table_header, table_row_height,
};
use x4kas_core::app::{App, OpenLogFile};
use x4kas_core::controller::{CommandSender, UiCommand};
use x4kas_core::format::{format_number, format_size, format_utc, now_ms};
use x4kas_core::log_files::{LogFile, LogFileInfo, LogSource, SearchHit, SearchText};
use x4kas_core::logging::{LogBuffer, LogLevel};

/// Width of the navigation.
const NAV_WIDTH: f32 = 250.0;
/// How often the file list is read again while the page is on show.
const LIST_EVERY_SECS: f64 = 5.0;
/// How often a followed file is checked for new lines.
const TAIL_EVERY_SECS: f64 = 1.0;
/// The search results shown (of the most the search keeps).
const SHOWN_HITS: usize = 10_000;
/// The most lines "Copy all" copies from a file (a selection may be any size).
const COPY_ALL_MAX: usize = 50_000;

#[derive(Clone, PartialEq, Eq, Default)]
enum View {
    #[default]
    Live,
    Search,
    File(PathBuf),
}

#[derive(Default)]
pub struct LogsPage {
    view: View,
    live: LogViewer,
    file: FileView,
    search: SearchForm,
    /// When the file list was last asked for (egui time).
    listed_at: Option<f64>,
}

impl LogsPage {
    pub fn new(buffer: Option<Arc<LogBuffer>>) -> Self {
        Self {
            live: LogViewer::new(buffer),
            ..Self::default()
        }
    }

    pub fn show(&mut self, ui: &mut Ui, app: &mut App, cmd_tx: &CommandSender) {
        let now = ui.input(|i| i.time);
        if self.listed_at.is_none_or(|t| now - t >= LIST_EVERY_SECS) {
            self.listed_at = Some(now);
            let _ = cmd_tx.send(UiCommand::LogFilesRefresh);
        }
        egui::SidePanel::left("logs_nav")
            .resizable(true)
            .default_width(NAV_WIDTH)
            .width_range(200.0..=420.0)
            .frame(
                egui::Frame::new()
                    .fill(theme::SURFACE)
                    .inner_margin(egui::Margin::same(8)),
            )
            .show_inside(ui, |ui| self.nav(ui, app, cmd_tx));
        egui::CentralPanel::default()
            .frame(egui::Frame::new().inner_margin(egui::Margin {
                left: 10,
                right: 0,
                top: 0,
                bottom: 0,
            }))
            .show_inside(ui, |ui| {
                let height = ui.available_height() - card_overhead(ui) - 4.0;
                match self.view.clone() {
                    View::Live => {
                        card(ui, "Live", |ui| self.live.show(ui, height));
                    }
                    View::Search => {
                        self.search
                            .show(ui, app, cmd_tx, &mut self.file, &mut self.view, height)
                    }
                    View::File(path) => self.file.show(ui, app, cmd_tx, &path, now, height),
                }
            });
    }

    fn nav(&mut self, ui: &mut Ui, app: &mut App, cmd_tx: &CommandSender) {
        ui.label(
            RichText::new("Logs")
                .color(theme::TEXT_BRIGHT)
                .size(theme::CARD_TITLE_SIZE),
        );
        ui.add_space(6.0);
        ui.with_layout(egui::Layout::top_down_justified(egui::Align::Min), |ui| {
            if ui
                .selectable_label(self.view == View::Live, "Live")
                .on_hover_text("What this session logs, as it logs it")
                .clicked()
            {
                self.view = View::Live;
            }
            if ui
                .selectable_label(self.view == View::Search, "Search files")
                .on_hover_text("Search every log file, the GUI's and the CLI's")
                .clicked()
            {
                self.view = View::Search;
            }
        });
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Files").color(theme::LABEL));
            if let Some(files) = &app.logs.files {
                let bytes = files.iter().map(|f| f.bytes).sum();
                ui.label(RichText::new(format!("{} | {}", files.len(), format_size(bytes))).weak());
            }
        });
        ui.add_space(4.0);
        let Some(files) = app.logs.files.clone() else {
            placeholder(ui, "Reading the folder…");
            return;
        };
        if files.is_empty() {
            placeholder(ui, "No log files yet.");
            return;
        }
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.with_layout(egui::Layout::top_down_justified(egui::Align::Min), |ui| {
                for info in &files {
                    let selected = self.view == View::File(info.path.clone());
                    let text = format!(
                        "{} | {} | {}",
                        info.date,
                        info.source.label(),
                        format_size(info.bytes)
                    );
                    if ui
                        .selectable_label(selected, text)
                        .on_hover_text(info.path.display().to_string())
                        .clicked()
                    {
                        open_file(
                            &mut self.view,
                            &mut self.file,
                            app,
                            cmd_tx,
                            &info.path,
                            None,
                        );
                    }
                }
            });
        });
    }
}

/// Show `path`, opening it unless it is the open file, at `line` (selected) if given.
fn open_file(
    view: &mut View,
    file: &mut FileView,
    app: &App,
    cmd_tx: &CommandSender,
    path: &Path,
    line: Option<usize>,
) {
    *view = View::File(path.to_path_buf());
    let open = app.logs.open.as_ref().is_some_and(|o| o.path == path);
    if !open {
        let _ = cmd_tx.send(UiCommand::LogFileOpen(path.to_path_buf()));
        *file = FileView::default();
    }
    if let Some(line) = line {
        // Shown among all lines, so it is there to scroll to.
        file.min_level = None;
        file.only_matches = false;
        file.jump_to(line);
    }
}

/// One file's view: its filters, find, position and selection.
struct FileView {
    /// Only lines of records this severe or more.
    min_level: Option<LogLevel>,
    /// The text to find, as typed; sent with Enter or Find.
    find: SearchText,
    /// Show only the lines that match the find (else all, with ◀ ▶ through matches).
    only_matches: bool,
    /// The match ◀ ▶ are at, an index into the find's lines.
    current: Option<usize>,
    /// Keep the end in view, reading new lines as they come.
    follow: bool,
    go_to_text: String,
    /// The line to scroll to next frame (0-based).
    go_to: Option<usize>,
    /// The lines picked, by line number (a click, a search hit, a match, go to).
    selection: RowSelection,
    /// The level filter's lines: (lines in the file, level, the lines).
    level_rows: Option<(usize, LogLevel, Arc<Vec<u32>>)>,
    last_tail: f64,
}

impl Default for FileView {
    fn default() -> Self {
        Self {
            min_level: None,
            find: SearchText::default(),
            only_matches: true,
            current: None,
            follow: false,
            go_to_text: String::new(),
            go_to: None,
            selection: RowSelection::default(),
            level_rows: None,
            last_tail: 0.0,
        }
    }
}

impl FileView {
    /// Scroll to `line` and select it.
    fn jump_to(&mut self, line: usize) {
        self.go_to = Some(line);
        self.selection.set_single(line as u64);
        self.follow = false;
    }

    fn show(
        &mut self,
        ui: &mut Ui,
        app: &mut App,
        cmd_tx: &CommandSender,
        path: &Path,
        now: f64,
        height: f32,
    ) {
        let Some(open) = app.logs.open.clone().filter(|o| o.path == path) else {
            placeholder(ui, "Opening…");
            return;
        };
        let title = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        card_with_header(
            ui,
            &title,
            self,
            |ui, _| {
                if let Some(file) = &open.file
                    && let Ok(file) = file.try_read()
                {
                    ui.label(
                        RichText::new(format!(
                            "{} lines | {}",
                            format_number(file.len() as u64),
                            format_size(file.bytes())
                        ))
                        .weak(),
                    );
                }
            },
            |ui, view| view.body(ui, cmd_tx, &open, now, height),
        );
    }

    fn body(
        &mut self,
        ui: &mut Ui,
        cmd_tx: &CommandSender,
        open: &OpenLogFile,
        now: f64,
        height: f32,
    ) {
        ui.set_min_height(height);
        if let Some(e) = &open.error {
            ui.label(RichText::new(format!("Couldn't read the file: {e}")).color(theme::ERROR));
            return;
        }
        let Some(file) = &open.file else {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(format!("Indexing… {:.0}%", open.progress * 100.0));
            });
            return;
        };
        if self.follow && now - self.last_tail >= TAIL_EVERY_SECS {
            self.last_tail = now;
            let _ = cmd_tx.send(UiCommand::LogFileTail);
        }
        // Busy (the controller adding lines): try again next frame rather than wait.
        let Ok(file) = file.try_read() else {
            ui.ctx().request_repaint();
            placeholder(ui, "…");
            return;
        };
        let top = ui.cursor().top();
        let matches = open.find.as_ref().and_then(|f| f.lines.as_ref());
        // Which lines the table shows.
        let rows: Option<Arc<Vec<u32>>> = match (matches, self.only_matches, self.min_level) {
            (Some(lines), true, _) => Some(lines.clone()),
            (_, _, Some(min)) => Some(self.level_rows(&file, min)),
            _ => None,
        };
        let count = rows.as_ref().map_or(file.len(), |r| r.len());
        let line_at = |i: usize| rows.as_ref().map_or(i, |r| r[i] as usize);

        if self.toolbar(ui, cmd_tx, open, count) {
            let lines: Vec<String> = if self.selection.is_empty() {
                (0..count).filter_map(|i| file.line(line_at(i))).collect()
            } else {
                self.selection
                    .keys()
                    .filter_map(|l| file.line(l as usize))
                    .collect()
            };
            ui.ctx().copy_text(lines.join("\n"));
        }
        ui.add_space(4.0);

        let row_of = |line: usize| match &rows {
            Some(rows) => rows.binary_search(&(line as u32)).ok(),
            None => (line < file.len()).then_some(line),
        };
        let scroll_to = self.go_to.take().and_then(row_of);
        let row_height = table_row_height(ui);
        let table_height = height - (ui.cursor().top() - top) - footer_height(ui);
        let size = egui::vec2(ui.available_width(), table_height);
        ui.allocate_ui_with_layout(size, egui::Layout::top_down(egui::Align::Min), |ui| {
            ui.set_min_height(table_height);
            let mut table = selectable_table(ui, table_height - header_height(ui))
                .stick_to_bottom(self.follow)
                .column(Column::auto().at_least(56.0))
                .column(Column::remainder().at_least(300.0).clip(true));
            if let Some(row) = scroll_to {
                table = table.scroll_to_row(row, Some(egui::Align::Center));
            }
            table_header(table, &["Line", "Text"]).body(|body| {
                if count == 0 {
                    body.rows(row_height, 1, |mut row| {
                        row.col(|_| {});
                        row.col(|ui| placeholder(ui, "No lines match."));
                    });
                    return;
                }
                body.rows(row_height, count, |mut row| {
                    let index = row.index();
                    let line = line_at(index);
                    row.set_selected(self.selection.contains(line as u64));
                    let text = file.line(line).unwrap_or_default();
                    let color = match file.level(line) {
                        Some(level @ (LogLevel::Error | LogLevel::Warn)) => level_color(level),
                        Some(LogLevel::Debug | LogLevel::Trace) => theme::TEXT_DIM,
                        _ => theme::TEXT,
                    };
                    row.col(|ui| {
                        cell(ui, RichText::new(format_number(line as u64 + 1)).weak());
                    });
                    row.col(|ui| {
                        cell(ui, RichText::new(text).color(color));
                    });
                    let response = row.response();
                    if self
                        .selection
                        .interact(&response, index, count, |i| line_at(i) as u64)
                    {
                        self.selection.click(line as u64);
                    }
                });
            });
        });
        ui.add_space(4.0);
        ui.label(
            RichText::new(format!(
                "{} of {} lines shown",
                format_number(count as u64),
                format_number(file.len() as u64)
            ))
            .weak(),
        );
    }

    /// The controls (level, follow, copy, go to line), the find line and the find's
    /// state. Returns whether to copy.
    fn toolbar(
        &mut self,
        ui: &mut Ui,
        cmd_tx: &CommandSender,
        open: &OpenLogFile,
        count: usize,
    ) -> bool {
        let find = open.find.as_ref();
        let mut copy = copy_requested(ui) && !self.selection.is_empty();
        ui.horizontal(|ui| {
            level_combo(ui, "log_file_level", &mut self.min_level);
            ui.add_space(8.0);
            ui.checkbox(&mut self.follow, "Follow")
                .on_hover_text("Keep the end in view, reading new lines as they are written");
            copy |= copy_button(ui, self.selection.len(), count <= COPY_ALL_MAX);
            ui.add_space(8.0);
            let go = ui.add(
                TextEdit::singleline(&mut self.go_to_text)
                    .hint_text("Go to line")
                    .desired_width(100.0),
            );
            if submitted(&go)
                && let Ok(n) = self.go_to_text.trim().replace(',', "").parse::<usize>()
            {
                self.only_matches = false;
                self.min_level = None;
                self.jump_to(n.saturating_sub(1));
            }
        });

        let (mut find_clicked, mut clear) = (false, false);
        let SearchText {
            text,
            regex,
            case_sensitive,
        } = &mut self.find;
        let field = search_line(ui, text, "Find in this file", |ui| {
            find_clicked = ui.button("Find").clicked();
            if find.is_some()
                && ui
                    .small_button("×")
                    .on_hover_text("Clear the find")
                    .clicked()
            {
                clear = true;
            }
            regex_and_case(ui, regex, case_sensitive);
        });
        let sent = find.map(|f| (&f.text, f.min));
        let changed = sent != Some((&self.find, self.min_level));
        if (submitted(&field) || find_clicked) && (changed || find.is_none()) {
            self.current = None;
            let _ = cmd_tx.send(UiCommand::LogFileFind {
                text: self.find.clone(),
                min: self.min_level,
            });
        }
        if clear {
            self.find.text.clear();
            self.current = None;
            let _ = cmd_tx.send(UiCommand::LogFileFind {
                text: SearchText::default(),
                min: None,
            });
        }
        if let Some(find) = find {
            ui.horizontal(|ui| match (&find.error, &find.lines) {
                (Some(e), _) => {
                    ui.label(RichText::new(e).color(theme::ERROR));
                }
                (None, None) => {
                    ui.spinner();
                    ui.label(format!("Finding… {:.0}%", find.progress * 100.0));
                }
                (None, Some(lines)) => {
                    let at = self
                        .current
                        .map(|i| format!("{} of ", format_number(i as u64 + 1)))
                        .unwrap_or_default();
                    ui.label(format!("{at}{} matches", format_number(lines.len() as u64)));
                    let prev = ui
                        .small_button("◀")
                        .on_hover_text("Previous match")
                        .clicked();
                    let next = ui.small_button("▶").on_hover_text("Next match").clicked();
                    if (prev || next) && !lines.is_empty() {
                        let len = lines.len() as isize;
                        let i = match self.current {
                            Some(i) => (i as isize + if next { 1 } else { -1 }).rem_euclid(len),
                            None if next => 0,
                            None => len - 1,
                        } as usize;
                        self.current = Some(i);
                        self.jump_to(lines[i] as usize);
                    }
                    ui.checkbox(&mut self.only_matches, "Only matches");
                }
            });
        }
        copy
    }

    /// The lines at `min` or above, kept until the file grows or the level changes.
    fn level_rows(&mut self, file: &LogFile, min: LogLevel) -> Arc<Vec<u32>> {
        match &self.level_rows {
            Some((len, level, rows)) if *len == file.len() && *level == min => rows.clone(),
            _ => {
                let rows = Arc::new(file.lines_at(min));
                self.level_rows = Some((file.len(), min, rows.clone()));
                rows
            }
        }
    }
}

/// Which files a search reads.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Scope {
    #[default]
    All,
    Gui,
    Cli,
}

/// The search form and its results' view.
struct SearchForm {
    text: SearchText,
    min_level: Option<LogLevel>,
    scope: Scope,
    /// Only files of the last this many days; `None` for all.
    days: Option<u32>,
    /// The hits picked, by their index in the result.
    selection: RowSelection,
    /// The result the selection is about (its generation), cleared by a new one.
    selection_of: u64,
}

impl Default for SearchForm {
    fn default() -> Self {
        Self {
            text: SearchText::default(),
            min_level: None,
            scope: Scope::All,
            days: Some(7),
            selection: RowSelection::default(),
            selection_of: 0,
        }
    }
}

impl SearchForm {
    fn show(
        &mut self,
        ui: &mut Ui,
        app: &mut App,
        cmd_tx: &CommandSender,
        file: &mut FileView,
        view: &mut View,
        height: f32,
    ) {
        let files = self.files(app);
        let search = app.logs.search.clone();
        if self.selection_of != search.generation {
            self.selection_of = search.generation;
            self.selection.clear();
        }
        let hits: &[SearchHit] = search
            .result
            .as_ref()
            .map_or(&[], |r| &r.hits[..r.hits.len().min(SHOWN_HITS)]);
        card_with_header(
            ui,
            "Search Files",
            &mut (),
            |ui, _| {
                ui.label(
                    RichText::new(format!(
                        "{} files | {}",
                        files.len(),
                        format_size(files.iter().map(|f| f.bytes).sum())
                    ))
                    .weak(),
                );
            },
            |ui, _| {
                ui.set_min_height(height);
                let top = ui.cursor().top();
                let mut copy = copy_requested(ui) && !self.selection.is_empty();
                let mut start = false;
                ui.horizontal(|ui| {
                    level_combo(ui, "log_search_level", &mut self.min_level);
                    ui.add_space(8.0);
                    ui.label(RichText::new("Files:").color(theme::LABEL));
                    egui::ComboBox::from_id_salt("log_search_scope")
                        .selected_text(scope_text(self.scope))
                        .show_ui(ui, |ui| {
                            for scope in [Scope::All, Scope::Gui, Scope::Cli] {
                                ui.selectable_value(&mut self.scope, scope, scope_text(scope));
                            }
                        });
                    egui::ComboBox::from_id_salt("log_search_days")
                        .selected_text(days_text(self.days))
                        .show_ui(ui, |ui| {
                            for days in [Some(1), Some(7), Some(30), None] {
                                ui.selectable_value(&mut self.days, days, days_text(days));
                            }
                        });
                    ui.add_space(8.0);
                    if search.running {
                        if ui.button("Cancel").clicked() {
                            let _ = cmd_tx.send(UiCommand::LogSearchCancel);
                        }
                        ui.spinner();
                        ui.label(format!("{:.0}%", search.progress * 100.0));
                    } else {
                        start = ui
                            .add_enabled(!files.is_empty(), egui::Button::new("Search"))
                            .clicked();
                    }
                    copy |= copy_button(ui, self.selection.len(), !hits.is_empty());
                });
                ui.add_space(4.0);
                let SearchText {
                    text,
                    regex,
                    case_sensitive,
                } = &mut self.text;
                let field = search_line(
                    ui,
                    text,
                    "Text to find (empty: every line at the level)",
                    |ui| regex_and_case(ui, regex, case_sensitive),
                );
                if (start || submitted(&field)) && !search.running && !files.is_empty() {
                    let _ = cmd_tx.send(UiCommand::LogSearch {
                        files: files.iter().map(|f| f.path.clone()).collect(),
                        text: self.text.clone(),
                        min: self.min_level,
                    });
                }
                if copy {
                    let line = |hit: &SearchHit| {
                        let name = hit.file.file_name().unwrap_or_default().to_string_lossy();
                        format!("{name}:{}: {}", hit.line + 1, hit.text)
                    };
                    let lines: Vec<String> = if self.selection.is_empty() {
                        hits.iter().map(line).collect()
                    } else {
                        self.selection
                            .keys()
                            .filter_map(|i| hits.get(i as usize))
                            .map(line)
                            .collect()
                    };
                    ui.ctx().copy_text(lines.join("\n"));
                }
                ui.add_space(CARD_GAP / 2.0);
                if let Some(e) = &search.error {
                    ui.label(RichText::new(e).color(theme::ERROR));
                }
                let Some(result) = &search.result else {
                    return;
                };
                ui.label(
                    RichText::new(format!(
                        "{} {}{} in {} files | {} read",
                        format_number(result.hits.len() as u64),
                        if result.hits.len() == 1 {
                            "hit"
                        } else {
                            "hits"
                        },
                        if result.truncated {
                            " (the first ones: refine it)"
                        } else {
                            ""
                        },
                        result.files_searched,
                        format_size(result.bytes_searched),
                    ))
                    .weak(),
                );
                ui.add_space(4.0);
                let row_height = table_row_height(ui);
                let table_height = height - (ui.cursor().top() - top);
                let table = selectable_table(ui, table_height - header_height(ui))
                    .column(Column::auto().at_least(150.0))
                    .column(Column::auto().at_least(56.0))
                    .column(Column::remainder().at_least(300.0).clip(true));
                let mut picked = None;
                table_header(table, &["File", "Line", "Text"]).body(|body| {
                    if hits.is_empty() {
                        body.rows(row_height, 1, |mut row| {
                            row.col(|_| {});
                            row.col(|_| {});
                            row.col(|ui| placeholder(ui, "Nothing found."));
                        });
                        return;
                    }
                    body.rows(row_height, hits.len(), |mut row| {
                        let index = row.index();
                        let hit = &hits[index];
                        row.set_selected(self.selection.contains(index as u64));
                        let name = hit
                            .file
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_default();
                        row.col(|ui| {
                            cell(ui, RichText::new(name).weak());
                        });
                        row.col(|ui| {
                            cell(ui, RichText::new(format_number(hit.line as u64 + 1)).weak());
                        });
                        row.col(|ui| {
                            let color = hit.level.map_or(theme::TEXT, level_color);
                            cell(ui, RichText::new(&hit.text).color(color));
                        });
                        let response = row.response();
                        // A plain click opens the hit; drag and Cmd/Shift+click select.
                        if self
                            .selection
                            .interact(&response, index, hits.len(), |i| i as u64)
                        {
                            picked = Some((hit.file.clone(), hit.line));
                        }
                    });
                });
                if let Some((path, line)) = picked {
                    open_file(view, file, app, cmd_tx, &path, Some(line));
                }
            },
        );
    }

    /// The files the scope and period pick, newest first.
    fn files(&self, app: &App) -> Vec<LogFileInfo> {
        let since = self.days.map(|days| {
            // Today counts as the first day.
            let ms = now_ms().saturating_sub(u64::from(days.saturating_sub(1)) * 86_400_000);
            format_utc(ms)[..10].to_string()
        });
        app.logs
            .files
            .iter()
            .flatten()
            .filter(|f| match self.scope {
                Scope::All => true,
                Scope::Gui => f.source == LogSource::Gui,
                Scope::Cli => f.source == LogSource::Cli,
            })
            .filter(|f| since.as_ref().is_none_or(|since| f.date >= *since))
            .cloned()
            .collect()
    }
}

fn scope_text(scope: Scope) -> &'static str {
    match scope {
        Scope::All => "GUI and CLI",
        Scope::Gui => "GUI",
        Scope::Cli => "CLI",
    }
}

fn days_text(days: Option<u32>) -> String {
    match days {
        Some(1) => "Today".into(),
        Some(d) => format!("Last {d} days"),
        None => "All days".into(),
    }
}
