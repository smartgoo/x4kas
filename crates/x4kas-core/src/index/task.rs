//! The index writer as a background task: a blocking thread that drains the chain
//! stream's batches into the store, publishes the analytics views the engine computes
//! from them, and reports progress in `app.chain`.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;
use kaspa_rpc_core::RpcHash;
use tokio::sync::{RwLock, mpsc};

use super::writer::IndexWriter;
use super::{IndexStore, Position};
use crate::analytics::{AnalyticsEngine, coinbase_miners};
use crate::app::App;
use crate::chain_stream::{BatchSender, ChainBatch, SINK_QUEUE};
use crate::format::now_ms;
use crate::labels::{LabelBook, MinerTally};
use crate::logging::{ErrorLog, log_error_change};
use crate::polling::PollingHandles;

/// Refresh disk usage in the status every this many batches (it walks the directory).
const DISK_EVERY: u64 = 20;

/// The writer's end of the stream, plus where it left off.
pub struct IndexSink {
    pub sender: BatchSender,
    pub position: Option<Position>,
}

impl IndexSink {
    /// The position as the chain stream wants it: the hash and when it was written.
    pub fn stream_position(&self) -> Option<(RpcHash, Option<SystemTime>)> {
        self.position.map(|p| {
            (
                RpcHash::from_bytes(p.chain_block),
                SystemTime::UNIX_EPOCH.checked_add(Duration::from_millis(p.time_ms)),
            )
        })
    }
}

/// Start the writer thread on `store`, tracked in `handles.index`. Batches sent to the
/// returned sender are applied in order; the thread exits when the stream drops it.
/// The analytics views (`app.analytics`) follow the first batch, like the counters.
pub fn start_writer(
    store: Arc<IndexStore>,
    labels: Arc<LabelBook>,
    app: Arc<RwLock<App>>,
    handles: &mut PollingHandles,
) -> Result<IndexSink> {
    let writer = IndexWriter::new(store, labels)?;
    let position = writer.manifest().position;
    let (sender, receiver) = mpsc::channel::<Arc<ChainBatch>>(SINK_QUEUE);
    let span = tracing::info_span!("index_writer");
    handles.index = Some(tokio::task::spawn_blocking(move || {
        let _span = span.enter();
        run(writer, receiver, app);
    }));
    Ok(IndexSink { sender, position })
}

fn run(
    mut writer: IndexWriter,
    mut receiver: mpsc::Receiver<Arc<ChainBatch>>,
    app: Arc<RwLock<App>>,
) {
    let store = writer.store().clone();
    {
        let mut app = app.blocking_write();
        let manifest = *writer.manifest();
        let status = &mut app.chain;
        status.txs_indexed = manifest.txs_indexed;
        status.blocks_indexed = manifest.blocks_indexed;
        status.addresses = manifest.next_addr_id as u64;
        status.position = manifest.position;
        status.rebuilt_from = store.rebuilt_from();
        status.slabs = store.slabs().len();
        status.slab_hours = Some(store.slab_hours());
        status.coverage = store.coverage();
        status.oldest_ms = store.oldest_ms();
        status.gaps = store.gaps().unwrap_or_default();
        status.disk_bytes = store.disk_space();
        tracing::info!(
            txs = manifest.txs_indexed,
            blocks = manifest.blocks_indexed,
            addresses = status.addresses,
            slabs = status.slabs,
            slab_hours = store.slab_hours(),
            gaps = status.gaps.len(),
            disk_mb = status.disk_bytes / 1_000_000,
            "index writer started"
        );
        if let Some(from) = status.rebuilt_from {
            tracing::warn!(
                from_format = from,
                "the index was rebuilt from scratch: its format changed"
            );
        }
        app.mark_dirty();
    }

    // Pools reveal themselves by mining many blocks; label them as they do.
    let mut miners = MinerTally::default();
    let mut batches = 0u64;
    // The same failure tends to repeat with every batch: logged when it changes.
    let mut write_errors = ErrorLog::default();
    let mut prune_errors = ErrorLog::default();
    while let Some(batch) = receiver.blocking_recv() {
        // The latest labels for the clustering guard, and the retention floor (the
        // node's pruning point, which polling keeps in the app state, by default) to
        // prune behind.
        let (labels, floor, features) = {
            let app = app.blocking_read();
            (
                app.labels.clone(),
                app.index_floor_ms(now_ms()),
                app.index_settings,
            )
        };
        writer.set_labels(labels);
        writer.set_features(features);
        let started = Instant::now();
        if batch.jumped {
            writer.mark_jump();
        }
        let result = writer.apply(&batch.response);
        batches += 1;
        // Logged here, before pruning and the app lock, so the time is the batch's own
        // and the GUI doesn't wait on the logging.
        let write_error = result.as_ref().err().map(|e| format!("{e:#}"));
        log_error_change!(
            tracing::Level::ERROR,
            write_errors.update(write_error.as_deref()),
            "index write"
        );
        if let Ok(report) = &result {
            log_report(report, started.elapsed());
        }

        let pruned = match floor.map(|floor| (floor, store.prune_before(floor))) {
            Some((floor, Ok(pruned))) => {
                log_error_change!(
                    tracing::Level::WARN,
                    prune_errors.update(None),
                    "index prune"
                );
                if pruned > 0 {
                    tracing::info!(slabs = pruned, floor_ms = floor, "pruned the index");
                }
                pruned
            }
            Some((_, Err(e))) => {
                let error = format!("{e:#}");
                log_error_change!(
                    tracing::Level::WARN,
                    prune_errors.update(Some(&error)),
                    "index prune"
                );
                0
            }
            None => 0,
        };
        // A gap with no data left before it is just where the index starts now.
        let gaps_changed = pruned > 0
            && store.forget_gaps().unwrap_or_else(|e| {
                tracing::warn!("couldn't forget pruned gaps: {e:#}");
                false
            });
        let pool_labels: Vec<(String, String)> = coinbase_miners(&batch.response)
            .into_iter()
            .filter_map(|(address, tag)| {
                let name = miners.observe(&address, tag.as_deref())?;
                tracing::debug!(%address, %name, "mining pool recognized");
                Some((address, name))
            })
            .collect();

        let mut app = app.blocking_write();
        match result {
            Ok(report) => {
                let manifest = *writer.manifest();
                let status = &mut app.chain;
                status.write_error = None;
                status.txs_indexed = manifest.txs_indexed;
                status.blocks_indexed = manifest.blocks_indexed;
                status.addresses = manifest.next_addr_id as u64;
                status.position = manifest.position;
                status.record_write(report.txs, started.elapsed());
                status.unresolved_reorgs += report.unresolved_reorgs.len() as u64;
                status.cluster_cap_hits += report.cluster_cap_hits;
                if report.gap.is_some() {
                    status.gaps = store.gaps().unwrap_or_default();
                }
                if let Some(hash) = report.analytics_reorgs.first() {
                    app.analytics.reorg_notification = Some(format!(
                        "Reorg detected affecting finalized block {hash}. Analytics may be slightly inaccurate.",
                    ));
                }
                publish_views(&mut app, writer.analytics());
            }
            Err(_) => app.chain.write_error = write_error,
        }
        app.chain.backlog = receiver.len();
        if pruned > 0 || batches % DISK_EVERY == 1 {
            app.chain.slabs = store.slabs().len();
            app.chain.coverage = store.coverage();
            app.chain.oldest_ms = store.oldest_ms();
            app.chain.disk_bytes = store.disk_space();
        }
        if gaps_changed {
            app.chain.gaps = store.gaps().unwrap_or_default();
        }
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

    match store.persist() {
        Ok(()) => tracing::info!(batches, "index writer stopped"),
        Err(e) => tracing::warn!("index writer stopped; the final flush failed: {e:#}"),
    }
}

/// What a batch did: the counts at debug, what deserves attention above it.
fn log_report(report: &super::writer::BatchReport, took: Duration) {
    tracing::debug!(
        chain_blocks = report.chain_blocks,
        txs = report.txs,
        blocks = report.blocks,
        skipped = report.skipped_blocks,
        reorged = report.reorged_blocks,
        ms = took.as_millis() as u64,
        "indexed batch"
    );
    if report.reorged_blocks > 0 {
        tracing::info!(
            chain_blocks = report.reorged_blocks,
            "index undid reorged chain blocks"
        );
    }
    if !report.unresolved_reorgs.is_empty() {
        tracing::warn!(
            count = report.unresolved_reorgs.len(),
            first = %report.unresolved_reorgs[0],
            "reorg removed chain blocks the index doesn't have"
        );
    }
    if let Some(gap) = &report.gap {
        tracing::warn!(
            from_ms = gap.from_ms,
            to_ms = gap.to_ms,
            "index gap recorded"
        );
    }
    if let Some(hash) = report.analytics_reorgs.first() {
        tracing::warn!(%hash, "reorg of a block analytics already finalized: its counts may be slightly off");
    }
    if report.cluster_cap_hits > 0 {
        tracing::debug!(
            hits = report.cluster_cap_hits,
            "cluster size cap refused unions"
        );
    }
}

/// Recompute the Dashboard's views from `engine`.
fn publish_views(app: &mut App, engine: &AnalyticsEngine) {
    let now = now_ms();
    app.analytics.cached_views = Some(engine.views(now));
    app.analytics.tx_histogram = Some(engine.tx_histogram(now));
}
