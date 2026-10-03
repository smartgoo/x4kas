//! Integrated terminal: the user's login shell in a resizable bottom panel, rendered by
//! the vendored `egui_term` widget (alacritty_terminal on a real PTY).
//!
//! The shell starts the first time the panel opens and keeps running while it is
//! hidden. When it exits (`exit`, Ctrl+D) the panel closes and the next open starts a
//! fresh one.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};

use eframe::egui::{self, FontId, RichText};
use egui_term::{
    BackendSettings, FontSettings, PtyEvent, TerminalBackend, TerminalFont, TerminalTheme,
    TerminalView,
};

use super::theme;

pub struct TerminalPane {
    pub open: bool,
    /// Whether keystrokes go to the shell. Set by clicking into the terminal (or opening
    /// it), cleared by clicking anywhere else.
    focused: bool,
    session: Option<Session>,
    /// Why the shell failed to start, shown in the panel.
    error: Option<String>,
    /// Window title set by the shell (OSC 0/2), shown in the panel header.
    title: Option<String>,
    next_id: u64,
}

/// A running shell. The backend is declared first so it shuts the PTY down before the
/// event channel closes.
struct Session {
    backend: TerminalBackend,
    events: Receiver<(u64, PtyEvent)>,
}

impl TerminalPane {
    pub fn new() -> Self {
        Self {
            open: false,
            focused: false,
            session: None,
            error: None,
            title: None,
            next_id: 0,
        }
    }

    pub fn toggle(&mut self) {
        self.open = !self.open;
        self.focused = self.open;
    }

    /// Whether the shell has keyboard focus (app shortcuts are off meanwhile).
    pub fn has_focus(&self) -> bool {
        self.open && self.focused
    }

    pub fn show(&mut self, ctx: &egui::Context) {
        self.handle_events();
        if !self.open {
            return;
        }
        if self.session.is_none() && self.error.is_none() {
            self.start(ctx);
        }

        egui::TopBottomPanel::bottom("terminal")
            .frame(
                egui::Frame::new()
                    .fill(theme::BG_DEEP)
                    .stroke(egui::Stroke::new(1.0_f32, theme::BORDER_HI))
                    .inner_margin(egui::Margin::symmetric(8, 4)),
            )
            .resizable(true)
            .default_height(280.0)
            .min_height(100.0)
            .show(ctx, |ui| {
                self.header(ui);
                self.body(ui);
            });
    }

    fn header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("terminal").color(theme::ACCENT));
            if let Some(ref title) = self.title {
                ui.label(RichText::new(title).weak());
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .small_button("×")
                    .on_hover_text("Hide (Ctrl+`)")
                    .clicked()
                {
                    self.toggle();
                }
                if self.error.is_some() && ui.small_button("Retry").clicked() {
                    self.error = None;
                }
            });
        });
        ui.add_space(2.0);
    }

    fn body(&mut self, ui: &mut egui::Ui) {
        if let Some(ref err) = self.error {
            ui.label(RichText::new(format!("Failed to start shell: {err}")).color(theme::ERROR));
            return;
        }
        let Some(session) = self.session.as_mut() else {
            return;
        };

        let view = TerminalView::new(ui, &mut session.backend)
            .set_theme(TerminalTheme::new(Box::new(theme::terminal_palette())))
            .set_font(TerminalFont::new(FontSettings {
                font_type: FontId::monospace(theme::FONT_SIZE + 1.0),
            }))
            .set_focus(self.focused)
            .set_size(ui.available_size());
        let response = ui.add(view);

        // Focus follows clicks: into the terminal to type, anywhere else to leave it.
        let pressed_at = ui.input(|i| {
            i.pointer
                .any_pressed()
                .then(|| i.pointer.interact_pos())
                .flatten()
        });
        if let Some(pos) = pressed_at {
            self.focused = response.rect.contains(pos);
        }
    }

    fn start(&mut self, ctx: &egui::Context) {
        let (shell, args) = shell();
        let (tx, rx) = mpsc::channel();
        let id = self.next_id;
        self.next_id += 1;
        match TerminalBackend::new(
            id,
            ctx.clone(),
            tx,
            BackendSettings {
                shell,
                args,
                working_directory: dirs::home_dir(),
            },
        ) {
            Ok(backend) => {
                self.session = Some(Session {
                    backend,
                    events: rx,
                });
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    fn handle_events(&mut self) {
        let Some(ref session) = self.session else {
            return;
        };
        let mut exited = false;
        while let Ok((_, event)) = session.events.try_recv() {
            match event {
                PtyEvent::Exit => exited = true,
                PtyEvent::Title(title) => self.title = Some(title),
                PtyEvent::ResetTitle => self.title = None,
                _ => {}
            }
        }
        if exited {
            self.session = None;
            self.title = None;
            self.open = false;
            self.focused = false;
        }
    }
}

/// The user's shell, as a login shell so it picks up the same environment (PATH etc.)
/// as a new terminal window, even when the app was launched from the Finder.
fn shell() -> (String, Vec<String>) {
    if cfg!(windows) {
        let shell = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into());
        return (shell, vec![]);
    }
    let shell = std::env::var("SHELL")
        .ok()
        .filter(|s| !s.is_empty() && PathBuf::from(s).exists())
        .unwrap_or_else(|| {
            if cfg!(target_os = "macos") {
                "/bin/zsh".into()
            } else {
                "/bin/sh".into()
            }
        });
    (shell, vec!["-l".into()])
}
