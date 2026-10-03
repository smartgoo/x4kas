//! Owns the node connection lifecycle (embedded daemon, RPC manager, polling tasks)
//! and executes commands sent from the frontend.

use std::sync::Arc;

use tokio::sync::{RwLock, mpsc, oneshot};

use crate::analytics_streaming;
use crate::app::{ActiveConnection, App, CommandLine, ConnectionStatus, DaemonStatus};
use crate::config::DaemonConfig;
use crate::daemon::DaemonHandle;
use crate::daemon_lifecycle::{self, PollingHandles, create_and_start_rpc, start_mining_polling};
use crate::rpc::client::RpcManager;

/// Commands sent from the frontend to the controller task.
pub enum UiCommand {
    /// Stop whatever is running and connect to a remote node (URL or resolver).
    Connect(RemoteTarget),
    /// Stop whatever is running (including the embedded node) and stay disconnected.
    Disconnect,
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

/// A node reached over the network rather than the embedded daemon.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteTarget {
    /// wRPC URL, or `None` to let the public resolver pick a node.
    pub url: Option<String>,
    pub network: String,
}

pub struct ControllerArgs {
    /// Connect here on startup (from `--url`).
    pub remote: Option<RemoteTarget>,
    pub refresh_interval_ms: u64,
}

struct Controller {
    app: Arc<RwLock<App>>,
    refresh_interval_ms: u64,
    /// The remote node to use when the embedded node is not running.
    remote: Option<RemoteTarget>,
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
        refresh_interval_ms: args.refresh_interval_ms,
        remote: args.remote,
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
                UiCommand::Connect(target) => self.connect(target).await,
                UiCommand::Disconnect => self.disconnect().await,
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
    /// 3. neither: stay disconnected until the user picks a connection
    async fn startup(&mut self, config: &DaemonConfig) {
        if self.remote.is_some() {
            self.connect_remote().await;
        } else if config.auto_start_daemon {
            self.start_daemon(config).await;
        }
    }

    async fn connect(&mut self, target: RemoteTarget) {
        self.stop_all().await;
        self.remote = Some(target);
        self.connect_remote().await;
    }

    async fn disconnect(&mut self) {
        self.stop_all().await;
        self.remote = None;
    }

    /// Connect to `self.remote`. Mining and analytics need a direct node, so they are
    /// only started for a URL, not for the resolver.
    async fn connect_remote(&mut self) {
        let Some(target) = self.remote.clone() else {
            return;
        };
        let direct = target.url.is_some();
        {
            let mut app = self.app.write().await;
            app.has_direct_node = direct;
            app.connection = match target.url {
                Some(ref url) => ActiveConnection::Url(url.clone()),
                None => ActiveConnection::Resolver,
            };
            app.node.connection_status = ConnectionStatus::Connecting;
            app.mark_dirty();
        }

        match create_and_start_rpc(
            target.url,
            &target.network,
            &self.app,
            self.refresh_interval_ms,
            false,
            &mut self.polling,
        )
        .await
        {
            Ok(rpc) => {
                if direct {
                    start_mining_polling(&rpc, &self.app, &mut self.polling);
                    analytics_streaming::start_analytics_streaming(
                        &rpc,
                        &self.app,
                        &mut self.polling,
                    );
                }
                self.rpc = Some(rpc);
            }
            Err(e) => {
                let mut app = self.app.write().await;
                app.node.connection_status = ConnectionStatus::Error(e.to_string());
                app.node.last_error = Some(e.to_string());
                app.mark_dirty();
            }
        }
    }

    async fn start_daemon(&mut self, config: &DaemonConfig) {
        self.stop_background_tasks().await;
        {
            let mut app = self.app.write().await;
            app.clear_node_data();
            app.connection = ActiveConnection::Embedded;
            app.integrated_node.status = DaemonStatus::Starting;
            app.mark_dirty();
        }

        match daemon_lifecycle::start_daemon_and_connect(
            config,
            &self.app,
            self.refresh_interval_ms,
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
                self.stop_background_tasks().await;
                self.shutdown_daemon().await;
                let mut app = self.app.write().await;
                app.clear_node_data();
                app.connection = ActiveConnection::None;
                app.has_direct_node = false;
                app.integrated_node.status = DaemonStatus::Error(e.to_string());
                app.mark_dirty();
            }
        }
    }

    /// Stop the embedded node, then fall back to the remote node, if there is one.
    async fn stop_daemon(&mut self) {
        self.stop_all().await;
        self.connect_remote().await;
    }

    /// Stop polling and the embedded node, and clear all node data.
    async fn stop_all(&mut self) {
        let had_daemon = self.daemon.is_some();
        if had_daemon {
            let mut app = self.app.write().await;
            app.integrated_node.status = DaemonStatus::Stopping;
            app.mark_dirty();
        }
        self.stop_background_tasks().await;
        self.shutdown_daemon().await;

        let mut app = self.app.write().await;
        save_analytics_cache(&app);
        app.clear_node_data();
        app.connection = ActiveConnection::None;
        app.has_direct_node = false;
        if had_daemon {
            app.integrated_node.status = DaemonStatus::Stopped;
            app.integrated_node.started_at = None;
        }
        app.mark_dirty();
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
        self.stop_all().await;
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
                        "\nOpen with ':' or Ctrl+K · Tab completes · Up/Down for history · Esc closes",
                    );
                    (help_text, false)
                }
                "clear" => {
                    let mut app = app.write().await;
                    app.command_line.output.clear();
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

/// Persist the analytics cache (best-effort; the streaming task is aborted, not stopped).
fn save_analytics_cache(app: &App) {
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
