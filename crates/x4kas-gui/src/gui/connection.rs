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
    /// Connect was clicked: the window stays until the connection is up, so a failure
    /// is seen where it was caused.
    connecting: bool,
}

impl ConnectionWindow {
    pub fn new(form: ConnectionSettings, open: bool) -> Self {
        Self {
            open,
            form,
            error: None,
            connecting: false,
        }
    }

    pub fn toggle(&mut self) {
        self.open = !self.open;
        self.connecting = false;
    }

    pub fn show(&mut self, ctx: &egui::Context, app: &mut App, cmd_tx: &CommandSender) {
        if !self.open {
            return;
        }
        // Close once the connection the window asked for is up.
        if self.connecting && matches!(app.node.connection_status, ConnectionStatus::Connected) {
            self.open = false;
            self.connecting = false;
            return;
        }
        let window = egui::Window::new("Connection")
            .resizable(false)
            .default_width(440.0);
        let still_open = modal_window(ctx, window, |ui| self.contents(ui, app, cmd_tx));
        if !still_open {
            self.open = false;
            self.connecting = false;
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

        let mut submit = false;
        let url_problem = match self.form.kind {
            ConnectionKind::Url => config::validate_url(&self.form.url).err(),
            ConnectionKind::Resolver => None,
        };
        match self.form.kind {
            ConnectionKind::Url => {
                kv_grid(ui, "connection_url", |ui| {
                    field_label(ui, "URL");
                    let field = ui.add(
                        TextEdit::singleline(&mut self.form.url)
                            .hint_text("ws://host:17110")
                            .desired_width(280.0),
                    );
                    // Enter connects, like a form.
                    submit = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    ui.end_row();
                    field_label(ui, "Network");
                    network_combo(ui, &mut self.form.network);
                    ui.end_row();
                });
                // Only once something is typed: an empty field isn't a mistake yet.
                if let Some(problem) = url_problem.as_deref()
                    && !self.form.url.trim().is_empty()
                {
                    ui.label(RichText::new(problem).color(theme::ERROR));
                }
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
            let can_connect = url_problem.is_none();
            if ui
                .add_enabled(can_connect, primary_button("Connect"))
                .clicked()
                || (submit && can_connect)
            {
                self.connect(cmd_tx);
            }
            let connected = app.connection != ActiveConnection::None;
            if ui
                .add_enabled(connected, Button::new("Disconnect"))
                .on_hover_text("Also stops a connection attempt")
                .clicked()
            {
                self.connecting = false;
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
        self.connecting = true;
    }
}

/// The connection's current state and target; while connecting, a spinner, and on an
/// error its message.
fn current(ui: &mut Ui, app: &App) {
    let (text, color) = theme::connection_status(&app.node.connection_status);
    ui.horizontal(|ui| {
        if matches!(app.node.connection_status, ConnectionStatus::Connecting) {
            ui.spinner();
        } else {
            ui.label(RichText::new("●").color(color));
        }
        // With no target, "Not connected" says it all; otherwise status + target.
        if app.connection == ActiveConnection::None {
            ui.label(RichText::new(app.connection.label()).color(color));
        } else {
            ui.label(RichText::new(text).color(color));
            ui.label(RichText::new(app.connection.label()).weak());
        }
    });
    match app.node.connection_status {
        ConnectionStatus::Error(ref e) => {
            ui.label(RichText::new(e).color(theme::ERROR));
        }
        ConnectionStatus::Connecting => {
            let reason = app
                .node
                .last_error
                .as_deref()
                .map(|e| format!("{e}. "))
                .unwrap_or_default();
            ui.label(
                RichText::new(format!(
                    "{reason}Keeps trying until the node answers; Disconnect stops it."
                ))
                .weak(),
            );
        }
        _ => {}
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
