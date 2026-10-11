//! The address dialogs opened from the action bar (`gui/actions.rs`): the user's label
//! and an address's settings on a watchlist, each a small modal window with explicit
//! Save/Cancel buttons, and the name dialog that creates or renames a watchlist. One of
//! each at most, owned by `GuiApp`; a page asks for one with [`request_label`] /
//! [`request_watch`] / [`request_list_name`], and the frame loop opens it through
//! [`Dialogs::take_requests`].

use eframe::egui::text::{CCursor, CCursorRange};
use eframe::egui::{self, RichText, TextEdit, Ui};

use super::theme;
use super::widgets::{address, field_label, kv_grid, modal_window, primary_button};
use x4kas_core::app::App;
use x4kas_core::controller::{CommandSender, UiCommand};
use x4kas_core::format::{format_kas, shorten_middle};
use x4kas_core::labels::LabelSource;
use x4kas_core::watch::{AlertRules, ListId, WatchEdit, WatchEntry, Watchlists};

/// Focus `field` with its text selected, so typing replaces a name already there.
fn focus_all(ui: &Ui, field: &egui::Response, text: &str) {
    field.request_focus();
    let mut state = TextEdit::load_state(ui.ctx(), field.id).unwrap_or_default();
    state.cursor.set_char_range(Some(CCursorRange::two(
        CCursor::new(0),
        CCursor::new(text.chars().count()),
    )));
    TextEdit::store_state(ui.ctx(), field.id, state);
}

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

fn list_name_request_id() -> egui::Id {
    egui::Id::new("list_name_dialog_request")
}

fn created_list_id() -> egui::Id {
    egui::Id::new("watchlist_created")
}

/// Ask for the watchlist dialog of `addr`: its settings on `list`, or with `None`, to
/// add it to a list it isn't on yet.
pub fn request_watch(ctx: &egui::Context, addr: &str, list: Option<ListId>) {
    ctx.data_mut(|d| d.insert_temp(watch_request_id(), (addr.to_string(), list)));
}

/// Ask for the name dialog: a new watchlist, or with `Some`, renaming that one.
pub fn request_list_name(ctx: &egui::Context, list: Option<ListId>) {
    ctx.data_mut(|d| d.insert_temp(list_name_request_id(), list));
}

/// How long a created list is waited for: the controller applies the edit in moments,
/// and one that failed (another process took the name) never arrives.
const CREATED_WAIT_SECS: f64 = 5.0;

/// A list the name dialog asked the controller to create: its name, the first id it
/// can have (ids are never reused) and when it was asked for.
#[derive(Clone, Default)]
struct PendingList {
    name: String,
    min_id: ListId,
    asked_at: f64,
}

/// The id of the list the name dialog created, once it is in `lists` (the controller
/// applies the edit after the dialog closes); taken once, and forgotten when it hasn't
/// arrived within `CREATED_WAIT_SECS`.
pub fn take_created_list(ctx: &egui::Context, lists: &Watchlists) -> Option<ListId> {
    let pending = ctx.data(|d| d.get_temp::<PendingList>(created_list_id()))?;
    let found = lists
        .by_name(&pending.name)
        .filter(|l| l.id >= pending.min_id)
        .map(|l| l.id);
    let expired = ctx.input(|i| i.time) - pending.asked_at > CREATED_WAIT_SECS;
    if found.is_some() || expired {
        ctx.data_mut(|d| d.remove_temp::<PendingList>(created_list_id()));
    }
    found
}

#[derive(Default)]
pub struct Dialogs {
    label: Option<LabelDialog>,
    watch: Option<WatchDialog>,
    list_name: Option<ListNameDialog>,
}

impl Dialogs {
    /// A dialog is on show.
    pub fn any_open(&self) -> bool {
        self.label.is_some() || self.watch.is_some() || self.list_name.is_some()
    }

    /// Open the dialogs asked for this frame. `target`: the list the Monitoring tab
    /// shows, which an address is added to unless it is already on it.
    pub fn take_requests(&mut self, ctx: &egui::Context, app: &App, target: Option<ListId>) {
        if let Some(addr) = ctx.data_mut(|d| d.remove_temp::<String>(label_request_id())) {
            self.label = Some(LabelDialog::new(app, &addr));
        }
        if let Some((addr, list)) =
            ctx.data_mut(|d| d.remove_temp::<(String, Option<ListId>)>(watch_request_id()))
        {
            self.watch = WatchDialog::new(app, &addr, list, target);
        }
        if let Some(list) =
            ctx.data_mut(|d| d.remove_temp::<Option<ListId>>(list_name_request_id()))
        {
            self.list_name = Some(ListNameDialog::new(app, list));
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
        if let Some(dialog) = &mut self.list_name
            && !dialog.show(ctx, app, cmd_tx, close)
        {
            self.list_name = None;
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
            .id(egui::Id::new("label_dialog"))
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
                    focus_all(ui, &field, &self.draft);
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
    network: String,
    /// The list whose entry the dialog edits; `None` when it adds the address.
    editing: Option<ListId>,
    /// The list it saves to: on a list being edited, another one moves the entry.
    list: ListId,
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
    /// The dialog for `addr` on `list`, or with `None`, adding it to `target` (else the
    /// first list it isn't on); `None` when it is on every list already.
    fn new(app: &App, addr: &str, list: Option<ListId>, target: Option<ListId>) -> Option<Self> {
        let network = app.network_name();
        let lists = &app.watch.lists;
        let saved = list.and_then(|id| Some((id, lists.get(id)?.entry(addr, &network)?.clone())));
        let (editing, list, entry) = match saved {
            Some((id, entry)) => (Some(id), id, entry),
            None => {
                let free = |id: &ListId| {
                    lists
                        .get(*id)
                        .is_some_and(|l| l.entry(addr, &network).is_none())
                };
                let id = list
                    .filter(free)
                    .or(target.filter(free))
                    .or_else(|| lists.lists.iter().map(|l| l.id).find(free))?;
                (None, id, WatchEntry::new(addr, &network))
            }
        };
        let kas = |v: Option<u64>| v.map(|s| format_kas(s as f64, 8)).unwrap_or_default();
        Some(Self {
            address: addr.to_string(),
            network,
            editing,
            list,
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
        })
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
        let lists = &app.watch.lists;
        // The list went away (deleted elsewhere): nothing left to edit or add to.
        let Some(list_name) = lists.get(self.list).map(|l| l.name.clone()) else {
            return false;
        };
        // The list the entry is on (while editing) names the window, as the address does.
        let editing_name = self
            .editing
            .and_then(|id| lists.get(id))
            .map(|l| l.name.clone());
        let short = shorten_middle(&self.address, 20);
        let title = match &editing_name {
            Some(name) => format!("Watch settings on {name} · {short}"),
            None => format!("Add to watchlist · {short}"),
        };
        let window = egui::Window::new(title)
            .id(egui::Id::new("watch_dialog"))
            .resizable(false)
            .default_width(460.0);
        let mut done = false;
        let open = modal_window(ctx, window, close, |ui| {
            address(ui, &self.address);
            ui.add_space(4.0);
            ui.label(
                RichText::new("Follows the balance and raises an alert when a rule below trips.")
                    .weak(),
            );
            ui.add_space(4.0);
            let mut enter = false;
            kv_grid(ui, "watch_dialog", |ui| {
                field_label(ui, "List");
                let hint = if self.editing.is_some() {
                    "Another list moves the address there, with these rules"
                } else {
                    "The watchlist to add the address to"
                };
                egui::ComboBox::from_id_salt("watch_dialog_list")
                    .selected_text(&list_name)
                    .show_ui(ui, |ui| {
                        for list in &lists.lists {
                            // A list that has the address already (other than the one
                            // edited) can't take it again.
                            let taken = Some(list.id) != self.editing
                                && list.entry(&self.address, &self.network).is_some();
                            ui.add_enabled_ui(!taken, |ui| {
                                ui.selectable_value(&mut self.list, list.id, &list.name)
                            })
                            .response
                            .on_disabled_hover_text("Already on this list");
                        }
                    })
                    .response
                    .on_hover_text(hint);
                ui.end_row();
                field_label(ui, "Alerts");
                ui.checkbox(&mut self.entry.enabled, "on")
                    .on_hover_text("Off: the address is still followed, its rules raise nothing");
                ui.end_row();
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
            let primary = match self.editing {
                Some(from) if from != self.list => format!("Move to {list_name}"),
                Some(_) => "Save".to_string(),
                None => format!("Add to {list_name}"),
            };
            let remove_text = editing_name.as_ref().map(|n| format!("Remove from {n}"));
            let remove = remove_text.as_deref().map(|text| {
                (
                    text,
                    "Stop following this address on this list; its events stay listed",
                )
            });
            let action = buttons(ui, &primary, enter, remove);
            let send = |edit| {
                let _ = cmd_tx.send(UiCommand::WatchEdit(edit));
            };
            match (action, self.editing) {
                (Action::Save, editing) => {
                    self.entry.rules = self.rules();
                    if let Some(from) = editing
                        && from != self.list
                    {
                        send(WatchEdit::Move {
                            from,
                            to: self.list,
                            address: self.address.clone(),
                            network: self.network.clone(),
                        });
                    }
                    send(WatchEdit::Put {
                        list: self.list,
                        entry: self.entry.clone(),
                    });
                    done = true;
                }
                (Action::Remove, Some(list)) => {
                    send(WatchEdit::Remove {
                        list,
                        address: self.address.clone(),
                        network: self.network.clone(),
                    });
                    done = true;
                }
                (Action::Cancel, _) => done = true,
                _ => {}
            }
        });
        open && !done
    }
}

/// Create a watchlist, or rename one: a name no other list has.
struct ListNameDialog {
    /// The list renamed; `None` creates one.
    list: Option<ListId>,
    draft: String,
    focus: bool,
}

impl ListNameDialog {
    fn new(app: &App, list: Option<ListId>) -> Self {
        let name = list
            .and_then(|id| app.watch.lists.get(id))
            .map(|l| l.name.clone())
            .unwrap_or_default();
        Self {
            list,
            draft: name,
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
        let title = if self.list.is_some() {
            "Rename watchlist"
        } else {
            "New watchlist"
        };
        let window = egui::Window::new(title)
            .id(egui::Id::new("list_name_dialog"))
            .resizable(false)
            .default_width(380.0);
        let mut done = false;
        let open = modal_window(ctx, window, close, |ui| {
            let mut enter = false;
            kv_grid(ui, "list_name_dialog", |ui| {
                field_label(ui, "Name");
                let field = ui.add(
                    TextEdit::singleline(&mut self.draft)
                        .hint_text("e.g. Exchanges")
                        .desired_width(240.0),
                );
                if std::mem::take(&mut self.focus) {
                    focus_all(ui, &field, &self.draft);
                }
                enter = entered(ui, &field);
                ui.end_row();
            });
            let problem = app.watch.lists.validate_name(&self.draft, self.list).err();
            if let Some(why) = &problem
                && !self.draft.trim().is_empty()
            {
                ui.label(RichText::new(why).color(theme::ERROR));
            }
            ui.add_space(8.0);
            let primary = if self.list.is_some() {
                "Rename"
            } else {
                "Create"
            };
            let mut save = false;
            ui.horizontal(|ui| {
                save = ui
                    .add_enabled(problem.is_none(), primary_button(primary))
                    .on_disabled_hover_text(problem.as_deref().unwrap_or_default())
                    .clicked()
                    || (enter && problem.is_none());
                if ui.button("Cancel").clicked() {
                    done = true;
                }
            });
            if !save {
                return;
            }
            let name = self.draft.trim().to_string();
            let edit = match self.list {
                Some(id) => WatchEdit::RenameList { id, name },
                None => {
                    let pending = PendingList {
                        name: name.clone(),
                        min_id: app.watch.lists.next_id,
                        asked_at: ui.ctx().input(|i| i.time),
                    };
                    ui.ctx()
                        .data_mut(|d| d.insert_temp(created_list_id(), pending));
                    WatchEdit::CreateList { name }
                }
            };
            let _ = cmd_tx.send(UiCommand::WatchEdit(edit));
            done = true;
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
