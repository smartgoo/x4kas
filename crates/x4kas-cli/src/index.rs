//! `x4kas-cli index …`: the headless chain pipeline. `run` connects to a node and
//! streams its chain into the index (and the Dashboard's analytics, which live in it)
//! until interrupted, so a server can keep the index current without the GUI; `status`
//! reads the index on disk.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use clap::Subcommand;
use serde::Serialize;
use tokio::sync::RwLock;

use x4kas_core::app::{App, ChainPhase};
use x4kas_core::chain_stream::{self, StreamStart};
use x4kas_core::config::{IndexFeature, IndexSettings};
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
    /// Show or change the opt-in data the index keeps (off by default; `index run` and
    /// the GUI apply a change from their next launch, the GUI's Settings › Index at once)
    Settings {
        /// Keep every transaction's whole payload, not only its first 128 bytes: on|off
        #[arg(long, value_parser = parse_on_off)]
        full_payloads: Option<bool>,
        /// Keep the redeem script every P2SH spend reveals: on|off
        #[arg(long, value_parser = parse_on_off)]
        redeem_scripts: Option<bool>,
    },
}

fn parse_on_off(s: &str) -> Result<bool, String> {
    match s.to_ascii_lowercase().as_str() {
        "on" | "true" | "yes" | "1" => Ok(true),
        "off" | "false" | "no" | "0" => Ok(false),
        _ => Err(format!("expected on or off, not \"{s}\"")),
    }
}

/// Show the index settings, after applying any change.
fn settings(full_payloads: Option<bool>, redeem_scripts: Option<bool>) -> Result<()> {
    let mut s = IndexSettings::load()?;
    let before = s;
    if let Some(on) = full_payloads {
        s.set(IndexFeature::FullPayloads, on);
    }
    if let Some(on) = redeem_scripts {
        s.set(IndexFeature::RedeemScripts, on);
    }
    if s != before {
        s.save()?;
    }
    let mut out = serde_json::Map::new();
    for f in IndexFeature::ALL {
        out.insert(
            f.flag().trim_start_matches("--").replace('-', "_"),
            serde_json::Value::Bool(s.enabled(f)),
        );
    }
    println!("{}", serde_json::to_string_pretty(&out)?);
    if s != before {
        eprintln!(
            "saved {}: applies to transactions indexed from the next launch of `index run` \
             or the GUI on; Resync to cover the whole window",
            IndexSettings::path().display()
        );
    }
    Ok(())
}

#[derive(Serialize)]
struct Status {
    path: String,
    format: u32,
    txs_indexed: u64,
    blocks_indexed: u64,
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
        IndexCommand::Settings {
            full_payloads,
            redeem_scripts,
        } => settings(full_payloads, redeem_scripts),
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
    let store = IndexStore::open_existing(network)?;
    let manifest = store.manifest()?;
    let status = Status {
        path: index::index_dir(network).display().to_string(),
        format: index::FORMAT_VERSION,
        txs_indexed: manifest.txs_indexed,
        blocks_indexed: manifest.blocks_indexed,
        addresses: manifest.next_addr_id,
        slabs: store.slabs().len(),
        coverage: store.coverage(),
        position: manifest.position,
        disk_bytes: store.disk_space(),
    };
    println!("{}", serde_json::to_string_pretty(&status)?);
    Ok(())
}

/// The headless chain pipeline: the index writer fed by the chain stream, with node
/// polling (the sync state and pruning point, as in the GUI).
pub(crate) struct Pipeline {
    pub app: Arc<RwLock<App>>,
    pub handles: PollingHandles,
    pub rpc: Arc<x4kas_core::rpc::client::RpcManager>,
    pub store: Arc<IndexStore>,
}

/// Open the index and start the pipeline from `url`.
pub(crate) fn start_pipeline(
    url: &str,
    network: &str,
    backfill: Option<Duration>,
) -> Result<Pipeline> {
    let app = Arc::new(RwLock::new(App {
        index_settings: IndexSettings::load()?,
        ..App::default()
    }));
    let mut handles = PollingHandles::default();

    let store = Arc::new(IndexStore::open(network)?);
    let labels = Arc::new(x4kas_core::labels::LabelBook::load());
    let sink = index::task::start_writer(store.clone(), labels, app.clone(), &mut handles)?;
    let position = sink.stream_position();

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
    Ok(Pipeline {
        app,
        handles,
        rpc,
        store,
    })
}

/// Stop the pipeline: the stream, then the writer (which closes the store), then the node.
pub(crate) async fn stop_pipeline(mut pipeline: Pipeline) {
    eprintln!("stopping…");
    pipeline.handles.abort_all();
    pipeline.handles.stop_index().await;
    let _ = pipeline.rpc.disconnect().await;
}

async fn run_indexer(
    url: &str,
    network: &str,
    backfill: Option<Duration>,
    progress_secs: u64,
) -> Result<()> {
    let pipeline = start_pipeline(url, network, backfill)?;
    let mut ticker = tokio::time::interval(Duration::from_secs(progress_secs.max(1)));
    ticker.tick().await;
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = ticker.tick() => {
                if progress_secs > 0 {
                    eprintln!("{}", progress_line(&*pipeline.app.read().await));
                }
            }
        }
    }
    stop_pipeline(pipeline).await;
    Ok(())
}

/// One line of progress: the stream's phase and the writer's counters.
pub(crate) fn progress_line(app: &App) -> String {
    let index = &app.chain;
    let phase = match (&index.phase, &index.write_error) {
        (_, Some(e)) => format!("write error: {e}"),
        (ChainPhase::Error(e), _) => format!("stream error: {e}"),
        (ChainPhase::Live, _) => "live".to_string(),
        (ChainPhase::CatchingUp, _) => {
            let behind = index
                .position
                .map(|p| now_ms().saturating_sub(p.time_ms) / 1000)
                .map(|s| format!(", {s}s behind"))
                .unwrap_or_default();
            format!("catching up{behind}")
        }
        (phase, _) => format!("{phase:?}").to_lowercase(),
    };
    let refused = if index.cluster_cap_hits > 0 {
        format!(", {} cluster merges refused", index.cluster_cap_hits)
    } else {
        String::new()
    };
    format!(
        "{phase} | {} txs, {} addresses, {} slabs, {} MB | {} tx/s, backlog {}{refused}",
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
