//! The VSPC v2 chain stream: one fetch loop per connection that hands every response
//! to its sinks (the analytics engine, the address index) over bounded channels, so a
//! slow sink slows the fetch instead of piling up memory. Progress is reported in
//! `app.analytics.status`, which the status bar and the Dashboard show.
//!
//! The stream starts from the oldest position its sinks remember (or the pruning
//! point), skipped ahead to the backfill window, and then polls the tip every second.
//! Sinks skip what they've already seen, so an overlap is harmless.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use kaspa_rpc_core::{GetVirtualChainFromBlockV2Response, RpcHash};
use tokio::sync::{RwLock, mpsc};

use crate::app::{AnalyticsPhase, App, ConnectionStatus, StartPoint, TimeWindow};
use crate::format::now_ms;
use crate::polling::PollingHandles;
use crate::rpc::client::RpcManager;

pub type ChainBatch = GetVirtualChainFromBlockV2Response;
pub type BatchSender = mpsc::Sender<Arc<ChainBatch>>;

/// Responses a sink may have queued before the fetcher waits for it.
pub const SINK_QUEUE: usize = 4;

/// Delay between requests while catching up to the tip.
const CATCH_UP_INTERVAL: Duration = Duration::from_millis(100);
/// Delay between requests once at the tip.
const LIVE_INTERVAL: Duration = Duration::from_secs(1);
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

/// Where to start streaming.
#[derive(Debug, Clone, Default)]
pub struct StreamStart {
    /// The last chain block a sink has processed, with when it was saved if known. The
    /// oldest of the sinks' positions, so none misses anything.
    pub position: Option<(RpcHash, Option<SystemTime>)>,
    /// Skip ahead from the position (or pruning point) to this far back from now.
    /// `None` keeps everything the node has.
    pub backfill: Option<Duration>,
}

impl StreamStart {
    /// The default window: what the Dashboard shows.
    pub fn default_backfill() -> Duration {
        Duration::from_millis(TimeWindow::TwentyFourHour.duration_ms())
    }
}

/// Start the stream task, tracked in `handles.analytics`.
pub fn start_chain_stream(
    rpc: &Arc<RpcManager>,
    app: &Arc<RwLock<App>>,
    handles: &mut PollingHandles,
    start: StreamStart,
    sinks: Vec<BatchSender>,
) {
    handles.analytics = Some(tokio::spawn(run(rpc.clone(), app.clone(), start, sinks)));
}

/// The stream loop, also used directly by the CLI's headless indexer.
pub async fn run(
    rpc: Arc<RpcManager>,
    app: Arc<RwLock<App>>,
    start: StreamStart,
    sinks: Vec<BatchSender>,
) {
    // The first request must wait for the connection: this task starts before the
    // RPC client has finished connecting.
    wait_for_node(&app).await;

    let backfill = start.backfill;
    let mut current_hash = resolve_start(&rpc, &app, start.position, backfill).await;

    // Initial catch-up, then incremental polling.
    let mut synced = false;
    loop {
        wait_for_node(&app).await;

        let response = match rpc.fetch_vspc_v2(current_hash).await {
            Ok(response) => response,
            Err(e) if is_out_of_retention(&e) => {
                // The node pruned past our position (a catch-up slower than the pruning
                // point moved), so this hash can never succeed. Start over in the window.
                current_hash = resolve_start(&rpc, &app, None, backfill).await;
                continue;
            }
            Err(e) => {
                set_phase(&app, AnalyticsPhase::Error(e.to_string())).await;
                tokio::time::sleep(RETRY_DELAY).await;
                continue;
            }
        };

        let block_count = response.chain_block_accepted_transactions.len();
        let newest_daa = response
            .chain_block_accepted_transactions
            .iter()
            .filter_map(|cb| cb.chain_block_header.daa_score)
            .max()
            .unwrap_or(0);
        if let Some(last_added) = response.added_chain_block_hashes.last() {
            current_hash = *last_added;
        }

        // Hand the batch to every sink; a full queue makes this wait (backpressure).
        let batch = Arc::new(response);
        for sink in &sinks {
            if sink.send(batch.clone()).await.is_err() {
                // A sink that went away (its task ended) can't be fed; the others go on.
                continue;
            }
        }

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

/// Pick the chain block to stream from and report it in the status: the saved position
/// if given, otherwise the pruning point, skipped ahead to the backfill window. Retries
/// until the node answers; a position the node has pruned falls back to the pruning
/// point.
async fn resolve_start(
    rpc: &RpcManager,
    app: &RwLock<App>,
    mut position: Option<(RpcHash, Option<SystemTime>)>,
    backfill: Option<Duration>,
) -> RpcHash {
    set_phase(app, AnalyticsPhase::Seeking).await;
    loop {
        wait_for_node(app).await;
        let found = match position {
            Some((hash, _)) => skip_to_window(rpc, hash, backfill).await,
            None => match rpc.get_pruning_point_hash().await {
                Ok(hash) => skip_to_window(rpc, hash, backfill).await,
                Err(e) => Err(e.context("pruning point")),
            },
        };
        match found {
            Ok((hash, skipped)) => {
                let started_from = match position {
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
            Err(e) if position.is_some() && is_out_of_retention(&e) => position = None,
            Err(e) => {
                set_phase(app, AnalyticsPhase::Error(format!("{e:#}"))).await;
                tokio::time::sleep(RETRY_DELAY).await;
                set_phase(app, AnalyticsPhase::Seeking).await;
            }
        }
    }
}

/// Walk the selected chain forward from `start` in cheap hash-only batches until the
/// next batch reaches the start of the backfill window: fetching the transactions of
/// older blocks nobody asked for makes the first sync take hours. Returns the chain
/// block to stream from (at most one batch before the window) and whether anything was
/// skipped. No window keeps `start`.
async fn skip_to_window(
    rpc: &RpcManager,
    start: RpcHash,
    backfill: Option<Duration>,
) -> anyhow::Result<(RpcHash, bool)> {
    let Some(backfill) = backfill else {
        return Ok((start, false));
    };
    let cutoff = now_ms().saturating_sub(backfill.as_millis() as u64);
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

pub(crate) async fn set_phase(app: &RwLock<App>, phase: AnalyticsPhase) {
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
