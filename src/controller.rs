//! Owns the node connection lifecycle (embedded daemon, RPC manager, polling tasks)
//! and executes commands sent from the frontend.

use std::sync::Arc;

use tokio::sync::{RwLock, mpsc, oneshot};

use crate::analytics_streaming;
use crate::app::{App, CommandLine, ConnectionStatus, DaemonStatus};
use crate::config::DaemonConfig;
use crate::daemon::DaemonHandle;
use crate::daemon_lifecycle::{self, PollingHandles, create_and_start_rpc, start_mining_polling};
use crate::rpc::client::RpcManager;

/// Commands sent from the frontend to the controller task.
pub enum UiCommand {
    StartDaemon(Box<DaemonConfig>),
    StopDaemon,
    /// Run an RPC method and store the result in `app.rpc_explorer`.
    ExecuteRpc(String),
    /// Fetch block info and store the result in `app.dag_selection`.
    LookupBlock(String),
    /// Run a command-line command and push the result to `app.command_line`.
    RunCommandLine(String),
    /// Tear everything down; the sender is notified once the node has stopped.
    Shutdown(oneshot::Sender<()>),
}

pub type CommandSender = mpsc::UnboundedSender<UiCommand>;

pub struct ControllerArgs {
    pub url: Option<String>,
    pub network: String,
    pub refresh_interval_ms: u64,
}

struct Controller {
    app: Arc<RwLock<App>>,
    args: ControllerArgs,
    rpc: Option<Arc<RpcManager>>,
    daemon: Option<DaemonHandle>,
    log_tail: Option<tokio::task::JoinHandle<()>>,
    polling: PollingHandles,
}

/// Spawn the controller on the given runtime and return the command channel.
pub fn spawn(
    rt: &tokio::runtime::Handle,
    app: Arc<RwLock<App>>,
    args: ControllerArgs,
    config: DaemonConfig,
) -> CommandSender {
    let (tx, rx) = mpsc::unbounded_channel();
    let controller = Controller {
        app,
        args,
        rpc: None,
        daemon: None,
        log_tail: None,
        polling: PollingHandles::new(),
    };
    rt.spawn(controller.run(config, rx));
    tx
}

impl Controller {
    async fn run(mut self, config: DaemonConfig, mut rx: mpsc::UnboundedReceiver<UiCommand>) {
        self.startup(&config).await;

        while let Some(cmd) = rx.recv().await {
            match cmd {
                UiCommand::StartDaemon(config) => self.start_daemon(&config).await,
                UiCommand::StopDaemon => self.stop_daemon().await,
                UiCommand::ExecuteRpc(method) => self.execute_rpc(method),
                UiCommand::LookupBlock(hash) => self.lookup_block(hash),
                UiCommand::RunCommandLine(cmd) => self.run_command_line(cmd),
                UiCommand::Shutdown(done) => {
                    self.shutdown().await;
                    let _ = done.send(());
                    return;
                }
            }
        }
        // Frontend dropped the channel without an explicit shutdown.
        self.shutdown().await;
    }

    /// Startup modes:
    /// 1. --url provided: connect directly to that node
    /// 2. auto_start_daemon (no --url): start integrated daemon, connect to it
    /// 3. neither: start with no connection, user starts daemon from the Node tab
    async fn startup(&mut self, config: &DaemonConfig) {
        if self.args.url.is_some() {
            self.app.write().await.has_direct_node = true;
            self.connect_direct().await;
        } else if config.auto_start_daemon {
            self.start_daemon(config).await;
        } else {
            self.app.write().await.has_direct_node = false;
            self.set_disconnected_rpc().await;
        }
    }

    async fn connect_direct(&mut self) {
        match create_and_start_rpc(
            self.args.url.clone(),
            &self.args.network,
            &self.app,
            self.args.refresh_interval_ms,
            false,
        )
        .await
        {
            Ok(rpc) => {
                start_mining_polling(&rpc, &self.app, &mut self.polling);
                analytics_streaming::start_analytics_streaming(&rpc, &self.app, &mut self.polling);
                self.rpc = Some(rpc);
            }
            Err(e) => {
                let mut app = self.app.write().await;
                app.node.last_error = Some(e.to_string());
                app.mark_dirty();
            }
        }
    }

    async fn set_disconnected_rpc(&mut self) {
        self.rpc = RpcManager::new(None, &self.args.network, self.app.clone())
            .await
            .ok()
            .map(Arc::new);
    }

    async fn start_daemon(&mut self, config: &DaemonConfig) {
        {
            let mut app = self.app.write().await;
            app.integrated_node.status = DaemonStatus::Starting;
            app.mark_dirty();
        }

        self.polling.abort_all();
        if let Some(rpc) = self.rpc.take() {
            let _ = rpc.disconnect().await;
        }

        match daemon_lifecycle::start_daemon_and_connect(
            config,
            &self.app,
            self.args.refresh_interval_ms,
            &mut self.polling,
        )
        .await
        {
            Ok((handle, rpc, log_handle)) => {
                self.daemon = Some(handle);
                self.rpc = Some(rpc);
                self.log_tail = Some(log_handle);
            }
            Err(e) => {
                // Shut down daemon if it was started but RPC failed
                self.shutdown_daemon().await;
                {
                    let mut app = self.app.write().await;
                    app.integrated_node.status = DaemonStatus::Error(e.to_string());
                    app.mark_dirty();
                }
                self.set_disconnected_rpc().await;
            }
        }
    }

    async fn stop_daemon(&mut self) {
        self.stop_background_tasks().await;
        self.shutdown_daemon().await;

        let has_url = self.args.url.is_some();
        {
            let mut app = self.app.write().await;
            app.node.server_info = None;
            app.node.dag_info = None;
            app.node.mempool_state = None;
            app.node.coin_supply = None;
            app.node.fee_estimate = None;
            app.node.mining_info = None;
            app.analytics.engine = None;
            app.analytics.sync_progress = None;
            app.analytics.cached_views = None;
            app.node.node_url = None;
            app.node.node_uid = None;
            app.integrated_node.status = DaemonStatus::Stopped;
            app.integrated_node.started_at = None;
            app.has_direct_node = has_url;
            if !has_url {
                app.node.connection_status = ConnectionStatus::Disconnected;
            }
            app.mark_dirty();
        }

        if has_url {
            // Restore original direct URL connection
            self.connect_direct().await;
        } else {
            self.set_disconnected_rpc().await;
        }
    }

    async fn stop_background_tasks(&mut self) {
        self.polling.abort_all();
        if let Some(h) = self.log_tail.take() {
            h.abort();
        }
        if let Some(rpc) = self.rpc.take() {
            let _ = rpc.disconnect().await;
        }
    }

    /// Shut down the embedded daemon (blocks until the node finishes, so run it off the
    /// async worker threads).
    async fn shutdown_daemon(&mut self) {
        if let Some(mut handle) = self.daemon.take() {
            let _ = tokio::task::spawn_blocking(move || handle.shutdown()).await;
        }
    }

    async fn shutdown(&mut self) {
        if self.daemon.is_some() {
            let mut app = self.app.write().await;
            app.integrated_node.status = DaemonStatus::Stopping;
            app.mark_dirty();
        }
        self.stop_background_tasks().await;
        self.shutdown_daemon().await;

        // Persist analytics cache (best-effort — analytics streaming also saves on exit)
        let app = self.app.read().await;
        if let Some(ref engine) = app.analytics.engine
            && let Ok(eng) = engine.try_read()
        {
            let cache_path = dirs::home_dir()
                .unwrap_or_default()
                .join(".tui4kas")
                .join("analytics_cache.bin");
            let _ = eng.save(&cache_path);
        }
    }

    fn execute_rpc(&self, method: String) {
        let rpc = self.rpc.clone();
        let app = self.app.clone();
        tokio::spawn(async move {
            let result = match rpc {
                Some(rpc) => match rpc.execute_rpc_call(&method).await {
                    Ok(response) => response,
                    Err(e) => format!("Error: {}", e),
                },
                None => "Error: not connected".to_string(),
            };
            let mut app = app.write().await;
            app.rpc_explorer.last_response = Some(result);
            app.rpc_explorer.is_loading = false;
            app.mark_dirty();
        });
    }

    fn lookup_block(&self, hash: String) {
        let rpc = self.rpc.clone();
        let app = self.app.clone();
        tokio::spawn(async move {
            let result = match rpc {
                Some(rpc) => match rpc.get_block_by_hash(&hash).await {
                    Ok(info) => info,
                    Err(e) => format!("Error: {}", e),
                },
                None => "Error: not connected".to_string(),
            };
            let mut app = app.write().await;
            app.dag_selection.block_detail = Some(result);
            app.dag_selection.block_loading = false;
            app.mark_dirty();
        });
    }

    fn run_command_line(&self, cmd: String) {
        let rpc = self.rpc.clone();
        let app = self.app.clone();
        tokio::spawn(async move {
            let command = cmd.trim().split(' ').next().unwrap_or_default().to_string();
            let (output, is_error) = match command.as_str() {
                "help" => {
                    let mut help_text = String::from("Available commands:\n\n");
                    for (name, desc) in CommandLine::available_commands() {
                        help_text.push_str(&format!("  {:<28} {}\n", name, desc));
                    }
                    help_text.push_str(
                        "\nPress ':' to open command line, Esc to close, Up/Down for history",
                    );
                    (help_text, false)
                }
                "clear" => {
                    let mut app = app.write().await;
                    app.command_line.output.clear();
                    app.command_line.show_output = false;
                    app.mark_dirty();
                    return;
                }
                _ => match rpc {
                    Some(rpc) => match rpc.execute_rpc_call(&command).await {
                        Ok(response) => (response, false),
                        Err(e) => (e.to_string(), true),
                    },
                    None => ("not connected".to_string(), true),
                },
            };
            let mut app = app.write().await;
            app.command_line.push_output(cmd, output, is_error);
            app.mark_dirty();
        });
    }
}
