//! egui/eframe desktop frontend.

mod analytics;
mod blockdag;
mod command;
mod connection;
mod dashboard;
mod help;
mod mempool;
mod mining;
mod node;
mod rpc_explorer;
mod theme;
mod widgets;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use eframe::egui::{self, Button, Event, Key, Modifiers, RichText, Stroke, ViewportCommand};
use tokio::sync::{RwLock, oneshot};

use crate::app::{App, DaemonStatus, Tab};
use crate::cli::CliArgs;
use crate::config::{ConnectionKind, ConnectionSettings, DaemonConfig};
use crate::controller::{self, CommandSender, ControllerArgs, RemoteTarget, UiCommand};
use crate::rpc::market;
use crate::rpc::types::format_number;
use connection::ConnectionWindow;

/// Start background tasks on `rt` and run the GUI on the current (main) thread.
pub fn run(rt: &tokio::runtime::Runtime, args: CliArgs, daemon_config: DaemonConfig) -> Result<()> {
    let app = Arc::new(RwLock::new(App::new(daemon_config.clone())));

    // `--url` overrides the saved connection choice; with neither a URL nor an
    // auto-started node, open the connection window so the user can pick one.
    let mut settings = ConnectionSettings::load().unwrap_or_default();
    let remote = args.url.clone().map(|url| {
        settings.kind = ConnectionKind::Url;
        settings.url = url.clone();
        settings.network = args.network.clone();
        RemoteTarget {
            url: Some(url),
            network: args.network.clone(),
        }
    });
    let connection = ConnectionWindow::new(
        settings,
        remote.is_none() && !daemon_config.auto_start_daemon,
    );

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
                remote,
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
            theme::apply(&cc.egui_ctx);
            let ctx = cc.egui_ctx.clone();
            app.blocking_write().repaint = Some(Arc::new(move || ctx.request_repaint()));
            Ok(Box::new(GuiApp::new(app, cmd_tx, connection)))
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
    connection: ConnectionWindow,
}

impl GuiApp {
    fn new(app: Arc<RwLock<App>>, cmd_tx: CommandSender, connection: ConnectionWindow) -> Self {
        Self {
            app,
            cmd_tx,
            shutdown_rx: None,
            shutdown_complete: false,
            show_help: false,
            connection,
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
            .frame(bar_frame())
            .show(ctx, |ui| top_bar(ui, &mut app, &mut self.show_help));
        // Added before the palette so it stays at the very bottom, below it.
        egui::TopBottomPanel::bottom("status_bar")
            .frame(bar_frame())
            .show(ctx, |ui| status_bar(ui, &mut app, &mut self.connection));
        command::show(ctx, &mut app.command_line, &self.cmd_tx);

        egui::CentralPanel::default().show(ctx, |ui| match app.active_tab {
            Tab::Dashboard => dashboard::show(ui, &app),
            Tab::Mempool => mempool::show(ui, &mut app),
            Tab::Mining => mining::show(ui, &app),
            Tab::RpcExplorer => rpc_explorer::show(ui, &mut app, &self.cmd_tx),
            Tab::IntegratedNode => node::show(ui, &mut app, &self.cmd_tx),
            Tab::Analytics => analytics::show(ui, &mut app),
            Tab::BlockDag => blockdag::show(ui, &mut app, &self.cmd_tx),
        });

        self.connection.show(ctx, &mut app, &self.cmd_tx);
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
    const TAB_KEYS: [Key; 7] = [
        Key::Num1,
        Key::Num2,
        Key::Num3,
        Key::Num4,
        Key::Num5,
        Key::Num6,
        Key::Num7,
    ];

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

fn bar_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(theme::SURFACE)
        .stroke(Stroke::new(1.0_f32, theme::BORDER))
        .inner_margin(egui::Margin::symmetric(10, 4))
}

/// Brand prompt, tmux-style tab strip, and the palette/help buttons.
fn top_bar(ui: &mut egui::Ui, app: &mut App, show_help: &mut bool) {
    ui.horizontal(|ui| {
        brand(ui);
        ui.add_space(12.0);

        ui.spacing_mut().item_spacing.x = 2.0;
        for (i, tab) in Tab::all().iter().enumerate() {
            if tab_button(ui, i + 1, tab.label(), app.active_tab == *tab)
                .on_hover_text(format!("Shortcut: {}", i + 1))
                .clicked()
            {
                app.active_tab = *tab;
            }
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            if ui.button("?").on_hover_text("Help (? / F1)").clicked() {
                *show_help = !*show_help;
            }
            if ui
                .selectable_label(app.command_line.active, ">_")
                .on_hover_text("Command palette (: or ⌘K / Ctrl+K)")
                .clicked()
            {
                toggle_palette(app);
            }
        });
    });
}

/// `$ tui4kas█` with a cursor that blinks on the 1s repaint tick.
fn brand(ui: &mut egui::Ui) {
    let cursor_on = (ui.input(|i| i.time) as u64).is_multiple_of(2);
    let cursor = if cursor_on {
        theme::ACCENT_BRIGHT
    } else {
        egui::Color32::TRANSPARENT
    };
    ui.spacing_mut().item_spacing.x = 0.0;
    ui.label(RichText::new("$ ").color(theme::TEXT_DIM).size(15.0));
    ui.label(RichText::new("tui").color(theme::TEXT_BRIGHT).size(15.0));
    ui.label(RichText::new("4").color(theme::ACCENT_BRIGHT).size(15.0));
    ui.label(RichText::new("kas").color(theme::TEXT_BRIGHT).size(15.0));
    ui.label(RichText::new("█").color(cursor).size(15.0));
}

/// A tab in the strip: the shortcut number, then the name. The active tab is inverted.
fn tab_button(ui: &mut egui::Ui, number: usize, label: &str, selected: bool) -> egui::Response {
    let (num_color, text_color, fill) = if selected {
        (theme::BG_DEEP, theme::BG_DEEP, theme::ACCENT)
    } else {
        (theme::ACCENT, theme::TEXT, egui::Color32::TRANSPARENT)
    };
    ui.add(
        Button::new((
            RichText::new(number.to_string()).color(num_color),
            RichText::new(label).color(text_color),
        ))
        .fill(fill)
        .stroke(Stroke::NONE)
        .frame_when_inactive(selected),
    )
}

/// Connection (click to change), embedded node, network, DAA score, poll latency and pause.
fn status_bar(ui: &mut egui::Ui, app: &mut App, connection: &mut ConnectionWindow) {
    ui.horizontal(|ui| {
        let (text, color) = theme::connection_status(&app.node.connection_status);
        if ui
            .selectable_label(
                connection.open,
                RichText::new(format!("● {text} · {}", app.connection.label())).color(color),
            )
            .on_hover_text("Change connection")
            .clicked()
        {
            connection.toggle();
        }

        if app.integrated_node.status != DaemonStatus::Stopped {
            widgets::divider(ui);
            let (text, color) = theme::daemon_status(&app.integrated_node.status);
            widgets::field_label(ui, "node");
            ui.label(RichText::new(text).color(color));
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let pause_label = if app.paused {
                "▶ Resume"
            } else {
                "⏸ Pause"
            };
            if ui
                .selectable_label(app.paused, pause_label)
                .on_hover_text("Pause / resume polling (P)")
                .clicked()
            {
                app.paused = !app.paused;
            }

            if app.paused {
                ui.label(RichText::new("PAUSED").color(theme::WARN));
            } else if let Some(ms) = app.node.last_poll_duration_ms {
                ui.label(format!("{ms:.0} ms"));
                widgets::field_label(ui, "poll");
            }
            if let Some(ref info) = app.node.server_info {
                widgets::divider(ui);
                ui.label(format_number(info.virtual_daa_score));
                widgets::field_label(ui, "daa");
                widgets::divider(ui);
                ui.label(RichText::new(&info.network_id).color(theme::ACCENT));
            }
        });
    });
}
