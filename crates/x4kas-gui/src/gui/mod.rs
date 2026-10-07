//! egui/eframe desktop frontend.

mod analytics;
mod blockdag;
mod connection;
mod dashboard;
mod help;
mod mempool;
mod rpc_explorer;
mod terminal;
mod theme;
mod widgets;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use eframe::egui::{self, Button, Event, Key, Modifiers, RichText, Stroke, ViewportCommand};
use tokio::sync::{RwLock, oneshot};

use crate::Args;
use connection::ConnectionWindow;
use terminal::TerminalPane;
use widgets::kv;
use x4kas_core::analytics_streaming;
use x4kas_core::app::{ActiveConnection, AnalyticsPhase, App, ConnectionStatus, StartPoint, Tab};
use x4kas_core::config::{ConnectionKind, ConnectionSettings};
use x4kas_core::controller::{self, CommandSender, ControllerArgs, RemoteTarget, UiCommand};
use x4kas_core::format::{format_duration, format_number, now_ms};
use x4kas_core::rpc::market;

/// Start background tasks on `rt` and run the GUI on the current (main) thread.
pub fn run(rt: &tokio::runtime::Runtime, args: Args) -> Result<()> {
    let app = Arc::new(RwLock::new(App::default()));

    // `--url` overrides the saved connection choice; without it, open the connection
    // window so the user can pick one.
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
    let connection = ConnectionWindow::new(settings, remote.is_none());

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
        )
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("x4kas")
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([800.0, 500.0])
            // macOS: hide the title bar and draw under it, so the window buttons sit on
            // the top bar. No effect on other platforms.
            .with_fullsize_content_view(true)
            .with_titlebar_shown(false)
            .with_title_shown(false),
        ..Default::default()
    };

    eframe::run_native(
        "x4kas",
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
    terminal: TerminalPane,
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
            terminal: TerminalPane::new(),
        }
    }

    /// Defer window close until the controller has disconnected and saved state.
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

        handle_shortcuts(ctx, &mut app, &mut self.show_help, &mut self.terminal);
        let testnet = app
            .node
            .server_info
            .as_ref()
            .is_some_and(|s| s.network_id.contains("testnet"));
        widgets::set_testnet(ctx, testnet);

        egui::TopBottomPanel::top("top_bar")
            .frame(bar_frame())
            .exact_height(TITLE_BAR_HEIGHT)
            .show(ctx, |ui| {
                top_bar(ui, &mut app, &mut self.show_help, &mut self.terminal)
            });
        // Added before the terminal so it stays at the very bottom, below it.
        egui::TopBottomPanel::bottom("status_bar")
            .frame(bar_frame())
            .show(ctx, |ui| status_bar(ui, &mut app, &mut self.connection));
        self.terminal.show(ctx);
        if self.terminal.has_focus() {
            // Esc belongs to the shell (vim etc.), not the popups drawn below.
            ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape));
        }

        egui::CentralPanel::default().show(ctx, |ui| match app.active_tab {
            Tab::Dashboard => dashboard::show(ui, &mut app),
            Tab::Mempool => mempool::show(ui, &mut app),
            Tab::RpcExplorer => rpc_explorer::show(ui, &mut app, &self.cmd_tx),
        });

        // A click on any block hash (or a block in the DAG visualizer) opens Block Info here, whatever
        // the tab.
        if let Some(hash) = widgets::take_block_request(ctx) {
            app.dag_selection.request(hash.clone());
            let _ = self.cmd_tx.send(UiCommand::LookupBlock(hash));
        }
        blockdag::block_window(ctx, &mut app);

        self.connection.show(ctx, &mut app, &self.cmd_tx);
        help::show(ctx, &mut self.show_help);

        if self.is_shutting_down() {
            egui::Modal::new(egui::Id::new("shutdown_modal")).show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Shutting down…");
                });
            });
        }

        // Keep time-based values (seconds behind the tip, "ago" times) ticking. Wake on the
        // next whole second rather than 1s from now, so they tick evenly even when
        // data updates trigger frames at arbitrary times.
        let to_next_second = 1.0 - ctx.input(|i| i.time).fract();
        ctx.request_repaint_after(Duration::from_secs_f64(to_next_second));
    }
}

fn handle_shortcuts(
    ctx: &egui::Context,
    app: &mut App,
    show_help: &mut bool,
    terminal: &mut TerminalPane,
) {
    const TAB_KEYS: [Key; 3] = [Key::Num1, Key::Num2, Key::Num3];

    // Works even while the terminal or a text field has focus.
    if ctx.input_mut(|i| i.consume_key(Modifiers::CTRL, Key::Backtick)) {
        terminal.toggle();
    }
    if terminal.has_focus() || ctx.wants_keyboard_input() {
        return;
    }

    let question = ctx.input_mut(|i| take_text_event(i, "?"));
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

fn bar_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(theme::SURFACE)
        .stroke(Stroke::new(1.0_f32, theme::BORDER))
        .inner_margin(egui::Margin::symmetric(10, 4))
}

/// Height of the macOS title bar, which the top bar replaces; the window buttons are
/// vertically centered in it.
const TITLE_BAR_HEIGHT: f32 = 28.0;
/// Room left of the brand for the macOS close/minimize/zoom buttons.
const TRAFFIC_LIGHTS_WIDTH: f32 = 76.0;

/// With the native title bar hidden, dragging the top bar's background moves the
/// window and double-clicking it zooms. Widgets added afterwards take precedence.
fn title_bar_drag(ui: &mut egui::Ui) {
    let response = ui.interact(
        ui.max_rect(),
        egui::Id::new("title_bar_drag"),
        egui::Sense::click_and_drag(),
    );
    if response.drag_started_by(egui::PointerButton::Primary) {
        ui.ctx().send_viewport_cmd(ViewportCommand::StartDrag);
    }
    if response.double_clicked() {
        let maximized = ui.input(|i| i.viewport().maximized.unwrap_or(false));
        ui.ctx()
            .send_viewport_cmd(ViewportCommand::Maximized(!maximized));
    }
}

/// Brand prompt, tmux-style tab strip, and the terminal/help buttons.
fn top_bar(ui: &mut egui::Ui, app: &mut App, show_help: &mut bool, terminal: &mut TerminalPane) {
    title_bar_drag(ui);
    ui.horizontal_centered(|ui| {
        if cfg!(target_os = "macos") {
            ui.add_space(TRAFFIC_LIGHTS_WIDTH);
        }
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
                .selectable_label(terminal.open, ">_")
                .on_hover_text("Terminal (Ctrl+`)")
                .clicked()
            {
                terminal.toggle();
            }
        });
    });
}

/// `x4kas`, with the 4 accented.
fn brand(ui: &mut egui::Ui) {
    ui.spacing_mut().item_spacing.x = 0.0;
    ui.label(RichText::new("x").color(theme::TEXT_BRIGHT).size(15.0));
    ui.label(RichText::new("4").color(theme::ACCENT_BRIGHT).size(15.0));
    ui.label(RichText::new("kas").color(theme::TEXT_BRIGHT).size(15.0));
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

/// One decimal under 10s (a healthy node is usually under a second behind), whole seconds above.
fn format_seconds(secs: f64) -> String {
    if secs < 10.0 {
        format!("{secs:.1}")
    } else {
        format!("{secs:.0}")
    }
}

/// Node sync indicator; details (version, block counts, last poll) on hover.
fn node_chip(ui: &mut egui::Ui, app: &App) {
    if !matches!(app.node.connection_status, ConnectionStatus::Connected) {
        return;
    }
    let node = &app.node;
    let (text, color) = match node.server_info {
        None => ("◌ Node", theme::TEXT_DIM),
        Some(ref info) if info.is_synced => ("● Node synced", theme::OK),
        Some(_) => ("◐ Node syncing", theme::WARN),
    };
    widgets::divider(ui);
    widgets::status_chip(ui, "node_status", text, color, |ui| {
        if let Some(ref info) = node.server_info {
            kv(ui, "Version", &info.server_version);
            kv(ui, "Synced", widgets::yes_no(info.is_synced));
            kv(ui, "UTXO index", widgets::yes_no(info.has_utxo_index));
            kv(ui, "DAA score", format_number(info.virtual_daa_score));
        }
        if let Some(ref dag) = node.dag_info {
            kv(
                ui,
                "Blocks / headers",
                format!(
                    "{} / {}",
                    format_number(dag.block_count),
                    format_number(dag.header_count)
                ),
            );
        }
        if let Some(secs) = app.seconds_behind_tip(now_ms()) {
            kv(ui, "Behind tip", format!("{}s", format_seconds(secs)));
        }
        if let Some(at) = node.last_refresh {
            let took = node
                .last_poll_duration_ms
                .map(|ms| format!(", took {ms:.0} ms"))
                .unwrap_or_default();
            kv(
                ui,
                "Last poll",
                format!("{} ago{took}", format_duration(at.elapsed())),
            );
        }
        if let Some(ref err) = node.last_error {
            kv(ui, "Error", RichText::new(err).color(theme::ERROR));
        }
    });
}

/// Analytics task indicator; details (start point, progress, speed) on hover.
fn analytics_chip(ui: &mut egui::Ui, app: &App) {
    match app.connection {
        ActiveConnection::None => return,
        ActiveConnection::Resolver => {
            widgets::divider(ui);
            widgets::status_chip(
                ui,
                "analytics_status",
                "○ Analytics n/a",
                theme::TEXT_DIM,
                |ui| kv(ui, "Status", "Needs a direct node (URL), not the resolver"),
            );
            return;
        }
        ActiveConnection::Url(_) => {}
    }

    let status = &app.analytics.status;
    let tip = app.node.server_info.as_ref().map(|s| s.virtual_daa_score);
    let fraction = tip.and_then(|tip| status.fraction(tip));
    let (text, color, summary) = match status.phase {
        AnalyticsPhase::Idle => return,
        _ if app.paused => ("⏸ Analytics paused".into(), theme::TEXT_DIM, "Paused"),
        AnalyticsPhase::LoadingCache => (
            "◌ Analytics loading".into(),
            theme::TEXT_DIM,
            "Loading the saved cache",
        ),
        AnalyticsPhase::WaitingForNode => (
            "◌ Analytics waiting".into(),
            theme::TEXT_DIM,
            "Waiting for the node to connect and sync",
        ),
        AnalyticsPhase::Seeking => (
            "◐ Analytics seeking".into(),
            theme::WARN,
            "Skipping to the last 24 hours",
        ),
        AnalyticsPhase::CatchingUp => (
            match fraction {
                Some(f) => format!("◐ Analytics {:.0}%", f * 100.0),
                None => "◐ Analytics".into(),
            },
            theme::WARN,
            "Catching up to the DAG tip",
        ),
        AnalyticsPhase::Live => ("● Analytics synced".into(), theme::OK, "Up to date"),
        AnalyticsPhase::Error(_) => (
            "× Analytics error".into(),
            theme::ERROR,
            "Request failed, retrying",
        ),
    };

    widgets::divider(ui);
    widgets::status_chip(ui, "analytics_status", &text, color, |ui| {
        // Distance to the tip once known, otherwise what the task is doing.
        match tip.and_then(|tip| status.behind(tip)) {
            Some((daa, time)) => kv(
                ui,
                "Status",
                format!(
                    "{} DAA ({} seconds) behind DAG Tip",
                    format_number(daa),
                    format_number(time.as_secs())
                ),
            ),
            None => kv(ui, "Status", summary),
        }
        let frequency = if app.paused {
            Some("Paused".to_string())
        } else {
            analytics_streaming::poll_interval(&status.phase).map(|d| {
                if d < Duration::from_secs(1) {
                    format!("Every {} ms", d.as_millis())
                } else {
                    format!("Every {}", format_duration(d))
                }
            })
        };
        if let Some(frequency) = frequency {
            kv(ui, "Poll frequency", frequency);
        }
        if let AnalyticsPhase::Error(ref err) = status.phase {
            kv(ui, "Error", RichText::new(err).color(theme::ERROR));
        }
        match status.started_from {
            Some(StartPoint::Cache(Some(saved))) => {
                let age = saved.elapsed().unwrap_or_default();
                kv(
                    ui,
                    "Started from",
                    format!("cache (saved {} ago)", format_duration(age)),
                );
            }
            Some(StartPoint::Cache(None)) => kv(ui, "Started from", "cache"),
            Some(StartPoint::PruningPoint) => kv(ui, "Started from", "pruning point"),
            Some(StartPoint::LastDay) => kv(ui, "Started from", "24 hours ago"),
            None => {}
        }
        if status.phase == AnalyticsPhase::CatchingUp {
            if let Some(rate) = status.daa_per_sec {
                kv(ui, "Speed", format!("{} DAA/s", format_number(rate as u64)));
            }
            if let Some(eta) = tip.and_then(|tip| status.eta(tip)) {
                kv(ui, "Time left", format!("~{}", format_duration(eta)));
            }
        }
        kv(ui, "Chain blocks", format_number(status.blocks_processed));
    });
}

/// Hover text for the connection button: the node URL, then what a click does.
fn connection_tooltip(app: &App) -> String {
    // The resolver's node URL is only known once connected.
    let url = app.node.node_url.as_deref().or(match app.connection {
        ActiveConnection::Url(ref url) => Some(url.as_str()),
        _ => None,
    });
    match url {
        Some(url) => format!("{url}\nClick to change connection"),
        None => "Click to change connection".to_string(),
    }
}

/// Status bar text, e.g. "Connected to node".
fn connection_summary(app: &App) -> String {
    // The URL is in the hover text, so the label stays short.
    let target = match app.connection {
        ActiveConnection::Url(_) => "node",
        ActiveConnection::Resolver => "public resolver",
        ActiveConnection::None => return "Not connected".to_string(),
    };
    match app.node.connection_status {
        ConnectionStatus::Connected => format!("Connected to {target}"),
        ConnectionStatus::Connecting => format!("Connecting to {target}…"),
        ConnectionStatus::Disconnected => format!("Disconnected from {target}"),
        ConnectionStatus::Error(_) => format!("Error connecting to {target}"),
    }
}

/// Connection (click to change), network, DAA score, poll latency and pause.
fn status_bar(ui: &mut egui::Ui, app: &mut App, connection: &mut ConnectionWindow) {
    ui.horizontal(|ui| {
        let (_, color) = theme::connection_status(&app.node.connection_status);
        if ui
            .selectable_label(
                connection.open,
                RichText::new(format!("● {}", connection_summary(app))).color(color),
            )
            .on_hover_text(connection_tooltip(app))
            .clicked()
        {
            connection.toggle();
        }
        node_chip(ui, app);
        analytics_chip(ui, app);

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
            }
            if let Some(ref info) = app.node.server_info {
                widgets::divider(ui);
                // Colored by how far the node lags the DAG tip.
                let behind = app.seconds_behind_tip(now_ms());
                let color = match behind {
                    Some(s) if s > 30.0 => theme::ERROR,
                    Some(s) if s > 10.0 => theme::WARN,
                    _ => theme::TEXT,
                };
                let daa =
                    ui.label(RichText::new(format_number(info.virtual_daa_score)).color(color));
                if let Some(secs) = behind {
                    daa.on_hover_text(format!("{} seconds behind DAG sink", format_seconds(secs)));
                }
                widgets::field_label(ui, "daa");
                widgets::divider(ui);
                ui.label(RichText::new(&info.network_id).color(theme::ACCENT));
            }
        });
    });
}
