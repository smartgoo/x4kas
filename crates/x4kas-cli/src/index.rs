//! `x4kas-cli index …`: the headless address indexer. `run` connects to a node and
//! streams its chain into the index until interrupted, so a server can keep the index
//! current without the GUI; `status` reads the index on disk.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use clap::Subcommand;
use serde::Serialize;
use tokio::sync::RwLock;

use x4kas_core::app::{AnalyticsPhase, App, IndexPhase};
use x4kas_core::chain_stream::{self, StreamStart};
use x4kas_core::format::{format_number, now_ms};
use x4kas_core::index::{self, IndexStore};
use x4kas_core::polling::{PollingHandles, create_and_start_rpc};

#[derive(Subcommand, Debug, Clone, PartialEq)]
pub enum IndexCommand {
    /// Connect to a node and keep the index current until Ctrl+C
    Run {
        /// How many hours back to fill in on start; 0 for everything the node retains
        #[arg(long, default_value = "24")]
        backfill_hours: f64,
        /// Print progress this often, in seconds; 0 for no progress output
        #[arg(long, default_value = "5")]
        progress: u64,
    },
    /// What the index on disk holds
    Status,
}

#[derive(Serialize)]
struct Status {
    path: String,
    format: u32,
    txs_indexed: u64,
    addresses: u32,
    slabs: usize,
    /// `(from_ms, to_ms)`
    coverage: Option<(u64, u64)>,
    position: Option<index::Position>,
    disk_bytes: u64,
}

pub async fn run(url: Option<&str>, network: &str, cmd: IndexCommand) -> Result<()> {
    match cmd {
        IndexCommand::Status => status(network),
        IndexCommand::Run {
            backfill_hours,
            progress,
        } => {
            let url = url.ok_or_else(|| {
                anyhow!("index run needs a direct node: pass --url (the resolver won't do)")
            })?;
            let backfill =
                (backfill_hours > 0.0).then(|| Duration::from_secs_f64(backfill_hours * 3600.0));
            run_indexer(url, network, backfill, progress).await
        }
    }
}

fn status(network: &str) -> Result<()> {
    let store = IndexStore::open(network)?;
    let manifest = store.manifest()?;
    let status = Status {
        path: index::index_dir(network).display().to_string(),
        format: index::FORMAT_VERSION,
        txs_indexed: manifest.txs_indexed,
        addresses: manifest.next_addr_id,
        slabs: store.slabs().len(),
        coverage: store.coverage(),
        position: manifest.position,
        disk_bytes: store.disk_space(),
    };
    println!("{}", serde_json::to_string_pretty(&status)?);
    Ok(())
}

async fn run_indexer(
    url: &str,
    network: &str,
    backfill: Option<Duration>,
    progress_secs: u64,
) -> Result<()> {
    let app = Arc::new(RwLock::new(App::default()));
    let mut handles = PollingHandles::default();

    let store = Arc::new(IndexStore::open(network)?);
    let labels = Arc::new(x4kas_core::labels::LabelBook::load());
    let sink = index::task::start_writer(store, labels, app.clone(), &mut handles)?;
    let position = sink.position.map(|p| {
        (
            kaspa_rpc_core::RpcHash::from_bytes(p.chain_block),
            std::time::UNIX_EPOCH.checked_add(Duration::from_millis(p.time_ms)),
        )
    });

    // Polling keeps the node's sync state and pruning point in the app, as in the GUI.
    let rpc = create_and_start_rpc(Some(url), network, &app, 1000, &mut handles)?;
    chain_stream::start_chain_stream(
        &rpc,
        &app,
        &mut handles,
        StreamStart { position, backfill },
        vec![sink.sender],
    );
    eprintln!(
        "indexing {network} from {url} into {}",
        index::index_dir(network).display()
    );

    let mut ticker = tokio::time::interval(Duration::from_secs(progress_secs.max(1)));
    ticker.tick().await;
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = ticker.tick() => {
                if progress_secs > 0 {
                    eprintln!("{}", progress_line(&*app.read().await));
                }
            }
        }
    }

    eprintln!("stopping…");
    handles.abort_all();
    handles.stop_index().await;
    let _ = rpc.disconnect().await;
    Ok(())
}

/// One line of progress: the stream's phase and the writer's counters.
fn progress_line(app: &App) -> String {
    let stream = &app.analytics.status;
    let index = &app.index.status;
    let phase = match (&stream.phase, &index.phase) {
        (_, IndexPhase::Error(e)) => format!("index error: {e}"),
        (AnalyticsPhase::Error(e), _) => format!("stream error: {e}"),
        (AnalyticsPhase::Live, _) => "live".to_string(),
        (AnalyticsPhase::CatchingUp, _) => {
            let behind = index
                .position
                .map(|p| now_ms().saturating_sub(p.time_ms) / 1000)
                .map(|s| format!(", {s}s behind"))
                .unwrap_or_default();
            format!("catching up{behind}")
        }
        (phase, _) => format!("{phase:?}").to_lowercase(),
    };
    format!(
        "{phase} | {} txs, {} addresses, {} slabs, {} MB | {} tx/s, backlog {}",
        format_number(index.txs_indexed),
        format_number(index.addresses),
        index.slabs,
        index.disk_bytes / (1024 * 1024),
        index
            .tx_per_sec
            .map(|r| format_number(r as u64))
            .unwrap_or_else(|| "—".into()),
        index.backlog,
    )
}
