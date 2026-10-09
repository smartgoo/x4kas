//! egui/eframe desktop frontend.

mod address;
mod analytics;
mod blockdag;
mod connection;
mod dashboard;
mod explorer;
mod flows;
mod help;
mod mempool;
mod monitoring;
mod pane;
mod rpc_explorer;
mod settings;
mod terminal;
mod theme;
mod toasts;
mod widgets;

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use eframe::egui::{self, Event, Key, Modifiers, RichText, Stroke, ViewportCommand};
use tokio::sync::{RwLock, oneshot};

use crate::Args;
use connection::ConnectionWindow;
use explorer::ExplorerUi;
use flows::FlowWindowUi;
use monitoring::MonitoringTab;
use pane::InfoPane;
use settings::SettingsPage;
use terminal::TerminalPane;
use toasts::Toasts;
use widgets::kv;
use x4kas_core::app::{ActiveConnection, App, ChainPhase, ConnectionStatus, StartPoint, Tab};
use x4kas_core::chain_stream;
use x4kas_core::chain_stream::StreamStart;
use x4kas_core::config::{ConnectionKind, ConnectionSettings};
use x4kas_core::controller::{self, CommandSender, ControllerArgs, RemoteTarget, UiCommand};
use x4kas_core::format::{format_duration, format_number, now_ms};
use x4kas_core::labels;
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
        labels::start_label_refresh(app.clone());
        controller::spawn(
            rt.handle(),
            app.clone(),
            ControllerArgs {
                remote,
                refresh_interval_ms: args.refresh_interval_ms,
                backfill: Some(StreamStart::default_backfill()),
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
            .with_title_shown(false)
            // macOS: a window with a background draws its frame's light highlight line
            // along the top edge, over our top bar. Transparent windows don't; the
            // content stays opaque (`GuiApp::clear_color`).
            .with_transparent(true),
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

/// What this frame's Esc closes: one thing, the topmost. Windows first (in the order
/// they draw, so the one on top), then the info pane, then Settings.
#[derive(Clone, Copy, PartialEq, Eq)]
enum EscTarget {
    Help,
    Connection,
    Flows,
    Pane,
    Settings,
}

/// How long the shutdown waits before offering to quit without finishing.
const SHUTDOWN_PATIENCE: Duration = Duration::from_secs(5);

struct GuiApp {
    app: Arc<RwLock<App>>,
    cmd_tx: CommandSender,
    /// Set once a close was requested; resolves when the controller has shut down.
    shutdown_rx: Option<oneshot::Receiver<()>>,
    shutdown_since: Option<Instant>,
    shutdown_complete: bool,
    /// A text field had focus or a popup (a menu) was open at the end of the last
    /// frame: egui gives this frame's Esc to them (it clears the focus before the frame
    /// runs, so this is the only way to know), and nothing else should take it.
    esc_taken: bool,
    show_help: bool,
    connection: ConnectionWindow,
    terminal: TerminalPane,
    monitoring: MonitoringTab,
    explorer: ExplorerUi,
    settings: SettingsPage,
    pane: InfoPane,
    flow_window: FlowWindowUi,
    toasts: Toasts,
}

impl GuiApp {
    fn new(app: Arc<RwLock<App>>, cmd_tx: CommandSender, connection: ConnectionWindow) -> Self {
        Self {
            app,
            cmd_tx,
            shutdown_rx: None,
            shutdown_since: None,
            shutdown_complete: false,
            esc_taken: false,
            show_help: false,
            connection,
            terminal: TerminalPane::new(),
            monitoring: MonitoringTab::default(),
            explorer: ExplorerUi::default(),
            settings: SettingsPage::default(),
            pane: InfoPane::default(),
            flow_window: FlowWindowUi::default(),
            toasts: Toasts::default(),
        }
    }

    /// Defer window close until the controller has disconnected and saved state.
    /// Cmd+Q (Ctrl+Q elsewhere) asks for the same close, so it takes the same path.
    fn handle_close(&mut self, ctx: &egui::Context) {
        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Q)) {
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
        if ctx.input(|i| i.viewport().close_requested()) && !self.shutdown_complete {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            if self.shutdown_rx.is_none() {
                let (tx, rx) = oneshot::channel();
                let _ = self.cmd_tx.send(UiCommand::Shutdown(tx));
                self.shutdown_rx = Some(rx);
                self.shutdown_since = Some(Instant::now());
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

    /// A window (Help, Connection, the flow graph) is on show.
    fn modal_open(&self, app: &App) -> bool {
        self.show_help || self.connection.open || app.address.flows.open
    }

    /// What this frame's Esc closes, if anything.
    fn esc_target(&self, ctx: &egui::Context, app: &App) -> Option<EscTarget> {
        if self.esc_taken || !ctx.input(|i| i.key_pressed(Key::Escape)) {
            return None;
        }
        if self.show_help {
            Some(EscTarget::Help)
        } else if self.connection.open {
            Some(EscTarget::Connection)
        } else if app.address.flows.open {
            Some(EscTarget::Flows)
        } else if app.explorer.pane.is_some() {
            Some(EscTarget::Pane)
        } else if self.settings.open {
            Some(EscTarget::Settings)
        } else {
            None
        }
    }

    fn is_shutting_down(&self) -> bool {
        self.shutdown_rx.is_some() || self.shutdown_complete
    }
}

impl eframe::App for GuiApp {
    /// Opaque: the window is transparent only to lose macOS's frame highlight.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        theme::BG.to_normalized_gamma_f32()
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_close(ctx);

        let app_state = self.app.clone();
        let mut app = app_state.blocking_write();

        let modal_open = self.modal_open(&app);
        handle_shortcuts(
            ctx,
            &mut app,
            &mut self.show_help,
            &mut self.settings.open,
            &mut self.terminal,
            &mut self.explorer,
            modal_open,
        );
        let testnet = app
            .node
            .server_info
            .as_ref()
            .is_some_and(|s| s.network_id.contains("testnet"));
        widgets::set_testnet(ctx, testnet);
        widgets::set_labels(ctx, app.labels.clone());

        egui::TopBottomPanel::top("top_bar")
            .frame(bar_frame())
            .exact_height(TITLE_BAR_HEIGHT)
            .show(ctx, |ui| {
                top_bar(
                    ui,
                    &mut app,
                    &mut self.show_help,
                    &mut self.settings,
                    &mut self.terminal,
                )
            });
        // Added before the terminal so it stays at the very bottom, below it.
        egui::TopBottomPanel::bottom("status_bar")
            .frame(bar_frame())
            .show(ctx, |ui| {
                status_bar(ui, &mut app, &mut self.connection, &self.cmd_tx)
            });
        self.terminal.show(ctx);
        if self.terminal.has_focus() {
            // Esc belongs to the shell (vim etc.), not the popups drawn below.
            ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape));
        }
        // One thing closes per Esc: the topmost window, else the pane, else Settings.
        let esc = self.esc_target(ctx, &app);
        if esc == Some(EscTarget::Pane) {
            app.explorer.close_pane();
        } else if esc == Some(EscTarget::Settings) {
            self.settings.open = false;
        }
        // The info pane slides in over the right of the tab, between the bars and above
        // the terminal, without moving what is behind it.
        self.pane.show(ctx, &mut app, &self.cmd_tx);

        egui::CentralPanel::default().show(ctx, |ui| {
            if self.settings.open {
                self.settings.show(ui, &mut app, &self.cmd_tx);
                return;
            }
            match app.active_tab {
                Tab::Dashboard => dashboard::show(ui, &mut app),
                Tab::Explorer => self.explorer.show(ui, &mut app, &self.cmd_tx),
                Tab::Monitoring => self.monitoring.show(ui, &mut app, &self.cmd_tx),
                Tab::Mempool => mempool::show(ui, &app),
                Tab::RpcExplorer => rpc_explorer::show(ui, &mut app, &self.cmd_tx),
            }
        });
        // Label edits from any address widget (chip, right-click menu, Settings).
        for (address, name) in widgets::take_label_requests(ctx) {
            let _ = self.cmd_tx.send(UiCommand::SetLabel { address, name });
        }

        // "Open in Explorer" from anywhere (a right-click menu, the info pane, a
        // Cmd+click): switch to the tab, close the pane, and show the page: from outside
        // the Explorer in its own sub tab (a new one, or the one already on that page);
        // a click inside it navigates the active one.
        let requests = widgets::take_explorer_requests(ctx);
        if !requests.is_empty() {
            app.active_tab = Tab::Explorer;
            self.settings.open = false;
            app.explorer.close_pane();
        }
        for (page, new_tab) in requests {
            if new_tab {
                app.explorer.show_in_tab(page);
            } else {
                app.explorer.navigate(page);
            }
        }

        // A click on any address, block hash (or a block in the DAG visualizer) or
        // transaction id outside the Explorer shows it in the info pane, whatever the
        // tab; the pane draws it next frame.
        if let Some(page) = widgets::take_pane_request(ctx) {
            app.explorer.open_pane(page);
            ctx.request_repaint();
        }
        self.flow_window
            .show(ctx, &mut app, &self.cmd_tx, esc == Some(EscTarget::Flows));
        // Watchlist alerts pop up over any tab.
        self.toasts.collect(&app);
        self.toasts.show(ctx, &app);

        self.connection.show(
            ctx,
            &mut app,
            &self.cmd_tx,
            esc == Some(EscTarget::Connection),
        );
        help::show(ctx, &mut self.show_help, esc == Some(EscTarget::Help));

        if self.is_shutting_down() {
            let waited = self.shutdown_since.map_or(Duration::ZERO, |t| t.elapsed());
            egui::Modal::new(egui::Id::new("shutdown_modal")).show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Shutting down…");
                });
                // The controller is finishing the index writer's last batch and
                // disconnecting; if that takes too long, let the user leave anyway.
                if waited > SHUTDOWN_PATIENCE {
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new("Still finishing the last index batch and disconnecting.")
                            .weak(),
                    );
                    if ui
                        .button("Quit now")
                        .on_hover_text(
                            "Close without waiting; the index is written in atomic batches",
                        )
                        .clicked()
                    {
                        self.shutdown_rx = None;
                        self.shutdown_complete = true;
                        ctx.send_viewport_cmd(ViewportCommand::Close);
                    }
                }
            });
        }
        // For the next frame's Esc (see `esc_taken`).
        self.esc_taken = ctx.memory(|m| m.focused().is_some()) || egui::Popup::is_any_open(ctx);

        // Keep time-based values (seconds behind the tip, "ago" times) ticking. Wake on the
        // next whole second rather than 1s from now, so they tick evenly even when
        // data updates trigger frames at arbitrary times.
        let to_next_second = 1.0 - ctx.input(|i| i.time).fract();
        ctx.request_repaint_after(Duration::from_secs_f64(to_next_second));
    }
}

/// `modal_open`: a window is on show, so the page behind it keeps its keys (the tab
/// numbers, pause) to itself; only the terminal toggle and the help toggle still work.
fn handle_shortcuts(
    ctx: &egui::Context,
    app: &mut App,
    show_help: &mut bool,
    settings_open: &mut bool,
    terminal: &mut TerminalPane,
    explorer: &mut ExplorerUi,
    modal_open: bool,
) {
    const TAB_KEYS: [Key; 5] = [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5];

    // Works even while the terminal or a text field has focus.
    if ctx.input_mut(|i| i.consume_key(Modifiers::CTRL, Key::Backtick)) {
        terminal.toggle();
    }
    if terminal.has_focus() {
        return;
    }
    if !ctx.wants_keyboard_input() {
        let question = ctx.input_mut(|i| take_text_event(i, "?"));
        if question || ctx.input(|i| i.key_pressed(Key::F1)) {
            *show_help = !*show_help;
        }
    }
    if modal_open {
        return;
    }
    // Modifier combinations type nothing, so they work from the search field too.
    if app.active_tab == Tab::Explorer && !*settings_open {
        explorer.handle_shortcuts(ctx, app);
    }
    if ctx.wants_keyboard_input() {
        return;
    }

    ctx.input(|i| {
        for (key, tab) in TAB_KEYS.iter().zip(Tab::all()) {
            if i.key_pressed(*key) {
                app.active_tab = *tab;
                *settings_open = false;
            }
        }
        if i.modifiers.ctrl && i.key_pressed(Key::Tab) {
            if i.modifiers.shift {
                app.prev_tab();
            } else {
                app.next_tab();
            }
            *settings_open = false;
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

/// Brand prompt, tmux-style tab strip, and the terminal/help/settings buttons.
fn top_bar(
    ui: &mut egui::Ui,
    app: &mut App,
    show_help: &mut bool,
    settings: &mut SettingsPage,
    terminal: &mut TerminalPane,
) {
    title_bar_drag(ui);
    ui.horizontal_centered(|ui| {
        if cfg!(target_os = "macos") {
            ui.add_space(TRAFFIC_LIGHTS_WIDTH);
        }
        brand(ui);
        ui.add_space(12.0);

        ui.spacing_mut().item_spacing.x = 4.0;
        for (i, tab) in Tab::all().iter().enumerate() {
            if i > 0 {
                ui.label(RichText::new("|").color(theme::BORDER_HI));
            }
            let selected = app.active_tab == *tab && !settings.open;
            if tab_button(ui, tab.label(), selected)
                .on_hover_text(format!("Shortcut: {}", i + 1))
                .clicked()
            {
                app.active_tab = *tab;
                settings.open = false;
            }
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            if ui
                .selectable_label(settings.open, "⚙")
                .on_hover_text("Settings")
                .clicked()
            {
                settings.toggle();
            }
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

/// A tab in the strip (also the Explorer's sub tabs). The active tab is inverted; a
/// hovered one is raised on a lighter surface.
pub(super) fn tab_button(ui: &mut egui::Ui, label: &str, selected: bool) -> egui::Response {
    let padding = egui::vec2(6.0, 2.0);
    let font = egui::TextStyle::Button.resolve(ui.style());
    let text = ui
        .painter()
        .layout_no_wrap(label.to_owned(), font, theme::TEXT);
    let size = text.size() + 2.0 * padding;
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::SelectableLabel,
            ui.is_enabled(),
            selected,
            label,
        )
    });
    if ui.is_rect_visible(rect) {
        let (fill, color) = if selected {
            (theme::ACCENT, theme::BG_DEEP)
        } else if response.hovered() {
            (theme::SURFACE_HI, theme::TEXT_BRIGHT)
        } else {
            (egui::Color32::TRANSPARENT, theme::TEXT)
        };
        let painter = ui.painter();
        if fill != egui::Color32::TRANSPARENT {
            painter.rect_filled(rect, 2.0, fill);
        }
        painter.galley_with_override_text_color(rect.min + padding, text, color);
    }
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// One decimal under 10s (a healthy node is usually under a second behind), whole seconds above.
fn format_seconds(secs: f64) -> String {
    if secs < 10.0 {
        format!("{secs:.1}")
    } else {
        format!("{secs:.0}")
    }
}

/// The connection chip's hover: the target, the node's details (version, sync, block
/// counts, last poll), the last error, and what a click does.
fn connection_details(ui: &mut egui::Ui, app: &App) {
    let node = &app.node;
    // The resolver's node URL is only known once connected.
    let url = node.node_url.as_deref().or(match app.connection {
        ActiveConnection::Url(ref url) => Some(url.as_str()),
        _ => None,
    });
    match app.connection {
        ActiveConnection::None => {}
        ActiveConnection::Url(_) => kv(ui, "Node", url.unwrap_or("—")),
        ActiveConnection::Resolver => kv(ui, "Resolver node", url.unwrap_or("choosing…")),
    }
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
    if let Some(secs) = app.seconds_behind_tip(now_ms())
        && !app.paused
    {
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
    let error = match node.connection_status {
        ConnectionStatus::Error(ref e) => Some(e.as_str()),
        _ => node.last_error.as_deref(),
    };
    if let Some(err) = error {
        kv(ui, "Error", RichText::new(err).color(theme::ERROR));
    }
}

/// The chain pipeline indicator ("Analyzing DAG (73%)", "Analyzer synced"…): the
/// stream's catch-up progress, with its phase, the stream's and the index writer's
/// details (position, coverage, speed, disk) and a Resync button on hover.
fn chain_chip(ui: &mut egui::Ui, app: &App, cmd_tx: &CommandSender) {
    match app.connection {
        ActiveConnection::None => return,
        ActiveConnection::Resolver => {
            widgets::divider(ui);
            widgets::status_chip(
                ui,
                "chain_status",
                "○ Analyzer n/a",
                theme::TEXT_DIM,
                |ui| kv(ui, "Status", "Needs a direct node (URL), not the resolver"),
            );
            return;
        }
        ActiveConnection::Url(_) => {}
    }

    let status = &app.chain;
    let tip = app.node.server_info.as_ref().map(|s| s.virtual_daa_score);
    let fraction = tip.and_then(|tip| status.fraction(tip));
    // The chip keeps to a few words; the phase and what it is doing go in the hover.
    let (text, color, phase, summary) = match status.phase {
        ChainPhase::Idle => return,
        _ if app.paused => (
            "⏸ Analyzer paused".into(),
            theme::TEXT_DIM,
            "Paused",
            "Not fetching until resumed",
        ),
        ChainPhase::Opening => (
            "◌ Analyzing DAG".into(),
            theme::TEXT_DIM,
            "Opening",
            "Opening the index store",
        ),
        ChainPhase::WaitingForNode => (
            "◌ Analyzing DAG".into(),
            theme::TEXT_DIM,
            "Waiting for node",
            "Waiting for the node to connect and sync",
        ),
        ChainPhase::Seeking => (
            "◐ Analyzing DAG".into(),
            theme::WARN,
            "Seeking",
            "Skipping to the last 24 hours",
        ),
        ChainPhase::CatchingUp => (
            match fraction {
                Some(f) => format!("◐ Analyzing DAG ({:.0}%)", f * 100.0),
                None => "◐ Analyzing DAG".into(),
            },
            theme::WARN,
            "Catching up",
            "Catching up to the DAG tip",
        ),
        ChainPhase::Live if status.backlog > 0 => (
            "● Analyzer writing".into(),
            theme::OK,
            "Live",
            "Up to date; writing the latest batches",
        ),
        ChainPhase::Live => ("● Analyzer synced".into(), theme::OK, "Live", "Up to date"),
        ChainPhase::Error(_) => (
            "× Analyzer error".into(),
            theme::ERROR,
            "Error",
            "Request failed, retrying",
        ),
    };
    let (text, color, phase) = match status.write_error {
        Some(_) => ("× Analyzer error".to_string(), theme::ERROR, "Write error"),
        None => (text, color, phase),
    };

    widgets::divider(ui);
    let can_resync = !matches!(status.phase, ChainPhase::Opening);
    let details = |ui: &mut egui::Ui| {
        kv(ui, "Phase", phase);
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
            chain_stream::poll_interval(&status.phase).map(|d| {
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
        if let ChainPhase::Error(ref err) = status.phase {
            kv(ui, "Error", RichText::new(err).color(theme::ERROR));
        }
        if let Some(ref err) = status.write_error {
            kv(ui, "Write error", RichText::new(err).color(theme::ERROR));
        }
        match status.started_from {
            Some(StartPoint::Index(Some(saved))) => {
                let age = saved.elapsed().unwrap_or_default();
                kv(
                    ui,
                    "Started from",
                    format!("index position (written {} ago)", format_duration(age)),
                );
            }
            Some(StartPoint::Index(None)) => kv(ui, "Started from", "index position"),
            Some(StartPoint::PruningPoint) => kv(ui, "Started from", "pruning point"),
            Some(StartPoint::LastDay) => kv(ui, "Started from", "24 hours ago"),
            None => {}
        }
        if status.phase == ChainPhase::CatchingUp {
            if let Some(rate) = status.daa_per_sec {
                kv(ui, "Speed", format!("{} DAA/s", format_number(rate as u64)));
            }
            if let Some(eta) = tip.and_then(|tip| status.eta(tip)) {
                kv(ui, "Time left", format!("~{}", format_duration(eta)));
            }
        }
        kv(ui, "Chain blocks", format_number(status.blocks_processed));

        widgets::subheader(ui, "Index");
        ui.end_row();
        kv(ui, "Transactions", format_number(status.txs_indexed));
        kv(ui, "Addresses", format_number(status.addresses));
        if let Some(pos) = status.position {
            kv(
                ui,
                "Position",
                format!(
                    "DAA {} ({} ago)",
                    format_number(pos.daa_score),
                    format_duration(Duration::from_millis(now_ms().saturating_sub(pos.time_ms)))
                ),
            );
        }
        if let Some((from, to)) = status.coverage {
            kv(
                ui,
                "Covers",
                format!(
                    "{} → {} ago",
                    format_duration(Duration::from_millis(now_ms().saturating_sub(from))),
                    format_duration(Duration::from_millis(
                        now_ms().saturating_sub(to.min(now_ms()))
                    ))
                ),
            );
        }
        if let Some(rate) = status.tx_per_sec {
            kv(
                ui,
                "Write speed",
                format!("{} tx/s", format_number(rate as u64)),
            );
        }
        kv(ui, "Queued batches", status.backlog.to_string());
        kv(
            ui,
            "On disk",
            format!(
                "{} MB in {} slabs",
                status.disk_bytes / (1024 * 1024),
                status.slabs
            ),
        );
        if status.unresolved_reorgs > 0 {
            kv(
                ui,
                "Reorgs past coverage",
                format_number(status.unresolved_reorgs),
            );
        }
        if status.cluster_cap_hits > 0 {
            kv(
                ui,
                "Cluster merges refused (size cap)",
                format_number(status.cluster_cap_hits),
            );
        }
    };
    widgets::status_chip_with(ui, "chain_status", &text, color, details, |ui| {
        ui.add_space(6.0);
        if ui
            .add_enabled(can_resync, widgets::primary_button("Resync"))
            .on_hover_text(
                "Delete the index and the analytics and rebuild them from the node, \
                 from scratch",
            )
            .clicked()
        {
            let _ = cmd_tx.send(UiCommand::Resync);
        }
    });
}

/// The connection chip's text and color: the connection's state, and once connected
/// the node's sync state folded in ("Connected" is a synced node). The target and
/// every detail are in the hover.
fn connection_summary(app: &App) -> (String, egui::Color32) {
    let target = match app.connection {
        ActiveConnection::Url(_) => "node",
        ActiveConnection::Resolver => "public resolver",
        ActiveConnection::None => return ("● Not connected".to_string(), theme::ERROR),
    };
    match app.node.connection_status {
        ConnectionStatus::Connected => match app.node.server_info {
            None => ("◌ Connected".to_string(), theme::TEXT_DIM),
            Some(ref info) if info.is_synced => ("● Connected".to_string(), theme::OK),
            Some(_) => ("◐ Connected, node syncing".to_string(), theme::WARN),
        },
        ConnectionStatus::Connecting => (format!("◌ Connecting to {target}…"), theme::WARN),
        ConnectionStatus::Disconnected => ("● Disconnected".to_string(), theme::ERROR),
        ConnectionStatus::Error(_) => ("× Connection error".to_string(), theme::ERROR),
    }
}

/// Connection (click to change), network, DAA score, poll latency and pause.
fn status_bar(
    ui: &mut egui::Ui,
    app: &mut App,
    connection: &mut ConnectionWindow,
    cmd_tx: &CommandSender,
) {
    ui.horizontal(|ui| {
        let (text, color) = connection_summary(app);
        if ui
            .selectable_label(connection.open, RichText::new(text).color(color))
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_ui(|ui| {
                widgets::kv_grid(ui, "connection_status", |ui| connection_details(ui, app));
                ui.add_space(4.0);
                ui.label(RichText::new("Click to change the connection").weak());
            })
            .clicked()
        {
            connection.toggle();
        }
        chain_chip(ui, app, cmd_tx);

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

            if let Some(ref info) = app.node.server_info {
                widgets::divider(ui);
                // Colored by how far the view lags the DAG tip; while paused the view
                // is deliberately frozen, so no lag is implied.
                let behind = app.seconds_behind_tip(now_ms());
                let color = match behind {
                    _ if app.paused => theme::TEXT_DIM,
                    Some(s) if s > 30.0 => theme::ERROR,
                    Some(s) if s > 10.0 => theme::WARN,
                    _ => theme::TEXT,
                };
                let daa =
                    ui.label(RichText::new(format_number(info.virtual_daa_score)).color(color));
                if app.paused {
                    let since = app
                        .node
                        .last_refresh
                        .map(|at| format!(", last poll {} ago", format_duration(at.elapsed())))
                        .unwrap_or_default();
                    daa.on_hover_text(format!("Polling paused{since}"));
                } else if let Some(secs) = behind {
                    daa.on_hover_text(format!("{} seconds behind tip", format_seconds(secs)));
                }
                widgets::field_label(ui, "daa");
                widgets::divider(ui);
                ui.label(RichText::new(&info.network_id).color(theme::ACCENT));
            }
        });
    });
}
