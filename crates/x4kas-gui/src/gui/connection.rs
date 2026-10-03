//! Connection window: pick a custom wRPC URL or the public resolver.

use eframe::egui::{self, Button, ComboBox, RichText, TextEdit, Ui};

use super::theme;
use super::widgets::{field_label, kv_grid, modal_window, placeholder, primary_button};
use x4kas_core::app::{ActiveConnection, App, ConnectionStatus};
use x4kas_core::config::{self, ConnectionKind, ConnectionSettings};
use x4kas_core::controller::{CommandSender, RemoteTarget, UiCommand};

const KINDS: [(ConnectionKind, &str); 2] = [
    (ConnectionKind::Url, "Custom URL"),
    (ConnectionKind::Resolver, "Public resolver"),
];

pub struct ConnectionWindow {
    pub open: bool,
    /// Working copy of the form; saved to disk on Connect.
    form: ConnectionSettings,
    error: Option<String>,
}

impl ConnectionWindow {
    pub fn new(form: ConnectionSettings, open: bool) -> Self {
        Self {
            open,
            form,
            error: None,
        }
    }

    pub fn toggle(&mut self) {
        self.open = !self.open;
    }

    pub fn show(&mut self, ctx: &egui::Context, app: &mut App, cmd_tx: &CommandSender) {
        if !self.open {
            return;
        }
        let window = egui::Window::new("Connection")
            .resizable(false)
            .default_width(440.0);
        // `contents` closes the window itself after a successful Connect.
        let still_open = modal_window(ctx, window, |ui| self.contents(ui, app, cmd_tx));
        self.open &= still_open;
    }

    fn contents(&mut self, ui: &mut Ui, app: &mut App, cmd_tx: &CommandSender) {
        current(ui, app);
        ui.add_space(2.0);
        ui.separator();

        ui.horizontal(|ui| {
            for (kind, label) in KINDS {
                ui.selectable_value(&mut self.form.kind, kind, label);
            }
        });
        ui.add_space(4.0);

        match self.form.kind {
            ConnectionKind::Url => {
                kv_grid(ui, "connection_url", |ui| {
                    field_label(ui, "URL");
                    ui.add(
                        TextEdit::singleline(&mut self.form.url)
                            .hint_text("ws://host:17110")
                            .desired_width(280.0),
                    );
                    ui.end_row();
                    field_label(ui, "Network");
                    network_combo(ui, &mut self.form.network);
                    ui.end_row();
                });
            }
            ConnectionKind::Resolver => {
                kv_grid(ui, "connection_resolver", |ui| {
                    field_label(ui, "Network");
                    network_combo(ui, &mut self.form.network);
                    ui.end_row();
                });
                placeholder(
                    ui,
                    "Connects to a public node chosen by the Kaspa resolver. \
                     Mining and analytics need a direct node and are disabled.",
                );
            }
        }

        if let Some(ref err) = self.error {
            ui.label(RichText::new(err).color(theme::ERROR));
        }
        ui.add_space(8.0);

        ui.horizontal(|ui| {
            let can_connect = match self.form.kind {
                ConnectionKind::Url => !self.form.url.trim().is_empty(),
                ConnectionKind::Resolver => true,
            };
            if ui
                .add_enabled(can_connect, primary_button("Connect"))
                .clicked()
            {
                self.connect(cmd_tx);
            }
            let connected = app.connection != ActiveConnection::None;
            if ui
                .add_enabled(connected, Button::new("Disconnect"))
                .clicked()
            {
                let _ = cmd_tx.send(UiCommand::Disconnect);
            }
        });
    }

    fn connect(&mut self, cmd_tx: &CommandSender) {
        self.form.url = self.form.url.trim().to_string();
        self.error = self
            .form
            .save()
            .err()
            .map(|e| format!("Could not save connection settings: {e}"));

        let target = RemoteTarget {
            url: (self.form.kind == ConnectionKind::Url).then(|| self.form.url.clone()),
            network: self.form.network.clone(),
        };
        let _ = cmd_tx.send(UiCommand::Connect(target));
        if self.error.is_none() {
            self.open = false;
        }
    }
}

fn current(ui: &mut Ui, app: &App) {
    let (text, color) = theme::connection_status(&app.node.connection_status);
    ui.horizontal(|ui| {
        ui.label(RichText::new("●").color(color));
        // With no target, "Not connected" says it all; otherwise status + target.
        if app.connection == ActiveConnection::None {
            ui.label(RichText::new(app.connection.label()).color(color));
        } else {
            ui.label(RichText::new(text).color(color));
            ui.label(RichText::new(app.connection.label()).weak());
        }
    });
    if let ConnectionStatus::Error(ref e) = app.node.connection_status {
        ui.label(RichText::new(e).color(theme::ERROR));
    }
}

fn network_combo(ui: &mut Ui, network: &mut String) {
    ComboBox::from_id_salt("connection_network")
        .selected_text(network.as_str())
        .show_ui(ui, |ui| {
            for n in config::valid_networks() {
                ui.selectable_value(network, n.to_string(), *n);
            }
        });
}
