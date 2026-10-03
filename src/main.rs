mod analytics;
mod analytics_streaming;
mod app;
mod cli;
mod config;
mod controller;
mod daemon;
mod daemon_lifecycle;
mod event;
mod format;
mod keys;
mod rpc;
mod ui;

use std::io;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use crossterm::ExecutableCommand;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::RwLock;

use crate::app::App;
use crate::cli::CliArgs;
use crate::config::DaemonConfig;
use crate::controller::{ControllerArgs, UiCommand};
use crate::event::{AppEvent, EventHandler};
use crate::rpc::market;

#[tokio::main]
async fn main() -> Result<()> {
    let args = CliArgs::parse();

    // Load daemon config (always, for tab state initialization)
    let daemon_config = DaemonConfig::load().unwrap_or_default();

    // Set up panic hook to restore terminal
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = disable_raw_mode();
        let _ = io::stdout().execute(crossterm::event::DisableMouseCapture);
        let _ = io::stdout().execute(LeaveAlternateScreen);
        original_hook(panic_info);
    }));

    // Initialize terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    stdout.execute(crossterm::event::EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Create shared app state
    let app = Arc::new(RwLock::new(App::new(daemon_config.clone())));

    // Start market data polling (every 60 seconds) — independent of node
    market::start_market_polling(app.clone(), Duration::from_secs(60));

    // Controller owns the node connection / embedded daemon lifecycle
    let cmd_tx = controller::spawn(
        &tokio::runtime::Handle::current(),
        app.clone(),
        ControllerArgs {
            url: args.url.clone(),
            network: args.network.clone(),
            refresh_interval_ms: args.refresh_interval_ms,
        },
        daemon_config,
    );

    // Event loop
    let mut events = EventHandler::new(Duration::from_millis(250));

    loop {
        // Draw (skip if nothing changed)
        {
            let mut app_guard = app.write().await;
            if app_guard.dirty {
                app_guard.dirty = false;
                terminal.draw(|f| ui::draw(f, &app_guard))?;
            }
        }

        // Handle events
        let Some(event) = events.next().await else {
            break;
        };

        match event {
            AppEvent::Key(key) => {
                let mut app_guard = app.write().await;
                app_guard.dirty = true;

                if app_guard.command_line.active {
                    if let Some(cmd) = keys::handle_command_mode_keys(&mut app_guard, key.code) {
                        let _ = cmd_tx.send(UiCommand::RunCommandLine(cmd));
                        continue;
                    }
                } else if keys::handle_normal_keys(&mut app_guard, key, &cmd_tx) {
                    continue;
                }

                if app_guard.should_quit {
                    break;
                }
            }
            AppEvent::Mouse(mouse) => {
                let mut app_guard = app.write().await;
                app_guard.dirty = true;
                if keys::handle_mouse(&mut app_guard, mouse) {
                    continue;
                }
            }
            AppEvent::Tick => {
                // Tick just triggers a draw check — dirty is set by data updates
            }
            AppEvent::Resize(_, _) => {
                app.write().await.dirty = true;
            }
        }
    }

    // Shut down the controller (and embedded daemon) before restoring the terminal
    // so the user sees the "Stopping..." status.
    let (done_tx, mut done_rx) = tokio::sync::oneshot::channel();
    let _ = cmd_tx.send(UiCommand::Shutdown(done_tx));
    loop {
        {
            let app_guard = app.read().await;
            terminal.draw(|f| ui::draw(f, &app_guard))?;
        }
        tokio::select! {
            _ = &mut done_rx => break,
            _ = tokio::time::sleep(Duration::from_millis(250)) => {}
        }
    }

    // Restore terminal
    disable_raw_mode()?;
    terminal
        .backend_mut()
        .execute(crossterm::event::DisableMouseCapture)?;
    terminal.backend_mut().execute(LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    Ok(())
}
