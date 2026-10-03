//! egui/eframe desktop frontend.

mod analytics;
mod blockdag;
mod command;
mod dashboard;
mod help;
mod mempool;
mod node;
mod rpc_explorer;
mod theme;
mod widgets;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use eframe::egui::{self, Event, Key, Modifiers, RichText, ViewportCommand};
use tokio::sync::{RwLock, oneshot};

use crate::app::{App, DaemonStatus, Tab};
use crate::cli::CliArgs;
use crate::config::DaemonConfig;
use crate::controller::{self, CommandSender, ControllerArgs, UiCommand};
use crate::rpc::market;

/// Start background tasks on `rt` and run the GUI on the current (main) thread.
pub fn run(rt: &tokio::runtime::Runtime, args: CliArgs, daemon_config: DaemonConfig) -> Result<()> {
    let app = Arc::new(RwLock::new(App::new(daemon_config.clone())));

    // Background tasks use `tokio::spawn`, so spawn them inside the runtime context.
    // The guard is dropped before the GUI starts: blocking lock calls from the main
    // thread panic inside a runtime context.
    let cmd_tx = {
        let _guard = rt.enter();
        market::start_market_polling(app.clone(), Duration::from_secs(60));
        controller::spawn(
            rt.handle(),
            app.clone(),
            ControllerArgs {
                url: args.url.clone(),
                network: args.network.clone(),
                refresh_interval_ms: args.refresh_interval_ms,
            },
            daemon_config,
        )
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("tui4kas")
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([800.0, 500.0]),
        ..Default::default()
    };

    eframe::run_native(
        "tui4kas",
        options,
        Box::new(move |cc| {
            let ctx = cc.egui_ctx.clone();
            app.blocking_write().repaint = Some(Arc::new(move || ctx.request_repaint()));
            Ok(Box::new(GuiApp::new(app, cmd_tx)))
        }),
    )
    .map_err(|e| anyhow::anyhow!("GUI error: {e}"))
}

struct GuiApp {
    app: Arc<RwLock<App>>,
    cmd_tx: CommandSender,
    /// Set once a close was requested; resolves when the controller has shut down.
    shutdown_rx: Option<oneshot::Receiver<()>>,
    shutdown_complete: bool,
    show_help: bool,
}

impl GuiApp {
    fn new(app: Arc<RwLock<App>>, cmd_tx: CommandSender) -> Self {
        Self {
            app,
            cmd_tx,
            shutdown_rx: None,
            shutdown_complete: false,
            show_help: false,
        }
    }

    /// Defer window close until the controller has stopped the node and saved state.
    fn handle_close(&mut self, ctx: &egui::Context) {
        if ctx.input(|i| i.viewport().close_requested()) && !self.shutdown_complete {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            if self.shutdown_rx.is_none() {
                let (tx, rx) = oneshot::channel();
                let _ = self.cmd_tx.send(UiCommand::Shutdown(tx));
                self.shutdown_rx = Some(rx);
            }
        }

        if let Some(rx) = &mut self.shutdown_rx {
            match rx.try_recv() {
                Err(oneshot::error::TryRecvError::Empty) => {
                    ctx.request_repaint_after(Duration::from_millis(100));
                }
                // Done, or the controller is gone — either way it's safe to close.
                _ => {
                    self.shutdown_rx = None;
                    self.shutdown_complete = true;
                    ctx.send_viewport_cmd(ViewportCommand::Close);
                }
            }
        }
    }

    fn is_shutting_down(&self) -> bool {
        self.shutdown_rx.is_some() || self.shutdown_complete
    }
}

impl eframe::App for GuiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_close(ctx);

        let app_state = self.app.clone();
        let mut app = app_state.blocking_write();

        handle_shortcuts(ctx, &mut app, &mut self.show_help);

        egui::TopBottomPanel::top("top_bar")
            .show(ctx, |ui| top_bar(ui, &mut app, &mut self.show_help));
        command::show(ctx, &mut app.command_line, &self.cmd_tx);

        egui::CentralPanel::default().show(ctx, |ui| match app.active_tab {
            Tab::Dashboard => dashboard::show(ui, &app),
            Tab::Mempool => mempool::show(ui, &mut app),
            Tab::RpcExplorer => rpc_explorer::show(ui, &mut app, &self.cmd_tx),
            Tab::IntegratedNode => node::show(ui, &mut app, &self.cmd_tx),
            Tab::Analytics => analytics::show(ui, &mut app),
            Tab::BlockDag => blockdag::show(ui, &mut app, &self.cmd_tx),
        });

        help::show(ctx, &mut self.show_help);

        if self.is_shutting_down() {
            egui::Modal::new(egui::Id::new("shutdown_modal")).show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.spinner();
                    let msg = if app.integrated_node.status == DaemonStatus::Stopping {
                        "Stopping node…"
                    } else {
                        "Shutting down…"
                    };
                    ui.label(msg);
                });
            });
        }

        // Keep time-based values (uptime, "last refresh") ticking.
        ctx.request_repaint_after(Duration::from_secs(1));
    }
}

fn handle_shortcuts(ctx: &egui::Context, app: &mut App, show_help: &mut bool) {
    const TAB_KEYS: [Key; 6] = [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6];

    // Works even while the palette input has focus.
    if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::K)) {
        toggle_palette(app);
    }
    if ctx.wants_keyboard_input() {
        return;
    }

    // Consume the typed character so it doesn't land in the palette input.
    let (colon, question) = ctx.input_mut(|i| {
        let colon = take_text_event(i, ":");
        let question = take_text_event(i, "?");
        (colon, question)
    });
    if colon && !app.command_line.active {
        app.command_line.open();
    }
    if question || ctx.input(|i| i.key_pressed(Key::F1)) {
        *show_help = !*show_help;
    }

    ctx.input(|i| {
        for (key, tab) in TAB_KEYS.iter().zip(Tab::all()) {
            if i.key_pressed(*key) {
                app.active_tab = *tab;
            }
        }
        if i.modifiers.ctrl && i.key_pressed(Key::Tab) {
            if i.modifiers.shift {
                app.prev_tab();
            } else {
                app.next_tab();
            }
        }
        if i.key_pressed(Key::P) {
            app.paused = !app.paused;
        }
    });
}

fn take_text_event(input: &mut egui::InputState, text: &str) -> bool {
    let before = input.events.len();
    input
        .events
        .retain(|e| !matches!(e, Event::Text(t) if t == text));
    input.events.len() != before
}

fn toggle_palette(app: &mut App) {
    if app.command_line.active {
        app.command_line.close();
    } else {
        app.command_line.open();
    }
}

fn top_bar(ui: &mut egui::Ui, app: &mut App, show_help: &mut bool) {
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new("tui4kas").strong().color(theme::ACCENT).size(16.0));
        ui.separator();

        for (i, tab) in Tab::all().iter().enumerate() {
            let selected = app.active_tab == *tab;
            if ui
                .selectable_label(selected, tab.label())
                .on_hover_text(format!("Shortcut: {}", i + 1))
                .clicked()
            {
                app.active_tab = *tab;
            }
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("?").on_hover_text("Help (?)").clicked() {
                *show_help = !*show_help;
            }
            if ui
                .selectable_label(app.command_line.active, ">_")
                .on_hover_text("Command palette (: or ⌘K / Ctrl+K)")
                .clicked()
            {
                toggle_palette(app);
            }
            let pause_label = if app.paused { "▶ Resume" } else { "⏸ Pause" };
            if ui.button(pause_label).on_hover_text("Shortcut: P").clicked() {
                app.paused = !app.paused;
            }

            if app.paused {
                ui.label(RichText::new("Paused").color(theme::WARN));
            } else if let Some(ms) = app.node.last_poll_duration_ms {
                ui.label(RichText::new(format!("{ms:.0} ms")).weak());
            }

            if let Some(ref info) = app.node.server_info {
                ui.label(RichText::new(&info.network_id).weak());
            }

            if app.integrated_node.status != DaemonStatus::Stopped {
                let (text, color) = theme::daemon_status(&app.integrated_node.status);
                ui.label(RichText::new(format!("Node: {text}")).color(color));
                ui.separator();
            }

            let (text, color) = theme::connection_status(&app.node.connection_status);
            ui.label(RichText::new(text).color(color));
            ui.label(RichText::new("●").color(color));
        });
    });
    ui.add_space(4.0);
}
