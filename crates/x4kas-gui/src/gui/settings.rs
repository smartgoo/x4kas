//! Settings page (`SettingsPage`, opened from the ⚙ button in the top bar): a left
//! navigation of sections with the chosen one's content on the right, in place of the
//! active tab. Address Labels: the public list (its sources and refresh) and the user's
//! every known label (label, address, source; add, edit in place, remove). Index: the
//! opt-in data the address index keeps (full payloads, redeem scripts). Reset: wipe
//! everything the app keeps under `~/.x4kas` (but exports) and relaunch it; the GUI
//! closes through its usual shutdown first (`SettingsPage::reset_requested`), and
//! `main` deletes and relaunches once the window and runtime are gone.

use eframe::egui::{self, RichText, TextEdit, Ui};
use egui_extras::{Column, TableBuilder};

use super::theme;
use super::widgets::{
    CARD_GAP, address_bare, card, card_with_header, danger_button, edit_label_cell, kv_grid,
    kv_with, label_cell, modal_window, placeholder, primary_button, request_label, section_title,
};
use x4kas_core::app::{App, ChainPhase};
use x4kas_core::config::{IndexFeature, data_dir};
use x4kas_core::controller::{CommandSender, UiCommand};
use x4kas_core::format::{format_duration, format_number};
use x4kas_core::labels::LabelSource;
use x4kas_core::watch::validate_address;

/// Width of the section navigation.
const NAV_WIDTH: f32 = 170.0;
/// Rows of labels shown before the table scrolls.
const TABLE_HEIGHT: f32 = 420.0;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Section {
    #[default]
    AddressLabels,
    Index,
    Reset,
}

impl Section {
    const ALL: [Self; 3] = [Self::AddressLabels, Self::Index, Self::Reset];

    fn label(self) -> &'static str {
        match self {
            Section::AddressLabels => "Address Labels",
            Section::Index => "DAG Index",
            Section::Reset => "Reset",
        }
    }
}

/// The page's own state: whether it is on show, the chosen section and its forms.
#[derive(Default)]
pub struct SettingsPage {
    pub open: bool,
    section: Section,
    /// The "add a label" form.
    new_address: String,
    new_name: String,
    /// Narrows the user's labels table.
    filter: String,
    /// The address whose 🗑 was clicked once, and when: a second click within
    /// `CONFIRM_SECS` deletes.
    confirm_delete: Option<(String, f64)>,
    /// The window confirming a reset is open.
    confirm_reset: bool,
    /// What went wrong with the reset this launch followed (`--reset-report`).
    reset_report: Option<String>,
    /// The user confirmed a reset; the frame loop takes it and closes the app.
    pub reset_requested: bool,
}

/// How long a first click on 🗑 waits for the confirming second one.
const CONFIRM_SECS: f64 = 3.0;

impl SettingsPage {
    /// Open on the Reset section with what went wrong with the reset that relaunched
    /// the app.
    pub fn show_reset_report(&mut self, report: String) {
        self.open = true;
        self.section = Section::Reset;
        self.reset_report = Some(report);
    }

    /// A window of the page (the reset confirmation) is on show.
    pub fn modal_open(&self) -> bool {
        self.open && self.confirm_reset
    }

    /// Forget an unanswered reset confirmation, so it doesn't come back with the page.
    pub fn cancel_reset(&mut self) {
        self.confirm_reset = false;
    }

    pub fn toggle(&mut self) {
        self.open = !self.open;
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
                    Section::Index => index(ui, app, cmd_tx),
                    Section::Reset => self.reset(ui, app, cmd_tx),
                });
            });
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
                 and the app restarts. Exported files are kept.",
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
                dot(ui);
                ui.label(format!(
                    "{} addresses",
                    format_number(book.public_len() as u64)
                ));
                dot(ui);
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
fn dot(ui: &mut Ui) {
    ui.label(RichText::new("·").weak());
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

/// The opt-in data the address index keeps, each a switch with what it costs. A change
/// applies from the writer's next batch.
fn index(ui: &mut Ui, app: &mut App, cmd_tx: &CommandSender) {
    card(ui, "Optional Data", |ui| {
        let mut settings = app.index_settings;
        for feature in IndexFeature::ALL {
            let mut on = settings.enabled(feature);
            if ui.checkbox(&mut on, feature.label()).changed() {
                settings.set(feature, on);
                // Shown at once; the controller saves it and the writer picks it up.
                app.index_settings = settings;
                let _ = cmd_tx.send(UiCommand::IndexSettingsSet(settings));
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
        if let Some(err) = &app.index_settings_error {
            ui.label(RichText::new(err).color(theme::ERROR));
        }
    });
}
