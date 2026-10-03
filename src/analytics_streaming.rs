use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock;

use crate::app::App;
use crate::daemon_lifecycle::PollingHandles;
use crate::rpc::client::RpcManager;

/// Start the analytics VSPC V2 streaming task.
pub fn start_analytics_streaming(
    rpc: &Arc<RpcManager>,
    app: &Arc<RwLock<App>>,
    handles: &mut PollingHandles,
) {
    let rpc = rpc.clone();
    let app = app.clone();

    handles.analytics = Some(tokio::spawn(async move {
        use crate::analytics::AnalyticsEngine;
        use std::str::FromStr;

        let cache_path = dirs::home_dir()
            .unwrap_or_default()
            .join(".tui4kas")
            .join("analytics_cache.bin");

        // Try to load persisted state
        let engine = AnalyticsEngine::load(&cache_path).unwrap_or_else(|_| AnalyticsEngine::new());

        // Wrap engine in Arc<RwLock> for shared access with UI
        let engine = Arc::new(tokio::sync::RwLock::new(engine));

        // Store the shared engine reference in app state
        {
            let mut app_guard = app.write().await;
            app_guard.analytics.engine = Some(engine.clone());
        }

        // Determine start hash
        let start_hash = {
            let eng = engine.read().await;
            if let Some(ref last) = eng.last_known_chain_block {
                kaspa_rpc_core::RpcHash::from_str(last).ok()
            } else {
                None
            }
        };

        let start_hash = match start_hash {
            Some(h) => h,
            None => {
                // Get pruning point hash to start from scratch
                match rpc.get_pruning_point_hash().await {
                    Ok(h) => h,
                    Err(_) => {
                        // Retry after delay
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        match rpc.get_pruning_point_hash().await {
                            Ok(h) => h,
                            Err(_) => return,
                        }
                    }
                }
            }
        };

        // Get tip DAA score for progress tracking
        let tip_daa = rpc.get_daa_score().await.unwrap_or(0);

        // Initial sync + incremental polling loop
        let mut current_hash = start_hash;
        let mut synced = false;

        loop {
            // Check if paused
            {
                let app_guard = app.read().await;
                if app_guard.paused {
                    drop(app_guard);
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
                // Only run when node is synced
                if !app_guard
                    .node
                    .server_info
                    .as_ref()
                    .is_some_and(|s| s.is_synced)
                {
                    drop(app_guard);
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
            }

            match rpc.fetch_vspc_v2(current_hash).await {
                Ok(response) => {
                    let (summaries, removed) = RpcManager::extract_block_summaries(&response);

                    let block_count = summaries.len();

                    // Track last daa_score for progress
                    let last_daa = response
                        .chain_block_accepted_transactions
                        .iter()
                        .filter_map(|cb| cb.chain_block_header.daa_score)
                        .max()
                        .unwrap_or(0);

                    // Snapshot time windows before acquiring engine lock
                    // to avoid holding both locks simultaneously
                    let time_windows = {
                        let app_guard = app.read().await;
                        app_guard.analytics.time_windows
                    };

                    // Process blocks and compute views under engine write lock
                    let (reorg_msg, sync_progress, cached_views) = {
                        let mut eng = engine.write().await;

                        // Handle removed blocks (reorgs)
                        let mut reorg_msg = None;
                        for hash in &removed {
                            if !eng.remove_block(hash) {
                                let short = if hash.len() > 16 {
                                    format!("{}…", &hash[..16])
                                } else {
                                    hash.clone()
                                };
                                reorg_msg = Some(format!(
                                    "Reorg detected affecting finalized block {}. Analytics may be slightly inaccurate.",
                                    short
                                ));
                            }
                        }

                        for summary in summaries {
                            eng.add_block(summary);
                        }

                        // Update last known hash
                        if let Some(last_added) = response.added_chain_block_hashes.last() {
                            current_hash = *last_added;
                        }

                        let now_ms = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0);
                        eng.finalize_old_blocks(now_ms);
                        eng.prune_buckets(now_ms);

                        // Detect sync completion
                        if !synced && block_count == 0 {
                            synced = true;
                        }

                        let sync_progress = if !synced {
                            Some((last_daa, tip_daa))
                        } else {
                            None
                        };

                        let cached_views = [
                            eng.get_view(time_windows[0]),
                            eng.get_view(time_windows[1]),
                            eng.get_view(time_windows[2]),
                            eng.get_view(time_windows[3]),
                            eng.get_view(time_windows[4]),
                        ];

                        (reorg_msg, sync_progress, cached_views)
                    }; // engine lock released

                    // Now update app state without holding engine lock
                    {
                        let mut app_guard = app.write().await;
                        app_guard.analytics.sync_progress = sync_progress;
                        if let Some(msg) = reorg_msg {
                            app_guard.analytics.reorg_notification = Some(msg);
                        }
                        app_guard.analytics.cached_views = Some(cached_views);
                        app_guard.mark_dirty();
                    }
                }
                Err(_) => {
                    // RPC error — wait and retry
                }
            }

            if synced {
                tokio::time::sleep(Duration::from_secs(2)).await;
            } else {
                // During initial sync, poll fast but yield to UI
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }));
}
