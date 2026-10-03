//! Connection window: pick a custom wRPC URL, the public resolver, or the embedded node.

use eframe::egui::{self, Button, ComboBox, RichText, TextEdit, Ui};

use super::widgets::{field_label, kv_grid, primary_button};
use super::{node, theme};
use crate::app::{ActiveConnection, App, ConnectionStatus, DaemonStatus, Tab};
use crate::config::{ConnectionKind, ConnectionSettings, DaemonConfig};
use crate::controller::{CommandSender, RemoteTarget, UiCommand};

const KINDS: [(ConnectionKind, &str); 3] = [
    (ConnectionKind::Url, "Custom URL"),
    (ConnectionKind::Resolver, "Public resolver"),
    (ConnectionKind::Embedded, "Embedded node"),
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
        let mut open = true;
        egui::Window::new("Connection")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(440.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| self.contents(ui, app, cmd_tx));
        if !open || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.open = false;
        }
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

        let daemon_running = !matches!(
            app.integrated_node.status,
            DaemonStatus::Stopped | DaemonStatus::Error(_)
        );
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
                note(ui, "Borsh wRPC ports: mainnet 17110 · testnets 17210");
            }
            ConnectionKind::Resolver => {
                kv_grid(ui, "connection_resolver", |ui| {
                    field_label(ui, "Network");
                    network_combo(ui, &mut self.form.network);
                    ui.end_row();
                });
                note(
                    ui,
                    "Connects to a public node chosen by the Kaspa resolver. \
                     Mining and analytics need a direct node and are disabled.",
                );
            }
            ConnectionKind::Embedded => {
                note(
                    ui,
                    &format!(
                        "Runs kaspad inside the app using the Node tab settings (network: {}).",
                        app.integrated_node.config.network
                    ),
                );
                if ui.link("Open Node tab").clicked() {
                    app.active_tab = Tab::IntegratedNode;
                    self.open = false;
                }
            }
        }

        if daemon_running && self.form.kind != ConnectionKind::Embedded {
            ui.label(RichText::new("Connecting will stop the embedded node.").color(theme::WARN));
        }
        if let Some(ref err) = self.error {
            ui.label(RichText::new(err).color(theme::ERROR));
        }
        ui.add_space(8.0);

        ui.horizontal(|ui| {
            let (label, can_connect) = match self.form.kind {
                ConnectionKind::Url => ("Connect", !self.form.url.trim().is_empty()),
                ConnectionKind::Resolver => ("Connect", true),
                ConnectionKind::Embedded => (
                    "▶ Start node",
                    !daemon_running && !app.integrated_node.config.app_dir.trim().is_empty(),
                ),
            };
            if ui.add_enabled(can_connect, primary_button(label)).clicked() {
                self.connect(app, cmd_tx);
            }
            let connected = app.connection != ActiveConnection::None || daemon_running;
            if ui
                .add_enabled(connected, Button::new("Disconnect"))
                .clicked()
            {
                let _ = cmd_tx.send(UiCommand::Disconnect);
            }
        });
    }

    fn connect(&mut self, app: &mut App, cmd_tx: &CommandSender) {
        self.form.url = self.form.url.trim().to_string();
        self.error = self
            .form
            .save()
            .err()
            .map(|e| format!("Could not save connection settings: {e}"));

        match self.form.kind {
            ConnectionKind::Embedded => node::start_daemon(&mut app.integrated_node, cmd_tx),
            kind => {
                let target = RemoteTarget {
                    url: (kind == ConnectionKind::Url).then(|| self.form.url.clone()),
                    network: self.form.network.clone(),
                };
                let _ = cmd_tx.send(UiCommand::Connect(target));
            }
        }
        if self.error.is_none() {
            self.open = false;
        }
    }
}

fn current(ui: &mut Ui, app: &App) {
    let (text, color) = theme::connection_status(&app.node.connection_status);
    ui.horizontal(|ui| {
        ui.label(RichText::new("●").color(color));
        ui.label(RichText::new(text).color(color));
        ui.label(RichText::new(app.connection.label()).weak());
    });
    if let ConnectionStatus::Error(ref e) = app.node.connection_status {
        ui.label(RichText::new(e).color(theme::ERROR));
    }
}

fn network_combo(ui: &mut Ui, network: &mut String) {
    ComboBox::from_id_salt("connection_network")
        .selected_text(network.as_str())
        .show_ui(ui, |ui| {
            for n in DaemonConfig::valid_networks() {
                ui.selectable_value(network, n.to_string(), *n);
            }
        });
}

fn note(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).weak());
}
