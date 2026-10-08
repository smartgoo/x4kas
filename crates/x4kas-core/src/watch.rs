//! The watchlist: addresses the user follows, with live events from the node's
//! `UtxosChanged` notifications and alerts from per-address rules. Independent of the
//! address index, so it is live from the first notification and works through the
//! resolver when the node has a UTXO index.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use kaspa_rpc_core::{RpcAddress, UtxosChangedNotification};
use serde::{Deserialize, Serialize};
use tokio::sync::{RwLock, mpsc};

use crate::app::{App, ConnectionStatus, WatchPhase};
use crate::config;
use crate::format::{format_kas, now_ms};
use crate::polling::PollingHandles;
use crate::rpc::client::RpcManager;

/// Events and alerts kept in memory.
pub const MAX_EVENTS: usize = 500;
pub const MAX_ALERTS: usize = 200;
/// How often balances are re-read from the node (notifications keep them current in
/// between) and pending mempool activity is polled.
const REFRESH_INTERVAL: Duration = Duration::from_secs(10);
const NODE_CHECK_INTERVAL: Duration = Duration::from_secs(1);

/// When to raise an alert for a watched address. Amounts in sompi.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AlertRules {
    /// Any confirmed incoming or outgoing payment.
    pub any_activity: bool,
    pub received_min: Option<u64>,
    pub sent_min: Option<u64>,
    pub balance_below: Option<u64>,
    pub balance_above: Option<u64>,
}

impl Default for AlertRules {
    fn default() -> Self {
        Self {
            any_activity: true,
            received_min: None,
            sent_min: None,
            balance_below: None,
            balance_above: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WatchEntry {
    pub address: String,
    pub name: Option<String>,
    pub network: String,
    pub enabled: bool,
    pub rules: AlertRules,
}

impl Default for WatchEntry {
    fn default() -> Self {
        Self {
            address: String::new(),
            name: None,
            network: "mainnet".to_string(),
            enabled: true,
            rules: AlertRules::default(),
        }
    }
}

impl WatchEntry {
    pub fn new(address: &str, network: &str) -> Self {
        Self {
            address: address.trim().to_string(),
            network: network.to_string(),
            ..Default::default()
        }
    }

    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.address)
    }
}

/// The persisted watchlist, `~/.x4kas/watchlist.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Watchlist {
    pub entries: Vec<WatchEntry>,
}

impl Watchlist {
    pub fn path() -> PathBuf {
        config::data_dir().join("watchlist.toml")
    }

    pub fn load() -> Result<Self> {
        let path = Self::path();
        if !path.exists() {
            return Ok(Self::default());
        }
        Ok(toml::from_str(&std::fs::read_to_string(&path)?)?)
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, toml::to_string_pretty(self)?)?;
        Ok(())
    }

    /// Enabled addresses on `network`, each once.
    pub fn active(&self, network: &str) -> Vec<&WatchEntry> {
        let mut seen = HashSet::new();
        self.entries
            .iter()
            .filter(|e| e.enabled && e.network == network && seen.insert(e.address.as_str()))
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventKind {
    Received,
    Sent,
}

/// A confirmed balance change of a watched address, coalesced per notification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddressEvent {
    pub time_ms: u64,
    pub address: String,
    pub kind: EventKind,
    /// Net amount in sompi.
    pub amount: u64,
    /// The transaction that paid the address, when it is a single one (a spend's
    /// transaction isn't in the notification, only the UTXOs it consumed).
    pub txid: Option<String>,
    pub is_coinbase: bool,
    pub balance_after: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Alert {
    pub time_ms: u64,
    pub address: String,
    pub name: Option<String>,
    pub message: String,
}

/// Which rules an event trips; one message per rule, in rule order.
pub fn evaluate(
    rules: &AlertRules,
    event: &AddressEvent,
    balance_before: Option<u64>,
) -> Vec<String> {
    let mut out = Vec::new();
    let kas = |sompi: u64| format!("{} KAS", format_kas(sompi as f64, 8));
    let (verb, prep) = match event.kind {
        EventKind::Received => ("received", "from"),
        EventKind::Sent => ("sent", "to"),
    };
    let _ = prep;
    if rules.any_activity {
        out.push(format!("{verb} {}", kas(event.amount)));
    }
    if let (EventKind::Received, Some(min)) = (event.kind, rules.received_min)
        && event.amount >= min
    {
        out.push(format!("received {} (≥ {})", kas(event.amount), kas(min)));
    }
    if let (EventKind::Sent, Some(min)) = (event.kind, rules.sent_min)
        && event.amount >= min
    {
        out.push(format!("sent {} (≥ {})", kas(event.amount), kas(min)));
    }
    // Balance thresholds fire on the crossing, not on every event below/above.
    if let (Some(limit), Some(after)) = (rules.balance_below, event.balance_after)
        && after < limit
        && balance_before.is_none_or(|b| b >= limit)
    {
        out.push(format!("balance fell below {}: {}", kas(limit), kas(after)));
    }
    if let (Some(limit), Some(after)) = (rules.balance_above, event.balance_after)
        && after > limit
        && balance_before.is_none_or(|b| b <= limit)
    {
        out.push(format!("balance rose above {}: {}", kas(limit), kas(after)));
    }
    out
}

/// Per-address net change in one notification.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct NetChange {
    pub added: u64,
    pub removed: u64,
    pub txids: HashSet<String>,
    pub coinbase: bool,
}

/// Group a notification's UTXO changes by address.
pub fn net_changes(n: &UtxosChangedNotification) -> HashMap<String, NetChange> {
    let mut by_addr: HashMap<String, NetChange> = HashMap::new();
    for entry in n.added.iter() {
        let Some(addr) = entry.address.as_ref() else {
            continue;
        };
        let change = by_addr.entry(addr.to_string()).or_default();
        change.added += entry.utxo_entry.amount;
        change
            .txids
            .insert(entry.outpoint.transaction_id.to_string());
        change.coinbase |= entry.utxo_entry.is_coinbase;
    }
    for entry in n.removed.iter() {
        let Some(addr) = entry.address.as_ref() else {
            continue;
        };
        by_addr.entry(addr.to_string()).or_default().removed += entry.utxo_entry.amount;
    }
    by_addr
}

/// Start watching the enabled entries for `network`, tracked in `handles.watch`. Waits
/// for a synced node with a UTXO index; seeds balances, then follows notifications.
pub fn start_watch(
    rpc: &Arc<RpcManager>,
    app: &Arc<RwLock<App>>,
    handles: &mut PollingHandles,
    network: &str,
) {
    let rpc = rpc.clone();
    let app = app.clone();
    let network = network.to_string();
    handles.watch = Some(tokio::spawn(async move {
        run(rpc, app, network).await;
    }));
}

async fn run(rpc: Arc<RpcManager>, app: Arc<RwLock<App>>, network: String) {
    let addresses: Vec<String> = {
        let app = app.read().await;
        app.watch
            .list
            .active(&network)
            .iter()
            .map(|e| e.address.clone())
            .collect()
    };
    if addresses.is_empty() {
        set_phase(&app, WatchPhase::Idle).await;
        return;
    }
    let parsed: Vec<RpcAddress> = addresses
        .iter()
        .filter_map(|a| RpcAddress::try_from(a.as_str()).ok())
        .collect();

    // Wait for a synced node that can answer address queries.
    loop {
        let (ready, has_index) = {
            let app = app.read().await;
            let info = app.node.server_info.as_ref();
            (
                matches!(app.node.connection_status, ConnectionStatus::Connected)
                    && info.is_some_and(|s| s.is_synced),
                info.is_some_and(|s| s.has_utxo_index),
            )
        };
        if ready && has_index {
            break;
        }
        let phase = if ready {
            WatchPhase::Unavailable("The node has no UTXO index (start it with --utxoindex)".into())
        } else {
            WatchPhase::Waiting
        };
        set_phase(&app, phase).await;
        tokio::time::sleep(NODE_CHECK_INTERVAL).await;
    }

    let (sender, mut receiver) = mpsc::channel::<UtxosChangedNotification>(64);
    let subscription = rpc.watch_utxos(parsed.clone(), sender);
    let events = async {
        refresh(&rpc, &app, &parsed).await;
        set_phase(&app, WatchPhase::Active(parsed.len())).await;
        let mut ticker = tokio::time::interval(REFRESH_INTERVAL);
        ticker.tick().await;
        loop {
            tokio::select! {
                Some(n) = receiver.recv() => handle_notification(&app, &n).await,
                _ = ticker.tick() => refresh(&rpc, &app, &parsed).await,
            }
        }
    };
    tokio::join!(subscription, events);
}

/// Re-read balances and pending mempool activity from the node.
async fn refresh(rpc: &RpcManager, app: &RwLock<App>, addresses: &[RpcAddress]) {
    let balances = rpc.balances(addresses.to_vec()).await;
    let pending = rpc.pending_by_addresses(addresses.to_vec()).await;
    let mut app = app.write().await;
    match balances {
        Ok(list) => {
            for (address, balance) in list {
                app.watch.balances.insert(address, balance);
            }
        }
        Err(e) => app.watch.status.last_error = Some(format!("balances: {e}")),
    }
    match pending {
        Ok(list) => {
            app.watch.pending.clear();
            for (address, incoming, outgoing) in list {
                if incoming > 0 || outgoing > 0 {
                    app.watch.pending.insert(address, (incoming, outgoing));
                }
            }
        }
        Err(e) => app.watch.status.last_error = Some(format!("mempool: {e}")),
    }
    app.mark_dirty();
}

async fn handle_notification(app: &RwLock<App>, n: &UtxosChangedNotification) {
    let changes = net_changes(n);
    if changes.is_empty() {
        return;
    }
    let time_ms = now_ms();
    let mut app = app.write().await;
    for (address, change) in changes {
        let (kind, amount) = if change.added >= change.removed {
            (EventKind::Received, change.added - change.removed)
        } else {
            (EventKind::Sent, change.removed - change.added)
        };
        let before = app.watch.balances.get(&address).copied();
        let after = before.map(|b| (b + change.added).saturating_sub(change.removed));
        if let Some(after) = after {
            app.watch.balances.insert(address.clone(), after);
        }
        if amount == 0 {
            continue;
        }
        let txid = (change.txids.len() == 1)
            .then(|| change.txids.into_iter().next())
            .flatten();
        let event = AddressEvent {
            time_ms,
            address: address.clone(),
            kind,
            amount,
            txid,
            is_coinbase: change.coinbase,
            balance_after: after,
        };
        let entry = app
            .watch
            .list
            .entries
            .iter()
            .find(|e| e.address == address)
            .cloned();
        if let Some(entry) = entry {
            for message in evaluate(&entry.rules, &event, before) {
                app.watch.push_alert(Alert {
                    time_ms,
                    address: address.clone(),
                    name: entry.name.clone(),
                    message,
                });
            }
        }
        app.watch.push_event(event);
    }
    app.watch.status.last_event_at = Some(std::time::Instant::now());
    app.mark_dirty();
}

async fn set_phase(app: &RwLock<App>, phase: WatchPhase) {
    let mut app = app.write().await;
    if app.watch.status.phase != phase {
        app.watch.status.phase = phase;
        app.mark_dirty();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: EventKind, amount: u64, after: Option<u64>) -> AddressEvent {
        AddressEvent {
            time_ms: 0,
            address: "kaspa:x".into(),
            kind,
            amount,
            txid: None,
            is_coinbase: false,
            balance_after: after,
        }
    }

    #[test]
    fn rules_fire_on_amounts_and_crossings() {
        let rules = AlertRules {
            any_activity: false,
            received_min: Some(100),
            sent_min: Some(50),
            balance_below: Some(1_000),
            balance_above: None,
        };
        assert!(
            evaluate(
                &rules,
                &event(EventKind::Received, 99, Some(5_000)),
                Some(5_000)
            )
            .is_empty()
        );
        assert_eq!(
            evaluate(
                &rules,
                &event(EventKind::Received, 100, Some(5_000)),
                Some(5_000)
            )
            .len(),
            1
        );
        // Falling below fires once, on the crossing.
        assert_eq!(
            evaluate(&rules, &event(EventKind::Sent, 60, Some(900)), Some(1_200)).len(),
            2
        );
        assert_eq!(
            evaluate(&rules, &event(EventKind::Sent, 60, Some(800)), Some(900)).len(),
            1
        );
        // Unknown previous balance counts as a crossing.
        assert_eq!(
            evaluate(
                &AlertRules {
                    sent_min: None,
                    ..rules.clone()
                },
                &event(EventKind::Sent, 1, Some(10)),
                None
            )
            .len(),
            1
        );
        let any = AlertRules::default();
        assert_eq!(
            evaluate(&any, &event(EventKind::Received, 5, None), None),
            vec!["received 0.00000005 KAS".to_string()]
        );
    }

    #[test]
    fn watchlist_roundtrip_and_active_filter() {
        let list = Watchlist {
            entries: vec![
                WatchEntry::new("kaspa:a", "mainnet"),
                WatchEntry {
                    enabled: false,
                    ..WatchEntry::new("kaspa:b", "mainnet")
                },
                WatchEntry::new("kaspa:a", "mainnet"),
                WatchEntry::new("kaspa:c", "testnet-10"),
            ],
        };
        let text = toml::to_string_pretty(&list).unwrap();
        let loaded: Watchlist = toml::from_str(&text).unwrap();
        assert_eq!(loaded, list);
        let active: Vec<&str> = list
            .active("mainnet")
            .iter()
            .map(|e| e.address.as_str())
            .collect();
        assert_eq!(active, vec!["kaspa:a"]);
        let minimal: Watchlist = toml::from_str("[[entries]]\naddress = \"kaspa:z\"\n").unwrap();
        assert!(minimal.entries[0].enabled && minimal.entries[0].rules.any_activity);
    }
}
