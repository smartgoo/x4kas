use eframe::egui::{self, RichText, Ui};

use super::widgets::{placeholder, syncing_guard};
use crate::app::App;
use crate::controller::{CommandSender, UiCommand};

pub fn show(ui: &mut Ui, app: &mut App, cmd_tx: &CommandSender) {
    if syncing_guard(ui, app, "RPC Cmds") {
        return;
    }

    egui::SidePanel::left("rpc_methods")
        .resizable(true)
        .default_width(220.0)
        .show_inside(ui, |ui| {
            ui.label(RichText::new("RPC Methods").strong());
            ui.label(RichText::new("Click a method to run it").weak().small());
            ui.add_space(4.0);
            egui::ScrollArea::vertical().show(ui, |ui| {
                for (i, method) in app.rpc_explorer.available_methods.iter().enumerate() {
                    let selected = i == app.rpc_explorer.selected_method;
                    if ui.selectable_label(selected, *method).clicked() {
                        app.rpc_explorer.selected_method = i;
                        app.rpc_explorer.is_loading = true;
                        let _ = cmd_tx.send(UiCommand::ExecuteRpc(method.to_string()));
                    }
                }
            });
        });

    egui::CentralPanel::default().show_inside(ui, |ui| {
        let method = app
            .rpc_explorer
            .available_methods
            .get(app.rpc_explorer.selected_method)
            .copied()
            .unwrap_or_default();

        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("Response: {method}")).strong());
            if app.rpc_explorer.is_loading {
                ui.spinner();
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let response = app.rpc_explorer.last_response.as_deref();
                if ui
                    .add_enabled(response.is_some(), egui::Button::new("Copy"))
                    .clicked()
                    && let Some(text) = response
                {
                    ui.ctx().copy_text(text.to_string());
                }
                if ui
                    .add_enabled(!app.rpc_explorer.is_loading, egui::Button::new("Run again"))
                    .clicked()
                {
                    app.rpc_explorer.is_loading = true;
                    let _ = cmd_tx.send(UiCommand::ExecuteRpc(method.to_string()));
                }
            });
        });
        ui.separator();

        match app.rpc_explorer.last_response {
            Some(ref response) => {
                egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
                    // Read-only but selectable/copyable.
                    ui.add(
                        egui::TextEdit::multiline(&mut response.as_str())
                            .code_editor()
                            .desired_width(f32::INFINITY),
                    );
                });
            }
            None if app.rpc_explorer.is_loading => placeholder(ui, "Loading…"),
            None => placeholder(ui, "Select a method on the left to run it."),
        }
    });
}
