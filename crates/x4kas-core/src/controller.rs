//! Owns the node connection lifecycle (RPC manager, polling tasks) and executes
//! commands sent from the frontend.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use std::time::Duration;

use anyhow::{Result, anyhow};
use tokio::sync::{RwLock, mpsc, oneshot};

use crate::analytics_streaming;
use crate::app::{ActiveConnection, AddressView, App, ConnectionStatus, IndexPhase};
use crate::chain_stream::{self, StreamStart};
use crate::index::export::{self, ExportFormat};
use crate::index::query::{self, Cursor, FlowGraph};
use crate::index::{self, IndexStore};
use crate::labels;
use crate::polling::{PollingHandles, create_and_start_rpc, start_hashrate_polling};
use crate::rpc::client::RpcManager;
use crate::watch::{self, Watchlist};

/// Rows per page of an address's transactions.
pub const ADDRESS_PAGE: usize = 50;
/// Counterparties shown in the Address Info window.
const ADDRESS_PEERS: usize = 10;
/// Cluster members listed in the Address Info window.
const CLUSTER_MEMBERS: usize = 50;
/// Counterparties followed per node in the flow graph.
pub const FLOW_TOP: usize = 12;

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
    /// Load an address's profile, transactions, counterparties and balance into
    /// `app.address` (the Address Info window).
    LookupAddress(String),
    /// Append the next (older) page of transactions to the open Address Info window.
    AddressPage { address: String, before: Cursor },
    /// Expand the flow graph from `address` by `hops` counterparties (merged into
    /// `app.address.flows`).
    AddressFlows { address: String, hops: u8 },
    /// Write an export under `~/.x4kas/exports/`; the outcome lands in
    /// `app.address.export`.
    Export(ExportRequest),
    /// Replace the watchlist: saved to disk, then the subscription restarts.
    WatchSet(Watchlist),
    /// Set (or with `None` remove) the user's label for an address.
    SetLabel {
        address: String,
        name: Option<String>,
    },
    /// Fetch the public label list now.
    RefreshLabels,
    /// Ask the enabled online sources (kas.fyi, KNS) about an address.
    LookupLabelOnline(String),
    /// Save the online label settings.
    SetLabelSettings(labels::LabelSettings),
    /// Tear everything down; the sender is notified once state has been saved.
    Shutdown(oneshot::Sender<()>),
}

pub type CommandSender = mpsc::UnboundedSender<UiCommand>;

/// What `UiCommand::Export` writes.
#[derive(Debug, Clone, PartialEq)]
pub enum ExportRequest {
    /// Every indexed transaction of an address (up to `export::EXPORT_MAX_ROWS`).
    Transactions {
        address: String,
        format: ExportFormat,
    },
    /// A flow graph as the frontend shows it (collapsed or not), named after its roots.
    Flows {
        graph: FlowGraph,
        roots: Vec<String>,
        format: ExportFormat,
    },
}

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
    /// How far back the chain stream (analytics and the address index) fills in on a
    /// direct node; `None` for everything the node retains.
    pub backfill: Option<Duration>,
}

struct Controller {
    app: Arc<RwLock<App>>,
    refresh_interval_ms: u64,
    backfill: Option<Duration>,
    /// The node to connect to, if any.
    remote: Option<RemoteTarget>,
    rpc: Option<Arc<RpcManager>>,
    /// The address index of the current direct connection, for queries.
    index: Option<Arc<IndexStore>>,
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
        backfill: args.backfill,
        remote: args.remote,
        rpc: None,
        index: None,
        polling: PollingHandles::default(),
    };
    rt.spawn(controller.run(rx));
    tx
}

impl Controller {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<UiCommand>) {
        {
            let mut app = self.app.write().await;
            app.watch.list = Watchlist::load().unwrap_or_default();
            app.labels = Arc::new(labels::LabelBook::load());
            app.label_settings = labels::LabelSettings::load();
        }
        // Connect on startup if `--url` was given; otherwise wait for the user to pick.
        self.connect_remote().await;

        while let Some(cmd) = rx.recv().await {
            match cmd {
                UiCommand::Connect(target) => self.connect(target).await,
                UiCommand::Disconnect => self.disconnect().await,
                UiCommand::ExecuteRpc { method, args } => self.execute_rpc(method, args),
                UiCommand::LookupBlock(hash) => self.lookup_block(hash),
                UiCommand::LookupAddress(address) => self.lookup_address(address).await,
                UiCommand::AddressPage { address, before } => self.address_page(address, before),
                UiCommand::AddressFlows { address, hops } => self.address_flows(address, hops),
                UiCommand::Export(request) => self.export(request).await,
                UiCommand::WatchSet(list) => self.set_watchlist(list).await,
                UiCommand::SetLabel { address, name } => self.set_label(address, name).await,
                UiCommand::RefreshLabels => self.refresh_labels(),
                UiCommand::LookupLabelOnline(address) => self.lookup_label_online(address),
                UiCommand::SetLabelSettings(settings) => self.set_label_settings(settings).await,
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

    /// Connect to `self.remote`. The chain stream (analytics and the address index)
    /// needs a direct node, so it is only started for a URL, not for the resolver.
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
                    self.start_chain_stream(&rpc, &target.network).await;
                }
                watch::start_watch(&rpc, &self.app, &mut self.polling, &target.network);
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

    /// Start the analytics sink, the address index writer and the chain stream that
    /// feeds both. The index is optional: if its store can't be opened (another x4kas
    /// process holds it), analytics still runs and the status says why.
    async fn start_chain_stream(&mut self, rpc: &Arc<RpcManager>, network: &str) {
        let analytics =
            analytics_streaming::start_analytics_sink(&self.app, &mut self.polling).await;
        let mut sinks = vec![analytics.sender];
        let mut position = analytics.position;

        {
            let mut app = self.app.write().await;
            app.index.status.phase = IndexPhase::Opening;
            app.mark_dirty();
        }
        let network_owned = network.to_string();
        let opened = tokio::task::spawn_blocking(move || IndexStore::open(&network_owned))
            .await
            .map_err(|e| anyhow!("index open task: {e}"))
            .and_then(|r| r);
        match opened {
            Ok(store) => {
                let store = Arc::new(store);
                let labels = self.app.read().await.labels.clone();
                match index::task::start_writer(
                    store.clone(),
                    labels,
                    self.app.clone(),
                    &mut self.polling,
                ) {
                    Ok(sink) => {
                        // The index saves its position with every batch, the analytics
                        // cache only at shutdown, so the index position is never older
                        // when both exist: stream from it. The analytics engine prunes
                        // anything outside its windows, so an overlap is harmless.
                        if let Some(pos) = sink.position {
                            let hash = kaspa_rpc_core::RpcHash::from_bytes(pos.chain_block);
                            let saved_at = std::time::UNIX_EPOCH
                                .checked_add(Duration::from_millis(pos.time_ms));
                            position = Some((hash, saved_at));
                        }
                        sinks.push(sink.sender);
                        self.index = Some(store);
                    }
                    Err(e) => self.index_failed(e).await,
                }
            }
            Err(e) => self.index_failed(e).await,
        }

        chain_stream::start_chain_stream(
            rpc,
            &self.app,
            &mut self.polling,
            StreamStart {
                position,
                backfill: self.backfill,
            },
            sinks,
        );
    }

    async fn index_failed(&mut self, e: anyhow::Error) {
        let mut app = self.app.write().await;
        app.index.status.phase = IndexPhase::Error(format!("{e:#}"));
        app.mark_dirty();
    }

    /// Stop polling and clear all node data.
    async fn stop_all(&mut self) {
        self.polling.abort_all();
        self.index = None;
        // The writer finishes its batch and releases the store before we go on, so a
        // reconnect can reopen it.
        self.polling.stop_index().await;
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

    /// Load everything the Address Info window shows: the index's profile, first page
    /// and counterparties (in a blocking task, the store is synchronous) and the node's
    /// balance.
    async fn lookup_address(&mut self, address: String) {
        let store = self.index.clone();
        let key = address.clone();
        let labels = self.app.read().await.labels.clone();
        self.spawn_rpc(
            move |rpc| async move {
                let balance = match kaspa_rpc_core::RpcAddress::try_from(address.as_str()) {
                    Ok(parsed) => rpc.balance(parsed).await.ok(),
                    Err(e) => return Err(anyhow!("invalid address: {e}")),
                };
                let Some(store) = store else {
                    return Err(anyhow!(
                        "The address index needs a direct node connection (a URL), not the resolver"
                    ));
                };
                tokio::task::spawn_blocking(move || -> Result<AddressView> {
                    let profile = query::profile(&store, &address)?;
                    let (page, peers, curve, cluster) = match profile.id {
                        Some(id) => {
                            let deltas = query::balance_deltas(&store, id, 0)?;
                            (
                                query::transactions(&store, id, None, ADDRESS_PAGE)?,
                                query::counterparties(&store, id, ADDRESS_PEERS)?,
                                query::balance_curve(&deltas, balance),
                                Some(query::cluster(&store, &labels, id, CLUSTER_MEMBERS)?),
                            )
                        }
                        None => (
                            query::Page {
                                items: Vec::new(),
                                next: None,
                            },
                            Vec::new(),
                            Vec::new(),
                            None,
                        ),
                    };
                    Ok(AddressView {
                        profile,
                        balance,
                        page,
                        peers,
                        curve,
                        cluster,
                    })
                })
                .await
                .map_err(|e| anyhow!("address lookup task: {e}"))?
            },
            move |app, result| {
                app.address
                    .set_view(&key, result.map_err(|e| format!("{e:#}")));
            },
        );
    }

    fn address_page(&mut self, address: String, before: Cursor) {
        let store = self.index.clone();
        let key = address.clone();
        self.spawn_rpc(
            move |_rpc| async move {
                let store = store.ok_or_else(|| anyhow!("no address index"))?;
                tokio::task::spawn_blocking(move || {
                    let id = store
                        .lookup(&address)?
                        .ok_or_else(|| anyhow!("address not indexed"))?;
                    query::transactions(&store, id, Some(before), ADDRESS_PAGE)
                })
                .await
                .map_err(|e| anyhow!("address page task: {e}"))?
            },
            move |app, result| {
                app.address
                    .append_page(&key, result.map_err(|e| format!("{e:#}")));
            },
        );
    }

    fn address_flows(&mut self, address: String, hops: u8) {
        let store = self.index.clone();
        self.spawn_rpc(
            move |_rpc| async move {
                let store = store.ok_or_else(|| {
                    anyhow!(
                        "The flow graph needs a direct node connection (a URL), not the resolver"
                    )
                })?;
                tokio::task::spawn_blocking(move || {
                    let id = store
                        .lookup(&address)?
                        .ok_or_else(|| anyhow!("address not seen in the indexed window"))?;
                    query::flows(&store, &[id], hops, FLOW_TOP)
                })
                .await
                .map_err(|e| anyhow!("flow task: {e}"))?
            },
            |app, result| {
                app.address
                    .flows
                    .set_result(result.map_err(|e| format!("{e:#}")));
            },
        );
    }

    /// Write the export in a blocking task (the store and the disk are synchronous).
    async fn export(&mut self, request: ExportRequest) {
        let store = self.index.clone();
        let labels = {
            let mut app = self.app.write().await;
            app.address.export.start();
            app.mark_dirty();
            app.labels.clone()
        };
        let app = self.app.clone();
        self.polling.spawn_request(async move {
            let result = tokio::task::spawn_blocking(move || -> Result<PathBuf> {
                let (path, contents) = match request {
                    ExportRequest::Transactions { address, format } => {
                        let store = store.ok_or_else(|| anyhow!("no address index"))?;
                        let id = store
                            .lookup(&address)?
                            .ok_or_else(|| anyhow!("address not seen in the indexed window"))?;
                        let rows = export::all_transactions(&store, id, export::EXPORT_MAX_ROWS)?;
                        let stem = format!("{}-txs", export::address_stem(&address));
                        let contents = match format {
                            ExportFormat::Csv => export::transactions_csv(&address, &rows),
                            ExportFormat::Json => export::transactions_json(&address, &rows)?,
                        };
                        (export::export_path(&stem, format), contents)
                    }
                    ExportRequest::Flows {
                        graph,
                        roots,
                        format,
                    } => {
                        let root = roots.first().map(String::as_str).unwrap_or("flows");
                        let stem = format!("{}-flows", export::address_stem(root));
                        let contents = match format {
                            ExportFormat::Csv => export::flows_csv(&graph, &labels),
                            ExportFormat::Json => export::flows_json(&graph)?,
                        };
                        (export::export_path(&stem, format), contents)
                    }
                };
                export::write(&path, &contents)?;
                Ok(path)
            })
            .await
            .map_err(|e| anyhow!("export task: {e}"))
            .and_then(|r| r);
            let mut app = app.write().await;
            app.address
                .export
                .finish(result.map_err(|e| format!("{e:#}")));
            app.mark_dirty();
        });
    }

    /// Save the new watchlist and restart the subscription with it.
    async fn set_watchlist(&mut self, list: Watchlist) {
        let network = {
            let mut app = self.app.write().await;
            if let Err(e) = list.save() {
                app.watch.status.last_error = Some(format!("save watchlist: {e}"));
            }
            app.watch.list = list;
            app.watch.status.last_event_at = None;
            app.mark_dirty();
            self.remote.as_ref().map(|r| r.network.clone())
        };
        if let Some(h) = self.polling.watch.take() {
            h.abort();
        }
        if let (Some(rpc), Some(network)) = (self.rpc.clone(), network) {
            watch::start_watch(&rpc, &self.app, &mut self.polling, &network);
        }
    }

    async fn set_label(&mut self, address: String, name: Option<String>) {
        let mut app = self.app.write().await;
        let mut book = (*app.labels).clone();
        if let Err(e) = book.set_user(&address, name.as_deref()) {
            app.watch.status.last_error = Some(format!("save labels: {e}"));
        }
        app.labels = Arc::new(book);
        app.mark_dirty();
    }

    fn lookup_label_online(&mut self, address: String) {
        let app = self.app.clone();
        self.polling.spawn_request(async move {
            let settings = app.read().await.label_settings.clone();
            let result = labels::lookup_online(&settings, &address).await;
            let mut app = app.write().await;
            match result {
                Ok(entries) => {
                    let mut book = (*app.labels).clone();
                    for entry in &entries {
                        book.apply_online(&address, entry);
                    }
                    app.labels = Arc::new(book);
                    if let Some(window) = app.address.open.as_mut().filter(|w| w.address == address)
                    {
                        window.online_result = Some(entries);
                    }
                }
                Err(e) => {
                    if let Some(window) = app.address.open.as_mut().filter(|w| w.address == address)
                    {
                        window.error = Some(format!("online lookup: {e:#}"));
                    }
                }
            }
            app.mark_dirty();
        });
    }

    async fn set_label_settings(&mut self, settings: labels::LabelSettings) {
        let mut app = self.app.write().await;
        if let Err(e) = settings.save() {
            app.watch.status.last_error = Some(format!("save label settings: {e}"));
        }
        app.label_settings = settings;
        app.mark_dirty();
    }

    fn refresh_labels(&mut self) {
        let app = self.app.clone();
        self.polling.spawn_request(async move {
            if let Ok(list) = labels::fetch_kaspa_org_names().await {
                let mut app = app.write().await;
                let mut book = (*app.labels).clone();
                book.apply_kaspa_org(&list, std::time::SystemTime::now());
                app.labels = Arc::new(book);
                app.mark_dirty();
            }
        });
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
