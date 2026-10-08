//! The index writer as a background task: a blocking thread that drains the chain
//! stream's batches into the store and reports progress in `app.index`.

use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use tokio::sync::{RwLock, mpsc};

use super::writer::IndexWriter;
use super::{IndexStore, Position};
use crate::app::{App, IndexPhase};
use crate::chain_stream::{BatchSender, ChainBatch, SINK_QUEUE};
use crate::labels::LabelBook;
use crate::polling::PollingHandles;

/// Refresh disk usage in the status every this many batches (it walks the directory).
const DISK_EVERY: u64 = 20;

/// The writer's end of the stream, plus where it left off.
pub struct IndexSink {
    pub sender: BatchSender,
    pub position: Option<Position>,
}

/// Start the writer thread on `store`, tracked in `handles.index`. Batches sent to the
/// returned sender are applied in order; the thread exits when the stream drops it.
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
        let status = &mut app.index.status;
        status.phase = IndexPhase::Indexing;
        let manifest = writer.manifest();
        status.txs_indexed = manifest.txs_indexed;
        status.addresses = manifest.next_addr_id as u64;
        status.position = manifest.position;
        status.slabs = store.slabs().len();
        status.coverage = store.coverage();
        status.disk_bytes = store.disk_space();
        app.mark_dirty();
    }

    let mut batches = 0u64;
    while let Some(batch) = receiver.blocking_recv() {
        let started = Instant::now();
        let result = writer.apply(&batch);
        batches += 1;

        // Prune behind the node's pruning point, which polling keeps in the app state.
        let floor = app.blocking_read().node.pruning_point_timestamp_ms;
        let pruned = match floor {
            Some(floor) => store.prune_before(floor).unwrap_or(0),
            None => 0,
        };

        let mut app = app.blocking_write();
        let status = &mut app.index.status;
        match result {
            Ok(report) => {
                status.phase = IndexPhase::Indexing;
                let manifest = writer.manifest();
                status.txs_indexed = manifest.txs_indexed;
                status.addresses = manifest.next_addr_id as u64;
                status.position = manifest.position;
                status.record_batch(report.txs, started.elapsed());
                if !report.unresolved_reorgs.is_empty() {
                    status.unresolved_reorgs += report.unresolved_reorgs.len() as u64;
                }
            }
            Err(e) => status.phase = IndexPhase::Error(format!("{e:#}")),
        }
        status.backlog = receiver.len();
        if pruned > 0 || batches % DISK_EVERY == 1 {
            status.slabs = store.slabs().len();
            status.coverage = store.coverage();
            status.disk_bytes = store.disk_space();
        }
        app.mark_dirty();
    }

    let _ = store.persist();
    let mut app = app.blocking_write();
    app.index.status.phase = IndexPhase::Idle;
    app.mark_dirty();
}
