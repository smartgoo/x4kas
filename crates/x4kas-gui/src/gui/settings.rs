//! Settings page (`SettingsPage`, opened from the ⚙ button in the top bar): a left
//! navigation of sections with the chosen one's content on the right, in place of the
//! active tab. Address Labels: the public list refresh, the opt-in online sources, and
//! the user's own labels (add, edit in place, remove, the whole saved list).

use eframe::egui::{self, RichText, TextEdit, Ui};
use egui_extras::{Column, TableBuilder};

use super::monitoring::looks_like_address;
use super::theme;
use super::widgets::{
    CARD_GAP, address, card, card_with_header, copy_value, placeholder, primary_button,
    request_label, section_title,
};
use x4kas_core::app::App;
use x4kas_core::controller::{CommandSender, UiCommand};
use x4kas_core::format::{format_duration, format_number};
use x4kas_core::labels::UserLabels;

/// Width of the section navigation.
const NAV_WIDTH: f32 = 170.0;
/// Rows of the user's labels shown before the table scrolls.
const TABLE_HEIGHT: f32 = 420.0;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Section {
    #[default]
    AddressLabels,
}

impl Section {
    const ALL: [Self; 1] = [Self::AddressLabels];

    fn label(self) -> &'static str {
        match self {
            Section::AddressLabels => "Address Labels",
        }
    }
}

/// The page's own state: whether it is on show, the chosen section and its forms.
#[derive(Default)]
pub struct SettingsPage {
    pub open: bool,
    section: Section,
    /// The kas.fyi API key field, filled from the settings on first show.
    key_input: Option<String>,
    /// The "add a label" form.
    new_address: String,
    new_name: String,
    /// Narrows the user's labels table.
    filter: String,
}

impl SettingsPage {
    pub fn toggle(&mut self) {
        self.open = !self.open;
    }

    pub fn show(&mut self, ui: &mut Ui, app: &mut App, cmd_tx: &CommandSender) {
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
                ui.add_space(8.0);
                ui.label(
                    RichText::new("Esc or a tab returns to the app.")
                        .weak()
                        .small(),
                );
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
                });
            });
    }

    fn address_labels(&mut self, ui: &mut Ui, app: &mut App, cmd_tx: &CommandSender) {
        card(ui, "Sources", |ui| self.sources(ui, app, cmd_tx));
        ui.add_space(CARD_GAP);
        card_with_header(
            ui,
            "Your Labels",
            app,
            |ui, app| {
                ui.label(
                    RichText::new(format!(
                        "{} saved",
                        format_number(app.labels.user_labels().len() as u64)
                    ))
                    .weak(),
                );
            },
            |ui, app| self.user_labels(ui, app, cmd_tx),
        );
    }

    /// The public list is always on; per-address online lookups are opt-in because they
    /// reveal which addresses the user looks at.
    fn sources(&mut self, ui: &mut Ui, app: &mut App, cmd_tx: &CommandSender) {
        let book = app.labels.clone();
        ui.label(
            RichText::new(format!(
                "{} labels known: your own, the public api.kaspa.org list (fetched in bulk, refreshed daily) and the bundled snapshot.",
                format_number(book.len() as u64)
            ))
            .weak(),
        );
        ui.horizontal(|ui| {
            if ui.button("Refresh public list").clicked() {
                let _ = cmd_tx.send(UiCommand::RefreshLabels);
            }
            if let Some(at) = book.kaspa_org_refreshed {
                ui.label(
                    RichText::new(format!(
                        "fetched {} ago",
                        format_duration(at.elapsed().unwrap_or_default())
                    ))
                    .weak(),
                );
            }
        });
        ui.add_space(4.0);
        ui.label(
            RichText::new(
                "Per-address lookups below send the address you open to that service. Off until you enable them; run from \"Look up\" in Address Info.",
            )
            .weak()
            .small(),
        );
        if self.key_input.is_none() {
            self.key_input = Some(
                app.label_settings
                    .kas_fyi_api_key
                    .clone()
                    .unwrap_or_default(),
            );
        }
        let mut changed = false;
        let mut settings = app.label_settings.clone();
        ui.horizontal(|ui| {
            ui.label(RichText::new("kas.fyi API key:").color(theme::LABEL));
            let key = self.key_input.get_or_insert_with(String::new);
            let response = ui.add(
                TextEdit::singleline(key)
                    .password(true)
                    .hint_text("from developer.kas.fyi")
                    .desired_width(260.0),
            );
            if response.lost_focus() {
                settings.kas_fyi_api_key = Some(key.trim().to_string()).filter(|k| !k.is_empty());
                changed = true;
            }
            changed |= ui
                .checkbox(&mut settings.kns, "Resolve .kas names (KNS)")
                .changed();
        });
        if changed {
            let _ = cmd_tx.send(UiCommand::SetLabelSettings(settings));
        }
    }

    /// The add form, a filter, and every saved label: the chip edits in place, ✕ removes.
    fn user_labels(&mut self, ui: &mut Ui, app: &mut App, cmd_tx: &CommandSender) {
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
            let valid = looks_like_address(&self.new_address) && !self.new_name.trim().is_empty();
            if ui
                .add_enabled(valid, primary_button("Add"))
                .on_hover_text("Save this label; it shows over any public one")
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
                    .hint_text("filter by label or address")
                    .desired_width(260.0),
            );
            ui.label(
                RichText::new("Click a label to edit it; Enter saves, Esc cancels.")
                    .weak()
                    .small(),
            );
        });
        ui.add_space(4.0);

        let filter = self.filter.trim().to_lowercase();
        let mut rows: Vec<(String, String)> = app
            .labels
            .user_labels()
            .iter()
            .filter(|(address, name)| {
                filter.is_empty()
                    || name.to_lowercase().contains(&filter)
                    || address.to_lowercase().contains(&filter)
            })
            .map(|(a, n)| (a.clone(), n.clone()))
            .collect();
        rows.sort_by(|a, b| {
            a.1.to_lowercase()
                .cmp(&b.1.to_lowercase())
                .then(a.0.cmp(&b.0))
        });
        if rows.is_empty() {
            placeholder(
                ui,
                if filter.is_empty() {
                    "No labels of your own yet. Add one above, or right-click any address."
                } else {
                    "No labels match the filter."
                },
            );
        } else {
            labels_table(ui, &rows);
        }
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Saved in").weak().small());
            copy_value(ui, &UserLabels::path().display().to_string(), "Copy path");
        });
    }
}

/// The user's labels: each row the label chip (click to edit) and address, and a remove
/// button.
fn labels_table(ui: &mut Ui, rows: &[(String, String)]) {
    ui.push_id("user_labels", |ui| {
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
        // Room for the clickable address cell, as in `wide_table`.
        let row_height =
            ui.spacing().interact_size.y + 2.0 * ui.visuals().widgets.hovered.expansion;
        TableBuilder::new(ui)
            .striped(true)
            .max_scroll_height(TABLE_HEIGHT)
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::remainder().clip(true))
            .column(Column::exact(36.0))
            .header(theme::ROW_HEIGHT + 4.0, |mut h| {
                h.col(|ui| section_title(ui, "Label / Address"));
                h.col(|_| {});
            })
            .body(|body| {
                body.rows(row_height, rows.len(), |mut row| {
                    let (addr, name) = &rows[row.index()];
                    row.col(|ui| address(ui, addr));
                    row.col(|ui| {
                        if ui
                            .small_button("✕")
                            .on_hover_text(format!("Remove the label \"{name}\""))
                            .clicked()
                        {
                            request_label(ui.ctx(), addr, None);
                        }
                    });
                });
            });
    });
}
