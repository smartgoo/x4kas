//! The analytics engine as a sink of the chain stream (see `chain_stream`): loads the
//! cache, then folds every batch into the engine and refreshes the Dashboard's views.

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::SystemTime;

use kaspa_rpc_core::RpcHash;
use tokio::sync::{RwLock, mpsc};

use crate::analytics::{AnalyticsEngine, coinbase_miners, summarize_chain_blocks};
use crate::app::{AnalyticsPhase, App};
use crate::chain_stream::{BatchSender, ChainBatch, SINK_QUEUE};
use crate::config;
use crate::format::now_ms;
use crate::labels::MinerTally;
use crate::polling::PollingHandles;

pub use crate::chain_stream::poll_interval;

/// Where the analytics engine is persisted (`~/.x4kas/analytics_cache.bin`).
pub fn cache_path() -> PathBuf {
    config::data_dir().join("analytics_cache.bin")
}

/// The engine's end of the stream, plus where its cache left off.
pub struct AnalyticsSink {
    pub sender: BatchSender,
    /// The last chain block in the cache and when the cache was saved.
    pub position: Option<(RpcHash, Option<SystemTime>)>,
}

/// Load the cache into `app.analytics.engine` and start the task that feeds the engine
/// from the returned sender, tracked in `handles.analytics_sink`.
pub async fn start_analytics_sink(
    app: &Arc<RwLock<App>>,
    handles: &mut PollingHandles,
) -> AnalyticsSink {
    {
        let mut app = app.write().await;
        app.analytics.status.phase = AnalyticsPhase::LoadingCache;
        app.mark_dirty();
    }
    let path = cache_path();
    let (engine, saved_at) = match AnalyticsEngine::load(&path) {
        Ok(engine) => {
            let saved_at = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
            (engine, saved_at)
        }
        Err(_) => (AnalyticsEngine::default(), None),
    };
    let position = engine
        .last_known_chain_block
        .as_deref()
        .and_then(|h| RpcHash::from_str(h).ok())
        .map(|h| (h, saved_at));

    // Shared with the UI, which reads views from it.
    let engine = Arc::new(RwLock::new(engine));
    app.write().await.analytics.engine = Some(engine.clone());

    let (sender, receiver) = mpsc::channel(SINK_QUEUE);
    handles.analytics_sink = Some(tokio::spawn(run(engine, receiver, app.clone())));
    AnalyticsSink { sender, position }
}

async fn run(
    engine: Arc<RwLock<AnalyticsEngine>>,
    mut receiver: mpsc::Receiver<Arc<ChainBatch>>,
    app: Arc<RwLock<App>>,
) {
    // Pools reveal themselves by mining many blocks; label them as they do.
    let mut miners = MinerTally::default();
    while let Some(response) = receiver.recv().await {
        let (summaries, removed) = summarize_chain_blocks(&response);
        let pool_labels: Vec<(String, String)> = coinbase_miners(&response)
            .into_iter()
            .filter_map(|(address, tag)| {
                let name = miners.observe(&address, tag.as_deref())?;
                Some((address, name))
            })
            .collect();

        // Process blocks and compute views under the engine write lock.
        let (reorg_msg, cached_views, tx_histogram) = {
            let mut eng = engine.write().await;

            // Handle removed blocks (reorgs)
            let mut reorg_msg = None;
            for hash in &removed {
                if !eng.remove_block(hash) {
                    reorg_msg = Some(format!(
                        "Reorg detected affecting finalized block {hash}. Analytics may be slightly inaccurate.",
                    ));
                }
            }

            for summary in summaries {
                eng.add_block(summary);
            }

            let now_ms = now_ms();
            eng.finalize_old_blocks(now_ms);
            eng.prune_buckets(now_ms);

            (reorg_msg, eng.views(now_ms), eng.tx_histogram(now_ms))
        }; // engine lock released

        let mut app = app.write().await;
        if let Some(msg) = reorg_msg {
            app.analytics.reorg_notification = Some(msg);
        }
        app.analytics.cached_views = Some(cached_views);
        app.analytics.tx_histogram = Some(tx_histogram);
        if !pool_labels.is_empty() {
            let mut book = (*app.labels).clone();
            let mut changed = false;
            for (address, name) in &pool_labels {
                changed |= book.set_heuristic(address, name);
            }
            if changed {
                app.labels = Arc::new(book);
            }
        }
        app.mark_dirty();
    }
}
