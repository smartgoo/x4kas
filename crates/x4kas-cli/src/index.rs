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
use x4kas_core::config::{self, IndexFeature, IndexSettings, IndexSettingsEdit, Retention};
use x4kas_core::format::{format_number, now_ms};
use x4kas_core::index::{self, IndexStore};
use x4kas_core::polling::{PollingHandles, create_and_start_rpc};

#[derive(Subcommand, Debug, Clone, PartialEq)]
pub enum IndexCommand {
    /// Connect to a node and keep the index current until Ctrl+C
    Run {
        /// How many hours back to fill in on start; 0 for everything the node retains
        /// (default 24, or the retention period of `index settings` when longer)
        #[arg(long)]
        backfill_hours: Option<f64>,
        /// Print progress this often, in seconds; 0 for no progress output
        #[arg(long, default_value = "5")]
        progress: u64,
        /// Delete the index first and rebuild it from the node, as the GUI's Resync does
        /// (and with the slab size of `index settings`)
        #[arg(long)]
        resync: bool,
    },
    /// What the index on disk holds
    Status,
    /// Show or change the opt-in data the index keeps (off by default), how long it
    /// keeps data (`index run` and the GUI apply a change from their next launch, the
    /// GUI's Settings › DAG Index at once) and the slab size of the next index built
    Settings {
        /// Keep every transaction's whole payload, not only its first 128 bytes: on|off
        #[arg(long, value_parser = parse_on_off)]
        full_payloads: Option<bool>,
        /// Keep the redeem script every P2SH spend reveals: on|off
        #[arg(long, value_parser = parse_on_off)]
        redeem_scripts: Option<bool>,
        /// How long the index keeps data: `node` (the default) mirrors the node, pruning
        /// behind its pruning point; `<N>d` keeps N days (1 to 365), less or more than
        /// the node keeps. Shortening it deletes older data at the next batch
        #[arg(long, value_parser = parse_retention)]
        retention: Option<Retention>,
        /// The width of the index's time slabs, in hours (1, 2, 3, 4, 6, 8, 12 or 24;
        /// default 6): applies when the index is next rebuilt (`index run --resync`)
        ///
        /// A slab is one time window of the index: its own set of keyspaces (the separate
        /// key-value trees of fjall, the embedded database the index is stored in, see
        /// https://docs.rs/fjall), so a whole window is deleted at once.
        #[arg(long, value_parser = parse_slab_hours)]
        slab_hours: Option<u32>,
    },
}

fn parse_slab_hours(s: &str) -> Result<u32, String> {
    let hours = s
        .parse()
        .map_err(|_| format!("expected a number of hours, not \"{s}\""))?;
    config::validate_slab_hours(hours)?;
    Ok(hours)
}

fn parse_retention(s: &str) -> Result<Retention, String> {
    s.parse()
}

pub(crate) fn parse_on_off(s: &str) -> Result<bool, String> {
    match s.to_ascii_lowercase().as_str() {
        "on" | "true" | "yes" | "1" => Ok(true),
        "off" | "false" | "no" | "0" => Ok(false),
        _ => Err(format!("expected on or off, not \"{s}\"")),
    }
}

/// Show the index settings, after applying any change.
fn settings(
    network: &str,
    full_payloads: Option<bool>,
    redeem_scripts: Option<bool>,
    retention: Option<Retention>,
    slab_hours: Option<u32>,
) -> Result<()> {
    let (before, warnings) = IndexSettings::load_with_warnings()?;
    // A key this command sets no longer has the value warned about.
    let setting = |key: &str| match key {
        "retention" => retention.is_some(),
        "slab_hours" => slab_hours.is_some(),
        _ => false,
    };
    for (_, w) in warnings.iter().filter(|(key, _)| !setting(key)) {
        eprintln!("{}: {w}", IndexSettings::path().display());
    }
    let edits: Vec<IndexSettingsEdit> = [
        full_payloads.map(|on| IndexSettingsEdit::Feature(IndexFeature::FullPayloads, on)),
        redeem_scripts.map(|on| IndexSettingsEdit::Feature(IndexFeature::RedeemScripts, on)),
        retention.map(IndexSettingsEdit::Retention),
        slab_hours.map(IndexSettingsEdit::SlabHours),
    ]
    .into_iter()
    .flatten()
    .collect();
    let mut s = before;
    for edit in &edits {
        s.apply(*edit);
    }
    // Also save when a key set had a value that didn't load, though the fallback
    // matches, so the file stops warning.
    let replaces_bad = warnings.iter().any(|(key, _)| setting(key));
    let save = s != before || replaces_bad;
    if save {
        // Applied to the file as it is now, under its lock, so a GUI saving at the same
        // moment loses nothing.
        s = IndexSettings::update_file(&edits)?.0;
    }
    let mut out = serde_json::Map::new();
    for f in IndexFeature::ALL {
        out.insert(
            f.flag().trim_start_matches("--").replace('-', "_"),
            serde_json::Value::Bool(s.enabled(f)),
        );
    }
    out.insert("retention".into(), s.retention.to_string().into());
    out.insert("slab_hours".into(), s.slab_hours.into());
    println!("{}", serde_json::to_string_pretty(&out)?);
    if save {
        eprintln!("saved {}", IndexSettings::path().display());
    }
    let features = |s: &IndexSettings| IndexFeature::ALL.map(|f| s.enabled(f));
    if features(&s) != features(&before) {
        eprintln!(
            "the features apply to transactions indexed from the next launch of `index run` \
             or the GUI on; Resync to cover the whole window"
        );
    }
    if s.retention != before.retention {
        eprintln!(
            "the retention applies from the next launch of `index run` or the GUI: {}",
            retention_note(before.retention, s.retention)
        );
    }
    // The width the index on disk was built with, if there is one to read.
    let in_use = if index::index_dir(network).is_dir() {
        match IndexStore::open_existing(network) {
            Ok(store) => Some(store.slab_hours()),
            Err(e) => {
                eprintln!("couldn't check the {network} index's slab size: {e:#}");
                None
            }
        }
    } else {
        None
    };
    if let Some(hours) = in_use.filter(|h| *h != s.slab_hours) {
        eprintln!(
            "the {network} index has {hours}h slabs: rebuild it with `index run --resync` \
             (or Resync in the GUI) for {}h ones",
            s.slab_hours
        );
    }
    Ok(())
}

/// What changing the retention from `before` to `after` does to the index on disk.
fn retention_note(before: Retention, after: Retention) -> String {
    let grows = "the index doesn't fetch older data but grows into the period from now on";
    match (before.duration_ms(), after.duration_ms()) {
        (_, None) => "data before the node's pruning point is deleted".to_string(),
        // The node's period isn't known offline: either may be the longer.
        (None, Some(_)) => format!(
            "data older than {} is deleted; if that is longer than the node keeps, {grows}",
            after.label()
        ),
        (Some(b), Some(a)) if a >= b => format!("nothing is deleted; {grows}"),
        (Some(_), Some(_)) => format!("data older than {} is deleted", after.label()),
    }
}

#[derive(Serialize)]
struct Status {
    path: String,
    format: u32,
    txs_indexed: u64,
    blocks_indexed: u64,
    addresses: u32,
    slabs: usize,
    /// The width of the index's slabs, in hours
    slab_hours: u32,
    /// How long the index keeps data (`index settings`): `node` or `<N>d`; `None` when
    /// the settings file doesn't load
    retention: Option<String>,
    /// The time before which data is deleted, for a fixed retention (the node's
    /// pruning point isn't known offline)
    retention_floor_ms: Option<u64>,
    /// `(from_ms, to_ms)`
    coverage: Option<(u64, u64)>,
    /// Stretches between the data held that the index has no data for (it skipped
    /// ahead after a long stop), oldest first
    gaps: Vec<index::Gap>,
    position: Option<index::Position>,
    disk_bytes: u64,
}

pub async fn run(url: Option<&str>, network: &str, cmd: IndexCommand) -> Result<()> {
    match cmd {
        IndexCommand::Status => status(network),
        IndexCommand::Settings {
            full_payloads,
            redeem_scripts,
            retention,
            slab_hours,
        } => settings(
            network,
            full_payloads,
            redeem_scripts,
            retention,
            slab_hours,
        ),
        IndexCommand::Run {
            backfill_hours,
            progress,
            resync,
        } => {
            let url = url.ok_or_else(|| {
                anyhow!("index run needs a direct node: pass --url (the resolver won't do)")
            })?;
            if resync {
                index::discard(network)?;
                eprintln!("deleted the {network} index: rebuilding it from the node");
            }
            run_indexer(url, network, backfill_hours, progress).await
        }
    }
}

fn status(network: &str) -> Result<()> {
    let store = IndexStore::open_existing(network)?;
    let manifest = store.manifest()?;
    // Only extra information: a settings file that doesn't load leaves it out.
    let retention = match IndexSettings::load() {
        Ok(settings) => Some(settings.retention),
        Err(e) => {
            eprintln!("{e:#}");
            None
        }
    };
    let status = Status {
        path: index::index_dir(network).display().to_string(),
        format: index::FORMAT_VERSION,
        txs_indexed: manifest.txs_indexed,
        blocks_indexed: manifest.blocks_indexed,
        addresses: manifest.next_addr_id,
        slabs: store.slabs().len(),
        slab_hours: store.slab_hours(),
        retention: retention.map(|r| r.to_string()),
        retention_floor_ms: retention.and_then(|r| r.floor_ms(now_ms(), None)),
        coverage: store.coverage(),
        gaps: store.gaps()?,
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

/// Open the index and start the pipeline from `url`, filling in `backfill_hours` back
/// (0 for everything the node has; by default 24, or the retention period when longer).
pub(crate) fn start_pipeline(
    url: &str,
    network: &str,
    backfill_hours: Option<f64>,
) -> Result<Pipeline> {
    let (index_settings, warning) = IndexSettings::load_with_warning()?;
    if let Some(w) = warning {
        eprintln!("{}: {w}", IndexSettings::path().display());
    }
    let backfill = match backfill_hours {
        Some(hours) => (hours > 0.0).then(|| Duration::from_secs_f64(hours * 3600.0)),
        None => StreamStart::backfill_for(
            Some(StreamStart::default_backfill()),
            index_settings.retention,
        ),
    };
    let app = Arc::new(RwLock::new(App {
        index_settings,
        ..App::default()
    }));
    let mut handles = PollingHandles::default();

    let store = Arc::new(IndexStore::open(network, index_settings.slab_hours)?);
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
        "indexing {network} from {url} into {}, retention: {}",
        index::index_dir(network).display(),
        index_settings.retention.label()
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
    backfill_hours: Option<f64>,
    progress_secs: u64,
) -> Result<()> {
    let pipeline = start_pipeline(url, network, backfill_hours)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retention_notes_say_what_is_deleted() {
        let node = Retention::Node;
        let days = Retention::Days;
        assert!(retention_note(days(7), node).contains("pruning point"));
        // The node's period isn't known offline, so going to a fixed one says both.
        let note = retention_note(node, days(90));
        assert!(
            note.contains("90 days") && note.contains("grows into"),
            "{note}"
        );
        assert!(retention_note(days(7), days(30)).starts_with("nothing is deleted"));
        assert!(retention_note(days(30), days(7)).contains("older than 7 days is deleted"));
    }
}
