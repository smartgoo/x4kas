//! Owns the node connection lifecycle (RPC manager, polling tasks) and executes
//! commands sent from the frontend.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use tokio::sync::{RwLock, mpsc, oneshot};

use crate::analytics;
use crate::app::{ActiveConnection, AddressView, App, ChainPhase, ConnectionStatus, ExportOrigin};
use crate::chain_stream::{self, StreamStart};
use crate::explorer::{
    self, AddressPageData, BlockView, ExplorerPage, PageData, ProtocolPageData, TxView,
};
use crate::format::now_ms;
use crate::index::export::{self, ExportFormat};
use crate::index::query::{self, AddressProfile, Cursor, FlowGraph};
use crate::index::{self, IndexStore, parse_hex};
use crate::labels::{self, LabelBook};
use crate::polling::{PollingHandles, create_and_start_rpc, start_hashrate_polling};
use crate::query::exec::{self, Cell, ColumnSource, Inputs, QUERY_BUDGET, RunControl};
use crate::query::fields::FieldId;
use crate::query::saved::SavedQueries;
use crate::query::{Entity, Query};
use crate::rpc::client::RpcManager;
use crate::rpc::methods::parse_hash;
use crate::tx_inspect::TransactionProtocol;
use crate::watch::{self, Watchlist};

/// Rows per page of an address's transactions.
pub const ADDRESS_PAGE: usize = 50;
/// Counterparties shown on an address page.
const ADDRESS_PEERS: usize = 10;
/// Cluster members listed on an address page.
const CLUSTER_MEMBERS: usize = 50;
/// Counterparties followed per node in the flow graph.
pub const FLOW_TOP: usize = 12;
/// Address rows whose balance the node is asked for after a query.
const QUERY_BALANCES_MAX: usize = 200;
/// How often a running query reports its progress to the frontend.
const QUERY_PROGRESS_EVERY: Duration = Duration::from_millis(250);

/// Commands sent from the frontend to the controller task.
pub enum UiCommand {
    /// Stop whatever is running and connect to a remote node (URL or resolver).
    Connect(RemoteTarget),
    /// Stop whatever is running and stay disconnected.
    Disconnect,
    /// Run an RPC method with its arguments and store the result in `app.rpc_explorer`.
    ExecuteRpc { method: String, args: Vec<String> },
    /// Append the next (older) page of transactions to the address's loaded page
    /// (`app.explorer`'s cache, shown by the info pane or an Explorer tab).
    AddressPage { address: String, before: Cursor },
    /// Append the next (older) page of a protocol's transactions to its loaded page.
    ProtocolPage {
        protocol: TransactionProtocol,
        before: Cursor,
    },
    /// Load a page into `app.explorer`'s cache (for the info pane or an Explorer tab):
    /// a block from the node, an address's profile, transactions, counterparties and
    /// balance, a transaction from the index, the mempool or the block it is known to
    /// be in, a lookup that may be either a block or a transaction, or a protocol's
    /// transactions from the index.
    ExplorerLoad(ExplorerPage),
    /// Expand the flow graph from `address` by `hops` counterparties (merged into
    /// `app.address.flows`).
    AddressFlows { address: String, hops: u8 },
    /// Load a transaction and its outputs' spenders for the transaction flow window
    /// (`app.sankey`).
    TxSankey {
        txid: String,
        block_hint: Option<String>,
    },
    /// Write an export under `~/.x4kas/exports/`; the outcome lands in
    /// `app.address.export`.
    Export(ExportRequest),
    /// Answer a query against the index; progress and the result land in `app.query`.
    QueryRun {
        query: Query,
        /// The saved query it is run as, if any (named in the result and its export).
        name: Option<String>,
    },
    /// Stop the query in progress.
    QueryCancel,
    /// Replace the saved queries: saved to disk, then `app.query.saved`.
    QueriesSet(SavedQueries),
    /// Replace the watchlist: saved to disk, then the subscription restarts.
    WatchSet(Watchlist),
    /// Set (or with `None` remove) the user's label for an address.
    SetLabel {
        address: String,
        name: Option<String>,
    },
    /// Fetch the public label list now.
    RefreshLabels,
    /// Discard the index store (and with it the Dashboard's analytics) and rebuild it
    /// from the node, from scratch. Only with a direct node.
    Resync,
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
    /// A block as the Explorer shows it, as JSON.
    Block(Box<BlockView>),
    /// A query's result as the Query tab shows it, named after the query.
    Query {
        name: String,
        text: String,
        result: Box<exec::ResultSet>,
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
    /// The query being answered (`UiCommand::QueryRun`): it reads the store, so a
    /// connection switch cancels it and waits for it before closing the store.
    query_task: Option<tokio::task::JoinHandle<()>>,
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
        query_task: None,
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
            match SavedQueries::load() {
                Ok(list) => app.query.saved = list,
                Err(e) => app.query.save_error = Some(format!("load queries: {e}")),
            }
        }
        // Connect on startup if `--url` was given; otherwise wait for the user to pick.
        self.connect_remote().await;

        while let Some(cmd) = rx.recv().await {
            match cmd {
                UiCommand::Connect(target) => self.connect(target).await,
                UiCommand::Disconnect => self.disconnect().await,
                UiCommand::ExecuteRpc { method, args } => self.execute_rpc(method, args),
                UiCommand::AddressPage { address, before } => {
                    self.address_page(address, before).await
                }
                UiCommand::ProtocolPage { protocol, before } => {
                    self.protocol_page(protocol, before).await
                }
                UiCommand::ExplorerLoad(page) => self.explorer_load(page).await,
                UiCommand::AddressFlows { address, hops } => self.address_flows(address, hops),
                UiCommand::TxSankey { txid, block_hint } => self.tx_sankey(txid, block_hint),
                UiCommand::Export(request) => self.export(request).await,
                UiCommand::QueryRun { query, name } => self.query_run(query, name).await,
                UiCommand::QueryCancel => self.query_cancel().await,
                UiCommand::QueriesSet(list) => self.queries_set(list).await,
                UiCommand::WatchSet(list) => self.set_watchlist(list).await,
                UiCommand::SetLabel { address, name } => self.set_label(address, name).await,
                UiCommand::RefreshLabels => self.refresh_labels(),
                UiCommand::Resync => self.resync().await,
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

    /// Open the index store (importing the analytics cache of earlier versions on first
    /// launch), start the writer thread, which also carries the analytics engine, and the
    /// chain stream that feeds it from the index position. If the store can't be opened
    /// (another x4kas process holds it), nothing runs and the status says why.
    async fn start_chain_stream(&mut self, rpc: &Arc<RpcManager>, network: &str) {
        {
            let mut app = self.app.write().await;
            app.chain.phase = ChainPhase::Opening;
            app.mark_dirty();
        }
        let network_owned = network.to_string();
        let opened = tokio::task::spawn_blocking(move || {
            let store = IndexStore::open(&network_owned)?;
            // Best effort: a cache that can't be imported is just stale.
            let _ = index::analytics::import_legacy_cache(&store, &analytics::legacy_cache_path());
            Ok::<_, anyhow::Error>(store)
        })
        .await
        .map_err(|e| anyhow!("index open task: {e}"))
        .and_then(|r| r);
        let store = match opened {
            Ok(store) => Arc::new(store),
            Err(e) => return self.chain_failed(e).await,
        };
        let labels = self.app.read().await.labels.clone();
        let sink = match index::task::start_writer(
            store.clone(),
            labels,
            self.app.clone(),
            &mut self.polling,
        ) {
            Ok(sink) => sink,
            Err(e) => return self.chain_failed(e).await,
        };
        self.index = Some(store.clone());
        // Watched saved queries re-run as the index moves.
        crate::query::watch::start_query_watch(store, self.app.clone(), &mut self.polling);

        chain_stream::start_chain_stream(
            rpc,
            &self.app,
            &mut self.polling,
            StreamStart {
                position: sink.stream_position(),
                backfill: self.backfill,
            },
            vec![sink.sender],
        );
    }

    /// Stop the chain pipeline, delete the index from disk and start the pipeline again,
    /// so it rebuilds from the pruning point with nothing carried over. Node polling, the
    /// hashrate and the watchlist keep running.
    async fn resync(&mut self) {
        let Some(rpc) = self.rpc.clone() else {
            return;
        };
        let Some(network) = self
            .remote
            .as_ref()
            .filter(|r| r.url.is_some())
            .map(|r| r.network.clone())
        else {
            return;
        };
        self.polling.stop_chain().await;
        self.stop_query().await;
        self.index = None;
        {
            let mut app = self.app.write().await;
            app.clear_chain_data();
            app.query.clear();
            app.chain.phase = ChainPhase::Opening;
            app.mark_dirty();
        }
        let discarded = {
            let network = network.clone();
            tokio::task::spawn_blocking(move || index::discard(&network))
                .await
                .map_err(|e| anyhow!("index discard task: {e}"))
                .and_then(|r| r)
        };
        if let Err(e) = discarded {
            return self.chain_failed(e).await;
        }
        self.start_chain_stream(&rpc, &network).await;
    }

    async fn chain_failed(&mut self, e: anyhow::Error) {
        let mut app = self.app.write().await;
        app.chain.phase = ChainPhase::Error(format!("{e:#}"));
        app.mark_dirty();
    }

    /// Stop polling and clear all node data.
    async fn stop_all(&mut self) {
        self.polling.abort_all();
        self.stop_query().await;
        self.index = None;
        // The writer finishes its batch and releases the store before we go on, so a
        // reconnect can reopen it.
        self.polling.stop_index().await;
        if let Some(rpc) = self.rpc.take() {
            let _ = rpc.disconnect().await;
        }

        let mut app = self.app.write().await;
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

    async fn address_page(&mut self, address: String, before: Cursor) {
        let store = self.index.clone();
        let key = address.clone();
        {
            // One page at a time: the button shows a spinner meanwhile.
            let mut app = self.app.write().await;
            match app.explorer.address_page_mut(&key) {
                Some(data) if data.loading_more => return,
                Some(data) => data.loading_more = true,
                None => return,
            }
            app.mark_dirty();
        }
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
                let result = result.map_err(|e| format!("{e:#}"));
                if let Some(data) = app.explorer.address_page_mut(&key) {
                    data.loading_more = false;
                    match result {
                        Ok(page) => {
                            data.view.page.append(page, |t| &t.txid);
                            data.error = None;
                        }
                        Err(e) => data.error = Some(e),
                    }
                }
            },
        );
    }

    async fn protocol_page(&mut self, protocol: TransactionProtocol, before: Cursor) {
        let store = self.index.clone();
        {
            let mut app = self.app.write().await;
            match app.explorer.protocol_page_mut(protocol) {
                Some(data) if data.loading_more => return,
                Some(data) => data.loading_more = true,
                None => return,
            }
            app.mark_dirty();
        }
        self.spawn_rpc(
            move |_rpc| async move {
                let store = store.ok_or_else(|| anyhow!("no address index"))?;
                tokio::task::spawn_blocking(move || {
                    query::protocol_transactions(&store, protocol, Some(before), ADDRESS_PAGE)
                })
                .await
                .map_err(|e| anyhow!("protocol page task: {e}"))?
            },
            move |app, result| {
                let result = result.map_err(|e| format!("{e:#}"));
                if let Some(data) = app.explorer.protocol_page_mut(protocol) {
                    data.loading_more = false;
                    match result {
                        Ok(page) => {
                            data.page.append(page, |t| &t.txid);
                            data.error = None;
                        }
                        Err(e) => data.error = Some(e),
                    }
                }
            },
        );
    }

    /// Load an Explorer page; the outcome lands in `app.explorer`'s cache.
    async fn explorer_load(&mut self, page: ExplorerPage) {
        let store = self.index.clone();
        let key = page.clone();
        let done = move |app: &mut App, result: Result<PageData>| {
            app.explorer
                .set_loaded(key, result.map_err(|e| format!("{e:#}")));
        };
        match page {
            ExplorerPage::Home => {}
            ExplorerPage::Block(hash) => self.spawn_rpc(
                move |rpc| async move { block_view(rpc, store, hash).await.map(PageData::Block) },
                done,
            ),
            ExplorerPage::Address(address) => {
                let labels = self.app.read().await.labels.clone();
                self.spawn_rpc(
                    move |rpc| async move {
                        let view = address_view(rpc, store, labels, address).await?;
                        Ok(PageData::Address(AddressPageData {
                            view,
                            loading_more: false,
                            error: None,
                        }))
                    },
                    done,
                );
            }
            ExplorerPage::Transaction { txid, block } => self.spawn_rpc(
                move |rpc| async move {
                    tx_view(rpc, store, txid, block)
                        .await
                        .map(PageData::Transaction)
                },
                done,
            ),
            ExplorerPage::Protocol(protocol) => self.spawn_rpc(
                move |_rpc| async move {
                    let store = store.ok_or_else(|| {
                        anyhow!(
                            "Protocol transactions need a direct node connection (a URL), not the resolver"
                        )
                    })?;
                    tokio::task::spawn_blocking(move || {
                        let page =
                            query::protocol_transactions(&store, protocol, None, ADDRESS_PAGE)?;
                        Ok(PageData::Protocol(ProtocolPageData {
                            protocol,
                            page,
                            loading_more: false,
                            error: None,
                        }))
                    })
                    .await
                    .map_err(|e| anyhow!("protocol page task: {e}"))?
                },
                done,
            ),
            ExplorerPage::Lookup(ref id) => {
                let (lookup, id) = (page.clone(), id.clone());
                self.spawn_rpc(
                    move |rpc| async move {
                        // A block first: the node answers at once; a transaction may
                        // need the index, the mempool and a block hint.
                        if let Ok(block) = block_view(rpc.clone(), store.clone(), id.clone()).await
                        {
                            return Ok((ExplorerPage::Block(id), PageData::Block(block)));
                        }
                        match tx_view(rpc, store, id.clone(), None).await {
                            Ok(tx) => {
                                Ok((ExplorerPage::transaction(&id), PageData::Transaction(tx)))
                            }
                            Err(e) => Err(anyhow!(
                                "No block has this hash, and no transaction has this id: {e:#}"
                            )),
                        }
                    },
                    move |app, result| match result {
                        Ok((target, data)) => app.explorer.resolve(lookup, target, data),
                        Err(e) => app.explorer.set_loaded(lookup, Err(format!("{e:#}"))),
                    },
                );
            }
        }
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

    /// The transaction flow window's data: the transaction as the Explorer shows it,
    /// and from the index which transaction spent each output.
    fn tx_sankey(&mut self, txid: String, block_hint: Option<String>) {
        let store = self.index.clone();
        let id = txid.clone();
        self.spawn_rpc(
            move |rpc| async move {
                let view = tx_view(rpc, store.clone(), id.clone(), block_hint).await?;
                let spenders = match (store, parse_hex(&id)) {
                    (Some(store), Some(hash)) => {
                        tokio::task::spawn_blocking(move || query::spenders(&store, &hash))
                            .await
                            .map_err(|e| anyhow!("spender lookup task: {e}"))??
                    }
                    _ => Vec::new(),
                };
                Ok((view, spenders))
            },
            move |app, result| {
                app.sankey
                    .set_result(&txid, result.map_err(|e| format!("{e:#}")));
            },
        );
    }

    /// Write the export in a blocking task (the store and the disk are synchronous).
    async fn export(&mut self, request: ExportRequest) {
        let store = self.index.clone();
        let origin = match &request {
            ExportRequest::Transactions { address, .. } => ExportOrigin::Address(address.clone()),
            ExportRequest::Flows { .. } => ExportOrigin::Flows,
            ExportRequest::Block(view) => ExportOrigin::Block(view.hash.clone()),
            ExportRequest::Query { name, .. } => ExportOrigin::Query(name.clone()),
        };
        let labels = {
            let mut app = self.app.write().await;
            app.address.export.start(origin);
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
                    ExportRequest::Block(view) => (
                        export::export_path(&export::block_stem(&view.hash), ExportFormat::Json),
                        export::block_json(&view)?,
                    ),
                    ExportRequest::Query {
                        name,
                        text,
                        result,
                        format,
                    } => {
                        let contents = match format {
                            ExportFormat::Csv => export::result_csv(&result),
                            ExportFormat::Json => export::result_json(&result, &text)?,
                        };
                        (
                            export::export_path(&export::query_stem(&name), format),
                            contents,
                        )
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

    /// Answer `query` in a blocking task (the store is synchronous), reporting progress
    /// every `QUERY_PROGRESS_EVERY` and the result through `app.query`. An address
    /// result with a balance column then gets its balances from the node.
    async fn query_run(&mut self, query: Query, name: Option<String>) {
        self.stop_query().await;
        let store = self.index.clone();
        let rpc = self.rpc.clone();
        let (generation, cancel, labels, watchlist, prune_floor, not_open) = {
            let mut app = self.app.write().await;
            let (generation, cancel) = app.query.start(query.clone(), name);
            app.mark_dirty();
            (
                generation,
                cancel,
                app.labels.clone(),
                app.watch.list.clone(),
                app.node.pruning_point_timestamp_ms,
                index_not_open(&app),
            )
        };
        let app = self.app.clone();
        self.query_task = Some(tokio::spawn(async move {
            let result = match store {
                None => Err(anyhow!("{not_open}")),
                Some(store) => {
                    let progress_app = app.clone();
                    let run_query = query.clone();
                    tokio::task::spawn_blocking(move || {
                        let inputs = Inputs {
                            store: &store,
                            labels: &labels,
                            watchlist: &watchlist,
                            now_ms: now_ms(),
                            prune_floor_ms: prune_floor,
                        };
                        let mut last = Instant::now();
                        let mut ctl = RunControl {
                            cancel,
                            deadline: Some(Instant::now() + QUERY_BUDGET),
                            progress: Some(Box::new(move |p| {
                                if last.elapsed() >= QUERY_PROGRESS_EVERY {
                                    last = Instant::now();
                                    let mut app = progress_app.blocking_write();
                                    app.query.progress(generation, p);
                                    app.mark_dirty();
                                }
                            })),
                        };
                        exec::run(&inputs, &run_query, &mut ctl)
                    })
                    .await
                    .map_err(|e| anyhow!("query task: {e}"))
                    .and_then(|r| r)
                }
            };
            let result = match (result, rpc) {
                (Ok(mut result), Some(rpc)) if result.balances_pending => {
                    {
                        let mut app = app.write().await;
                        app.query.fetching_balances(generation);
                        app.mark_dirty();
                    }
                    fill_balances(&rpc, &mut result).await;
                    Ok(result)
                }
                (result, _) => result,
            };
            let mut app = app.write().await;
            app.query
                .finish(generation, result.map_err(|e| format!("{e:#}")));
            app.mark_dirty();
        }));
    }

    async fn query_cancel(&mut self) {
        let mut app = self.app.write().await;
        app.query.cancel();
        app.mark_dirty();
    }

    /// Cancel the query in progress and wait for it, so the store can be closed.
    async fn stop_query(&mut self) {
        if let Some(task) = self.query_task.take() {
            self.app.write().await.query.cancel();
            let _ = task.await;
        }
    }

    /// Save the queries and show the new list. Queries another process (the CLI's
    /// `query save`) added to the file since the list was loaded are kept.
    async fn queries_set(&mut self, mut list: SavedQueries) {
        let mut app = self.app.write().await;
        if let Ok(on_disk) = SavedQueries::load() {
            for q in on_disk.queries {
                if list.get(&q.id).is_none() && app.query.saved.get(&q.id).is_none() {
                    list.upsert(q);
                }
            }
        }
        app.query.save_error = list.save().err().map(|e| format!("save queries: {e}"));
        app.query.saved = list;
        app.mark_dirty();
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

    fn refresh_labels(&mut self) {
        let app = self.app.clone();
        self.polling.spawn_request(async move {
            labels::refresh_kaspa_org(&app).await;
        });
    }
}

/// Why a query can't run on `app`: the index isn't open (yet), and the reason.
fn index_not_open(app: &App) -> String {
    use crate::app::ChainPhase;
    if !app.connection.is_direct() {
        return "Queries read the address index, which needs a direct node connection (a URL), not the resolver".to_string();
    }
    match &app.chain.phase {
        ChainPhase::Error(e) => format!("The address index isn't open: {e}"),
        ChainPhase::Idle => "The address index isn't open yet: connecting".to_string(),
        ChainPhase::Opening => {
            "The address index is still opening; try again in a moment".to_string()
        }
        _ => "The address index isn't open yet".to_string(),
    }
}

/// Fill an address result's balance column from the node, for the first
/// `QUERY_BALANCES_MAX` rows (best effort: a failed call leaves them unknown).
async fn fill_balances(rpc: &RpcManager, result: &mut exec::ResultSet) {
    result.balances_pending = false;
    if result.entity != Entity::Addresses {
        return;
    }
    let (Some(address_col), Some(balance_col)) = (
        result
            .columns
            .iter()
            .position(|c| c.source == ColumnSource::Field(FieldId::AddrAddress)),
        result
            .columns
            .iter()
            .position(|c| c.source == ColumnSource::Field(FieldId::AddrBalance)),
    ) else {
        return;
    };
    let addresses: Vec<kaspa_rpc_core::RpcAddress> = result
        .rows
        .iter()
        .take(QUERY_BALANCES_MAX)
        .filter_map(|row| match &row[address_col] {
            Cell::Address(a) => kaspa_rpc_core::RpcAddress::try_from(a.as_str()).ok(),
            _ => None,
        })
        .collect();
    if addresses.is_empty() {
        return;
    }
    let Ok(balances) = rpc.balances(addresses).await else {
        return;
    };
    let by_address: std::collections::HashMap<String, u64> = balances.into_iter().collect();
    for row in &mut result.rows {
        if let Cell::Address(a) = &row[address_col]
            && let Some(balance) = by_address.get(a)
        {
            row[balance_col] = Cell::Amount(*balance as i64);
        }
    }
}

/// Everything the address info pane and the Explorer's address page show: the
/// node's balance, and from the index (in a blocking task, the store is synchronous)
/// the profile, first page of transactions, counterparties, balance curve and cluster.
/// Without an index (the resolver) only the balance, with a note saying why.
async fn address_view(
    rpc: Arc<RpcManager>,
    store: Option<Arc<IndexStore>>,
    labels: Arc<LabelBook>,
    address: String,
) -> Result<AddressView> {
    let balance = match kaspa_rpc_core::RpcAddress::try_from(address.as_str()) {
        Ok(parsed) => rpc.balance(parsed).await.ok(),
        Err(e) => return Err(anyhow!("invalid address: {e}")),
    };
    let Some(store) = store else {
        return Ok(AddressView {
            profile: AddressProfile::unindexed(&address),
            balance,
            page: query::Page {
                items: Vec::new(),
                next: None,
            },
            peers: Vec::new(),
            curve: Vec::new(),
            cluster: None,
            index_note: Some(
                "Transactions and totals need the address index, which runs with a direct \
                 node connection (a URL), not the resolver."
                    .to_string(),
            ),
        });
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
            index_note: None,
        })
    })
    .await
    .map_err(|e| anyhow!("address lookup task: {e}"))?
}

/// A block page: `get_block` and `get_block_reward_info` from the node, then the
/// index's acceptance data for its transactions.
async fn block_view(
    rpc: Arc<RpcManager>,
    store: Option<Arc<IndexStore>>,
    hash: String,
) -> Result<BlockView> {
    let hash = parse_hash(&hash)?;
    let (block, reward) = tokio::join!(rpc.block(hash), rpc.block_reward(hash));
    let mut view = BlockView::from_rpc(&block?);
    // Older nodes don't answer; the page just lacks the row.
    view.reward = reward.ok().map(Into::into);
    let Some(store) = store else {
        return Ok(view);
    };
    tokio::task::spawn_blocking(move || {
        explorer::enrich_block(&store, &mut view)?;
        Ok(view)
    })
    .await
    .map_err(|e| anyhow!("block enrich task: {e}"))?
}

/// A transaction page: from the index if it was accepted in the indexed window, else
/// from the mempool, else from `block_hint` (a block it is known to be in). Inputs of
/// a node transaction are resolved from the index where possible.
async fn tx_view(
    rpc: Arc<RpcManager>,
    store: Option<Arc<IndexStore>>,
    txid: String,
    block_hint: Option<String>,
) -> Result<TxView> {
    let id = parse_hex(&txid).ok_or_else(|| anyhow!("a transaction id is 64 hex characters"))?;
    if let Some(store) = store.clone() {
        let found = tokio::task::spawn_blocking(move || query::transaction(&store, &id))
            .await
            .map_err(|e| anyhow!("transaction lookup task: {e}"))??;
        if let Some(detail) = found {
            return Ok(TxView::from_index(detail));
        }
    }
    let rpc_id = parse_hash(&txid)?;
    let from_node = match rpc.mempool_entry(rpc_id).await {
        Ok(entry) => Some(TxView::from_mempool(&entry)),
        Err(_) => match block_hint {
            Some(block) => {
                let block = rpc.block(parse_hash(&block)?).await?;
                TxView::from_block(&block, &txid)
            }
            None => None,
        },
    };
    let Some(view) = from_node else {
        return Err(anyhow!(match store {
            Some(_) => {
                "Not in the mempool and not accepted within the indexed window. \
                 Open it from its block's page if you know the block."
            }
            None => {
                "Not in the mempool. Accepted transactions need the address index, which \
                 runs with a direct node connection (a URL), not the resolver."
            }
        }));
    };
    let Some(store) = store else {
        return Ok(view);
    };
    let mut view = view;
    tokio::task::spawn_blocking(move || {
        explorer::enrich_tx(&store, &mut view)?;
        Ok(view)
    })
    .await
    .map_err(|e| anyhow!("transaction enrich task: {e}"))?
}

/// An error as a JSON object, for views that show JSON responses.
fn error_json(e: &anyhow::Error) -> String {
    serde_json::to_string_pretty(&serde_json::json!({ "error": e.to_string() })).unwrap_or_default()
}
