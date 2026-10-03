//! Owns the node connection lifecycle (RPC manager, polling tasks) and executes
//! commands sent from the frontend.

use std::sync::Arc;

use tokio::sync::{RwLock, mpsc, oneshot};

use crate::analytics_streaming;
use crate::app::{ActiveConnection, App, CommandLine, ConnectionStatus};
use crate::polling::{PollingHandles, create_and_start_rpc, start_mining_polling};
use crate::rpc::client::RpcManager;
use crate::rpc::methods;

/// Commands sent from the frontend to the controller task.
pub enum UiCommand {
    /// Stop whatever is running and connect to a remote node (URL or resolver).
    Connect(RemoteTarget),
    /// Stop whatever is running and stay disconnected.
    Disconnect,
    /// Run an RPC method with its arguments and store the result in `app.rpc_explorer`.
    ExecuteRpc { method: String, args: Vec<String> },
    /// Fetch block info and store the result in `app.dag_selection`.
    LookupBlock(String),
    /// Run a command-line command and push the result to `app.command_line`.
    RunCommandLine(String),
    /// Tear everything down; the sender is notified once state has been saved.
    Shutdown(oneshot::Sender<()>),
}

pub type CommandSender = mpsc::UnboundedSender<UiCommand>;

/// A node reached over the network: a wRPC URL or the public resolver.
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
    /// The node to connect to, if any.
    remote: Option<RemoteTarget>,
    rpc: Option<Arc<RpcManager>>,
    polling: PollingHandles,
}

/// Spawn the controller on the given runtime and return the command channel.
pub fn spawn(
    rt: &tokio::runtime::Handle,
    app: Arc<RwLock<App>>,
    args: ControllerArgs,
) -> CommandSender {
    let (tx, rx) = mpsc::unbounded_channel();
    let controller = Controller {
        app,
        refresh_interval_ms: args.refresh_interval_ms,
        remote: args.remote,
        rpc: None,
        polling: PollingHandles::new(),
    };
    rt.spawn(controller.run(rx));
    tx
}

impl Controller {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<UiCommand>) {
        // Connect on startup if `--url` was given; otherwise wait for the user to pick.
        self.connect_remote().await;

        while let Some(cmd) = rx.recv().await {
            match cmd {
                UiCommand::Connect(target) => self.connect(target).await,
                UiCommand::Disconnect => self.disconnect().await,
                UiCommand::ExecuteRpc { method, args } => self.execute_rpc(method, args),
                UiCommand::LookupBlock(hash) => self.lookup_block(hash),
                UiCommand::RunCommandLine(cmd) => self.run_command_line(cmd),
                UiCommand::Shutdown(done) => {
                    self.stop_all().await;
                    let _ = done.send(());
                    return;
                }
            }
        }
        // Frontend dropped the channel without an explicit shutdown.
        self.stop_all().await;
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

    /// Stop polling and clear all node data.
    async fn stop_all(&mut self) {
        self.polling.abort_all();
        if let Some(rpc) = self.rpc.take() {
            let _ = rpc.disconnect().await;
        }

        let mut app = self.app.write().await;
        save_analytics_cache(&app);
        app.clear_node_data();
        app.connection = ActiveConnection::None;
        app.has_direct_node = false;
        app.mark_dirty();
    }

    fn execute_rpc(&self, method: String, args: Vec<String>) {
        let rpc = self.rpc.clone();
        let app = self.app.clone();
        tokio::spawn(async move {
            let result = match rpc {
                Some(rpc) => rpc.execute_rpc_call(&method, &args).await,
                None => Err(anyhow::anyhow!("not connected")),
            };
            // The result viewer shows JSON, errors included.
            let result = result.unwrap_or_else(|e| {
                serde_json::to_string_pretty(&serde_json::json!({ "error": e.to_string() }))
                    .unwrap_or_default()
            });
            let mut app = app.write().await;
            app.rpc_explorer.set_response(Some(result));
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
            let mut words = cmd.split_whitespace().map(str::to_string);
            let command = words.next().unwrap_or_default();
            let args: Vec<String> = words.collect();
            let (output, is_error) = match command.as_str() {
                "help" => {
                    let mut help_text = String::from("Available commands:\n\n");
                    for (name, desc) in CommandLine::available_commands() {
                        help_text.push_str(&format!("  {:<34} {}\n", name, desc));
                        if let Some(m) = methods::find(name)
                            && !m.params.is_empty()
                        {
                            help_text.push_str(&format!("      usage: {}\n", m.usage()));
                        }
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
                    Some(rpc) => match rpc.execute_rpc_call(&command, &args).await {
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
        let _ = eng.save(&analytics_streaming::cache_path());
    }
}
