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
    handles.index = Some(tokio::task::spawn_blocking(move || {
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
        status.coverage = store.coverage();
        status.disk_bytes = store.disk_space();
        app.mark_dirty();
    }

    // Pools reveal themselves by mining many blocks; label them as they do.
    let mut miners = MinerTally::default();
    let mut batches = 0u64;
    while let Some(batch) = receiver.blocking_recv() {
        // The latest labels for the clustering guard, and the node's pruning point
        // (which polling keeps in the app state) to prune behind.
        let (labels, floor, features) = {
            let app = app.blocking_read();
            (
                app.labels.clone(),
                app.node.pruning_point_timestamp_ms,
                app.index_settings,
            )
        };
        writer.set_labels(labels);
        writer.set_features(features);
        let started = Instant::now();
        let result = writer.apply(&batch);
        batches += 1;

        let pruned = match floor {
            Some(floor) => store.prune_before(floor).unwrap_or(0),
            None => 0,
        };
        let pool_labels: Vec<(String, String)> = coinbase_miners(&batch)
            .into_iter()
            .filter_map(|(address, tag)| {
                let name = miners.observe(&address, tag.as_deref())?;
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
                if let Some(hash) = report.analytics_reorgs.first() {
                    app.analytics.reorg_notification = Some(format!(
                        "Reorg detected affecting finalized block {hash}. Analytics may be slightly inaccurate.",
                    ));
                }
                publish_views(&mut app, writer.analytics());
            }
            Err(e) => app.chain.write_error = Some(format!("{e:#}")),
        }
        app.chain.backlog = receiver.len();
        if pruned > 0 || batches % DISK_EVERY == 1 {
            app.chain.slabs = store.slabs().len();
            app.chain.coverage = store.coverage();
            app.chain.disk_bytes = store.disk_space();
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

    let _ = store.persist();
}

/// Recompute the Dashboard's views from `engine`.
fn publish_views(app: &mut App, engine: &AnalyticsEngine) {
    let now = now_ms();
    app.analytics.cached_views = Some(engine.views(now));
    app.analytics.tx_histogram = Some(engine.tx_histogram(now));
}
