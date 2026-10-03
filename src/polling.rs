//! Background tasks that poll the connected node.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::RwLock;

use crate::app::App;
use crate::rpc::client::RpcManager;

/// Tracks cancellable background polling tasks.
pub struct PollingHandles {
    /// Connects the RPC client, then polls node state.
    pub node: Option<tokio::task::JoinHandle<()>>,
    pub mining: Option<tokio::task::JoinHandle<()>>,
    pub analytics: Option<tokio::task::JoinHandle<()>>,
}

impl PollingHandles {
    pub fn new() -> Self {
        Self {
            node: None,
            mining: None,
            analytics: None,
        }
    }

    pub fn abort_all(&mut self) {
        if let Some(h) = self.node.take() {
            h.abort();
        }
        if let Some(h) = self.mining.take() {
            h.abort();
        }
        if let Some(h) = self.analytics.take() {
            h.abort();
        }
    }
}

pub fn start_mining_polling(
    rpc: &Arc<RpcManager>,
    app: &Arc<RwLock<App>>,
    handles: &mut PollingHandles,
) {
    let rpc_for_mining = rpc.clone();
    let app_for_mining = app.clone();
    handles.mining = Some(tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let mut ticker = tokio::time::interval(Duration::from_secs(30));
        loop {
            ticker.tick().await;
            let app_guard = app_for_mining.read().await;
            let is_synced = app_guard
                .node
                .server_info
                .as_ref()
                .is_some_and(|s| s.is_synced);
            let is_paused = app_guard.paused;
            drop(app_guard);
            if !is_paused
                && is_synced
                && let Ok(info) = rpc_for_mining.fetch_mining_info().await
            {
                let mut app = app_for_mining.write().await;
                app.node.mining_info = Some(info);
                app.mark_dirty();
            }
        }
    }));

    // Analytics streaming will be set up separately via start_analytics_streaming()
    handles.analytics = None;
}

/// Create an RPC manager, then connect and poll in a task tracked by `handles.node`.
/// `url: None` connects through the public node resolver.
pub async fn create_and_start_rpc(
    url: Option<String>,
    network: &str,
    app: &Arc<RwLock<App>>,
    refresh_interval_ms: u64,
    handles: &mut PollingHandles,
) -> Result<Arc<RpcManager>> {
    let rpc_manager = RpcManager::new(url, network, app.clone()).await?;
    let rpc = Arc::new(rpc_manager);

    let rpc_for_connect = rpc.clone();
    let interval = refresh_interval_ms;
    let app_clone = app.clone();
    handles.node = Some(tokio::spawn(async move {
        let _ = rpc_for_connect.connect().await;
        rpc_for_connect
            .poll_forever(Duration::from_millis(interval), app_clone)
            .await;
    }));

    Ok(rpc)
}
