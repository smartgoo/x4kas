//! Settings page (`SettingsPage`, opened from the ⚙ button in the top bar): a left
//! navigation of sections with the chosen one's content on the right, in place of the
//! active tab. Address Labels: the public list (its sources and refresh) and the user's
//! every known label (label, address, source; add, edit in place, remove). Index: the
//! opt-in data the address index keeps (full payloads, redeem scripts), how long it
//! keeps data (shortening it asks first) and the width of its time slabs (applied by a
//! Resync). Reset: wipe
//! everything the app keeps under `~/.x4kas` (but exports and logs) and relaunch it; the GUI
//! closes through its usual shutdown first (`SettingsPage::reset_requested`), and
//! `main` deletes and relaunches once the window and runtime are gone.

use eframe::egui::{self, RichText, TextEdit, Ui};
use egui_extras::{Column, TableBuilder};

use super::theme;
use super::widgets::{
    CARD_GAP, address_bare, card, card_with_header, copy_value, danger_button, edit_label_cell,
    kv_grid, kv_with, label_cell, modal_window, or_dash, placeholder, primary_button,
    request_label, section_title,
};
use x4kas_core::app::{App, ChainPhase};
use x4kas_core::config::{
    DEFAULT_SLAB_HOURS, IndexFeature, IndexSettingsEdit, RETENTION_DAYS_CHOICES, Retention,
    SLAB_HOURS_CHOICES, data_dir, open_folder,
};
use x4kas_core::controller::{CommandSender, UiCommand};
use x4kas_core::format::{format_duration, format_number, format_size, format_utc, now_ms};
use x4kas_core::labels::LabelSource;
use x4kas_core::logging::{
    DEFAULT_KEEP_DAYS, KEEP_DAYS_CHOICES, LogFormat, LogLevel, LogSettingsEdit, logs_dir,
    validate_filter,
};
use x4kas_core::watch::validate_address;

/// Width of the section navigation.
const NAV_WIDTH: f32 = 170.0;
/// fjall's documentation, linked from the Slab Size card.
const FJALL_DOCS: &str = "https://docs.rs/fjall";
/// Rows of labels shown before the table scrolls.
const TABLE_HEIGHT: f32 = 420.0;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Section {
    #[default]
    AddressLabels,
    Index,
    Logs,
    Reset,
}

impl Section {
    const ALL: [Self; 4] = [Self::AddressLabels, Self::Index, Self::Logs, Self::Reset];

    fn label(self) -> &'static str {
        match self {
            Section::AddressLabels => "Address Labels",
            Section::Index => "DAG Index",
            Section::Logs => "Logs",
            Section::Reset => "Reset",
        }
    }
}

/// The page's own state: the chosen section and its forms (whether it is on show is
/// `GuiApp::page`).
#[derive(Default)]
pub struct SettingsPage {
    section: Section,
    /// The "add a label" form.
    new_address: String,
    new_name: String,
    /// Narrows the user's labels table.
    filter: String,
    /// The address whose 🗑 was clicked once, and when: a second click within
    /// `CONFIRM_SECS` deletes.
    confirm_delete: Option<(String, f64)>,
    /// A shorter retention picked, waiting for the user to confirm the deletion.
    confirm_retention: Option<Retention>,
    /// The window confirming a reset is open.
    confirm_reset: bool,
    /// What went wrong with the reset this launch followed (`--reset-report`).
    reset_report: Option<String>,
    /// The user confirmed a reset; the frame loop takes it and closes the app.
    pub reset_requested: bool,
    /// The extra log filter being typed, until applied (`None`: the saved one).
    log_filter: Option<String>,
    /// The log file settings as the app launched with them (taken when the section is
    /// first drawn, before anything here can change them).
    log_files_launch: Option<LogFilesAtLaunch>,
}

/// How long a first click on 🗑 waits for the confirming second one.
const CONFIRM_SECS: f64 = 3.0;

impl SettingsPage {
    /// Open on the Reset section with what went wrong with the reset that relaunched
    /// the app.
    pub fn show_reset_report(&mut self, report: String) {
        self.section = Section::Reset;
        self.reset_report = Some(report);
    }

    /// A window of the page (the reset confirmation) is on show.
    pub fn modal_open(&self) -> bool {
        self.confirm_reset
    }

    /// Forget an unanswered reset confirmation, so it doesn't come back with the page.
    pub fn cancel_reset(&mut self) {
        self.confirm_reset = false;
    }

    /// `close_dialog`: the frame loop hands the page's window this frame's Esc.
    pub fn show(&mut self, ui: &mut Ui, app: &mut App, cmd_tx: &CommandSender, close_dialog: bool) {
        self.reset_dialog(ui.ctx(), close_dialog);
        egui::SidePanel::left("settings_nav")
            .resizable(false)
            .exact_width(NAV_WIDTH)
            .frame(
                egui::Frame::new()
                    .fill(theme::SURFACE)
                    .inner_margin(egui::Margin::same(8)),
            )
            .show_inside(ui, |ui| {
                ui.label(
                    RichText::new("Settings")
                        .color(theme::TEXT_BRIGHT)
                        .size(theme::CARD_TITLE_SIZE),
                );
                ui.add_space(6.0);
                ui.with_layout(egui::Layout::top_down_justified(egui::Align::Min), |ui| {
                    for section in Section::ALL {
                        if ui
                            .selectable_label(self.section == section, section.label())
                            .clicked()
                        {
                            self.section = section;
                            if section == Section::Logs {
                                let _ = cmd_tx.send(UiCommand::LogFilesRefresh);
                            }
                        }
                    }
                });
            });
        egui::CentralPanel::default()
            .frame(egui::Frame::new().inner_margin(egui::Margin {
                left: 10,
                right: 0,
                top: 0,
                bottom: 0,
            }))
            .show_inside(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| match self.section {
                    Section::AddressLabels => self.address_labels(ui, app, cmd_tx),
                    Section::Index => index(ui, app, cmd_tx, &mut self.confirm_retention),
                    Section::Logs => self.logs(ui, app, cmd_tx),
                    Section::Reset => self.reset(ui, app, cmd_tx),
                });
            });
    }

    /// The log level and extra filter (applied at once) and the files (from the next
    /// launch).
    fn logs(&mut self, ui: &mut Ui, app: &mut App, cmd_tx: &CommandSender) {
        if let Some(err) = &app.logs.error {
            ui.label(RichText::new(err).color(theme::ERROR));
            ui.add_space(CARD_GAP);
        }
        card(ui, "Log Level", |ui| self.log_level(ui, app, cmd_tx));
        ui.add_space(CARD_GAP);
        let launch = self
            .log_files_launch
            .get_or_insert_with(|| LogFilesAtLaunch::of(&app.logs.settings))
            .clone();
        card(ui, "Log Files", |ui| log_files(ui, app, cmd_tx, &launch));
    }

    fn log_level(&mut self, ui: &mut Ui, app: &mut App, cmd_tx: &CommandSender) {
        if let Some(var) = app.logs.env_override {
            ui.label(
                RichText::new(format!(
                    "{var} is set, so it decides what is logged. These settings apply when \
                     the app is launched without it."
                ))
                .color(theme::WARN),
            );
            ui.add_space(6.0);
        }
        let send = |edit: LogSettingsEdit| {
            let _ = cmd_tx.send(UiCommand::LogSettingsEdit(edit));
        };
        ui.add_enabled_ui(app.logs.env_override.is_none(), |ui| {
            let mut level = app.logs.settings.level;
            ui.horizontal(|ui| {
                ui.label(RichText::new("Level:").color(theme::LABEL));
                egui::ComboBox::from_id_salt("log_level")
                    .selected_text(level_text(level))
                    .show_ui(ui, |ui| {
                        for l in LogLevel::ALL {
                            ui.selectable_value(&mut level, l, level_text(l))
                                .on_hover_text(level_doc(l));
                        }
                    });
            });
            if level != app.logs.settings.level {
                // Shown at once; the controller saves it and changes the filter.
                app.logs.settings.level = level;
                send(LogSettingsEdit::Level(level));
            }
            ui.add_space(8.0);

            let saved = app.logs.settings.filter.clone();
            let draft = self.log_filter.get_or_insert_with(|| saved.clone());
            let valid = validate_filter(draft);
            let changed = draft.trim() != saved;
            let mut apply = false;
            ui.horizontal(|ui| {
                ui.label(RichText::new("Extra filter:").color(theme::LABEL));
                let field = ui.add(
                    TextEdit::singleline(draft)
                        .hint_text("e.g. fjall=info,x4kas_core::index=trace")
                        .desired_width(380.0),
                );
                if field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    apply = true;
                }
                if field.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    *draft = saved.clone();
                }
                apply |= ui
                    .add_enabled(changed && valid.is_ok(), egui::Button::new("Apply"))
                    .clicked();
            });
            if let Err(e) = &valid {
                ui.label(RichText::new(e).color(theme::ERROR));
            }
            if apply && changed && valid.is_ok() {
                let filter = draft.trim().to_string();
                *draft = filter.clone();
                app.logs.settings.filter = filter.clone();
                send(LogSettingsEdit::Filter(filter));
            }
        });
        if !app.logs.live && app.logs.env_override.is_none() {
            ui.add_space(4.0);
            ui.label(
                RichText::new("Logging didn't start: a change applies from the next launch.")
                    .color(theme::WARN),
            );
        }
    }

    fn address_labels(&mut self, ui: &mut Ui, app: &mut App, cmd_tx: &CommandSender) {
        card(ui, "Sources", |ui| self.sources(ui, app, cmd_tx));
        ui.add_space(CARD_GAP);
        card_with_header(
            ui,
            "Known Labels",
            app,
            |ui, app| {
                ui.label(
                    RichText::new(format!(
                        "{} labels, {} manual",
                        format_number(app.labels.len() as u64),
                        format_number(app.labels.user_labels().len() as u64)
                    ))
                    .weak(),
                );
            },
            |ui, app| self.known_labels(ui, app, cmd_tx),
        );
    }

    /// Resyncing the index, then what a reset deletes and the button that asks to
    /// confirm it.
    fn reset(&mut self, ui: &mut Ui, app: &App, cmd_tx: &CommandSender) {
        if let Some(report) = &self.reset_report {
            card(ui, "Last Reset", |ui| {
                ui.label(RichText::new(report).color(theme::ERROR));
            });
            ui.add_space(CARD_GAP);
        }
        card(ui, "Resync DAG Index", |ui| resync(ui, app, cmd_tx));
        ui.add_space(CARD_GAP);
        card(ui, "Reset App", |ui| {
            ui.label(format!(
                "Reset app to initial state. Deletes ALL saved application data in {}. \
                 Including (but not limited to):",
                data_dir().display()
            ));
            ui.add_space(4.0);
            for item in [
                "the connection choice and the window's layout and preferences",
                "watchlists, their alert rules and the alert history",
                "your address labels, the cached public list and online lookups",
                "saved queries and their alerts",
                "the index settings and the whole index (it rebuilds from the node)",
            ] {
                ui.label(format!("  • {item}"));
            }
            ui.add_space(4.0);
            ui.label(RichText::new("Exported files and logs are kept.").weak());
            ui.add_space(8.0);
            if ui
                .add(danger_button("Reset and restart"))
                .on_hover_text(
                    "Asks to confirm, then shuts down, deletes the app's data and relaunches",
                )
                .clicked()
            {
                self.confirm_reset = true;
            }
        });
    }

    /// The window confirming a reset, while `confirm_reset`. Nothing in it answers
    /// Enter: a reset takes a click on its button. `close`: the frame loop's Esc.
    fn reset_dialog(&mut self, ctx: &egui::Context, close: bool) {
        if !self.confirm_reset {
            return;
        }
        let window = egui::Window::new("Reset x4kas?")
            .id(egui::Id::new("reset_dialog"))
            .resizable(false)
            .default_width(380.0);
        let mut done = false;
        let open = modal_window(ctx, window, close, |ui| {
            ui.label(
                "All settings, watchlists, labels, saved queries and the index are deleted \
                 and the app restarts. Exported files and logs are kept.",
            );
            ui.add_space(4.0);
            ui.label(RichText::new("This can't be undone.").color(theme::ERROR));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.add(danger_button("Reset and restart")).clicked() {
                    self.reset_report = None;
                    self.reset_requested = true;
                    ui.ctx().request_repaint();
                    done = true;
                }
                if ui.button("Cancel").clicked() {
                    done = true;
                }
            });
        });
        if done || !open {
            self.confirm_reset = false;
        }
    }

    /// The public list: its count, where it comes from and how its fetch is going.
    fn sources(&mut self, ui: &mut Ui, app: &mut App, cmd_tx: &CommandSender) {
        let book = app.labels.clone();
        kv_grid(ui, "label_sources", |ui| {
            // Right to left: the button sits rightmost, then the fetch frequency, the
            // count and the fetch state.
            kv_with(ui, "api.kaspa.org", |ui| {
                if ui
                    .add_enabled(!app.label_refresh.fetching, egui::Button::new("Refresh"))
                    .on_hover_text("Fetch the public list again now")
                    .clicked()
                {
                    let _ = cmd_tx.send(UiCommand::RefreshLabels);
                }
                ui.label(RichText::new("fetched on launch and hourly").weak());
                separator(ui);
                ui.label(format!(
                    "{} addresses",
                    format_number(book.public_len() as u64)
                ));
                separator(ui);
                if app.label_refresh.fetching {
                    ui.label(RichText::new("fetching…").weak());
                    ui.spinner();
                } else if let Some(err) = &app.label_refresh.last_error {
                    ui.label(RichText::new(format!("fetch failed: {err}")).color(theme::ERROR));
                } else if let Some(at) = book.kaspa_org_refreshed {
                    ui.label(format!(
                        "fetched {} ago",
                        format_duration(at.elapsed().unwrap_or_default())
                    ));
                } else {
                    ui.label(RichText::new("not fetched yet").weak());
                }
            });
        });
    }

    /// The add form, a filter, and every known label with its address and source;
    /// the label edits in place.
    fn known_labels(&mut self, ui: &mut Ui, app: &mut App, cmd_tx: &CommandSender) {
        ui.horizontal(|ui| {
            ui.add(
                TextEdit::singleline(&mut self.new_address)
                    .hint_text("kaspa:… address")
                    .desired_width(ui.available_width() - 330.0),
            );
            let name = ui.add(
                TextEdit::singleline(&mut self.new_name)
                    .hint_text("label")
                    .desired_width(200.0),
            );
            let submitted = name.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let problem = if self.new_address.trim().is_empty() {
                Some("Paste an address".to_string())
            } else {
                match validate_address(&self.new_address, &app.network_name()) {
                    Ok(()) if self.new_name.trim().is_empty() => Some("Enter a label".to_string()),
                    Ok(()) => None,
                    Err(why) => Some(why),
                }
            };
            let valid = problem.is_none();
            if ui
                .add_enabled(valid, primary_button("Add"))
                .on_hover_text("Save this label; it shows over any public one")
                .on_disabled_hover_text(problem.as_deref().unwrap_or_default())
                .clicked()
                || (submitted && valid)
            {
                let _ = cmd_tx.send(UiCommand::SetLabel {
                    address: self.new_address.trim().to_string(),
                    name: Some(self.new_name.trim().to_string()),
                });
                self.new_address.clear();
                self.new_name.clear();
            }
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.add(
                TextEdit::singleline(&mut self.filter)
                    .hint_text("filter by label, address or source")
                    .desired_width(260.0),
            );
            if !self.filter.is_empty()
                && ui
                    .small_button("×")
                    .on_hover_text("Clear the filter")
                    .clicked()
            {
                self.filter.clear();
            }
        });
        ui.add_space(4.0);

        let filter = self.filter.trim().to_lowercase();
        let rows: Vec<LabelRow> = app
            .labels
            .all()
            .into_iter()
            .map(|(address, label)| LabelRow {
                address: address.to_string(),
                name: label.name.clone(),
                source: source_name(label.source),
                manual: label.source == LabelSource::User,
            })
            .filter(|r| {
                filter.is_empty()
                    || r.name.to_lowercase().contains(&filter)
                    || r.address.to_lowercase().contains(&filter)
                    || r.source.to_lowercase().contains(&filter)
            })
            .collect();
        if rows.is_empty() {
            placeholder(
                ui,
                if filter.is_empty() {
                    "No labels known yet. Refresh the public list, or add one above."
                } else {
                    "No labels match the filter."
                },
            );
        } else {
            labels_table(ui, &rows, &mut self.confirm_delete);
        }
    }
}

struct LabelRow {
    address: String,
    name: String,
    source: &'static str,
    /// The user's own, so it can be deleted.
    manual: bool,
}

/// How a label's source reads in the table: "manual" for the user's own.
/// A dim separator between the items of a row.
fn separator(ui: &mut Ui) {
    ui.label(RichText::new("|").weak());
}

fn source_name(source: LabelSource) -> &'static str {
    match source {
        LabelSource::User => "manual",
        other => other.label(),
    }
}

/// Every known label: the chip (click to edit), the address, the source, and edit and
/// delete buttons.
fn labels_table(ui: &mut Ui, rows: &[LabelRow], confirm_delete: &mut Option<(String, f64)>) {
    ui.push_id("known_labels", |ui| {
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
        // A solid scroll bar beside the rows: the default floating one lies over the
        // last column and takes the pointer from its buttons (and their tooltips).
        ui.style_mut().spacing.scroll = egui::style::ScrollStyle::solid();
        // Room for the clickable cells, as in `wide_table`.
        let row_height =
            ui.spacing().interact_size.y + 2.0 * ui.visuals().widgets.hovered.expansion;
        TableBuilder::new(ui)
            .striped(true)
            .max_scroll_height(TABLE_HEIGHT)
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::auto().at_least(120.0).clip(true))
            .column(Column::remainder().at_least(200.0).clip(true))
            .column(Column::auto().at_least(90.0))
            .column(Column::exact(52.0))
            .header(theme::ROW_HEIGHT + 4.0, |mut h| {
                for title in ["Label", "Address", "Source"] {
                    h.col(|ui| section_title(ui, title));
                }
                h.col(|_| {});
            })
            .body(|body| {
                body.rows(row_height, rows.len(), |mut row| {
                    let r = &rows[row.index()];
                    row.col(|ui| label_cell(ui, &r.address));
                    row.col(|ui| address_bare(ui, &r.address));
                    row.col(|ui| {
                        ui.label(RichText::new(r.source).weak());
                    });
                    row.col(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        if ui
                            .small_button("✏")
                            .on_hover_text("Edit the label (Enter saves, Esc cancels)")
                            .clicked()
                        {
                            edit_label_cell(ui.ctx(), &r.address);
                        }
                        // Deleting takes two clicks: the first turns the button into
                        // a red "Delete?" for a moment.
                        let now = ui.input(|i| i.time);
                        let armed = confirm_delete
                            .as_ref()
                            .is_some_and(|(a, at)| *a == r.address && now - at < CONFIRM_SECS);
                        let button = if armed {
                            egui::Button::new(RichText::new("Delete?").color(theme::ERROR)).small()
                        } else {
                            egui::Button::new("🗑").small()
                        };
                        if ui
                            .add_enabled(r.manual, button)
                            .on_hover_text(if armed {
                                "Click again to delete this label"
                            } else {
                                "Delete this label"
                            })
                            .on_disabled_hover_text(
                                "Public labels come from api.kaspa.org; edit one to override it",
                            )
                            .clicked()
                        {
                            if armed {
                                request_label(ui.ctx(), &r.address, None);
                                *confirm_delete = None;
                            } else {
                                *confirm_delete = Some((r.address.clone(), now));
                                ui.ctx()
                                    .request_repaint_after(std::time::Duration::from_secs_f64(
                                        CONFIRM_SECS,
                                    ));
                            }
                        }
                    });
                });
            });
    });
}

/// Rebuild the index from the node, as the Analyzer chip's Resync button does
/// (`UiCommand::Resync`).
fn resync(ui: &mut Ui, app: &App, cmd_tx: &CommandSender) {
    ui.label(
        "Deletes the local DAG index and rebuilds from scratch. While index is rebuilding, \
         many app services are unavailable or show partial data. Rebuild progress can be \
         tracked in lower left corner.",
    );
    ui.add_space(8.0);
    resync_button(ui, app, cmd_tx);
}

/// The button that sends `UiCommand::Resync`, disabled (with the reason) without a
/// direct node or while the index opens.
fn resync_button(ui: &mut Ui, app: &App, cmd_tx: &CommandSender) {
    let direct = app.connection.is_direct();
    let opening = matches!(app.chain.phase, ChainPhase::Opening);
    let button = ui
        .add_enabled(direct && !opening, primary_button("Resync DAG index"))
        .on_hover_text("Delete the index and the analytics and rebuild them from the node")
        .on_disabled_hover_text(if !direct {
            "Needs a direct node connection: the index isn't built from the public resolver"
        } else {
            "The index is opening"
        });
    if button.clicked() {
        let _ = cmd_tx.send(UiCommand::Resync);
    }
}

/// The opt-in data the address index keeps, each a switch with what it costs, and how
/// long it keeps data. A change applies from the writer's next batch.
fn index(
    ui: &mut Ui,
    app: &mut App,
    cmd_tx: &CommandSender,
    confirm_retention: &mut Option<Retention>,
) {
    // One for the section: a save that failed, or a hand edit the file fell back from.
    if let Some(err) = &app.index_settings_error {
        ui.label(RichText::new(err).color(theme::ERROR));
        ui.add_space(CARD_GAP);
    }
    card(ui, "Optional Data", |ui| {
        let mut settings = app.index_settings;
        for feature in IndexFeature::ALL {
            let mut on = settings.enabled(feature);
            if ui.checkbox(&mut on, feature.label()).changed() {
                settings.set(feature, on);
                // Shown at once; the controller saves it and the writer picks it up.
                app.index_settings = settings;
                let _ = cmd_tx.send(UiCommand::IndexSettingsEdit(IndexSettingsEdit::Feature(
                    feature, on,
                )));
            }
            ui.indent(("index_feature_doc", feature.label()), |ui| {
                ui.label(RichText::new(feature.doc()).weak());
            });
            ui.add_space(6.0);
        }
        ui.label(
            RichText::new(
                "A change applies to transactions indexed from that moment on. To index the whole \
                 window, Resync DAG index under Reset (or Resync on the Analyzer chip).",
            )
            .weak(),
        );
    });
    ui.add_space(CARD_GAP);
    card(ui, "Retention", |ui| {
        retention(ui, app, cmd_tx, confirm_retention)
    });
    ui.add_space(CARD_GAP);
    card(ui, "Slab Size", |ui| slab_size(ui, app, cmd_tx));
}

/// How long the index keeps data: the node's pruning point (the default) or a fixed
/// number of days. A setting that deletes indexed data asks first, in place.
fn retention(ui: &mut Ui, app: &mut App, cmd_tx: &CommandSender, confirm: &mut Option<Retention>) {
    let text = |r: Retention| {
        if r == Retention::default() {
            format!("{} (default)", r.label())
        } else {
            r.label()
        }
    };
    let current = app.index_settings.retention;
    let mut choices: Vec<Retention> = std::iter::once(Retention::Node)
        .chain(RETENTION_DAYS_CHOICES.map(Retention::Days))
        .collect();
    // A period set from the CLI that Settings doesn't offer.
    if !choices.contains(&current) {
        choices.push(current);
        choices.sort_by_key(|r| r.duration_ms());
    }
    let mut picked = confirm.unwrap_or(current);
    ui.horizontal(|ui| {
        ui.label(RichText::new("Keep data for:").color(theme::LABEL));
        egui::ComboBox::from_id_salt("index_retention")
            .selected_text(text(picked))
            .show_ui(ui, |ui| {
                for r in &choices {
                    ui.selectable_value(&mut picked, *r, text(*r));
                }
            });
    });
    let now = now_ms();
    let pruning_point = app.node.pruning_point_timestamp_ms;
    let floor = |r: Retention| r.floor_ms(now, pruning_point);
    // Pruning drops whole slabs: a floor deletes what lies before the start of its slab.
    let slab_ms = app.chain.slab_hours.map(|h| u64::from(h) * 3_600_000);
    let cut = |floor: u64| slab_ms.map_or(floor, |w| floor - floor % w);
    let oldest = app.chain.oldest_ms;
    if picked == current {
        *confirm = None;
    } else if Some(picked) != *confirm {
        // Ask first when it would delete what the index holds now.
        let deletes = match (floor(current), floor(picked)) {
            // The node's, before its pruning point is known: it may keep less, unless
            // there is nothing to delete.
            (_, None) => oldest.is_some(),
            (Some(old), Some(new)) if new <= old => false,
            (_, Some(new)) => oldest.is_none_or(|oldest| oldest < cut(new)),
        };
        if deletes {
            *confirm = Some(picked);
        } else {
            set_retention(app, cmd_tx, picked);
            *confirm = None;
        }
    }
    if let Some(pending) = *confirm {
        ui.add_space(4.0);
        let what = match floor(pending) {
            Some(at) => format!("Deletes indexed data from before {}.", format_utc(cut(at))),
            None => "May delete indexed data: the node's pruning point isn't known yet, and \
                     data older than it is deleted once it is."
                .to_string(),
        };
        ui.label(
            RichText::new(format!(
                "{what} Data deleted can't be indexed again once the node has pruned it."
            ))
            .color(theme::WARN),
        );
        ui.horizontal(|ui| {
            if ui.add(danger_button("Delete older data")).clicked() {
                set_retention(app, cmd_tx, pending);
                *confirm = None;
            }
            if ui.button("Cancel").clicked() {
                *confirm = None;
            }
        });
    }
    ui.indent("index_retention_doc", |ui| {
        ui.label(
            RichText::new(
                "The default setting \"Mirror Kaspa node\" drops data once the node prunes \
                 past it (about 30 hours on mainnet). Fewer days saves disk space, more keeps \
                 data the node has pruned.",
            )
            .weak(),
        );
    });
    ui.add_space(6.0);
    if let Some(estimate) = disk_estimate(app, app.index_settings.retention, now) {
        ui.label(RichText::new(estimate).weak());
    }
}

/// Save `retention`: the writer prunes by it from its next batch.
fn set_retention(app: &mut App, cmd_tx: &CommandSender, retention: Retention) {
    app.index_settings.retention = retention;
    let _ = cmd_tx.send(UiCommand::IndexSettingsEdit(IndexSettingsEdit::Retention(
        retention,
    )));
}

/// The index's disk use, and for a fixed period what it would come to at the rate it
/// has grown at over the time it covers.
fn disk_estimate(app: &App, retention: Retention, now: u64) -> Option<String> {
    let (from, to) = app.chain.coverage?;
    let bytes = app.chain.disk_bytes;
    let mb = |b: f64| format_number((b / (1024.0 * 1024.0)) as u64);
    let used = format!("The index uses {} MB on disk.", mb(bytes as f64));
    // The first slab may hold less than its width: measure from the oldest block.
    let from = app.chain.oldest_ms.map_or(from, |oldest| oldest.max(from));
    let covered = to.min(now).saturating_sub(from);
    match retention.duration_ms() {
        // Under an hour of data says little about the rate.
        Some(period) if covered >= 3_600_000 => Some(format!(
            "{used} About {} MB for {} at the rate so far.",
            mb(bytes as f64 * period as f64 / covered as f64),
            retention.label()
        )),
        _ => Some(used),
    }
}

/// The width of the index's time slabs: picked here, applied when the index is next
/// built (a Resync), since the index keeps the width it was created with.
fn slab_size(ui: &mut Ui, app: &mut App, cmd_tx: &CommandSender) {
    let hours_text = |h: u32| {
        let unit = if h == 1 { "hour" } else { "hours" };
        if h == DEFAULT_SLAB_HOURS {
            format!("{h} {unit} (default)")
        } else {
            format!("{h} {unit}")
        }
    };
    let mut settings = app.index_settings;
    ui.horizontal(|ui| {
        ui.label(RichText::new("Slab size:").color(theme::LABEL));
        egui::ComboBox::from_id_salt("index_slab_hours")
            .selected_text(hours_text(settings.slab_hours))
            .show_ui(ui, |ui| {
                for h in SLAB_HOURS_CHOICES {
                    ui.selectable_value(&mut settings.slab_hours, h, hours_text(h));
                }
            });
    });
    if settings != app.index_settings {
        app.index_settings = settings;
        let _ = cmd_tx.send(UiCommand::IndexSettingsEdit(IndexSettingsEdit::SlabHours(
            settings.slab_hours,
        )));
    }
    ui.indent("index_slab_doc", |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.label(
                RichText::new(
                    "A slab is one time window of the index: its own set of keyspaces (the \
                     separate key-value trees of fjall, the embedded database the index is \
                     stored in), so a whole window is deleted at once.",
                )
                .weak(),
            );
            ui.hyperlink_to("fjall docs ↗", FJALL_DOCS)
                .on_hover_text(FJALL_DOCS);
        });
        ui.add_space(4.0);
        ui.label(
            RichText::new(
                "The index splits its data into time slabs and drops whole slabs once they \
                 fall out of the retention period. Smaller slabs keep less data past it but \
                 add keyspaces, and a lookup by transaction id alone reads one per slab. \
                 Larger slabs keep up to one slab of extra data on disk.",
            )
            .weak(),
        );
    });
    ui.add_space(6.0);
    match app.chain.slab_hours {
        Some(in_use) if in_use != settings.slab_hours => {
            ui.label(
                RichText::new(format!(
                    "The index in use has {in_use}-hour slabs. Resync DAG index to rebuild it \
                     with {}-hour ones.",
                    settings.slab_hours
                ))
                .color(theme::WARN),
            );
            ui.add_space(6.0);
            resync_button(ui, app, cmd_tx);
        }
        Some(in_use) => {
            ui.label(RichText::new(format!("The index in use has {in_use}-hour slabs.")).weak());
        }
        None => {
            ui.label(
                RichText::new(
                    "Fixed when the index is built: a change applies from the next Resync.",
                )
                .weak(),
            );
        }
    }
}

fn level_text(level: LogLevel) -> String {
    let name = match level {
        LogLevel::Error => "Error",
        LogLevel::Warn => "Warn",
        LogLevel::Info => "Info",
        LogLevel::Debug => "Debug",
        LogLevel::Trace => "Trace",
    };
    if level == LogLevel::default() {
        format!("{name} (default)")
    } else {
        name.to_string()
    }
}

fn level_doc(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Error => "Only failures.",
        LogLevel::Warn => {
            "Failures, and problems worth a look: a lost connection, a gap in the index."
        }
        LogLevel::Info => {
            "Also what the app does: connections, the index's progress, alerts, exports."
        }
        LogLevel::Debug => {
            "Also every batch, query, RPC call and UI command, with the addresses and \
             transactions they name."
        }
        LogLevel::Trace => "Also every node poll. Very verbose.",
    }
}

/// The file settings the log was opened with: a change to them applies from the next
/// launch, which the Log Files card says while they differ.
#[derive(Clone, PartialEq, Eq)]
struct LogFilesAtLaunch {
    file: bool,
    format: LogFormat,
    keep_days: u32,
}

impl LogFilesAtLaunch {
    fn of(settings: &x4kas_core::logging::LogSettings) -> Self {
        Self {
            file: settings.file,
            format: settings.format,
            keep_days: settings.keep_days,
        }
    }
}

/// The files the GUI writes: on or off, their format and how long they are kept (from
/// the next launch), where they are and their size.
fn log_files(ui: &mut Ui, app: &mut App, cmd_tx: &CommandSender, launch: &LogFilesAtLaunch) {
    let mut settings = app.logs.settings.clone();
    ui.checkbox(&mut settings.file, "Write log files");
    ui.add_enabled_ui(settings.file, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Format:").color(theme::LABEL));
            egui::ComboBox::from_id_salt("log_format")
                .selected_text(format_text(settings.format))
                .show_ui(ui, |ui| {
                    for f in LogFormat::ALL {
                        ui.selectable_value(&mut settings.format, f, format_text(f));
                    }
                });
            ui.add_space(12.0);
            ui.label(RichText::new("Keep:").color(theme::LABEL));
            let days_text = |d: u32| {
                let unit = if d == 1 { "day" } else { "days" };
                if d == DEFAULT_KEEP_DAYS {
                    format!("{d} {unit} (default)")
                } else {
                    format!("{d} {unit}")
                }
            };
            egui::ComboBox::from_id_salt("log_keep_days")
                .selected_text(days_text(settings.keep_days))
                .show_ui(ui, |ui| {
                    let current = settings.keep_days;
                    // A period set with the CLI that isn't offered stays pickable.
                    let extra = (!KEEP_DAYS_CHOICES.contains(&current)).then_some(current);
                    for d in KEEP_DAYS_CHOICES.into_iter().chain(extra) {
                        ui.selectable_value(&mut settings.keep_days, d, days_text(d));
                    }
                });
        });
    });
    let saved = &app.logs.settings;
    let edits = [
        (settings.file != saved.file).then_some(LogSettingsEdit::File(settings.file)),
        (settings.format != saved.format).then_some(LogSettingsEdit::Format(settings.format)),
        (settings.keep_days != saved.keep_days)
            .then_some(LogSettingsEdit::KeepDays(settings.keep_days)),
    ];
    for edit in edits.into_iter().flatten() {
        let _ = cmd_tx.send(UiCommand::LogSettingsEdit(edit));
    }
    if LogFilesAtLaunch::of(&settings) != *launch {
        ui.label(RichText::new("Restart app to apply").color(theme::WARN));
    }
    app.logs.settings = settings;
    ui.add_space(6.0);

    let dir = logs_dir();
    kv_grid(ui, "log_files", |ui| {
        kv_with(ui, "Folder", |ui| {
            if ui
                .button("Open folder")
                .on_hover_text("Show the folder in the file manager")
                .clicked()
                && let Err(e) = open_folder(&dir)
            {
                tracing::warn!("couldn't open {}: {e}", dir.display());
            }
            copy_value(ui, &dir.display().to_string(), "Copy the path");
        });
        kv_with(ui, "On disk", |ui| {
            if ui
                .small_button("⟳")
                .on_hover_text("Measure again")
                .clicked()
            {
                let _ = cmd_tx.send(UiCommand::LogFilesRefresh);
            }
            ui.label(or_dash(app.logs.files.as_ref(), |files| {
                let unit = if files.len() == 1 { "file" } else { "files" };
                let bytes = files.iter().map(|f| f.bytes).sum();
                format!("{} {unit}, {}", files.len(), format_size(bytes))
            }));
        });
    });
}

fn format_text(format: LogFormat) -> &'static str {
    match format {
        LogFormat::Text => "Text (default)",
        LogFormat::Json => "JSON lines",
    }
}
