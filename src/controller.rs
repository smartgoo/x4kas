//! Owns the node connection lifecycle (RPC manager, polling tasks) and executes
//! commands sent from the frontend.

use std::future::Future;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use tokio::sync::{RwLock, mpsc, oneshot};

use crate::analytics_streaming;
use crate::app::{ActiveConnection, App, CommandLine, ConnectionStatus};
use crate::polling::{PollingHandles, create_and_start_rpc, start_hashrate_polling};
use crate::rpc::client::RpcManager;

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
        polling: PollingHandles::default(),
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
                UiCommand::RunCommandLine(cmd) => self.run_command_line(cmd).await,
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

    /// Connect to `self.remote`. Analytics needs a direct node, so it is only started
    /// for a URL, not for the resolver.
    async fn connect_remote(&mut self) {
        let Some(target) = self.remote.clone() else {
            return;
        };
        {
            let mut app = self.app.write().await;
            app.connection = match target.url {
                Some(ref url) => ActiveConnection::Url(url.clone()),
                None => ActiveConnection::Resolver,
            };
            app.node.connection_status = ConnectionStatus::Connecting;
            app.mark_dirty();
        }

        match create_and_start_rpc(
            target.url.as_deref(),
            &target.network,
            &self.app,
            self.refresh_interval_ms,
            &mut self.polling,
        ) {
            Ok(rpc) => {
                start_hashrate_polling(&rpc, &self.app, &mut self.polling);
                if target.url.is_some() {
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
        app.mark_dirty();
    }

    /// Run `request` against the current RPC manager in a tracked task (aborted by a
    /// connection switch, so a late reply can't land in the next node's data), then
    /// hand its result to `apply` under the app write lock.
    fn spawn_rpc<T, F>(
        &mut self,
        request: impl FnOnce(Arc<RpcManager>) -> F + Send + 'static,
        apply: impl FnOnce(&mut App, Result<T>) + Send + 'static,
    ) where
        T: Send + 'static,
        F: Future<Output = Result<T>> + Send + 'static,
    {
        let rpc = self.rpc.clone();
        let app = self.app.clone();
        self.polling.spawn_request(async move {
            let result = match rpc {
                Some(rpc) => request(rpc).await,
                None => Err(anyhow!("not connected")),
            };
            let mut app = app.write().await;
            apply(&mut app, result);
            app.mark_dirty();
        });
    }

    fn execute_rpc(&mut self, method: String, args: Vec<String>) {
        self.spawn_rpc(
            move |rpc| async move { rpc.execute_rpc_call(&method, &args).await },
            |app, result| {
                // The result viewer shows JSON, errors included.
                let response = result.unwrap_or_else(|e| error_json(&e));
                app.rpc_explorer.set_response(Some(response));
                app.rpc_explorer.is_loading = false;
            },
        );
    }

    fn lookup_block(&mut self, hash: String) {
        self.spawn_rpc(
            move |rpc| async move {
                rpc.execute_rpc_call("get_block", &[hash, "true".to_string()])
                    .await
            },
            |app, result| {
                let detail = result.unwrap_or_else(|e| error_json(&e));
                app.dag_selection.set_detail(Some(detail));
                app.dag_selection.block_loading = false;
            },
        );
    }

    async fn run_command_line(&mut self, cmd: String) {
        let mut words = cmd.split_whitespace().map(str::to_string);
        let command = words.next().unwrap_or_default();
        let args: Vec<String> = words.collect();
        match command.as_str() {
            "help" => {
                let mut app = self.app.write().await;
                app.command_line
                    .push_output(cmd, CommandLine::help_text(), false);
                app.mark_dirty();
            }
            "clear" => {
                let mut app = self.app.write().await;
                app.command_line.output.clear();
                app.mark_dirty();
            }
            _ => self.spawn_rpc(
                move |rpc| async move { rpc.execute_rpc_call(&command, &args).await },
                move |app, result| {
                    let (output, is_error) = match result {
                        Ok(response) => (response, false),
                        Err(e) => (e.to_string(), true),
                    };
                    app.command_line.push_output(cmd, output, is_error);
                },
            ),
        }
    }
}

/// An error as a JSON object, for views that show JSON responses.
fn error_json(e: &anyhow::Error) -> String {
    serde_json::to_string_pretty(&serde_json::json!({ "error": e.to_string() })).unwrap_or_default()
}

/// Persist the analytics cache (best-effort; the streaming task is aborted, not stopped).
fn save_analytics_cache(app: &App) {
    if let Some(ref engine) = app.analytics.engine
        && let Ok(eng) = engine.try_read()
    {
        let _ = eng.save(&analytics_streaming::cache_path());
    }
}
