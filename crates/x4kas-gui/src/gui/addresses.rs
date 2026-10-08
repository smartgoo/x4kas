//! Addresses tab: the watchlist with live balances and pending activity, the alerts
//! raised by its rules, and the activity feed. A row click opens Address Info.

use eframe::egui::{self, RichText, TextEdit, Ui};

use super::theme;
use super::widgets::{
    CARD_GAP, address, card, card_with_header, placeholder, primary_button, request_address,
    status_chip, weighted_columns, wide_table,
};
use x4kas_core::app::{App, WatchPhase};
use x4kas_core::controller::{CommandSender, UiCommand};
use x4kas_core::format::{format_duration, format_kas};
use x4kas_core::watch::{EventKind, WatchEntry};

/// Narrowest a card gets before its row wraps.
const CARD_MIN: f32 = 380.0;
const DAY_MS: u64 = 24 * 3_600_000;

/// The tab's own state: the add/search field.
#[derive(Default)]
pub struct AddressesTab {
    input: String,
    /// The API key field, filled from the settings on first show.
    key_input: Option<String>,
}

impl AddressesTab {
    pub fn show(&mut self, ui: &mut Ui, app: &mut App, cmd_tx: &CommandSender) {
        app.watch.unread_alerts = 0;
        egui::ScrollArea::vertical().show(ui, |ui| {
            card_with_header(
                ui,
                "Watchlist",
                app,
                |ui, app| watch_status(ui, app),
                |ui, app| self.watchlist(ui, app, cmd_tx),
            );
            ui.add_space(CARD_GAP);
            weighted_columns(ui, [1.0, 1.0], CARD_MIN, |[left, right]| {
                card(left, "Alerts", |ui| alerts(ui, app));
                card(right, "Activity", |ui| activity(ui, app));
            });
            ui.add_space(CARD_GAP);
            card(ui, "Label Sources", |ui| {
                self.label_sources(ui, app, cmd_tx)
            });
        });
    }

    /// The public list is always on; per-address online lookups are opt-in because they
    /// reveal which addresses the user looks at.
    fn label_sources(&mut self, ui: &mut Ui, app: &mut App, cmd_tx: &CommandSender) {
        let book = app.labels.clone();
        ui.label(
            RichText::new(format!(
                "{} labels known: your own, the public api.kaspa.org list (fetched in bulk, refreshed daily) and the bundled snapshot.",
                book.len()
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
                "Per-address lookups below send the address you open to that service. Off until you enable them.",
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

    fn watchlist(&mut self, ui: &mut Ui, app: &mut App, cmd_tx: &CommandSender) {
        let network = network(app);
        // Add or look up an address, or find a labelled one.
        let mut submitted = false;
        ui.horizontal(|ui| {
            let edit = TextEdit::singleline(&mut self.input)
                .hint_text("kaspa:… address, or a label such as \"Bybit\"")
                .desired_width(ui.available_width() - 190.0);
            let response = ui.add(edit);
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
                .on_hover_text("Open Address Info")
                .clicked()
                || (submitted && is_address)
            {
                request_address(ui.ctx(), self.input.trim());
            }
        });
        // Label matches, when the field isn't an address.
        if !self.input.trim().is_empty() && !looks_like_address(&self.input) {
            let hits: Vec<(String, String)> = app
                .labels
                .search(&self.input)
                .into_iter()
                .take(8)
                .map(|(a, l)| (a.to_string(), l.name.clone()))
                .collect();
            if hits.is_empty() {
                placeholder(ui, "No label matches");
            }
            for (addr, name) in hits {
                ui.horizontal(|ui| {
                    if ui
                        .link(RichText::new(&name).color(theme::ACCENT_BRIGHT))
                        .clicked()
                    {
                        request_address(ui.ctx(), &addr);
                    }
                    address(ui, &addr);
                });
            }
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
        let rows = entries
            .iter()
            .map(|e| {
                let balance = app.watch.balances.get(&e.address).copied();
                let pending = app.watch.pending.get(&e.address).copied();
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
                    match pending {
                        Some((inc, out)) if inc > 0 || out > 0 => {
                            format!(
                                "+{} / -{}",
                                format_kas(inc as f64, 2),
                                format_kas(out as f64, 2)
                            )
                        }
                        _ => String::new(),
                    },
                    signed_kas(day),
                    last.unwrap_or_default(),
                    if e.enabled { "on".into() } else { "off".into() },
                ]
            })
            .collect();
        wide_table(
            ui,
            "watchlist",
            [
                "Address",
                "Name",
                "Balance (KAS)",
                "Pending",
                "24h change",
                "Last activity",
                "Alerts",
            ],
            rows,
            "No watched addresses yet. Paste one above and press Watch.",
            address,
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

/// Newest first; the watchlist name prefixes the message (labels show as chips on the
/// address).
fn alerts(ui: &mut Ui, app: &App) {
    let rows = app
        .watch
        .alerts
        .iter()
        .map(|a| {
            let message = match &a.name {
                Some(name) => format!("{name}: {}", a.message),
                None => a.message.clone(),
            };
            [a.address.clone(), message, ago(a.time_ms)]
        })
        .collect();
    wide_table(
        ui,
        "alerts",
        ["Address", "Alert", "When"],
        rows,
        "No alerts yet",
        address,
    );
}

/// Confirmed balance changes, newest first.
fn activity(ui: &mut Ui, app: &App) {
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
    wide_table(
        ui,
        "activity",
        ["Address", "Name", "Amount (KAS)", "", "When"],
        rows,
        "No confirmed activity yet",
        address,
    );
}
