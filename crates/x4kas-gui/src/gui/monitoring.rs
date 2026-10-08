//! Monitoring tab: the Watchlist pane (half the width) with live balances, and
//! the Activity pane with the feed, where an event that tripped the address's alert
//! rules gets an alert dot that a click marks as read. Both fill the tab's height, their
//! tables scrolling inside; below the wrap width they stack and the tab scrolls. A row
//! click opens the address's info pane.

use eframe::egui::{self, RichText, Sense, Stroke, TextEdit, Ui, vec2};

use super::theme;
use super::widgets::{
    TABLE_HEIGHT, address, card_with_header, columns_wrap, label_search_popup, primary_button,
    request_address, status_chip, weighted_columns, wide_table_with_lead,
};
use x4kas_core::app::{App, WatchPhase};
use x4kas_core::controller::{CommandSender, UiCommand};
use x4kas_core::format::{format_duration, format_kas};
use x4kas_core::watch::{EventKind, WatchEntry};

const DAY_MS: u64 = 24 * 3_600_000;
/// Watchlist | Activity, half and half.
const PANE_WEIGHTS: [f32; 2] = [1.0, 1.0];
/// Narrowest a pane gets before the panes stack.
const PANE_MIN: f32 = 480.0;

/// The tab's own state: the add/search field.
#[derive(Default)]
pub struct MonitoringTab {
    input: String,
    /// The label matches popup is showing under the field.
    search_open: bool,
}

impl MonitoringTab {
    pub fn show(&mut self, ui: &mut Ui, app: &mut App, cmd_tx: &CommandSender) {
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
                "Watchlist",
                app,
                |ui, app| watch_status(ui, app),
                |ui, app| self.watchlist(ui, app, cmd_tx, fill),
            );
            card_with_header(
                right,
                "Activity",
                app,
                |ui, app| {
                    let unread = app.watch.unread_alerts();
                    if unread > 0
                        && ui
                            .small_button(format!("Mark all read ({unread})"))
                            .clicked()
                    {
                        app.watch.mark_all_read();
                    }
                },
                |ui, app| activity(ui, app, fill),
            );
        });
    }

    fn watchlist(&mut self, ui: &mut Ui, app: &mut App, cmd_tx: &CommandSender, fill: bool) {
        let network = network(app);
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
            if ui
                .add_enabled(is_address, primary_button("Watch"))
                .on_hover_text("Add to the watchlist")
                .clicked()
            {
                self.add(app, cmd_tx, &network);
            }
            if ui
                .add_enabled(is_address, egui::Button::new("Info"))
                .on_hover_text("Show address info")
                .clicked()
                || (submitted && is_address)
            {
                request_address(ui.ctx(), self.input.trim());
            }
        });
        // Label matches, when the field isn't an address.
        if let Some(field) = field
            && !looks_like_address(&self.input)
            && let Some(addr) =
                label_search_popup(ui, &field, &mut self.search_open, &self.input, &app.labels)
        {
            request_address(ui.ctx(), &addr);
        }
        ui.add_space(4.0);

        let entries: Vec<WatchEntry> = app
            .watch
            .list
            .entries
            .iter()
            .filter(|e| e.network == network)
            .cloned()
            .collect();
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
                    .or(e.last_activity_ms)
                    .map(ago);
                // Net of the events seen in the last 24h (since this session started).
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
                    e.name
                        .clone()
                        .or_else(|| app.labels.name(&e.address).map(str::to_string))
                        .unwrap_or_default(),
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
            [
                "Address",
                "Name",
                "Balance (KAS)",
                "24h change",
                "Last activity",
            ],
            rows,
            "No watched addresses yet. Paste one above and press Watch.",
            address,
            |_, _| {},
            height,
        );
        ui.add_space(2.0);
        ui.label(
            RichText::new("Click an address for its info, history and alert settings.")
                .weak()
                .small(),
        );
    }

    fn add(&mut self, app: &mut App, cmd_tx: &CommandSender, network: &str) {
        let addr = self.input.trim();
        if addr.is_empty() {
            return;
        }
        let mut list = app.watch.list.clone();
        if !list
            .entries
            .iter()
            .any(|e| e.address == addr && e.network == network)
        {
            list.entries.push(WatchEntry::new(addr, network));
            let _ = cmd_tx.send(UiCommand::WatchSet(list));
        }
        self.input.clear();
    }
}

/// The connected network, or the saved one before connecting.
pub fn network(app: &App) -> String {
    app.node
        .server_info
        .as_ref()
        .map(|s| s.network_id.clone())
        .unwrap_or_else(|| "mainnet".to_string())
}

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
    let hint = 2.0 + ui.text_style_height(&egui::TextStyle::Small) + ui.spacing().item_spacing.y;
    (ui.available_height() - hint).max(3.0 * ui.spacing().interact_size.y)
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
fn activity(ui: &mut Ui, app: &mut App, fill: bool) {
    let rows = app
        .watch
        .events
        .iter()
        .map(|e| {
            let sign = match e.kind {
                EventKind::Received => "+",
                EventKind::Sent => "-",
            };
            let name = app
                .watch
                .entry(&e.address)
                .and_then(|w| w.name.clone())
                .unwrap_or_default();
            [
                e.address.clone(),
                name,
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
    let dots: Vec<Option<(bool, String)>> = app
        .watch
        .events
        .iter()
        .map(|e| {
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
    let mut toggled = Vec::new();
    let height = table_height(ui, fill);
    wide_table_with_lead(
        ui,
        "activity",
        ["Address", "Name", "Amount (KAS)", "", "When"],
        rows,
        "No confirmed activity yet",
        address,
        |ui, index| {
            if alert_dot(ui, dots[index].as_ref()) {
                toggled.push(index);
            }
        },
        height,
    );
    for index in toggled {
        let read = app.watch.events.get(index).is_some_and(|e| e.read);
        app.watch.set_read(index, !read);
    }
    ui.add_space(2.0);
    ui.label(
        RichText::new("● marks an alert the address's rules raised; click it to mark it read.")
            .weak()
            .small(),
    );
}

/// The alert dot at the start of an Activity row: filled while unread, a ring once
/// read, and an empty space of the same width for an event without an alert. Returns
/// whether it was clicked.
fn alert_dot(ui: &mut Ui, dot: Option<&(bool, String)>) -> bool {
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
