//! Command palette: a bottom panel with command output and an input line.

use eframe::egui::{self, Key, Modifiers, RichText, TextEdit, Ui};

use super::theme;
use crate::app::CommandLine;
use crate::controller::{CommandSender, UiCommand};
use crate::rpc::methods;

const MAX_SUGGESTIONS: usize = 6;

pub fn show(ctx: &egui::Context, cl: &mut CommandLine, cmd_tx: &CommandSender) {
    if !cl.active {
        return;
    }
    if ctx.input(|i| i.key_pressed(Key::Escape)) {
        cl.close();
        return;
    }

    egui::TopBottomPanel::bottom("command_palette")
        .frame(
            egui::Frame::new()
                .fill(theme::BG_DEEP)
                .stroke(egui::Stroke::new(1.0_f32, theme::BORDER_HI))
                .inner_margin(egui::Margin::symmetric(10, 6)),
        )
        .resizable(true)
        .default_height(220.0)
        .min_height(80.0)
        .show(ctx, |ui| {
            input_row(ui, cl, cmd_tx);
            suggestions(ui, cl);
            ui.separator();
            output(ui, cl);
        });
}

fn input_row(ui: &mut Ui, cl: &mut CommandLine, cmd_tx: &CommandSender) {
    // Consume navigation keys before the text field sees them.
    let (up, down, tab) = ui.input_mut(|i| {
        (
            i.consume_key(Modifiers::NONE, Key::ArrowUp),
            i.consume_key(Modifiers::NONE, Key::ArrowDown),
            i.consume_key(Modifiers::NONE, Key::Tab),
        )
    });
    if up {
        cl.history_up();
    }
    if down {
        cl.history_down();
    }
    if tab && let Some((name, _)) = cl.suggestions().first() {
        cl.input = name.to_string();
    }

    ui.horizontal(|ui| {
        ui.label(RichText::new(":").color(theme::ACCENT_BRIGHT));
        let edit = TextEdit::singleline(&mut cl.input)
            .id_salt("command_input")
            .hint_text("command — Tab to complete, ↑/↓ history, Esc to close")
            .desired_width(f32::INFINITY);
        let response = ui.add(edit);

        if response.lost_focus()
            && ui.input(|i| i.key_pressed(Key::Enter))
            && let Some(cmd) = cl.submit()
        {
            let _ = cmd_tx.send(UiCommand::RunCommandLine(cmd));
        }
        // Keep focus while the palette is open (Enter drops it).
        if !response.has_focus() {
            response.request_focus();
        }
        if up || down || tab {
            move_cursor_to_end(ui.ctx(), response.id, &cl.input);
        }
    });
}

fn move_cursor_to_end(ctx: &egui::Context, id: egui::Id, text: &str) {
    if let Some(mut state) = egui::TextEdit::load_state(ctx, id) {
        let end = egui::text::CCursor::new(text.chars().count());
        state
            .cursor
            .set_char_range(Some(egui::text::CCursorRange::one(end)));
        state.store(ctx, id);
    }
}

fn suggestions(ui: &mut Ui, cl: &mut CommandLine) {
    let typed = cl.input.trim();
    if typed.is_empty() {
        return;
    }
    // Once a method with arguments is typed, show its usage instead of suggestions.
    let first = typed.split_whitespace().next().unwrap_or_default();
    if let Some(m) = methods::find(first)
        && !m.params.is_empty()
    {
        ui.label(RichText::new(format!("usage: {}", m.usage())).weak());
        return;
    }
    let matches = cl.suggestions();
    if matches.iter().any(|(name, _)| *name == typed) {
        return;
    }
    ui.horizontal_wrapped(|ui| {
        for (name, desc) in matches.iter().take(MAX_SUGGESTIONS) {
            if ui.small_button(*name).on_hover_text(*desc).clicked() {
                cl.input = name.to_string();
            }
        }
        if matches.len() > MAX_SUGGESTIONS {
            ui.label(RichText::new(format!("+{} more", matches.len() - MAX_SUGGESTIONS)).weak());
        }
    });
}

fn output(ui: &mut Ui, cl: &CommandLine) {
    if cl.output.is_empty() {
        ui.label(RichText::new("Type `help` to list commands.").weak());
        return;
    }
    egui::ScrollArea::vertical()
        .auto_shrink(false)
        .stick_to_bottom(true)
        .show(ui, |ui| {
            for entry in &cl.output {
                ui.label(RichText::new(format!("> {}", entry.command)).color(theme::ACCENT));
                let color = if entry.is_error {
                    theme::ERROR
                } else {
                    ui.visuals().text_color()
                };
                ui.label(RichText::new(&entry.result).color(color));
                ui.add_space(6.0);
            }
        });
}
