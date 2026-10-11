//! Monitoring tab: the Watchlist pane (half the width) with one watchlist's live
//! balances, picked (and created, renamed, deleted) in its header's list menu, and the
//! Activity pane with the feed of every list or the one its filter picks, where an event
//! that tripped an alert rule gets an alert dot that a click marks as read. Both fill
//! the tab's height, their tables scrolling inside; below the wrap width they stack and
//! the tab scrolls. A row click opens the address's info pane.

use eframe::egui::containers::menu::{MenuButton, MenuConfig};
use eframe::egui::{self, Button, PopupCloseBehavior, RichText, Sense, Stroke, TextEdit, Ui, vec2};

use super::dialogs;
use super::theme;
use super::widgets::{
    TABLE_HEIGHT, address, card_with_header, columns_wrap, label_search_popup, primary_button,
    request_address, status_chip, weighted_columns, wide_table_with_lead,
};
use x4kas_core::app::{App, WatchPhase};
use x4kas_core::controller::{CommandSender, UiCommand};
use x4kas_core::format::{format_duration, format_kas, shorten_middle};
use x4kas_core::watch::{
    AddressEvent, EventKind, ListId, WatchEdit, WatchEntry, Watchlist, Watchlists, validate_address,
};

const DAY_MS: u64 = 24 * 3_600_000;
/// Watchlist | Activity, half and half.
const PANE_WEIGHTS: [f32; 2] = [1.0, 1.0];
/// Narrowest a pane gets before the panes stack.
const PANE_MIN: f32 = 480.0;

/// How long a notice under the add field stays.
const NOTICE_SECS: f64 = 4.0;
/// How long "Delete list…" stays armed for the confirming click.
const CONFIRM_SECS: f64 = 3.0;

/// The tab's own state: the list on show, the Activity filter, the add/search field.
#[derive(Default)]
pub struct MonitoringTab {
    /// The watchlist the Watchlist pane shows and adds to (remembered in `Prefs`); the
    /// first list when unset or gone.
    pub selected: Option<ListId>,
    /// The list whose addresses' events Activity shows; `None` for every list.
    pub activity_filter: Option<ListId>,
    input: String,
    /// The label matches popup is showing under the field.
    search_open: bool,
    /// Feedback under the field (added, already watched, invalid) and when it was set.
    notice: Option<(String, bool, f64)>,
    /// "Delete list…" was clicked for this list at this time; a second click deletes.
    confirm_delete: Option<(ListId, f64)>,
}

/// The list `selected` names, else the first.
fn current(lists: &Watchlists, selected: Option<ListId>) -> Option<&Watchlist> {
    selected
        .and_then(|id| lists.get(id))
        .or_else(|| lists.lists.first())
}

impl MonitoringTab {
    /// The list the pane shows, which the watch dialog adds to by default.
    pub fn target_list(&self, app: &App) -> Option<ListId> {
        current(&app.watch.lists, self.selected).map(|l| l.id)
    }

    pub fn show(&mut self, ui: &mut Ui, app: &mut App, cmd_tx: &CommandSender) {
        // A list just created in the name dialog: show it once it arrives.
        if let Some(name) = dialogs::take_created_list(ui.ctx(), &app.watch.lists) {
            self.selected = Some(name);
        }
        if self
            .activity_filter
            .is_some_and(|id| app.watch.lists.get(id).is_none())
        {
            self.activity_filter = None;
        }
        if columns_wrap(ui, PANE_WEIGHTS, PANE_MIN) {
            egui::ScrollArea::vertical().show(ui, |ui| self.panes(ui, app, cmd_tx, false));
        } else {
            self.panes(ui, app, cmd_tx, true);
        }
    }

    /// The two panes; `fill` stretches their tables to the tab's height.
    fn panes(&mut self, ui: &mut Ui, app: &mut App, cmd_tx: &CommandSender, fill: bool) {
        weighted_columns(ui, PANE_WEIGHTS, PANE_MIN, |[left, right]| {
            card_with_header(
                left,
                "Watchlists",
                &mut (&mut *app, &mut *self),
                |ui, (app, tab)| {
                    tab.list_menu(ui, app, cmd_tx);
                    watch_status(ui, app);
                },
                |ui, (app, tab)| tab.watchlist(ui, app, cmd_tx, fill),
            );
            let filter = self.activity_filter;
            card_with_header(
                right,
                "Activity",
                &mut (&mut *app, &mut self.activity_filter),
                |ui, (app, chosen)| {
                    filter_menu(ui, app, chosen);
                    let shown = shown_by(app, filter);
                    let unread = app.watch.unread_alerts_where(&shown);
                    if unread > 0
                        && ui
                            .small_button(format!("Mark all read ({unread})"))
                            .clicked()
                    {
                        app.watch.mark_read_where(&shown);
                    }
                },
                |ui, (app, _)| activity(ui, app, filter, fill),
            );
        });
    }

    fn watchlist(&mut self, ui: &mut Ui, app: &mut App, cmd_tx: &CommandSender, fill: bool) {
        let network = app.network_name();
        let Some(list) = current(&app.watch.lists, self.selected).cloned() else {
            return;
        };
        // Add or look up an address, or find a labelled one.
        let mut submitted = false;
        let mut field: Option<egui::Response> = None;
        ui.horizontal(|ui| {
            let edit = TextEdit::singleline(&mut self.input)
                .hint_text("kaspa:… address, or a label such as \"Bybit\"")
                .desired_width(ui.available_width() - 190.0);
            let response = ui.add(edit);
            field = Some(response.clone());
            if response.changed() || response.gained_focus() {
                self.search_open = true;
            }
            submitted = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let is_address = looks_like_address(&self.input);
            let validity = is_address.then(|| validate_address(&self.input, &network));
            let valid = matches!(validity, Some(Ok(())));
            let watched = valid && list.entry(self.input.trim(), &network).is_some();
            let already = format!("Already on {}", list.name);
            // Enter adds (as the hint under the table says), or shows an address that is
            // already watched.
            if ui
                .add_enabled(valid && !watched, primary_button("Watch"))
                .on_hover_text(format!("Add to {} (Enter)", list.name))
                .on_disabled_hover_text(match validity {
                    Some(Err(ref why)) => why.as_str(),
                    _ if watched => already.as_str(),
                    _ => "Paste a Kaspa address",
                })
                .clicked()
                || (submitted && valid && !watched)
            {
                self.add(ui.ctx(), &list, cmd_tx, &network);
            }
            if ui
                .add_enabled(valid, egui::Button::new("Info"))
                .on_hover_text("Show address info")
                .clicked()
                || (submitted && watched)
            {
                request_address(ui.ctx(), self.input.trim());
            }
            if let Some(Err(why)) = validity
                && submitted
            {
                self.notice = Some((why, true, ui.input(|i| i.time)));
            }
        });
        // Label matches, when the field isn't an address; Enter takes the first.
        if let Some(field) = field
            && !looks_like_address(&self.input)
        {
            let picked =
                label_search_popup(ui, &field, &mut self.search_open, &self.input, &app.labels);
            let first = (submitted && !self.input.trim().is_empty())
                .then(|| app.labels.search(&self.input).into_iter().next())
                .flatten()
                .map(|(addr, _)| addr.to_string());
            if let Some(addr) = picked.or(first) {
                request_address(ui.ctx(), &addr);
                self.search_open = false;
            }
        }
        ui.add_space(4.0);

        let entries: Vec<&WatchEntry> = list.on_network(&network).collect();
        let now = x4kas_core::format::now_ms();
        // Half the width fits the essentials; pending and the alert rules are in
        // the info pane.
        let rows = entries
            .iter()
            .map(|e| {
                let balance = app.watch.balances.get(&e.address).copied();
                let last = app
                    .watch
                    .events
                    .iter()
                    .find(|ev| ev.address == e.address)
                    .map(|ev| ev.time_ms)
                    .max(app.watch.lists.last_activity(&e.address, &network))
                    .map(ago);
                // Net of the events of the last 24h: the alert history keeps those of
                // earlier sessions, but nothing is seen while the app is closed.
                let day: i128 = app
                    .watch
                    .events
                    .iter()
                    .filter(|ev| ev.address == e.address && now.saturating_sub(ev.time_ms) < DAY_MS)
                    .map(|ev| match ev.kind {
                        EventKind::Received => i128::from(ev.amount),
                        EventKind::Sent => -i128::from(ev.amount),
                    })
                    .sum();
                [
                    e.address.clone(),
                    balance
                        .map(|b| format_kas(b as f64, 2))
                        .unwrap_or_else(|| "—".into()),
                    signed_kas(day),
                    last.unwrap_or_default(),
                ]
            })
            .collect();
        let height = table_height(ui, fill);
        wide_table_with_lead(
            ui,
            "watchlist",
            ["Address", "Balance (KAS)", "24h change", "Last activity"],
            rows,
            "No addresses on this list yet. Paste one above and press Watch.",
            address,
            Some(request_address),
            |_, _| {},
            height,
        );
        ui.add_space(2.0);
        // The notice (added, already on the list, invalid) takes the hint's line for a
        // few seconds, so nothing above it moves.
        let now = ui.input(|i| i.time);
        if self
            .notice
            .as_ref()
            .is_some_and(|(_, _, since)| now - since > NOTICE_SECS)
        {
            self.notice = None;
        }
        match &self.notice {
            Some((text, is_error, _)) => {
                let color = if *is_error { theme::ERROR } else { theme::OK };
                ui.label(RichText::new(text).color(color).small());
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_secs_f64(NOTICE_SECS));
            }
            None => {
                ui.label(
                    RichText::new(
                        "Click an address for its info, history and alert settings. \
                         24h change counts only what x4kas saw while running.",
                    )
                    .weak()
                    .small(),
                );
            }
        }
    }

    fn add(
        &mut self,
        ctx: &egui::Context,
        list: &Watchlist,
        cmd_tx: &CommandSender,
        network: &str,
    ) {
        let addr = self.input.trim();
        if addr.is_empty() {
            return;
        }
        let now = ctx.input(|i| i.time);
        if list.entry(addr, network).is_some() {
            self.notice = Some((format!("Already on {}", list.name), false, now));
        } else {
            let _ = cmd_tx.send(UiCommand::WatchEdit(WatchEdit::Put {
                list: list.id,
                entry: WatchEntry::new(addr, network),
            }));
            self.notice = Some((
                format!("Added {} to {}", shorten_middle(addr, 20), list.name),
                false,
                now,
            ));
        }
        self.input.clear();
    }

    /// The list menu in the Watchlist card's header: the list on show (a click picks
    /// another), then New, Rename and Delete; the delete asks once more in place.
    fn list_menu(&mut self, ui: &mut Ui, app: &App, cmd_tx: &CommandSender) {
        let lists = &app.watch.lists;
        let Some(shown) = current(lists, self.selected) else {
            return;
        };
        let network = app.network_name();
        let count = |l: &Watchlist| l.on_network(&network).count();
        let now = ui.input(|i| i.time);
        if self
            .confirm_delete
            .is_some_and(|(_, at)| now - at >= CONFIRM_SECS)
        {
            self.confirm_delete = None;
        }
        let button = Button::new(format!("{} ▾", shown.name)).small();
        let config = MenuConfig::new().close_behavior(PopupCloseBehavior::CloseOnClickOutside);
        let (response, _) = MenuButton::from_button(button).config(config).ui(ui, |ui| {
            for list in &lists.lists {
                let n = count(list);
                let label = ui.selectable_label(list.id == shown.id, &list.name);
                let label = label.on_hover_text(match n {
                    1 => "1 address".to_string(),
                    _ => format!("{n} addresses"),
                });
                if label.clicked() {
                    self.selected = Some(list.id);
                    ui.close();
                }
            }
            ui.separator();
            if ui.button("New list…").clicked() {
                dialogs::request_list_name(ui.ctx(), None);
                ui.close();
            }
            if ui.button(format!("Rename {}…", shown.name)).clicked() {
                dialogs::request_list_name(ui.ctx(), Some(shown.id));
                ui.close();
            }
            ui.separator();
            let last = lists.lists.len() == 1;
            let armed = self
                .confirm_delete
                .is_some_and(|(id, at)| id == shown.id && now - at < CONFIRM_SECS);
            // An empty list has nothing to lose: it goes on the first click.
            let empty = shown.entries.is_empty();
            if armed || (empty && !last) {
                let n = shown.entries.len();
                let text = if empty {
                    format!("Delete {}", shown.name)
                } else {
                    format!(
                        "Delete {} and its {n} address{}",
                        shown.name,
                        if n == 1 { "" } else { "es" }
                    )
                };
                if ui
                    .add(
                        Button::new(RichText::new(text).color(theme::ERROR))
                            .wrap_mode(egui::TextWrapMode::Extend),
                    )
                    .on_hover_text("Their alert rules go too; their events stay listed")
                    .clicked()
                {
                    let _ =
                        cmd_tx.send(UiCommand::WatchEdit(WatchEdit::DeleteList { id: shown.id }));
                    self.confirm_delete = None;
                    self.selected = None;
                    ui.close();
                }
            } else if ui
                .add_enabled(!last, Button::new(format!("Delete {}…", shown.name)))
                .on_hover_text("Asks once more")
                .on_disabled_hover_text("The last watchlist stays: there is always one to add to")
                .clicked()
            {
                self.confirm_delete = Some((shown.id, now));
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_secs_f64(CONFIRM_SECS));
            }
        });
        response.on_hover_text("The watchlist on show, which Watch adds to");
    }
}

/// The Activity card's filter: every list's events, or one list's.
fn filter_menu(ui: &mut Ui, app: &App, chosen: &mut Option<ListId>) {
    let lists = &app.watch.lists;
    if lists.lists.len() < 2 && chosen.is_none() {
        return;
    }
    let text = chosen
        .and_then(|id| lists.get(id))
        .map_or("All lists".to_string(), |l| l.name.clone());
    // Drawn as the list menu in the Watchlists header is, so the two headers match.
    let button = Button::new(format!("{text} ▾")).small();
    let (response, _) = MenuButton::from_button(button).ui(ui, |ui| {
        if ui.selectable_label(chosen.is_none(), "All lists").clicked() {
            *chosen = None;
            ui.close();
        }
        for list in &lists.lists {
            if ui
                .selectable_label(*chosen == Some(list.id), &list.name)
                .clicked()
            {
                *chosen = Some(list.id);
                ui.close();
            }
        }
    });
    response.on_hover_text("Show the events of the addresses on one watchlist");
}

/// Whether an event is one Activity shows with `filter`: an address on the list now.
fn shown_by(app: &App, filter: Option<ListId>) -> impl Fn(&AddressEvent) -> bool + 'static {
    let network = app.network_name();
    let on_list: Option<std::collections::HashSet<String>> = filter.map(|id| {
        app.watch
            .lists
            .get(id)
            .map(|l| l.on_network(&network).map(|e| e.address.clone()).collect())
            .unwrap_or_default()
    });
    move |e| on_list.as_ref().is_none_or(|set| set.contains(&e.address))
}

/// Whether `s` is shaped like an address rather than a label search; whether it is one
/// is `watch::validate_address`'s call.
pub fn looks_like_address(s: &str) -> bool {
    let s = s.trim();
    s.contains(':') && s.len() > 40 && !s.contains(' ')
}

/// A net amount with its sign, or blank for zero.
fn signed_kas(sompi: i128) -> String {
    match sompi.signum() {
        0 => String::new(),
        1 => format!("+{}", format_kas(sompi as f64, 2)),
        _ => format!("-{}", format_kas(-sompi as f64, 2)),
    }
}

/// A pane's table height: the rest of the pane above its one-line hint when the panes
/// fill the tab, else the default.
fn table_height(ui: &Ui, fill: bool) -> f32 {
    if !fill {
        return TABLE_HEIGHT;
    }
    let spacing = ui.spacing().item_spacing.y;
    // The table's header row (`wide_table_with_lead`) sits above the body of `height`.
    let header = theme::ROW_HEIGHT + spacing;
    let hint = 2.0 + ui.text_style_height(&egui::TextStyle::Small) + spacing;
    (ui.available_height() - header - hint).max(3.0 * ui.spacing().interact_size.y)
}

fn ago(time_ms: u64) -> String {
    let now = x4kas_core::format::now_ms();
    format!(
        "{} ago",
        format_duration(std::time::Duration::from_millis(
            now.saturating_sub(time_ms)
        ))
    )
}

/// The subscription's state, set into the card border.
fn watch_status(ui: &mut Ui, app: &App) {
    let (text, color, detail) = match &app.watch.status.phase {
        WatchPhase::Idle => (
            "○ not watching",
            theme::TEXT_DIM,
            "Add an address to start".to_string(),
        ),
        WatchPhase::Waiting => (
            "◌ waiting for node",
            theme::TEXT_DIM,
            "Waiting for the node to connect and sync".into(),
        ),
        WatchPhase::Unavailable(why) => ("× unavailable", theme::WARN, why.clone()),
        WatchPhase::Active(n) => ("● live", theme::OK, format!("Subscribed for {n} addresses")),
    };
    status_chip(ui, "watch_status", text, color, |ui| {
        super::widgets::kv(ui, "Status", detail);
        if let Some(at) = app.watch.status.last_event_at {
            super::widgets::kv(
                ui,
                "Last event",
                format!("{} ago", format_duration(at.elapsed())),
            );
        }
        if let Some(ref err) = app.watch.status.last_error {
            super::widgets::kv(ui, "Error", RichText::new(err).color(theme::ERROR));
        }
    });
}

/// Confirmed balance changes, newest first. An event that tripped the address's alert
/// rules leads with a dot: filled while unread, a ring once marked read (a click
/// toggles it; the rules it tripped are in its hover).
fn activity(ui: &mut Ui, app: &mut App, filter: Option<ListId>, fill: bool) {
    let shown = shown_by(app, filter);
    // The events on show with their index in `app.watch.events`, for the read marks.
    let events: Vec<(usize, &AddressEvent)> = app
        .watch
        .events
        .iter()
        .enumerate()
        .filter(|(_, e)| shown(e))
        .collect();
    let rows = events
        .iter()
        .map(|(_, e)| {
            let sign = match e.kind {
                EventKind::Received => "+",
                EventKind::Sent => "-",
            };
            [
                e.address.clone(),
                format!("{sign}{}", format_kas(e.amount as f64, 8)),
                if e.is_coinbase {
                    "coinbase".into()
                } else {
                    String::new()
                },
                ago(e.time_ms),
            ]
        })
        .collect();
    // Per row: `None` without an alert, else whether it is read and the hover text.
    let dots: Vec<Option<(bool, String)>> = events
        .iter()
        .map(|(_, e)| {
            (!e.alerts.is_empty()).then(|| {
                let mut text = e.alerts.join("\n");
                text.push_str(if e.read {
                    "\n\nClick to mark as unread"
                } else {
                    "\n\nClick to mark as read"
                });
                (e.read, text)
            })
        })
        .collect();
    let indices: Vec<usize> = events.iter().map(|(i, _)| *i).collect();
    let mut toggled = Vec::new();
    let height = table_height(ui, fill);
    wide_table_with_lead(
        ui,
        "activity",
        ["Address", "Amount (KAS)", "", "When"],
        rows,
        if filter.is_some() {
            "No confirmed activity on this list yet"
        } else {
            "No confirmed activity yet"
        },
        address,
        Some(request_address),
        |ui, index| {
            if alert_dot(ui, dots[index].as_ref()) {
                toggled.push(indices[index]);
            }
        },
        height,
    );
    for index in toggled {
        let read = app.watch.events.get(index).is_some_and(|e| e.read);
        app.watch.set_read(index, !read);
    }
    ui.add_space(2.0);
    // A history that couldn't be loaded or saved takes the hint's line until it can.
    match &app.watch.history_error {
        Some(err) => {
            ui.label(RichText::new(err).color(theme::ERROR).small());
        }
        None => {
            ui.label(
                RichText::new(
                    "● marks an alert the address's rules raised; click it to mark it read.",
                )
                .weak()
                .small(),
            );
        }
    }
}

/// The alert dot at the start of an Activity row: filled while unread, a ring once
/// read, and an empty space of the same width for an event without an alert. Returns
/// whether it was clicked.
pub(super) fn alert_dot(ui: &mut Ui, dot: Option<&(bool, String)>) -> bool {
    let size = ui.spacing().interact_size.y.min(14.0);
    let Some((read, hover)) = dot else {
        ui.allocate_exact_size(vec2(size, size), Sense::hover());
        return false;
    };
    let (rect, response) = ui.allocate_exact_size(vec2(size, size), Sense::click());
    let response = response.on_hover_text(hover.as_str());
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let center = rect.center();
    let radius = size * 0.28;
    let color = if response.hovered() {
        theme::TEXT_BRIGHT
    } else if *read {
        theme::TEXT_DIM
    } else {
        theme::WARN
    };
    if *read {
        ui.painter()
            .circle_stroke(center, radius, Stroke::new(1.0_f32, color));
    } else {
        ui.painter().circle_filled(center, radius, color);
    }
    response.clicked()
}
