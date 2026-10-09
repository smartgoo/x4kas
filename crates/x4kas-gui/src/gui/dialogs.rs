//! The address dialogs opened from the action bar (`gui/actions.rs`): the user's label
//! and the watchlist settings, each a small modal window with explicit Save/Cancel
//! buttons. One of each at most, owned by `GuiApp`; a page asks for one with
//! [`request_label`] / [`request_watch`], and the frame loop opens it through
//! [`Dialogs::take_requests`].

use eframe::egui::{self, RichText, TextEdit, Ui};

use super::monitoring::network;
use super::theme;
use super::widgets::{field_label, kv_grid, modal_window, primary_button};
use x4kas_core::app::App;
use x4kas_core::controller::{CommandSender, UiCommand};
use x4kas_core::format::{format_kas, shorten_middle};
use x4kas_core::labels::LabelSource;
use x4kas_core::watch::{AlertRules, WatchEntry};

fn label_request_id() -> egui::Id {
    egui::Id::new("label_dialog_request")
}

fn watch_request_id() -> egui::Id {
    egui::Id::new("watch_dialog_request")
}

/// Ask for the label dialog of `addr`.
pub fn request_label(ctx: &egui::Context, addr: &str) {
    ctx.data_mut(|d| d.insert_temp(label_request_id(), addr.to_string()));
}

/// Ask for the watchlist dialog of `addr`.
pub fn request_watch(ctx: &egui::Context, addr: &str) {
    ctx.data_mut(|d| d.insert_temp(watch_request_id(), addr.to_string()));
}

#[derive(Default)]
pub struct Dialogs {
    label: Option<LabelDialog>,
    watch: Option<WatchDialog>,
}

impl Dialogs {
    /// A dialog is on show.
    pub fn any_open(&self) -> bool {
        self.label.is_some() || self.watch.is_some()
    }

    /// Open the dialogs asked for this frame.
    pub fn take_requests(&mut self, ctx: &egui::Context, app: &App) {
        if let Some(addr) = ctx.data_mut(|d| d.remove_temp::<String>(label_request_id())) {
            self.label = Some(LabelDialog::new(app, &addr));
        }
        if let Some(addr) = ctx.data_mut(|d| d.remove_temp::<String>(watch_request_id())) {
            self.watch = Some(WatchDialog::new(app, &addr));
        }
    }

    /// `close`: this frame's Esc is for the dialog.
    pub fn show(&mut self, ctx: &egui::Context, app: &App, cmd_tx: &CommandSender, close: bool) {
        if let Some(dialog) = &mut self.label
            && !dialog.show(ctx, app, cmd_tx, close)
        {
            self.label = None;
        }
        if let Some(dialog) = &mut self.watch
            && !dialog.show(ctx, app, cmd_tx, close)
        {
            self.watch = None;
        }
    }
}

/// What a dialog's button row asked for.
#[derive(PartialEq, Eq)]
enum Action {
    None,
    Save,
    Remove,
    Cancel,
}

/// The button row: the primary button (or Enter in a field) saves, `remove` (with its
/// hover text) is offered when there is something to remove, Cancel (also Esc or the
/// window's ×) closes without a change.
fn buttons(ui: &mut Ui, primary: &str, enter: bool, remove: Option<(&str, &str)>) -> Action {
    let mut action = Action::None;
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        if ui.add(primary_button(primary)).clicked() || enter {
            action = Action::Save;
        }
        if ui.button("Cancel").clicked() {
            action = Action::Cancel;
        }
        if let Some((text, hint)) = remove {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(text).on_hover_text(hint).clicked() {
                    action = Action::Remove;
                }
            });
        }
    });
    action
}

/// Enter was pressed in `field`.
fn entered(ui: &Ui, field: &egui::Response) -> bool {
    field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))
}

struct LabelDialog {
    address: String,
    draft: String,
    /// The user's label when opened: the dialog removes it, or edits it.
    had_label: bool,
    focus: bool,
}

impl LabelDialog {
    fn new(app: &App, addr: &str) -> Self {
        let saved = app.labels.user_labels().get(addr).cloned();
        Self {
            address: addr.to_string(),
            had_label: saved.is_some(),
            draft: saved.unwrap_or_default(),
            focus: true,
        }
    }

    fn show(
        &mut self,
        ctx: &egui::Context,
        app: &App,
        cmd_tx: &CommandSender,
        close: bool,
    ) -> bool {
        let title = if self.had_label {
            "Edit label"
        } else {
            "Add label"
        };
        let window = egui::Window::new(title)
            .resizable(false)
            .default_width(420.0);
        let mut done = false;
        let open = modal_window(ctx, window, close, |ui| {
            ui.label(RichText::new(&self.address).color(theme::TEXT_DIM));
            // A public name is known: the user's label shows instead of it.
            if let Some(known) = app
                .labels
                .get(&self.address)
                .filter(|l| l.source != LabelSource::User)
            {
                ui.label(
                    RichText::new(format!(
                        "Known as {} ({}); your label shows instead.",
                        known.name,
                        known.source.label()
                    ))
                    .weak(),
                );
            }
            ui.add_space(6.0);
            let mut enter = false;
            kv_grid(ui, "label_dialog", |ui| {
                field_label(ui, "Label");
                let field = ui.add(
                    TextEdit::singleline(&mut self.draft)
                        .hint_text("your name for this address")
                        .desired_width(300.0),
                );
                if std::mem::take(&mut self.focus) {
                    field.request_focus();
                }
                enter = entered(ui, &field);
                ui.end_row();
            });
            let remove = self.had_label.then_some((
                "Remove",
                "Forget your label (a public one shows again, if any)",
            ));
            let action = buttons(ui, "Save", enter, remove);
            // An empty name removes the label too.
            let name = match action {
                Action::Save => Some(self.draft.trim().to_string()).filter(|d| !d.is_empty()),
                Action::Remove => None,
                Action::None | Action::Cancel => {
                    done = action == Action::Cancel;
                    return;
                }
            };
            let _ = cmd_tx.send(UiCommand::SetLabel {
                address: self.address.clone(),
                name,
            });
            done = true;
        });
        open && !done
    }
}

struct WatchDialog {
    address: String,
    /// On the watchlist when opened: the dialog edits the entry, else adds one.
    watched: bool,
    entry: WatchEntry,
    /// Threshold fields in KAS, as typed.
    received_min: String,
    sent_min: String,
    balance_below: String,
    balance_above: String,
    /// Hours, as typed.
    idle_hours: String,
}

impl WatchDialog {
    fn new(app: &App, addr: &str) -> Self {
        let saved = app.watch.entry(addr).cloned();
        let entry = saved
            .clone()
            .unwrap_or_else(|| WatchEntry::new(addr, &network(app)));
        let kas = |v: Option<u64>| v.map(|s| format_kas(s as f64, 8)).unwrap_or_default();
        Self {
            address: addr.to_string(),
            watched: saved.is_some(),
            received_min: kas(entry.rules.received_min),
            sent_min: kas(entry.rules.sent_min),
            balance_below: kas(entry.rules.balance_below),
            balance_above: kas(entry.rules.balance_above),
            idle_hours: entry
                .rules
                .idle_hours
                .map(|h| h.to_string())
                .unwrap_or_default(),
            entry,
        }
    }

    /// The rules as typed; a field that isn't a number is no threshold.
    fn rules(&self) -> AlertRules {
        AlertRules {
            any_activity: self.entry.rules.any_activity,
            received_min: parse_kas(&self.received_min),
            sent_min: parse_kas(&self.sent_min),
            balance_below: parse_kas(&self.balance_below),
            balance_above: parse_kas(&self.balance_above),
            idle_hours: parse_hours(&self.idle_hours),
        }
    }

    fn show(
        &mut self,
        ctx: &egui::Context,
        app: &App,
        cmd_tx: &CommandSender,
        close: bool,
    ) -> bool {
        let title = if self.watched {
            format!("Watch settings · {}", shorten_middle(&self.address, 20))
        } else {
            format!("Add to watchlist · {}", shorten_middle(&self.address, 20))
        };
        let window = egui::Window::new(title)
            .resizable(false)
            .default_width(460.0);
        let mut done = false;
        let open = modal_window(ctx, window, close, |ui| {
            ui.label(RichText::new(&self.address).color(theme::TEXT_DIM));
            ui.add_space(6.0);
            if self.watched {
                ui.checkbox(&mut self.entry.enabled, "Alerts on");
            } else {
                ui.label(
                    RichText::new(
                        "Follows the balance and raises an alert when a rule below trips.",
                    )
                    .weak(),
                );
            }
            ui.add_space(4.0);
            let mut enter = false;
            kv_grid(ui, "watch_dialog", |ui| {
                field_label(ui, "Any activity");
                ui.checkbox(&mut self.entry.rules.any_activity, "");
                ui.end_row();
                for (label, field, hint, is_hours) in [
                    ("Received ≥", &mut self.received_min, "KAS", false),
                    ("Sent ≥", &mut self.sent_min, "KAS", false),
                    ("Balance <", &mut self.balance_below, "KAS", false),
                    ("Balance >", &mut self.balance_above, "KAS", false),
                    ("Active after ≥", &mut self.idle_hours, "hours idle", true),
                ] {
                    field_label(ui, label);
                    let invalid = !field.trim().is_empty()
                        && if is_hours {
                            parse_hours(field).is_none()
                        } else {
                            parse_kas(field).is_none()
                        };
                    let mut edit = TextEdit::singleline(field)
                        .hint_text(hint)
                        .desired_width(140.0);
                    if invalid {
                        edit = edit.text_color(theme::ERROR);
                    }
                    let response = ui.add(edit);
                    if invalid {
                        response.clone().on_hover_text("Not a number: no threshold");
                    }
                    enter |= entered(ui, &response);
                    ui.end_row();
                }
            });
            let primary = if self.watched {
                "Save"
            } else {
                "Add to watchlist"
            };
            let remove = self.watched.then_some((
                "Remove from watchlist",
                "Stop following this address; its events stay listed",
            ));
            let action = buttons(ui, primary, enter, remove);
            let mut list = app.watch.list.clone();
            match action {
                Action::Save => {
                    self.entry.rules = self.rules();
                    match list.entries.iter_mut().find(|e| e.address == self.address) {
                        Some(entry) => *entry = self.entry.clone(),
                        None => list.entries.push(self.entry.clone()),
                    }
                    let _ = cmd_tx.send(UiCommand::WatchSet(list));
                }
                Action::Remove => {
                    list.entries.retain(|e| e.address != self.address);
                    let _ = cmd_tx.send(UiCommand::WatchSet(list));
                }
                Action::None | Action::Cancel => {}
            }
            done = action != Action::None;
        });
        open && !done
    }
}

/// KAS as typed to sompi; empty or invalid is no threshold.
fn parse_kas(s: &str) -> Option<u64> {
    let v: f64 = s.trim().replace(',', "").parse().ok()?;
    (v > 0.0).then(|| (v * 100_000_000.0).round() as u64)
}

/// Hours as typed; empty, zero or invalid is no threshold.
fn parse_hours(s: &str) -> Option<u32> {
    s.trim().parse().ok().filter(|h| *h > 0)
}
