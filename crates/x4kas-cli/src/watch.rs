//! `x4kas-cli watch <address>…`: follow addresses live and print one JSON line per
//! confirmed balance change (and the alerts the watchlist's rules raise) until Ctrl+C.
//! Addresses given on the command line are watched with the default rules; without
//! any, the saved watchlist for the network is used.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use clap::Args;
use serde::Serialize;
use tokio::sync::RwLock;

use x4kas_core::app::{App, WatchPhase};
use x4kas_core::polling::{PollingHandles, create_and_start_rpc};
use x4kas_core::watch::{self, AddressEvent, Alert, WatchEntry, Watchlist};

#[derive(Args, Debug, Clone, PartialEq)]
pub struct WatchArgs {
    /// Addresses to watch; none means the saved watchlist
    pub addresses: Vec<String>,
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
    Alert(&'a Alert),
}

pub async fn run(url: Option<&str>, network: &str, args: WatchArgs) -> Result<()> {
    let app = Arc::new(RwLock::new(App::default()));
    {
        let mut app = app.write().await;
        app.watch.list = if args.addresses.is_empty() {
            Watchlist::load()?
        } else {
            Watchlist {
                entries: args
                    .addresses
                    .iter()
                    .map(|a| WatchEntry::new(a, network))
                    .collect(),
            }
        };
        if app.watch.list.active(network).is_empty() {
            return Err(anyhow!("nothing to watch on {network}"));
        }
    }

    let mut handles = PollingHandles::default();
    let rpc = create_and_start_rpc(url, network, &app, 1000, &mut handles)?;
    watch::start_watch(&rpc, &app, &mut handles, network);

    let mut seen_events = 0usize;
    let mut seen_alerts = 0usize;
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
                // Events and alerts are newest first; print what arrived since last time.
                for event in watch.events.iter().take(watch.events.len() - seen_events.min(watch.events.len())).collect::<Vec<_>>().into_iter().rev() {
                    print(&Line::Event(event))?;
                }
                seen_events = watch.events.len();
                for alert in watch.alerts.iter().take(watch.alerts.len() - seen_alerts.min(watch.alerts.len())).collect::<Vec<_>>().into_iter().rev() {
                    print(&Line::Alert(alert))?;
                }
                seen_alerts = watch.alerts.len();
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
