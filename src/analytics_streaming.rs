use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use kaspa_rpc_core::RpcHash;
use tokio::sync::RwLock;

use crate::analytics::AnalyticsEngine;
use crate::app::{AnalyticsPhase, App, ConnectionStatus, StartPoint};
use crate::config;
use crate::polling::PollingHandles;
use crate::rpc::client::RpcManager;

/// Where the analytics engine is persisted (`~/.x4kas/analytics_cache.bin`).
pub fn cache_path() -> PathBuf {
    config::data_dir().join("analytics_cache.bin")
}

/// Delay between requests while catching up to the tip.
const CATCH_UP_INTERVAL: Duration = Duration::from_millis(100);
/// Delay between requests once at the tip.
const LIVE_INTERVAL: Duration = Duration::from_secs(2);
/// How long to wait before retrying a failed request.
const RETRY_DELAY: Duration = Duration::from_secs(5);
/// How often to check whether the node is ready.
const NODE_CHECK_INTERVAL: Duration = Duration::from_secs(1);

/// How often the task polls in `phase`, for display.
pub fn poll_interval(phase: &AnalyticsPhase) -> Option<Duration> {
    match phase {
        AnalyticsPhase::WaitingForNode => Some(NODE_CHECK_INTERVAL),
        AnalyticsPhase::CatchingUp => Some(CATCH_UP_INTERVAL),
        AnalyticsPhase::Live => Some(LIVE_INTERVAL),
        AnalyticsPhase::Error(_) => Some(RETRY_DELAY),
        AnalyticsPhase::Idle | AnalyticsPhase::LoadingCache => None,
    }
}

/// Start the analytics VSPC V2 streaming task.
pub fn start_analytics_streaming(
    rpc: &Arc<RpcManager>,
    app: &Arc<RwLock<App>>,
    handles: &mut PollingHandles,
) {
    handles.analytics = Some(tokio::spawn(run(rpc.clone(), app.clone())));
}

async fn run(rpc: Arc<RpcManager>, app: Arc<RwLock<App>>) {
    set_phase(&app, AnalyticsPhase::LoadingCache).await;
    let path = cache_path();
    let (engine, saved_at) = match AnalyticsEngine::load(&path) {
        Ok(engine) => {
            let saved_at = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
            (engine, saved_at)
        }
        Err(_) => (AnalyticsEngine::new(), None),
    };
    let cached_start = engine
        .last_known_chain_block
        .as_deref()
        .and_then(|h| RpcHash::from_str(h).ok());

    // Shared with the UI, which reads views from it.
    let engine = Arc::new(RwLock::new(engine));
    app.write().await.analytics.engine = Some(engine.clone());

    // The first request must wait for the connection: this task starts before the
    // RPC client has finished connecting.
    wait_for_node(&app).await;

    let (mut current_hash, started_from) = match cached_start {
        Some(hash) => (hash, StartPoint::Cache(saved_at)),
        None => loop {
            match rpc.get_pruning_point_hash().await {
                Ok(hash) => break (hash, StartPoint::PruningPoint),
                Err(e) => {
                    set_phase(&app, AnalyticsPhase::Error(format!("pruning point: {e}"))).await;
                    tokio::time::sleep(RETRY_DELAY).await;
                    wait_for_node(&app).await;
                }
            }
        },
    };
    {
        let mut app = app.write().await;
        app.analytics.status.started_from = Some(started_from);
        app.analytics.status.phase = AnalyticsPhase::CatchingUp;
        app.mark_dirty();
    }

    // Initial catch-up, then incremental polling.
    let mut synced = false;
    loop {
        wait_for_node(&app).await;

        let response = match rpc.fetch_vspc_v2(current_hash).await {
            Ok(response) => response,
            Err(e) => {
                set_phase(&app, AnalyticsPhase::Error(e.to_string())).await;
                tokio::time::sleep(RETRY_DELAY).await;
                continue;
            }
        };

        let (summaries, removed) = RpcManager::extract_block_summaries(&response);
        let block_count = summaries.len();
        let newest_daa = response
            .chain_block_accepted_transactions
            .iter()
            .filter_map(|cb| cb.chain_block_header.daa_score)
            .max()
            .unwrap_or(0);

        // Process blocks and compute views under the engine write lock.
        let (reorg_msg, cached_views) = {
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

            if let Some(last_added) = response.added_chain_block_hashes.last() {
                current_hash = *last_added;
            }

            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            eng.finalize_old_blocks(now_ms);
            eng.prune_buckets(now_ms);

            (reorg_msg, eng.views(now_ms))
        }; // engine lock released

        // An empty batch means the tip has been reached.
        if !synced && block_count == 0 {
            synced = true;
        }

        {
            let mut app = app.write().await;
            let status = &mut app.analytics.status;
            status.record_batch(block_count, newest_daa, Instant::now());
            status.phase = if synced {
                AnalyticsPhase::Live
            } else {
                AnalyticsPhase::CatchingUp
            };
            if let Some(msg) = reorg_msg {
                app.analytics.reorg_notification = Some(msg);
            }
            app.analytics.cached_views = Some(cached_views);
            app.mark_dirty();
        }

        // Poll fast while catching up, but yield to the UI.
        tokio::time::sleep(if synced {
            LIVE_INTERVAL
        } else {
            CATCH_UP_INTERVAL
        })
        .await;
    }
}

async fn set_phase(app: &RwLock<App>, phase: AnalyticsPhase) {
    let mut app = app.write().await;
    if app.analytics.status.phase != phase {
        app.analytics.status.phase = phase;
        app.mark_dirty();
    }
}

/// Wait until polling isn't paused and the node is connected and synced. Shows
/// `WaitingForNode` while the node isn't ready; a pause keeps the current phase.
async fn wait_for_node(app: &RwLock<App>) {
    loop {
        {
            let mut app = app.write().await;
            let ready = matches!(app.node.connection_status, ConnectionStatus::Connected)
                && app.node.server_info.as_ref().is_some_and(|s| s.is_synced);
            if ready && !app.paused {
                return;
            }
            if !ready && app.analytics.status.phase != AnalyticsPhase::WaitingForNode {
                app.analytics.status.phase = AnalyticsPhase::WaitingForNode;
                app.mark_dirty();
            }
        }
        tokio::time::sleep(NODE_CHECK_INTERVAL).await;
    }
}
