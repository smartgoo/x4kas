use std::time::Instant;

use eframe::egui::{self, Button, ComboBox, DragValue, RichText, TextEdit, Ui};

use super::widgets::{kv_grid, placeholder, syncing_guard};
use crate::app::App;
use crate::controller::{CommandSender, UiCommand};
use crate::rpc::methods::{self, ParamKind, RpcMethod};

pub fn show(ui: &mut Ui, app: &mut App, cmd_tx: &CommandSender) {
    if syncing_guard(ui, app, "RPC Cmds") {
        return;
    }

    egui::SidePanel::left("rpc_methods")
        .resizable(true)
        .default_width(260.0)
        .show_inside(ui, |ui| {
            ui.label(RichText::new("RPC Methods").strong());
            ui.add_space(4.0);
            egui::ScrollArea::vertical().show(ui, |ui| method_list(ui, app, cmd_tx));
        });

    egui::CentralPanel::default().show_inside(ui, |ui| {
        let Some(method) = app.rpc_explorer.method() else {
            return;
        };
        ui.label(RichText::new(method.name).strong().monospace());
        ui.label(RichText::new(method.description).weak());
        ui.add_space(4.0);
        let submitted = if method.params.is_empty() {
            false
        } else {
            arg_form(ui, app, method)
        };

        if method.params.is_empty()
            && let Some(wait) = app.rpc_explorer.loop_wait(Instant::now())
        {
            if wait.is_zero() {
                run(app, cmd_tx, method);
            } else {
                ui.ctx().request_repaint_after(wait);
            }
        }

        ui.horizontal(|ui| {
            let can_run = !app.rpc_explorer.is_loading && required_filled(app, method);
            let label = if method.params.is_empty() {
                "Run again"
            } else {
                "▶ Run"
            };
            if ui.add_enabled(can_run, Button::new(label)).clicked() || (submitted && can_run) {
                run(app, cmd_tx, method);
            }
            if method.params.is_empty() {
                let state = &mut app.rpc_explorer;
                ui.toggle_value(&mut state.loop_enabled, "⟳ Loop")
                    .on_hover_text("Re-run this method on an interval");
                ui.add(
                    DragValue::new(&mut state.loop_interval_secs)
                        .range(0.1..=86_400.0)
                        .speed(0.1)
                        .max_decimals(1)
                        .suffix(" s"),
                )
                .on_hover_text("Loop interval in seconds");
            }
            if app.rpc_explorer.is_loading {
                ui.spinner();
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let response = app.rpc_explorer.last_response.as_deref();
                if ui
                    .add_enabled(response.is_some(), Button::new("Copy"))
                    .clicked()
                    && let Some(text) = response
                {
                    ui.ctx().copy_text(text.to_string());
                }
            });
        });
        ui.separator();

        match app.rpc_explorer.last_response {
            Some(ref response) => {
                egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
                    // Read-only but selectable/copyable.
                    ui.add(
                        TextEdit::multiline(&mut response.as_str())
                            .code_editor()
                            .desired_width(f32::INFINITY),
                    );
                });
            }
            None if app.rpc_explorer.is_loading => placeholder(ui, "Loading…"),
            None if method.params.is_empty() => {
                placeholder(ui, "Select a method on the left to run it.")
            }
            None => placeholder(ui, "Fill in the arguments and press Run."),
        }
    });
}

fn method_list(ui: &mut Ui, app: &mut App, cmd_tx: &CommandSender) {
    for i in 0..app.rpc_explorer.available_methods.len() {
        let Some(method) = methods::find(app.rpc_explorer.available_methods[i]) else {
            continue;
        };
        let selected = i == app.rpc_explorer.selected_method;
        let text = if method.params.is_empty() {
            RichText::new(method.name)
        } else {
            RichText::new(format!("{} …", method.name))
        };
        let response = ui.selectable_label(selected, text).on_hover_text(format!(
            "{}\n\n{}",
            method.description,
            method.usage()
        ));
        if response.clicked() {
            if !selected {
                app.rpc_explorer.select(i);
                app.rpc_explorer.last_response = None;
            }
            // Argument-free methods run on click; others wait for the form.
            if method.params.is_empty() && !app.rpc_explorer.is_loading {
                run(app, cmd_tx, method);
            }
        }
    }
}

/// Argument inputs for the selected method. Returns true when Enter was pressed in a field.
fn arg_form(ui: &mut Ui, app: &mut App, method: &'static RpcMethod) -> bool {
    let sink = app.node.dag_info.as_ref().map(|d| d.sink.clone());
    let args = &mut app.rpc_explorer.args;
    args.resize(method.params.len(), String::new());
    let mut submitted = false;

    kv_grid(ui, &format!("rpc_args_{}", method.name), |ui| {
        for (param, value) in method.params.iter().zip(args.iter_mut()) {
            let label = match param.default {
                None => RichText::new(param.name).weak().strong(),
                Some(_) => RichText::new(param.name).weak(),
            };
            ui.label(label);
            match param.kind {
                ParamKind::Bool => {
                    let mut checked = methods::parse_bool(value).unwrap_or(false);
                    if ui.checkbox(&mut checked, "").changed() {
                        *value = checked.to_string();
                    }
                }
                ParamKind::Choice(options) => {
                    ComboBox::from_id_salt(("rpc_arg", method.name, param.name))
                        .selected_text(value.as_str())
                        .show_ui(ui, |ui| {
                            for opt in options {
                                ui.selectable_value(value, opt.to_string(), *opt);
                            }
                        });
                }
                kind => {
                    ui.horizontal(|ui| {
                        let hint = if param.default == Some("") {
                            format!("{} (optional)", kind.hint())
                        } else {
                            kind.hint().to_string()
                        };
                        let multiline = matches!(kind, ParamKind::Addresses);
                        let edit = if multiline {
                            TextEdit::multiline(value).desired_rows(2)
                        } else {
                            TextEdit::singleline(value)
                        };
                        let response = ui.add(
                            edit.hint_text(hint)
                                .font(egui::TextStyle::Monospace)
                                .desired_width(460.0),
                        );
                        if !multiline
                            && response.lost_focus()
                            && ui.input(|i| i.key_pressed(egui::Key::Enter))
                        {
                            submitted = true;
                        }
                        if kind == ParamKind::Hash
                            && let Some(ref sink) = sink
                            && ui
                                .small_button("sink")
                                .on_hover_text("Use the current sink hash")
                                .clicked()
                        {
                            *value = sink.clone();
                        }
                    });
                }
            }
            ui.end_row();
        }
    });
    ui.add_space(4.0);
    submitted
}

fn required_filled(app: &App, method: &RpcMethod) -> bool {
    method
        .params
        .iter()
        .zip(&app.rpc_explorer.args)
        .all(|(p, v)| p.default.is_some() || !v.trim().is_empty())
}

fn run(app: &mut App, cmd_tx: &CommandSender, method: &RpcMethod) {
    app.rpc_explorer.is_loading = true;
    app.rpc_explorer.last_run = Some(Instant::now());
    let _ = cmd_tx.send(UiCommand::ExecuteRpc {
        method: method.name.to_string(),
        args: app.rpc_explorer.args.clone(),
    });
}
