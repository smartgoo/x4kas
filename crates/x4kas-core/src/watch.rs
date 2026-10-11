//! The watchlist: addresses the user follows, with live events from the node's
//! `UtxosChanged` notifications and alerts from per-address rules. Independent of the
//! address index, so it is live from the first notification and works through the
//! resolver when the node has a UTXO index.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use kaspa_rpc_core::{RpcAddress, UtxosChangedNotification};
use kaspa_wrpc_client::prelude::NetworkId;
use serde::{Deserialize, Serialize};
use tokio::sync::{RwLock, mpsc};

use crate::app::{App, ConnectionStatus, WatchPhase};
use crate::config;
use crate::format::{format_kas, now_ms};
use crate::polling::PollingHandles;
use crate::rpc::client::RpcManager;

/// Events kept in memory.
pub const MAX_EVENTS: usize = 500;
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
    /// First activity after at least this many hours without any.
    pub idle_hours: Option<u32>,
}

impl Default for AlertRules {
    fn default() -> Self {
        Self {
            any_activity: true,
            received_min: None,
            sent_min: None,
            balance_below: None,
            balance_above: None,
            idle_hours: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WatchEntry {
    pub address: String,
    pub network: String,
    pub enabled: bool,
    pub rules: AlertRules,
    /// When the address last moved funds while watched (kept across restarts for the
    /// idle rule); `None` until the first event.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_activity_ms: Option<u64>,
}

impl Default for WatchEntry {
    fn default() -> Self {
        Self {
            address: String::new(),
            network: "mainnet".to_string(),
            enabled: true,
            rules: AlertRules::default(),
            last_activity_ms: None,
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
}

/// A watchlist's stable id: names can change, ids don't.
pub type ListId = u32;

/// The name the list migrated from the single watchlist of earlier versions gets, and
/// the one `Watchlists::ensure_default` creates.
pub const DEFAULT_LIST: &str = "Default";

/// One named watchlist. An address is on it at most once per network.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Watchlist {
    pub id: ListId,
    pub name: String,
    pub entries: Vec<WatchEntry>,
}

impl Watchlist {
    pub fn entry(&self, address: &str, network: &str) -> Option<&WatchEntry> {
        self.entries
            .iter()
            .find(|e| e.address == address && e.network == network)
    }

    fn entry_mut(&mut self, address: &str, network: &str) -> Option<&mut WatchEntry> {
        self.entries
            .iter_mut()
            .find(|e| e.address == address && e.network == network)
    }

    /// Its entries on `network`.
    pub fn on_network<'a>(&'a self, network: &'a str) -> impl Iterator<Item = &'a WatchEntry> {
        self.entries.iter().filter(move |e| e.network == network)
    }
}

/// A change to the watchlists. Every frontend edits through [`Watchlists::apply`]
/// (and [`Watchlists::update_file`]), so the GUI, the CLI and the watch task never
/// overwrite each other's changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchEdit {
    CreateList {
        name: String,
    },
    RenameList {
        id: ListId,
        name: String,
    },
    /// Refused for the last list: there is always one to add to.
    DeleteList {
        id: ListId,
    },
    /// Add the entry to the list, or replace the one with its address and network.
    Put {
        list: ListId,
        entry: WatchEntry,
    },
    Remove {
        list: ListId,
        address: String,
        network: String,
    },
    /// Move an entry, rules and last activity included, to another list; when that
    /// list already has the address, it only leaves `from`.
    Move {
        from: ListId,
        to: ListId,
        address: String,
        network: String,
    },
    /// The watch task's record of activity, for the idle rule: sets the entries'
    /// `last_activity_ms` on every list that has the address.
    Touch {
        address: String,
        network: String,
        time_ms: u64,
    },
}

/// The persisted watchlists, `~/.x4kas/watchlist.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Watchlists {
    /// The id the next list gets. Ids are never reused, so an id held by the GUI (its
    /// prefs, an open dialog) can't come to mean a list created after its own was
    /// deleted.
    pub next_id: ListId,
    pub lists: Vec<Watchlist>,
}

/// The file as read: `lists`, or the single list's `entries` of earlier versions.
#[derive(Deserialize, Default)]
#[serde(default)]
struct WatchlistFile {
    next_id: ListId,
    lists: Vec<Watchlist>,
    entries: Vec<WatchEntry>,
}

impl Watchlists {
    pub fn path() -> PathBuf {
        config::data_dir().join("watchlist.toml")
    }

    /// The file's lists (none when it doesn't exist). The single watchlist of earlier
    /// versions becomes one list named [`DEFAULT_LIST`].
    pub fn load() -> Result<Self> {
        Self::load_from(&Self::path())
    }

    fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        Self::parse(&std::fs::read_to_string(path)?)
    }

    fn parse(text: &str) -> Result<Self> {
        let file: WatchlistFile = toml::from_str(text)?;
        let mut lists = Self {
            next_id: file.next_id,
            lists: file.lists,
        };
        if lists.lists.is_empty() && !file.entries.is_empty() {
            lists.lists.push(Watchlist {
                id: 1,
                name: DEFAULT_LIST.to_string(),
                entries: file.entries,
            });
        }
        // An address is on a list once per network; the single list of earlier
        // versions allowed it twice, so keep the first.
        for list in &mut lists.lists {
            let mut seen = HashSet::new();
            list.entries
                .retain(|e| seen.insert((e.address.clone(), e.network.clone())));
        }
        lists.next_id = lists.fresh_id();
        Ok(lists)
    }

    /// An id no list has had: past `next_id` and every list's.
    fn fresh_id(&self) -> ListId {
        let past = self.lists.iter().map(|l| l.id + 1).max().unwrap_or(1);
        self.next_id.max(past)
    }

    fn take_id(&mut self) -> ListId {
        let id = self.fresh_id();
        self.next_id = id + 1;
        id
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::path())
    }

    /// Write to a temporary file and rename it over the file, so a reader never sees
    /// it half written.
    fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, toml::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Load the file, apply `edit`, save it and return the result, so a change made by
    /// another process (the CLI while the GUI runs) since the last load is kept.
    pub fn update_file(edit: impl FnOnce(&mut Self) -> Result<(), String>) -> Result<Self> {
        Self::update_at(&Self::path(), edit)
    }

    /// Under an exclusive lock on a file beside it, so two writers (the GUI's watch
    /// task and its controller, the CLI) take turns instead of losing one's change.
    fn update_at(path: &Path, edit: impl FnOnce(&mut Self) -> Result<(), String>) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let lock = std::fs::File::create(path.with_extension("toml.lock"))?;
        lock.lock()?;
        let mut lists = Self::load_from(path)?;
        lists.ensure_default();
        edit(&mut lists).map_err(anyhow::Error::msg)?;
        lists.save_to(path)?;
        Ok(lists)
    }

    /// Make sure there is a list to add to.
    pub fn ensure_default(&mut self) {
        if self.lists.is_empty() {
            let id = self.take_id();
            self.lists.push(Watchlist {
                id,
                name: DEFAULT_LIST.to_string(),
                entries: Vec::new(),
            });
        }
    }

    pub fn get(&self, id: ListId) -> Option<&Watchlist> {
        self.lists.iter().find(|l| l.id == id)
    }

    fn get_mut(&mut self, id: ListId) -> Result<&mut Watchlist, String> {
        self.lists
            .iter_mut()
            .find(|l| l.id == id)
            .ok_or_else(|| "No such watchlist".to_string())
    }

    /// The list named `name`, ignoring case and surrounding space.
    pub fn by_name(&self, name: &str) -> Option<&Watchlist> {
        let name = name.trim();
        self.lists
            .iter()
            .find(|l| l.name.eq_ignore_ascii_case(name))
    }

    /// The lists with `address` on `network`, with its entry on each, in list order.
    pub fn memberships(&self, address: &str, network: &str) -> Vec<(&Watchlist, &WatchEntry)> {
        self.lists
            .iter()
            .filter_map(|l| l.entry(address, network).map(|e| (l, e)))
            .collect()
    }

    /// The latest activity any list has recorded for `address` on `network`.
    pub fn last_activity(&self, address: &str, network: &str) -> Option<u64> {
        self.memberships(address, network)
            .iter()
            .filter_map(|(_, e)| e.last_activity_ms)
            .max()
    }

    pub fn is_watched(&self, address: &str, network: &str) -> bool {
        self.lists
            .iter()
            .any(|l| l.entry(address, network).is_some())
    }

    /// Every address on any list on `network`, each once, in list order. The watch task
    /// follows them all; an entry's `enabled` only decides its alerts.
    pub fn addresses(&self, network: &str) -> Vec<&str> {
        let mut seen = HashSet::new();
        self.lists
            .iter()
            .flat_map(|l| &l.entries)
            .filter(|e| e.network == network)
            .map(|e| e.address.as_str())
            .filter(|a| seen.insert(*a))
            .collect()
    }

    /// Why `name` can't name a list (other than `except`), if it can't.
    pub fn validate_name(&self, name: &str, except: Option<ListId>) -> Result<(), String> {
        let name = name.trim();
        if name.is_empty() {
            return Err("A watchlist needs a name".into());
        }
        match self.by_name(name) {
            Some(l) if Some(l.id) != except => {
                Err(format!("There is already a list named {}", l.name))
            }
            _ => Ok(()),
        }
    }

    /// Apply `edit`; on an error nothing changed.
    pub fn apply(&mut self, edit: WatchEdit) -> Result<(), String> {
        match edit {
            WatchEdit::CreateList { name } => {
                self.validate_name(&name, None)?;
                let id = self.take_id();
                self.lists.push(Watchlist {
                    id,
                    name: name.trim().to_string(),
                    entries: Vec::new(),
                });
            }
            WatchEdit::RenameList { id, name } => {
                self.validate_name(&name, Some(id))?;
                self.get_mut(id)?.name = name.trim().to_string();
            }
            WatchEdit::DeleteList { id } => {
                self.get_mut(id)?;
                if self.lists.len() == 1 {
                    return Err("The last watchlist can't be deleted".into());
                }
                self.next_id = self.fresh_id();
                self.lists.retain(|l| l.id != id);
            }
            WatchEdit::Put { list, mut entry } => {
                entry.address = entry.address.trim().to_string();
                // Activity is the address's, not the list's: an entry new to this list
                // starts from the latest the other lists have seen, so its idle rule
                // has a history.
                let seen = self
                    .memberships(&entry.address, &entry.network)
                    .iter()
                    .filter_map(|(_, e)| e.last_activity_ms)
                    .max();
                entry.last_activity_ms = entry.last_activity_ms.max(seen);
                let list = self.get_mut(list)?;
                match list.entry_mut(&entry.address, &entry.network) {
                    // The watch task may have recorded activity since the entry was
                    // copied (into a dialog, say): keep the later time.
                    Some(existing) => {
                        entry.last_activity_ms =
                            entry.last_activity_ms.max(existing.last_activity_ms);
                        *existing = entry;
                    }
                    None => list.entries.push(entry),
                }
            }
            WatchEdit::Remove {
                list,
                address,
                network,
            } => {
                self.get_mut(list)?
                    .entries
                    .retain(|e| !(e.address == address && e.network == network));
            }
            WatchEdit::Move {
                from,
                to,
                address,
                network,
            } => {
                if from == to {
                    return Ok(());
                }
                self.get_mut(to)?;
                let source = self.get_mut(from)?;
                let Some(at) = source
                    .entries
                    .iter()
                    .position(|e| e.address == address && e.network == network)
                else {
                    return Err("The address isn't on that watchlist".into());
                };
                let entry = source.entries.remove(at);
                let target = self.get_mut(to)?;
                if target.entry(&address, &network).is_none() {
                    target.entries.push(entry);
                }
            }
            WatchEdit::Touch {
                address,
                network,
                time_ms,
            } => {
                for list in &mut self.lists {
                    if let Some(entry) = list.entry_mut(&address, &network) {
                        entry.last_activity_ms = Some(time_ms);
                    }
                }
            }
        }
        Ok(())
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
    /// The messages of the address's alert rules this event tripped ([`event_alerts`]),
    /// each led by its list's name when the address is on more than one; empty when
    /// none did, or the address isn't watched.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alerts: Vec<String>,
    /// The user has marked the alert as read (only meaningful with `alerts`).
    #[serde(skip)]
    pub read: bool,
}

impl AddressEvent {
    /// An alert the user hasn't marked as read yet.
    pub fn unread(&self) -> bool {
        !self.alerts.is_empty() && !self.read
    }
}

/// Which rules an event trips; one message per rule, in rule order. `balance_before`
/// and `last_activity_ms` are what was known before the event: the balance crossing
/// rules fire on the crossing only, and the idle rule needs a previous activity to
/// measure from.
pub fn evaluate(
    rules: &AlertRules,
    event: &AddressEvent,
    balance_before: Option<u64>,
    last_activity_ms: Option<u64>,
) -> Vec<String> {
    let mut out = Vec::new();
    let kas = |sompi: u64| format!("{} KAS", format_kas(sompi as f64, 8));
    let verb = match event.kind {
        EventKind::Received => "received",
        EventKind::Sent => "sent",
    };
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
    if let (Some(hours), Some(last)) = (rules.idle_hours, last_activity_ms)
        && hours > 0
        && event.time_ms.saturating_sub(last) >= u64::from(hours) * 3_600_000
    {
        let idle = Duration::from_millis(event.time_ms - last);
        out.push(format!(
            "{verb} {} after {} idle",
            kas(event.amount),
            crate::format::format_duration(idle)
        ));
    }
    out
}

/// The alerts `event` raises on the lists with its address (each entry's own rules,
/// those with alerts on), and records the activity on those entries for the idle rule.
/// With the address on more than one list, each message is led by its list's name.
/// Returns the alerts and the `Touch` applied, to save to the file; `None` when no
/// list has the address.
pub fn event_alerts(
    lists: &mut Watchlists,
    event: &AddressEvent,
    balance_before: Option<u64>,
    network: &str,
) -> Option<(Vec<String>, WatchEdit)> {
    let memberships = lists.memberships(&event.address, network);
    if memberships.is_empty() {
        return None;
    }
    let named = memberships.len() > 1;
    let alerts = memberships
        .iter()
        .filter(|(_, entry)| entry.enabled)
        .flat_map(|(list, entry)| {
            evaluate(&entry.rules, event, balance_before, entry.last_activity_ms)
                .into_iter()
                .map(move |m| {
                    if named {
                        format!("{}: {m}", list.name)
                    } else {
                        m
                    }
                })
        })
        .collect();
    let touch = WatchEdit::Touch {
        address: event.address.clone(),
        network: network.to_string(),
        time_ms: event.time_ms,
    };
    let _ = lists.apply(touch.clone());
    Some((alerts, touch))
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

/// Start watching every address on a list for `network`, tracked in `handles.watch`.
/// Waits for a synced node with a UTXO index; seeds balances, then follows
/// notifications. `persist`: `app.watch.lists` are the saved lists, so the activity
/// the idle rule needs is saved to their file.
pub fn start_watch(
    rpc: &Arc<RpcManager>,
    app: &Arc<RwLock<App>>,
    handles: &mut PollingHandles,
    network: &str,
    persist: bool,
) {
    let rpc = rpc.clone();
    let app = app.clone();
    let network = network.to_string();
    handles.watch = Some(tokio::spawn(async move {
        run(rpc, app, network, persist).await;
    }));
}

async fn run(rpc: Arc<RpcManager>, app: Arc<RwLock<App>>, network: String, persist: bool) {
    let addresses: Vec<String> = {
        let app = app.read().await;
        app.watch
            .lists
            .addresses(&network)
            .into_iter()
            .map(str::to_string)
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
                Some(n) = receiver.recv() => handle_notification(&app, &n, &network, persist).await,
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

/// Turn a notification's per-address changes into events, applying them to `balances`
/// on the way. Each event comes with the balance known before it (for the crossing
/// rules). An address whose UTXOs were only replaced by the same amount (a
/// self-transfer) gets no event. Sorted by address so the order is stable.
pub fn build_events(
    changes: HashMap<String, NetChange>,
    balances: &mut HashMap<String, u64>,
    time_ms: u64,
) -> Vec<(AddressEvent, Option<u64>)> {
    let mut changes: Vec<_> = changes.into_iter().collect();
    changes.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = Vec::new();
    for (address, change) in changes {
        let (kind, amount) = if change.added >= change.removed {
            (EventKind::Received, change.added - change.removed)
        } else {
            (EventKind::Sent, change.removed - change.added)
        };
        let before = balances.get(&address).copied();
        let after = before.map(|b| (b + change.added).saturating_sub(change.removed));
        if let Some(after) = after {
            balances.insert(address.clone(), after);
        }
        if amount == 0 {
            continue;
        }
        let txid = (change.txids.len() == 1)
            .then(|| change.txids.into_iter().next())
            .flatten();
        out.push((
            AddressEvent {
                time_ms,
                address,
                kind,
                amount,
                txid,
                is_coinbase: change.coinbase,
                balance_after: after,
                alerts: Vec::new(),
                read: false,
            },
            before,
        ));
    }
    out
}

/// `persist`: save the activity to the watchlists file (not for lists that aren't the
/// saved ones, such as the CLI's addresses given on the command line).
async fn handle_notification(
    app: &RwLock<App>,
    n: &UtxosChangedNotification,
    network: &str,
    persist: bool,
) {
    let changes = net_changes(n);
    if changes.is_empty() {
        return;
    }
    let time_ms = now_ms();
    let mut guard = app.write().await;
    let events = build_events(changes, &mut guard.watch.balances, time_ms);
    let mut touched = Vec::new();
    for (mut event, before) in events {
        if let Some((alerts, touch)) = event_alerts(&mut guard.watch.lists, &event, before, network)
        {
            event.alerts = alerts;
            touched.push(touch);
        }
        guard.watch.push_event(event);
    }
    guard.watch.status.last_event_at = Some(std::time::Instant::now());
    guard.mark_dirty();
    if !persist || touched.is_empty() {
        return;
    }
    // Keep the last-activity times for the idle rule across restarts. The file is
    // tiny, but write it off the async thread and without holding the lock; through
    // `update_file`, so a list the CLI changed meanwhile keeps its change.
    drop(guard);
    let saved = tokio::task::spawn_blocking(move || {
        Watchlists::update_file(|lists| touched.into_iter().try_for_each(|edit| lists.apply(edit)))
    })
    .await;
    if let Err(e) = saved.unwrap_or_else(|e| Err(e.into())) {
        let mut guard = app.write().await;
        guard.watch.status.last_error = Some(format!("watchlist: {e}"));
        guard.mark_dirty();
    }
}

async fn set_phase(app: &RwLock<App>, phase: WatchPhase) {
    let mut app = app.write().await;
    if app.watch.status.phase != phase {
        app.watch.status.phase = phase;
        app.mark_dirty();
    }
}

/// Why `s` can't be watched (or labelled) on `network`, if it can't: it must parse
/// as a Kaspa address, and its prefix must be the network's (`kaspa:` on mainnet,
/// `kaspatest:` on a testnet).
pub fn validate_address(s: &str, network: &str) -> Result<(), String> {
    let s = s.trim();
    let address = RpcAddress::try_from(s).map_err(|_| "Not a valid Kaspa address".to_string())?;
    let expected = NetworkId::from_str(network)
        .ok()
        .map(kaspa_addresses::Prefix::from);
    match expected {
        Some(prefix) if prefix != address.prefix => Err(format!(
            "A {} address, but the connection is {network}",
            address.prefix
        )),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_validation() {
        let main = "kaspa:qpzpfwcsqsxhxwup26r55fd0ghqlhyugz8cp6y3wxuddc02vcxtjg75pspnwz";
        assert!(validate_address(main, "mainnet").is_ok());
        assert!(validate_address(&format!("  {main} "), "mainnet").is_ok());
        assert!(
            validate_address(main, "testnet-10")
                .unwrap_err()
                .contains("kaspa address")
        );
        assert!(
            validate_address("kaspa:notanaddress", "mainnet")
                .unwrap_err()
                .contains("Not a valid")
        );
        assert!(validate_address("", "mainnet").is_err());
        // An unknown network name only checks the address itself.
        assert!(validate_address(main, "somenet").is_ok());
    }

    fn event(kind: EventKind, amount: u64, after: Option<u64>) -> AddressEvent {
        AddressEvent {
            time_ms: 0,
            address: "kaspa:x".into(),
            kind,
            amount,
            txid: None,
            is_coinbase: false,
            balance_after: after,
            alerts: Vec::new(),
            read: false,
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
            idle_hours: None,
        };
        let eval = |ev: AddressEvent, before: Option<u64>| evaluate(&rules, &ev, before, None);
        assert!(eval(event(EventKind::Received, 99, Some(5_000)), Some(5_000)).is_empty());
        assert_eq!(
            eval(event(EventKind::Received, 100, Some(5_000)), Some(5_000)).len(),
            1
        );
        // Falling below fires once, on the crossing.
        assert_eq!(
            eval(event(EventKind::Sent, 60, Some(900)), Some(1_200)).len(),
            2
        );
        assert_eq!(
            eval(event(EventKind::Sent, 60, Some(800)), Some(900)).len(),
            1
        );
        // Unknown previous balance counts as a crossing.
        let no_sent = AlertRules {
            sent_min: None,
            ..rules.clone()
        };
        assert_eq!(
            evaluate(&no_sent, &event(EventKind::Sent, 1, Some(10)), None, None).len(),
            1
        );
        let any = AlertRules::default();
        assert_eq!(
            evaluate(&any, &event(EventKind::Received, 5, None), None, None),
            vec!["received 0.00000005 KAS".to_string()]
        );
    }

    #[test]
    fn idle_rule_needs_a_known_gap() {
        let rules = AlertRules {
            any_activity: false,
            idle_hours: Some(2),
            ..AlertRules::default()
        };
        let mut ev = event(EventKind::Received, 7, None);
        ev.time_ms = 10 * 3_600_000;
        // No previous activity known: nothing to measure from.
        assert!(evaluate(&rules, &ev, None, None).is_empty());
        // Too recent.
        assert!(evaluate(&rules, &ev, None, Some(9 * 3_600_000)).is_empty());
        let fired = evaluate(&rules, &ev, None, Some(7 * 3_600_000));
        assert_eq!(
            fired,
            vec!["received 0.00000007 KAS after 3h 00m idle".to_string()]
        );
        // Zero hours is off.
        let off = AlertRules {
            idle_hours: Some(0),
            ..rules
        };
        assert!(evaluate(&off, &ev, None, Some(0)).is_empty());
    }

    #[test]
    fn events_are_built_from_a_notification() {
        use kaspa_rpc_core::{
            RpcHash, RpcScriptPublicKey, RpcTransactionOutpoint, RpcUtxoEntry,
            RpcUtxosByAddressesEntry,
        };
        let synthetic = |n: u32| crate::index::writer::testing::address(n).to_string();
        let (a, b, c) = (&synthetic(1), &synthetic(2), &synthetic(3));
        let entry =
            |address: &str, tx: u64, amount: u64, coinbase: bool| RpcUtxosByAddressesEntry {
                address: Some(RpcAddress::try_from(address).unwrap()),
                outpoint: RpcTransactionOutpoint {
                    transaction_id: RpcHash::from_u64_word(tx),
                    index: 0,
                },
                utxo_entry: RpcUtxoEntry {
                    amount,
                    script_public_key: RpcScriptPublicKey::default(),
                    block_daa_score: 1,
                    is_coinbase: coinbase,
                    covenant_id: None,
                },
            };
        // `a` spends a 500 UTXO and gets 120 change back (one tx); `b` receives 300 from
        // it plus a 50 coinbase (two txs, so no single txid); `c` is only shuffled.
        let n = UtxosChangedNotification {
            added: Arc::new(vec![
                entry(a, 1, 120, false),
                entry(b, 1, 300, false),
                entry(b, 2, 50, true),
                entry(c, 3, 10, false),
            ]),
            removed: Arc::new(vec![entry(a, 0, 500, false), entry(c, 0, 10, false)]),
        };
        let changes = net_changes(&n);
        assert_eq!(changes.len(), 3);
        let mut balances = HashMap::from([(a.clone(), 1_000u64), (c.clone(), 10)]);
        let events = build_events(changes, &mut balances, 99);
        assert_eq!(events.len(), 2);
        let (ea, before_a) = &events[0];
        assert_eq!(&ea.address, a);
        assert_eq!(ea.kind, EventKind::Sent);
        assert_eq!(ea.amount, 380);
        assert_eq!(ea.txid, Some(RpcHash::from_u64_word(1).to_string()));
        assert_eq!(ea.balance_after, Some(620));
        assert_eq!(*before_a, Some(1_000));
        assert_eq!(balances[a.as_str()], 620);
        let (eb, before_b) = &events[1];
        assert_eq!(&eb.address, b);
        assert_eq!(eb.kind, EventKind::Received);
        assert_eq!(eb.amount, 350);
        assert!(eb.txid.is_none() && eb.is_coinbase);
        // An unknown balance stays unknown; the next refresh from the node fills it.
        assert_eq!(eb.balance_after, None);
        assert_eq!(*before_b, None);
        assert!(!balances.contains_key(b.as_str()));
        assert_eq!(balances[c.as_str()], 10);
    }

    fn lists(names: &[&str]) -> Watchlists {
        let mut lists = Watchlists::default();
        for name in names {
            lists
                .apply(WatchEdit::CreateList {
                    name: name.to_string(),
                })
                .unwrap();
        }
        lists
    }

    fn put(lists: &mut Watchlists, list: ListId, entry: WatchEntry) {
        lists.apply(WatchEdit::Put { list, entry }).unwrap();
    }

    #[test]
    fn file_updates_keep_other_writers_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("watchlist.toml");
        // The first update creates the default list.
        let ours = Watchlists::update_at(&path, |l| {
            l.apply(WatchEdit::Put {
                list: 1,
                entry: WatchEntry::new("kaspa:a", "mainnet"),
            })
        })
        .unwrap();
        // Another process adds a list meanwhile; our next edit keeps it.
        let mut theirs = Watchlists::load_from(&path).unwrap();
        theirs
            .apply(WatchEdit::CreateList { name: "CLI".into() })
            .unwrap();
        theirs.save_to(&path).unwrap();
        let after = Watchlists::update_at(&path, |l| {
            l.apply(WatchEdit::Touch {
                address: "kaspa:a".into(),
                network: "mainnet".into(),
                time_ms: 9,
            })
        })
        .unwrap();
        assert_eq!(ours.lists.len(), 1);
        assert_eq!(after.lists.len(), 2);
        assert_eq!(after.lists[0].entries[0].last_activity_ms, Some(9));
        // A refused edit leaves the file alone.
        assert!(
            Watchlists::update_at(&path, |l| l.apply(WatchEdit::DeleteList { id: 7 })).is_err()
        );
        assert_eq!(Watchlists::load_from(&path).unwrap(), after);
    }

    #[test]
    fn legacy_file_becomes_one_list() {
        let legacy = "[[entries]]\naddress = \"kaspa:z\"\nlast_activity_ms = 5\n\n[[entries]]\naddress = \"kaspa:y\"\nenabled = false\n\n[[entries]]\naddress = \"kaspa:z\"\nenabled = false\n";
        let lists = Watchlists::parse(legacy).unwrap();
        assert_eq!(lists.lists.len(), 1);
        let list = &lists.lists[0];
        assert_eq!((list.id, list.name.as_str()), (1, DEFAULT_LIST));
        assert_eq!(list.entries.len(), 2);
        let z = &list.entries[0];
        assert!(z.enabled && z.rules.any_activity && z.network == "mainnet");
        assert_eq!(z.last_activity_ms, Some(5));
        assert!(!list.entries[1].enabled);
        // The duplicate kaspa:z is dropped; the first stays.
        assert_eq!(lists.next_id, 2);
        // An empty file has no lists until one is needed.
        let mut empty = Watchlists::parse("").unwrap();
        assert!(empty.lists.is_empty());
        empty.ensure_default();
        assert_eq!(empty.lists[0].name, DEFAULT_LIST);
    }

    #[test]
    fn lists_roundtrip_and_addresses() {
        let mut l = lists(&["Exchanges", "Friends"]);
        put(&mut l, 1, WatchEntry::new("kaspa:a", "mainnet"));
        put(
            &mut l,
            1,
            WatchEntry {
                enabled: false,
                ..WatchEntry::new("kaspa:b", "mainnet")
            },
        );
        put(&mut l, 2, WatchEntry::new("kaspa:a", "mainnet"));
        put(&mut l, 2, WatchEntry::new("kaspa:c", "testnet-10"));
        let text = toml::to_string_pretty(&l).unwrap();
        assert!(text.contains("[[lists]]"));
        assert_eq!(Watchlists::parse(&text).unwrap(), l);
        // Every address once, alerts on or off.
        assert_eq!(l.addresses("mainnet"), vec!["kaspa:a", "kaspa:b"]);
        assert_eq!(l.addresses("testnet-10"), vec!["kaspa:c"]);
        let on: Vec<&str> = l
            .memberships("kaspa:a", "mainnet")
            .iter()
            .map(|(list, _)| list.name.as_str())
            .collect();
        assert_eq!(on, vec!["Exchanges", "Friends"]);
        assert!(l.is_watched("kaspa:c", "testnet-10"));
        assert!(!l.is_watched("kaspa:c", "mainnet"));
        assert_eq!(l.by_name(" friends ").map(|l| l.id), Some(2));
    }

    #[test]
    fn edits_check_names_and_keep_one_list() {
        let mut l = lists(&["A"]);
        let create = |name: &str| WatchEdit::CreateList { name: name.into() };
        assert!(l.apply(create("  ")).is_err());
        assert!(l.apply(create("a")).unwrap_err().contains("already"));
        l.apply(create(" B ")).unwrap();
        assert_eq!(l.lists[1].name, "B");
        // Renaming to its own name (another case) is fine, to another list's isn't.
        l.apply(WatchEdit::RenameList {
            id: 2,
            name: "b".into(),
        })
        .unwrap();
        assert!(
            l.apply(WatchEdit::RenameList {
                id: 2,
                name: "A".into()
            })
            .is_err()
        );
        assert!(
            l.apply(WatchEdit::RenameList {
                id: 9,
                name: "C".into()
            })
            .is_err()
        );
        l.apply(WatchEdit::DeleteList { id: 1 }).unwrap();
        assert!(
            l.apply(WatchEdit::DeleteList { id: 2 })
                .unwrap_err()
                .contains("last")
        );
        // Ids aren't reused while a later list exists.
        l.apply(create("C")).unwrap();
        assert_eq!(l.lists.iter().map(|l| l.id).collect::<Vec<_>>(), vec![2, 3]);
    }

    #[test]
    fn put_replaces_and_move_keeps_the_entry() {
        let mut l = lists(&["A", "B"]);
        put(&mut l, 1, WatchEntry::new(" kaspa:a ", "mainnet"));
        put(&mut l, 1, WatchEntry::new("kaspa:a", "testnet-10"));
        let mut edited = WatchEntry::new("kaspa:a", "mainnet");
        edited.rules.received_min = Some(7);
        edited.last_activity_ms = Some(3);
        put(&mut l, 1, edited.clone());
        assert_eq!(l.lists[0].entries.len(), 2);
        assert_eq!(l.lists[0].entry("kaspa:a", "mainnet"), Some(&edited));
        let mv = |from, to| WatchEdit::Move {
            from,
            to,
            address: "kaspa:a".into(),
            network: "mainnet".into(),
        };
        l.apply(mv(1, 2)).unwrap();
        assert!(l.lists[0].entry("kaspa:a", "mainnet").is_none());
        assert_eq!(l.lists[1].entry("kaspa:a", "mainnet"), Some(&edited));
        // The other network's entry stays.
        assert!(l.lists[0].entry("kaspa:a", "testnet-10").is_some());
        assert!(l.apply(mv(1, 2)).is_err());
        // Into a list that already has it: it only leaves the source.
        put(&mut l, 1, WatchEntry::new("kaspa:a", "mainnet"));
        l.apply(mv(1, 2)).unwrap();
        assert!(l.lists[0].entry("kaspa:a", "mainnet").is_none());
        assert_eq!(l.lists[1].entry("kaspa:a", "mainnet"), Some(&edited));
        l.apply(WatchEdit::Remove {
            list: 2,
            address: "kaspa:a".into(),
            network: "mainnet".into(),
        })
        .unwrap();
        assert!(!l.is_watched("kaspa:a", "mainnet"));
    }

    #[test]
    fn alerts_come_from_every_list_with_the_address() {
        let mut l = lists(&["Exchanges", "Friends", "Muted"]);
        let mut big = WatchEntry::new("kaspa:x", "mainnet");
        big.rules = AlertRules {
            any_activity: false,
            received_min: Some(100),
            ..AlertRules::default()
        };
        put(&mut l, 1, big);
        put(&mut l, 2, WatchEntry::new("kaspa:x", "mainnet"));
        put(
            &mut l,
            3,
            WatchEntry {
                enabled: false,
                ..WatchEntry::new("kaspa:x", "mainnet")
            },
        );
        put(&mut l, 2, WatchEntry::new("kaspa:only", "mainnet"));
        let mut ev = event(EventKind::Received, 150, None);
        ev.time_ms = 42;
        let (alerts, touch) = event_alerts(&mut l, &ev, None, "mainnet").unwrap();
        assert_eq!(
            touch,
            WatchEdit::Touch {
                address: "kaspa:x".into(),
                network: "mainnet".into(),
                time_ms: 42,
            }
        );
        assert_eq!(
            alerts,
            vec![
                "Exchanges: received 0.00000150 KAS (≥ 0.00000100 KAS)".to_string(),
                "Friends: received 0.00000150 KAS".to_string(),
            ]
        );
        // Every entry records the activity, alerts on or off.
        for list in &l.lists {
            assert_eq!(
                list.entry("kaspa:x", "mainnet").unwrap().last_activity_ms,
                Some(42)
            );
        }
        // On one list: no name.
        let mut ev = event(EventKind::Received, 5, None);
        ev.address = "kaspa:only".into();
        assert_eq!(
            event_alerts(&mut l, &ev, None, "mainnet").unwrap().0,
            vec!["received 0.00000005 KAS".to_string()]
        );
        // Another network's entry doesn't count.
        assert!(event_alerts(&mut l, &ev, None, "testnet-10").is_none());
    }

    #[test]
    fn list_ids_are_never_reused() {
        let mut l = lists(&["A", "B", "C"]);
        l.apply(WatchEdit::DeleteList { id: 3 }).unwrap();
        l.apply(WatchEdit::CreateList { name: "D".into() }).unwrap();
        assert_eq!(l.by_name("D").unwrap().id, 4);
        // Also across a save and load.
        l.apply(WatchEdit::DeleteList { id: 4 }).unwrap();
        let mut l = Watchlists::parse(&toml::to_string_pretty(&l).unwrap()).unwrap();
        l.apply(WatchEdit::CreateList { name: "E".into() }).unwrap();
        assert_eq!(l.by_name("E").unwrap().id, 5);
    }

    #[test]
    fn put_keeps_later_activity() {
        let mut l = lists(&["A"]);
        let copy = WatchEntry::new("kaspa:a", "mainnet");
        put(&mut l, 1, copy.clone());
        l.apply(WatchEdit::Touch {
            address: "kaspa:a".into(),
            network: "mainnet".into(),
            time_ms: 77,
        })
        .unwrap();
        // A dialog saves the copy it took before the activity.
        let mut edited = copy;
        edited.enabled = false;
        put(&mut l, 1, edited);
        let entry = l.lists[0].entry("kaspa:a", "mainnet").unwrap();
        assert!(!entry.enabled);
        assert_eq!(entry.last_activity_ms, Some(77));
    }

    #[test]
    fn a_new_entry_takes_the_addresses_activity() {
        let mut l = lists(&["A", "B"]);
        put(&mut l, 1, WatchEntry::new("kaspa:a", "mainnet"));
        l.apply(WatchEdit::Touch {
            address: "kaspa:a".into(),
            network: "mainnet".into(),
            time_ms: 50,
        })
        .unwrap();
        put(&mut l, 2, WatchEntry::new("kaspa:a", "mainnet"));
        assert_eq!(
            l.lists[1]
                .entry("kaspa:a", "mainnet")
                .unwrap()
                .last_activity_ms,
            Some(50)
        );
        assert_eq!(l.last_activity("kaspa:a", "mainnet"), Some(50));
        assert_eq!(l.last_activity("kaspa:a", "testnet-10"), None);
    }
}
