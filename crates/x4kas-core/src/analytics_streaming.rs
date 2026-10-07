use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use kaspa_rpc_core::RpcHash;
use tokio::sync::RwLock;

use crate::analytics::{AnalyticsEngine, summarize_chain_blocks};
use crate::app::{AnalyticsPhase, App, ConnectionStatus, StartPoint, TimeWindow};
use crate::config;
use crate::format::now_ms;
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
        AnalyticsPhase::Idle | AnalyticsPhase::LoadingCache | AnalyticsPhase::Seeking => None,
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
        Err(_) => (AnalyticsEngine::default(), None),
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

    let mut current_hash = resolve_start(&rpc, &app, cached_start.map(|h| (h, saved_at))).await;

    // Initial catch-up, then incremental polling.
    let mut synced = false;
    loop {
        wait_for_node(&app).await;

        let response = match rpc.fetch_vspc_v2(current_hash).await {
            Ok(response) => response,
            Err(e) if is_out_of_retention(&e) => {
                // The node pruned past our position (a catch-up slower than the pruning
                // point moved), so this hash can never succeed. Start over in the window.
                current_hash = resolve_start(&rpc, &app, None).await;
                continue;
            }
            Err(e) => {
                set_phase(&app, AnalyticsPhase::Error(e.to_string())).await;
                tokio::time::sleep(RETRY_DELAY).await;
                continue;
            }
        };

        let (summaries, removed) = summarize_chain_blocks(&response);
        let block_count = summaries.len();
        let newest_daa = response
            .chain_block_accepted_transactions
            .iter()
            .filter_map(|cb| cb.chain_block_header.daa_score)
            .max()
            .unwrap_or(0);

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

            if let Some(last_added) = response.added_chain_block_hashes.last() {
                current_hash = *last_added;
            }

            let now_ms = now_ms();
            eng.finalize_old_blocks(now_ms);
            eng.prune_buckets(now_ms);

            (reorg_msg, eng.views(now_ms), eng.tx_histogram(now_ms))
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
            app.analytics.tx_histogram = Some(tx_histogram);
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

/// Pick the chain block to stream from and report it in the status: the cached position
/// if given, otherwise the pruning point, skipped ahead to the oldest data the cards
/// show. Retries until the node answers; a cached position the node has pruned falls
/// back to the pruning point.
async fn resolve_start(
    rpc: &RpcManager,
    app: &RwLock<App>,
    mut cached: Option<(RpcHash, Option<SystemTime>)>,
) -> RpcHash {
    set_phase(app, AnalyticsPhase::Seeking).await;
    loop {
        wait_for_node(app).await;
        let found = match cached {
            Some((hash, _)) => skip_to_window(rpc, hash).await,
            None => match rpc.get_pruning_point_hash().await {
                Ok(hash) => skip_to_window(rpc, hash).await,
                Err(e) => Err(e.context("pruning point")),
            },
        };
        match found {
            Ok((hash, skipped)) => {
                let started_from = match cached {
                    _ if skipped => StartPoint::LastDay,
                    Some((_, saved_at)) => StartPoint::Cache(saved_at),
                    None => StartPoint::PruningPoint,
                };
                let mut app = app.write().await;
                let status = &mut app.analytics.status;
                status.started_from = Some(started_from);
                // Progress and speed restart from the new position.
                status.start_daa = None;
                status.current_daa = None;
                status.daa_per_sec = None;
                status.phase = AnalyticsPhase::CatchingUp;
                app.mark_dirty();
                return hash;
            }
            Err(e) if cached.is_some() && is_out_of_retention(&e) => cached = None,
            Err(e) => {
                set_phase(app, AnalyticsPhase::Error(format!("{e:#}"))).await;
                tokio::time::sleep(RETRY_DELAY).await;
                set_phase(app, AnalyticsPhase::Seeking).await;
            }
        }
    }
}

/// Walk the selected chain forward from `start` in cheap hash-only batches until the
/// next batch reaches the start of the 24h window: older blocks would be pruned from
/// the views right away, and fetching their transactions makes the first sync take
/// hours. Returns the chain block to stream from (at most one batch before the
/// window) and whether anything was skipped.
async fn skip_to_window(rpc: &RpcManager, start: RpcHash) -> anyhow::Result<(RpcHash, bool)> {
    let cutoff = now_ms().saturating_sub(TimeWindow::TwentyFourHour.duration_ms());
    let mut current = start;
    if rpc.get_block_timestamp(current).await? >= cutoff {
        return Ok((current, false));
    }
    loop {
        let hashes = rpc.fetch_chain_hashes(current).await?;
        let Some(&last) = hashes.last() else {
            break;
        };
        if rpc.get_block_timestamp(last).await? >= cutoff {
            break;
        }
        current = last;
    }
    Ok((current, current != start))
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

/// Whether the node rejected a VSPC start hash for being older than its retention root
/// ("the queried hash does not have retention root on its chain"), which retrying the
/// same hash can't fix.
fn is_out_of_retention(err: &anyhow::Error) -> bool {
    format!("{err:#}").contains("retention root")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_out_of_retention_errors() {
        let err = anyhow::anyhow!(
            "RPC Server (remote error) -> the queried hash does not have retention root on its chain"
        );
        assert!(is_out_of_retention(&err));
        assert!(!is_out_of_retention(&anyhow::anyhow!("connection closed")));
    }
}
