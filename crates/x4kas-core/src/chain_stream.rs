//! The VSPC v2 chain stream: one fetch loop per connection that hands every response
//! to its sinks (the index writer, which also carries the analytics engine) over bounded
//! channels, so a slow sink slows the fetch instead of piling up memory. Progress is
//! reported in `app.chain`, which the status bar and the Dashboard show.
//!
//! The stream starts from the position the index remembers (or the pruning point),
//! skipped ahead to the backfill window, and then polls the tip every second. The writer
//! skips chain blocks it has already seen, so an overlap is harmless.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use kaspa_rpc_core::{GetVirtualChainFromBlockV2Response, RpcHash};
use tokio::sync::{RwLock, mpsc};
use tracing::Instrument;

use crate::app::{App, ChainPhase, ConnectionStatus, StartPoint, TimeWindow};
use crate::config::Retention;
use crate::format::now_ms;
use crate::polling::PollingHandles;
use crate::rpc::client::RpcManager;

/// One VSPC v2 response as the stream hands it to its sinks.
pub struct ChainBatch {
    pub response: GetVirtualChainFromBlockV2Response,
    /// The stream didn't continue from the index position before this batch: it
    /// skipped ahead (a pause longer than the backfill window) or the node had pruned
    /// past it. The index records the stretch it missed as a gap.
    pub jumped: bool,
}
pub type BatchSender = mpsc::Sender<Arc<ChainBatch>>;

/// Responses a sink may have queued before the fetcher waits for it. The node answers
/// with up to ten mergesets' worth of chain blocks (2,480 at 10 BPS), which at full
/// blocks is hundreds of thousands of transactions, so one queued response plus the one
/// being fetched and the one being applied is as much as should sit in memory.
pub const SINK_QUEUE: usize = 1;

/// Delay between requests while catching up to the tip.
const CATCH_UP_INTERVAL: Duration = Duration::from_millis(100);
/// Delay between requests once at the tip.
const LIVE_INTERVAL: Duration = Duration::from_secs(1);
/// How long to wait before retrying a failed request.
const RETRY_DELAY: Duration = Duration::from_secs(5);
/// How often to check whether the node is ready.
const NODE_CHECK_INTERVAL: Duration = Duration::from_secs(1);

/// How often the task polls in `phase`, for display.
pub fn poll_interval(phase: &ChainPhase) -> Option<Duration> {
    match phase {
        ChainPhase::WaitingForNode => Some(NODE_CHECK_INTERVAL),
        ChainPhase::CatchingUp => Some(CATCH_UP_INTERVAL),
        ChainPhase::Live => Some(LIVE_INTERVAL),
        ChainPhase::Error(_) => Some(RETRY_DELAY),
        ChainPhase::Idle | ChainPhase::Opening | ChainPhase::Seeking => None,
    }
}

/// Where to start streaming.
#[derive(Debug, Clone, Default)]
pub struct StreamStart {
    /// The last chain block the index has written, with when it was written if known.
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

    /// The backfill for an index kept for `retention`: a fixed period longer than
    /// `backfill` widens it, so a restart after a long pause doesn't skip what the
    /// index would keep (the node's pruning point still bounds it). `None` (everything
    /// the node has) stays.
    pub fn backfill_for(backfill: Option<Duration>, retention: Retention) -> Option<Duration> {
        let backfill = backfill?;
        Some(match retention.duration_ms() {
            Some(ms) => backfill.max(Duration::from_millis(ms)),
            None => backfill,
        })
    }
}

/// Start the stream task, tracked in `handles.stream`.
pub fn start_chain_stream(
    rpc: &Arc<RpcManager>,
    app: &Arc<RwLock<App>>,
    handles: &mut PollingHandles,
    start: StreamStart,
    sinks: Vec<BatchSender>,
) {
    handles.stream = Some(tokio::spawn(run(rpc.clone(), app.clone(), start, sinks)));
}

/// The stream loop, also used directly by the CLI's headless indexer.
pub async fn run(
    rpc: Arc<RpcManager>,
    app: Arc<RwLock<App>>,
    start: StreamStart,
    sinks: Vec<BatchSender>,
) {
    stream(rpc, app, start, sinks)
        .instrument(tracing::info_span!("chain_stream"))
        .await
}

async fn stream(
    rpc: Arc<RpcManager>,
    app: Arc<RwLock<App>>,
    start: StreamStart,
    sinks: Vec<BatchSender>,
) {
    // The first request must wait for the connection: this task starts before the
    // RPC client has finished connecting.
    wait_for_node(&app).await;

    let backfill = start.backfill;
    tracing::info!(
        position = ?start.position.map(|(hash, _)| hash),
        backfill_hours = backfill.map(|b| b.as_secs_f64() / 3600.0),
        "chain stream starting"
    );
    let (mut current_hash, mut jumped) = resolve_start(&rpc, &app, start.position, backfill).await;

    // Initial catch-up, then incremental polling.
    let mut synced = false;
    // Sinks found gone, so each is reported once.
    let mut closed = vec![false; sinks.len()];
    loop {
        wait_for_node(&app).await;

        let fetch_start = Instant::now();
        let response = match rpc.fetch_vspc_v2(current_hash).await {
            Ok(response) => response,
            Err(e) if is_out_of_retention(&e) => {
                tracing::warn!(
                    from = %current_hash,
                    "the node pruned past the stream position: starting over from the window"
                );
                // The node pruned past our position (a catch-up slower than the pruning
                // point moved), so this hash can never succeed. Start over in the window.
                (current_hash, _) = resolve_start(&rpc, &app, None, backfill).await;
                jumped = true;
                continue;
            }
            Err(e) => {
                set_phase(&app, ChainPhase::Error(e.to_string())).await;
                tokio::time::sleep(RETRY_DELAY).await;
                continue;
            }
        };

        let block_count = response.chain_block_accepted_transactions.len();
        let removed = response.removed_chain_block_hashes.len();
        tracing::debug!(
            blocks = block_count,
            removed,
            ms = fetch_start.elapsed().as_millis() as u64,
            "fetched VSPC batch"
        );
        if removed > 0 {
            tracing::info!(removed, "chain reorg: chain blocks removed");
        }
        let newest_daa = response
            .chain_block_accepted_transactions
            .iter()
            .filter_map(|cb| cb.chain_block_header.daa_score)
            .max()
            .unwrap_or(0);
        if let Some(last_added) = response.added_chain_block_hashes.last() {
            current_hash = *last_added;
        } else if let Some(&oldest_removed) = response.removed_chain_block_hashes.last() {
            // A reorg whose new chain blocks aren't confirmed enough to be sent yet: the
            // start hash left the chain, and asking from it again would repeat the same
            // removals every poll. Continue from the fork point, the selected parent of
            // the oldest removed block (the node lists them from the start backwards).
            match rpc.selected_parent(oldest_removed).await {
                Ok(fork_point) => {
                    tracing::debug!(%fork_point, "reorg without new chain blocks: continuing from the fork point");
                    current_hash = fork_point;
                }
                Err(e) => {
                    // Nothing applied yet: the same response comes again after the retry.
                    set_phase(&app, ChainPhase::Error(format!("{e:#}"))).await;
                    tokio::time::sleep(RETRY_DELAY).await;
                    continue;
                }
            }
        }

        // Hand the batch to every sink; a full queue makes this wait (backpressure).
        let batch = Arc::new(ChainBatch { response, jumped });
        jumped = false;
        for (sink, closed) in sinks.iter().zip(&mut closed) {
            // A sink that went away (its task ended) can't be fed; the others go on.
            if sink.send(batch.clone()).await.is_err() && !*closed {
                *closed = true;
                tracing::warn!("a chain stream sink has stopped; it gets no more batches");
            }
        }

        // An empty batch means the tip has been reached.
        if !synced && block_count == 0 {
            synced = true;
        }

        {
            let mut app = app.write().await;
            let status = &mut app.chain;
            status.record_batch(block_count, newest_daa, Instant::now());
            let phase = if synced {
                ChainPhase::Live
            } else {
                ChainPhase::CatchingUp
            };
            status.set_phase(phase);
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

/// Pick the chain block to stream from and report it in the status: the index position
/// if given, otherwise the pruning point, skipped ahead to the backfill window. Retries
/// until the node answers; a position the node has pruned falls back to the pruning
/// point. Also returns whether the stream jumps: it doesn't continue from `position`.
async fn resolve_start(
    rpc: &RpcManager,
    app: &RwLock<App>,
    mut position: Option<(RpcHash, Option<SystemTime>)>,
    backfill: Option<Duration>,
) -> (RpcHash, bool) {
    set_phase(app, ChainPhase::Seeking).await;
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
                    Some((_, saved_at)) => StartPoint::Index(saved_at),
                    None => StartPoint::PruningPoint,
                };
                tracing::info!(from = %hash, start = ?started_from, skipped, "chain stream start found");
                let mut app = app.write().await;
                let status = &mut app.chain;
                status.set_phase(ChainPhase::CatchingUp);
                status.started_from = Some(started_from);
                // Progress and speed restart from the new position.
                status.start_daa = None;
                status.current_daa = None;
                status.daa_per_sec = None;
                app.mark_dirty();
                return (hash, !matches!(started_from, StartPoint::Index(_)));
            }
            Err(e) if position.is_some() && is_out_of_retention(&e) => {
                tracing::warn!(
                    "the node pruned past the index position: starting from the pruning point"
                );
                position = None;
            }
            Err(e) => {
                set_phase(app, ChainPhase::Error(format!("{e:#}"))).await;
                tokio::time::sleep(RETRY_DELAY).await;
                set_phase(app, ChainPhase::Seeking).await;
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
    tracing::debug!(skipped_from = %start, to = %current, "skipped ahead to the backfill window");
    Ok((current, current != start))
}

pub(crate) async fn set_phase(app: &RwLock<App>, phase: ChainPhase) {
    let mut app = app.write().await;
    if app.chain.set_phase(phase) {
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
            if !ready && app.chain.set_phase(ChainPhase::WaitingForNode) {
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
    fn backfill_widens_to_a_longer_retention() {
        let day = StreamStart::default_backfill();
        assert_eq!(
            StreamStart::backfill_for(Some(day), Retention::Node),
            Some(day)
        );
        assert_eq!(
            StreamStart::backfill_for(Some(day), Retention::Days(1)),
            Some(day)
        );
        assert_eq!(
            StreamStart::backfill_for(Some(day), Retention::Days(7)),
            Some(day * 7)
        );
        assert_eq!(
            StreamStart::backfill_for(Some(day * 30), Retention::Days(7)),
            Some(day * 30)
        );
        assert_eq!(StreamStart::backfill_for(None, Retention::Days(7)), None);
    }

    #[test]
    fn detects_out_of_retention_errors() {
        let err = anyhow::anyhow!(
            "RPC Server (remote error) -> the queried hash does not have retention root on its chain"
        );
        assert!(is_out_of_retention(&err));
        assert!(!is_out_of_retention(&anyhow::anyhow!("connection closed")));
    }
}
