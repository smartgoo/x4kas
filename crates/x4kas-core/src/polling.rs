//! Background tasks that poll the connected node.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::RwLock;
use tokio::task::{JoinHandle, JoinSet};

use crate::app::App;
use crate::rpc::client::RpcManager;

/// Tracks the background tasks that write node data, so a connection switch can stop
/// them all before they write stale data.
#[derive(Default)]
pub struct PollingHandles {
    /// Connects the RPC client, then polls node state.
    pub node: Option<JoinHandle<()>>,
    pub hashrate: Option<JoinHandle<()>>,
    /// The chain stream (`chain_stream::run`), which feeds the sinks below.
    pub analytics: Option<JoinHandle<()>>,
    /// The analytics engine, fed by the chain stream.
    pub analytics_sink: Option<JoinHandle<()>>,
    /// The address index writer thread, fed by the chain stream. Not aborted: a blocking
    /// thread can't be, and it must finish its batch and release the store's lock. It
    /// exits once the stream (its sender) is gone; see [`Self::stop_index`].
    pub index: Option<JoinHandle<()>>,
    /// The watchlist's `UtxosChanged` subscription (`watch::start_watch`).
    pub watch: Option<JoinHandle<()>>,
    /// One-off requests from the frontend (RPC calls, block lookups, commands).
    requests: JoinSet<()>,
}

impl PollingHandles {
    /// Run a one-off request, aborted with everything else by [`Self::abort_all`].
    pub fn spawn_request(&mut self, task: impl Future<Output = ()> + Send + 'static) {
        // Reap finished requests so the set doesn't grow.
        while self.requests.try_join_next().is_some() {}
        self.requests.spawn(task);
    }

    /// Abort every task except the index writer; call [`Self::stop_index`] after.
    pub fn abort_all(&mut self) {
        for handle in [
            &mut self.node,
            &mut self.hashrate,
            &mut self.analytics,
            &mut self.analytics_sink,
            &mut self.watch,
        ] {
            if let Some(h) = handle.take() {
                h.abort();
            }
        }
        self.requests.abort_all();
    }

    /// Wait for the index writer to drain and close the store. Call after
    /// [`Self::abort_all`], which drops the stream that feeds it.
    pub async fn stop_index(&mut self) {
        if let Some(h) = self.index.take() {
            let _ = h.await;
        }
    }
}

/// Poll the network hashrate every 30s while the node is synced and not paused.
pub fn start_hashrate_polling(
    rpc: &Arc<RpcManager>,
    app: &Arc<RwLock<App>>,
    handles: &mut PollingHandles,
) {
    let rpc = rpc.clone();
    let app = app.clone();
    handles.hashrate = Some(tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let mut ticker = tokio::time::interval(Duration::from_secs(30));
        loop {
            ticker.tick().await;
            let ready = {
                let app = app.read().await;
                !app.paused && app.node.server_info.as_ref().is_some_and(|s| s.is_synced)
            };
            if ready && let Ok(hashrate) = rpc.estimate_hashrate().await {
                let mut app = app.write().await;
                app.node.hashrate = Some(hashrate as f64);
                app.mark_dirty();
            }
        }
    }));
}

/// Create an RPC manager, then connect, poll and stream blocks in a task tracked by
/// `handles.node`.
/// `url: None` connects through the public node resolver.
pub fn create_and_start_rpc(
    url: Option<&str>,
    network: &str,
    app: &Arc<RwLock<App>>,
    refresh_interval_ms: u64,
    handles: &mut PollingHandles,
) -> Result<Arc<RpcManager>> {
    let rpc = Arc::new(RpcManager::new(url, network, app.clone())?);

    let task_rpc = rpc.clone();
    handles.node = Some(tokio::spawn(async move {
        // The block stream goes first so it is listening before the first connect.
        let poll = async {
            let _ = task_rpc.connect().await;
            task_rpc
                .poll_forever(Duration::from_millis(refresh_interval_ms))
                .await;
        };
        tokio::join!(task_rpc.stream_blocks(), poll);
    }));

    Ok(rpc)
}
