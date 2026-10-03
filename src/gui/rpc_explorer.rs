use std::time::Instant;

use eframe::egui::{self, Button, ComboBox, RichText, TextEdit, Ui};

use super::theme;
use super::widgets::{field_label, json_view, kv_grid, placeholder, primary_button, section_title};
use crate::app::{App, RpcExplorerState};
use crate::controller::{CommandSender, UiCommand};
use crate::rpc::methods::{self, ParamKind, RpcMethod};

pub fn show(ui: &mut Ui, app: &mut App, cmd_tx: &CommandSender) {
    let list_width = method_list_width(ui);
    egui::SidePanel::left("rpc_methods")
        .resizable(true)
        .min_width(list_width)
        .default_width(list_width)
        .show_inside(ui, |ui| {
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
            section_title(ui, "RPC Methods");
            ui.add_space(2.0);
            egui::ScrollArea::vertical().show(ui, |ui| method_list(ui, app, cmd_tx));
        });

    egui::CentralPanel::default().show_inside(ui, |ui| {
        let Some(method) = app.rpc_explorer.method() else {
            return;
        };
        ui.label(
            RichText::new(method.name)
                .color(theme::ACCENT_BRIGHT)
                .size(15.0),
        );
        ui.label(RichText::new(method.description).weak());
        ui.add_space(2.0);
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
                "▶ Run again"
            } else {
                "▶ Run"
            };
            if ui.add_enabled(can_run, primary_button(label)).clicked() || (submitted && can_run) {
                run(app, cmd_tx, method);
            }
            if method.params.is_empty() {
                loop_control(ui, &mut app.rpc_explorer);
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
                let state = &app.rpc_explorer;
                let clicked = egui::ScrollArea::both()
                    .auto_shrink(false)
                    .show(ui, |ui| json_view(ui, response, &state.hash_links))
                    .inner;
                if let Some(hash) = clicked {
                    app.rpc_explorer.open_block(&hash);
                    if let Some(get_block) = app.rpc_explorer.method() {
                        run(app, cmd_tx, get_block);
                    }
                }
            }
            None if app.rpc_explorer.is_loading => placeholder(ui, "Loading…"),
            None if method.params.is_empty() => {
                placeholder(ui, "Select a method on the left to run it.")
            }
            None => placeholder(ui, "Fill in the arguments and press Run."),
        }
    });
}

/// List entry: methods that take arguments get a trailing ellipsis.
fn method_label(method: &RpcMethod) -> String {
    if method.params.is_empty() {
        method.name.to_string()
    } else {
        format!("{} …", method.name)
    }
}

/// Side panel width that fits the longest method label on one line.
fn method_list_width(ui: &Ui) -> f32 {
    let font = egui::TextStyle::Button.resolve(ui.style());
    let text = methods::RPC_METHODS
        .iter()
        .map(|m| {
            ui.painter()
                .layout_no_wrap(method_label(m), font.clone(), egui::Color32::WHITE)
                .size()
                .x
        })
        .fold(0.0, f32::max);
    let spacing = &ui.style().spacing;
    // Label padding, the panel's inner margins, and room for the scroll bar.
    text + 2.0 * spacing.button_padding.x + 16.0 + spacing.scroll.allocated_width() + 8.0
}

fn method_list(ui: &mut Ui, app: &mut App, cmd_tx: &CommandSender) {
    for i in 0..app.rpc_explorer.available_methods.len() {
        let method = app.rpc_explorer.available_methods[i];
        let selected = i == app.rpc_explorer.selected_method;
        let response = ui.selectable_label(selected, method_label(method));
        if response.clicked() {
            if !selected {
                app.rpc_explorer.select(i);
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
    let args = &mut app.rpc_explorer.args;
    args.resize(method.params.len(), String::new());
    let mut submitted = false;

    kv_grid(ui, &format!("rpc_args_{}", method.name), |ui| {
        for (param, value) in method.params.iter().zip(args.iter_mut()) {
            match param.default {
                None => field_label(ui, &format!("{}*", param.name)),
                Some(_) => field_label(ui, param.name),
            };
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
                        let response = ui.add(edit.hint_text(hint).desired_width(460.0));
                        if !multiline
                            && response.lost_focus()
                            && ui.input(|i| i.key_pressed(egui::Key::Enter))
                        {
                            submitted = true;
                        }
                    });
                }
            }
            ui.end_row();
        }
    });
    ui.add_space(2.0);
    submitted
}

/// Loop toggle and a typed interval in seconds, kept close together.
fn loop_control(ui: &mut Ui, state: &mut RpcExplorerState) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 3.0;
        ui.toggle_value(&mut state.loop_enabled, "⟳ Loop")
            .on_hover_text("Re-run this method on an interval");

        // Text buffer lives in egui memory so partial input ("0.") survives frames.
        let id = ui.id().with("loop_interval_text");
        let mut text = ui
            .data_mut(|d| d.get_temp::<String>(id))
            .unwrap_or_else(|| state.loop_interval_secs.to_string());
        let response = ui
            .add(
                TextEdit::singleline(&mut text)
                    .desired_width(44.0)
                    .horizontal_align(egui::Align::Center),
            )
            .on_hover_text("Loop interval in seconds (min 0.1)");
        if response.changed()
            && let Ok(secs) = text.trim().parse::<f64>()
            && secs.is_finite()
        {
            state.loop_interval_secs = secs.clamp(0.1, 86_400.0);
        }
        if response.lost_focus() {
            // Show the value actually in use (clamped, or reverted if invalid).
            text = state.loop_interval_secs.to_string();
        }
        ui.data_mut(|d| d.insert_temp(id, text));
        ui.label("s");
    });
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
