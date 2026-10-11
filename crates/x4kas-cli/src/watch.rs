//! `x4kas-cli watch <address>…`: follow addresses live and print one JSON line per
//! confirmed balance change (with the alerts the watchlists' rules raised for it in its
//! `alerts`) until Ctrl+C.
//! Addresses given on the command line are watched with the default rules; without
//! any, the saved watchlists for the network (all, or those named with `--list`).

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use clap::Args;
use serde::Serialize;
use tokio::sync::RwLock;

use x4kas_core::app::{App, WatchPhase};
use x4kas_core::polling::{PollingHandles, create_and_start_rpc};
use x4kas_core::watch::{self, AddressEvent, WatchEdit, WatchEntry, Watchlists};

#[derive(Args, Debug, Clone, PartialEq)]
pub struct WatchArgs {
    /// Addresses to watch; none means the saved watchlists
    pub addresses: Vec<String>,
    /// Follow only this saved watchlist (repeatable); without it, all of them
    #[arg(long = "list", value_name = "NAME", conflicts_with = "addresses")]
    pub lists: Vec<String>,
    /// Also print the current balances once subscribed
    #[arg(long)]
    pub balances: bool,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Line<'a> {
    Status { phase: String },
    Balance { address: &'a str, balance: u64 },
    Event(&'a AddressEvent),
}

pub async fn run(url: Option<&str>, network: &str, args: WatchArgs) -> Result<()> {
    let app = Arc::new(RwLock::new(App::default()));
    {
        let mut app = app.write().await;
        app.watch.lists = if args.addresses.is_empty() {
            let mut lists = Watchlists::load()?;
            if !args.lists.is_empty() {
                let mut kept = Vec::new();
                for name in &args.lists {
                    let list = lists
                        .by_name(name)
                        .ok_or_else(|| anyhow!("no watchlist named {name:?}"))?;
                    // Named twice, followed once (its alerts would print twice).
                    if !kept.iter().any(|k: &watch::Watchlist| k.id == list.id) {
                        kept.push(list.clone());
                    }
                }
                lists.lists = kept;
            }
            lists
        } else {
            let mut lists = Watchlists::default();
            lists.ensure_default();
            for address in &args.addresses {
                let entry = WatchEntry::new(address, network);
                lists
                    .apply(WatchEdit::Put { list: 1, entry })
                    .map_err(|e| anyhow!(e))?;
            }
            lists
        };
        if app.watch.lists.addresses(network).is_empty() {
            return Err(anyhow!("nothing to watch on {network}"));
        }
    }

    let mut handles = PollingHandles::default();
    let rpc = create_and_start_rpc(url, network, &app, 1000, &mut handles)?;
    // Addresses from the command line aren't the saved lists: save nothing.
    let persist = args.addresses.is_empty();
    watch::start_watch(&rpc, &app, &mut handles, network, persist);

    let mut seen_events = 0u64;
    let mut last_phase = WatchPhase::Idle;
    let mut printed_balances = false;
    let mut ticker = tokio::time::interval(Duration::from_millis(250));
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = ticker.tick() => {
                let app = app.read().await;
                let watch = &app.watch;
                if watch.status.phase != last_phase {
                    last_phase = watch.status.phase.clone();
                    print(&Line::Status { phase: format!("{last_phase:?}") })?;
                }
                if args.balances && !printed_balances && matches!(last_phase, WatchPhase::Active(_)) {
                    printed_balances = true;
                    let mut balances: Vec<_> = watch.balances.iter().collect();
                    balances.sort();
                    for (address, balance) in balances {
                        print(&Line::Balance { address, balance: *balance })?;
                    }
                }
                // Events are newest first; print what arrived since last time.
                let new = (watch.events_raised - seen_events) as usize;
                for event in watch.events.iter().take(new).collect::<Vec<_>>().into_iter().rev() {
                    print(&Line::Event(event))?;
                }
                seen_events = watch.events_raised;
            }
        }
    }
    handles.abort_all();
    let _ = rpc.disconnect().await;
    Ok(())
}

fn print(line: &Line<'_>) -> Result<()> {
    println!("{}", serde_json::to_string(line)?);
    Ok(())
}
