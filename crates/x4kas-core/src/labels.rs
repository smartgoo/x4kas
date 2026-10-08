//! Address labels: who an address belongs to. Sources, by precedence: the user's own
//! labels (`~/.x4kas/labels/user.toml`), the public list at `api.kaspa.org`
//! (exchanges, pools, funds, bridges; refreshed daily and cached at
//! `~/.x4kas/labels/kaspa_org.json`), and a snapshot of that list bundled with the app
//! so labels show offline. The kaspa.org list is fetched in bulk, so looking up an
//! address never tells anyone which one. Below all of these sit heuristics: the burn
//! address of every network, and mining pools detected from the coinbases the chain
//! stream sees ([`MinerTally`]).

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::app::App;
use crate::config;
use crate::emission;

const KASPA_ORG_NAMES_URL: &str = "https://api.kaspa.org/addresses/names";
/// Snapshot of the kaspa.org list, so labels show before (and without) a refresh.
const BUNDLED: &str = include_str!("../assets/kaspa_org_names.json");
/// How old the cached kaspa.org list may be before it is fetched again.
pub const REFRESH_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LabelSource {
    User,
    /// kas.fyi's per-address tag (needs an API key; opt-in).
    KasFyi,
    KaspaOrg,
    /// A `.kas` name from the KNS indexer (opt-in).
    Kns,
    Bundled,
    /// Derived locally: the burn address, a pool seen mining many blocks.
    Heuristic,
}

impl LabelSource {
    pub fn label(&self) -> &'static str {
        match self {
            Self::User => "your label",
            Self::KasFyi => "kas.fyi",
            Self::KaspaOrg => "api.kaspa.org",
            Self::Kns => "KNS",
            Self::Bundled => "bundled list",
            Self::Heuristic => "heuristic",
        }
    }

    /// Whether the source names an entity (an exchange, a pool, the burn address)
    /// rather than one address: the user's notes and `.kas` names are per address, so
    /// two of them never prove two owners and the clustering guard ignores them.
    pub fn is_entity(&self) -> bool {
        !matches!(self, Self::User | Self::Kns)
    }

    /// Precedence: a label only replaces one of a weaker (or the same) source. The
    /// bundled snapshot is the kaspa.org list, so a fresh list replaces it and KNS
    /// doesn't.
    fn rank(&self) -> u8 {
        match self {
            Self::User => 5,
            Self::KasFyi => 4,
            Self::KaspaOrg | Self::Bundled => 3,
            Self::Kns => 2,
            Self::Heuristic => 1,
        }
    }
}

/// Opt-in online lookups, `~/.x4kas/labels/settings.toml`. Each sends the address being
/// looked at to a third party, so they are off until the user turns them on.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LabelSettings {
    /// kas.fyi developer API key (developer.kas.fyi); lookups are on when set.
    pub kas_fyi_api_key: Option<String>,
    /// Resolve `.kas` names through the KNS indexer.
    pub kns: bool,
}

impl LabelSettings {
    pub fn path() -> PathBuf {
        labels_dir().join("settings.toml")
    }

    pub fn load() -> Self {
        std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(labels_dir())?;
        std::fs::write(Self::path(), toml::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn any_enabled(&self) -> bool {
        self.kas_fyi_api_key
            .as_deref()
            .is_some_and(|k| !k.trim().is_empty())
            || self.kns
    }
}

/// A cached online answer, positive or negative.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnlineEntry {
    pub source: LabelSource,
    pub name: Option<String>,
    /// The entity's web page, when the source gives one (kas.fyi).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    /// Categories such as `exchange` or `pool` (kas.fyi).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub categories: Vec<String>,
    pub fetched_at_ms: u64,
}

/// Online answers, `~/.x4kas/labels/online_cache.json`, keyed by `source:address`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnlineCache {
    pub entries: BTreeMap<String, OnlineEntry>,
}

/// How long an online answer (also a "no label" one) is trusted.
pub const ONLINE_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

impl OnlineCache {
    pub fn path() -> PathBuf {
        labels_dir().join("online_cache.json")
    }

    pub fn load() -> Self {
        std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(labels_dir())?;
        std::fs::write(Self::path(), serde_json::to_string(self)?)?;
        Ok(())
    }

    fn key(source: LabelSource, address: &str) -> String {
        format!("{}:{address}", source.label())
    }

    pub fn get(&self, source: LabelSource, address: &str, now_ms: u64) -> Option<&OnlineEntry> {
        self.entries
            .get(&Self::key(source, address))
            .filter(|e| now_ms.saturating_sub(e.fetched_at_ms) < ONLINE_TTL.as_millis() as u64)
    }

    pub fn put(&mut self, address: &str, entry: OnlineEntry) {
        self.entries.insert(Self::key(entry.source, address), entry);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Label {
    pub name: String,
    pub source: LabelSource,
    /// The entity's web page, when the source gives one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    /// Categories such as `exchange` or `pool`, when the source gives them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub categories: Vec<String>,
}

impl Default for Label {
    fn default() -> Self {
        Self::new("", LabelSource::Heuristic)
    }
}

impl Label {
    pub fn new(name: &str, source: LabelSource) -> Self {
        Self {
            name: name.to_string(),
            source,
            link: None,
            categories: Vec::new(),
        }
    }
}

/// One entry of the kaspa.org list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddressName {
    pub address: String,
    pub name: String,
}

/// The user's labels, `~/.x4kas/labels/user.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UserLabels {
    pub labels: BTreeMap<String, String>,
}

impl UserLabels {
    pub fn path() -> PathBuf {
        labels_dir().join("user.toml")
    }

    pub fn load() -> Result<Self> {
        let path = Self::path();
        if !path.exists() {
            return Ok(Self::default());
        }
        Ok(toml::from_str(&std::fs::read_to_string(&path)?)?)
    }

    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(labels_dir())?;
        std::fs::write(Self::path(), toml::to_string_pretty(self)?)?;
        Ok(())
    }
}

pub fn labels_dir() -> PathBuf {
    config::data_dir().join("labels")
}

fn kaspa_org_cache_path() -> PathBuf {
    labels_dir().join("kaspa_org.json")
}

/// Every known label, by address. Replaced as a whole (it's behind an `Arc` in the app
/// state) whenever a source changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelBook {
    map: HashMap<String, Label>,
    /// The strongest entity label per address (`LabelSource::is_entity`), kept even
    /// where a user label or `.kas` name is shown, for the clustering guard.
    entity: HashMap<String, Label>,
    user: UserLabels,
    /// Labels derived locally (burn addresses, detected pools); the weakest source,
    /// kept across rebuilds like the user's labels but not saved.
    heuristic: BTreeMap<String, String>,
    /// When the kaspa.org list was last fetched (from the cache file's age on load).
    pub kaspa_org_refreshed: Option<SystemTime>,
}

impl Default for LabelBook {
    fn default() -> Self {
        Self::bundled()
    }
}

impl LabelBook {
    /// The bundled snapshot and the burn address of every network.
    pub fn bundled() -> Self {
        let mut book = Self {
            map: HashMap::new(),
            entity: HashMap::new(),
            user: UserLabels::default(),
            heuristic: BTreeMap::new(),
            kaspa_org_refreshed: None,
        };
        if let Ok(list) = serde_json::from_str::<Vec<AddressName>>(BUNDLED) {
            book.apply_list(&list, LabelSource::Bundled);
        }
        for network in config::valid_networks() {
            if let Some(burn) = emission::burn_address(network) {
                book.heuristic
                    .insert(burn.to_string(), "Burn address".to_string());
            }
        }
        book.apply_heuristic();
        book
    }

    /// The bundled snapshot, the cached kaspa.org list if any, the fresh online
    /// answers, and the user's labels.
    pub fn load() -> Self {
        let mut book = Self::bundled();
        book.user = UserLabels::load().unwrap_or_default();
        book.rebuild();
        book
    }

    /// Reapply every source over a fresh bundled book, keeping the user's and the
    /// heuristic labels: how a removed label falls back to the next source.
    fn rebuild(&mut self) {
        let mut rebuilt = Self::bundled();
        if let Ok((list, saved_at)) = load_kaspa_org_cache() {
            rebuilt.apply_list(&list, LabelSource::KaspaOrg);
            rebuilt.kaspa_org_refreshed = saved_at;
        }
        rebuilt.apply_online_cache(&OnlineCache::load(), crate::format::now_ms());
        rebuilt
            .heuristic
            .extend(std::mem::take(&mut self.heuristic));
        rebuilt.apply_heuristic();
        rebuilt.user = std::mem::take(&mut self.user);
        rebuilt.apply_user();
        *self = rebuilt;
    }

    pub fn get(&self, address: &str) -> Option<&Label> {
        self.map.get(address)
    }

    pub fn name(&self, address: &str) -> Option<&str> {
        self.get(address).map(|l| l.name.as_str())
    }

    /// The strongest entity label of an address (kas.fyi, kaspa.org, heuristics), even
    /// when a user label or `.kas` name is shown instead.
    pub fn entity_name(&self, address: &str) -> Option<&str> {
        self.entity.get(address).map(|l| l.name.as_str())
    }

    /// The label that names a cluster: the strongest source among the members, the
    /// most common name within it. `None` when no member is labelled.
    pub fn name_cluster<'a, I>(&self, members: I) -> Option<String>
    where
        I: IntoIterator<Item = &'a str>,
    {
        let mut counts: HashMap<&str, (u8, usize)> = HashMap::new();
        for member in members {
            if let Some(label) = self.get(member) {
                let entry = counts.entry(label.name.as_str()).or_insert((0, 0));
                entry.0 = entry.0.max(label.source.rank());
                entry.1 += 1;
            }
        }
        counts
            .into_iter()
            .max_by_key(|(name, (rank, count))| (*rank, *count, std::cmp::Reverse(*name)))
            .map(|(name, _)| name.to_string())
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn user_labels(&self) -> &BTreeMap<String, String> {
        &self.user.labels
    }

    /// Addresses named by the public list (the fetched api.kaspa.org list, or the
    /// bundled snapshot of it), whatever is shown over them.
    pub fn public_len(&self) -> usize {
        self.entity
            .values()
            .filter(|l| matches!(l.source, LabelSource::KaspaOrg | LabelSource::Bundled))
            .count()
    }

    /// Every label, sorted by name.
    pub fn all(&self) -> Vec<(&str, &Label)> {
        let mut all: Vec<(&str, &Label)> = self.map.iter().map(|(a, l)| (a.as_str(), l)).collect();
        all.sort_by(|a, b| a.1.name.cmp(&b.1.name).then(a.0.cmp(b.0)));
        all
    }

    /// Addresses whose label contains `query` (case-insensitive), with their labels.
    pub fn search(&self, query: &str) -> Vec<(&str, &Label)> {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return Vec::new();
        }
        let mut hits: Vec<(&str, &Label)> = self
            .map
            .iter()
            .filter(|(_, l)| l.name.to_lowercase().contains(&q))
            .map(|(a, l)| (a.as_str(), l))
            .collect();
        hits.sort_by(|a, b| a.1.name.cmp(&b.1.name).then(a.0.cmp(b.0)));
        hits
    }

    /// Set (or with `None`, remove) the user's label for `address`; saved to disk.
    pub fn set_user(&mut self, address: &str, name: Option<&str>) -> Result<()> {
        let address = address.trim();
        match name.map(str::trim).filter(|n| !n.is_empty()) {
            Some(name) => {
                self.user
                    .labels
                    .insert(address.to_string(), name.to_string());
            }
            None => {
                self.user.labels.remove(address);
            }
        }
        self.user.save()?;
        // Rebuild so a removed user label falls back to the public one.
        self.rebuild();
        Ok(())
    }

    /// Add a locally derived label (the weakest source). Returns whether the label
    /// shown for the address changed, so callers can skip republishing the book.
    pub fn set_heuristic(&mut self, address: &str, name: &str) -> bool {
        self.heuristic.insert(address.to_string(), name.to_string());
        let before = self.map.get(address).cloned();
        self.apply_one(address, Label::new(name, LabelSource::Heuristic));
        self.map.get(address) != before.as_ref()
    }

    fn apply_heuristic(&mut self) {
        for (address, name) in &self.heuristic.clone() {
            self.apply_one(address, Label::new(name, LabelSource::Heuristic));
        }
    }

    /// Apply a freshly fetched kaspa.org list.
    pub fn apply_kaspa_org(&mut self, list: &[AddressName], fetched_at: SystemTime) {
        self.apply_list(list, LabelSource::KaspaOrg);
        self.kaspa_org_refreshed = Some(fetched_at);
        self.apply_user();
    }

    fn apply_list(&mut self, list: &[AddressName], source: LabelSource) {
        for entry in list {
            self.apply_one(&entry.address, Label::new(&entry.name, source));
        }
    }

    /// Set a label unless a stronger source already names the address.
    fn apply_one(&mut self, address: &str, label: Label) {
        if label.source.is_entity()
            && !self
                .entity
                .get(address)
                .is_some_and(|l| l.source.rank() > label.source.rank())
        {
            self.entity.insert(address.to_string(), label.clone());
        }
        if self
            .map
            .get(address)
            .is_some_and(|l| l.source.rank() > label.source.rank())
        {
            return;
        }
        self.map.insert(address.to_string(), label);
    }

    /// Apply an online answer (kas.fyi or KNS).
    pub fn apply_online(&mut self, address: &str, entry: &OnlineEntry) {
        if let Some(name) = &entry.name {
            self.apply_one(
                address,
                Label {
                    name: name.clone(),
                    source: entry.source,
                    link: entry.link.clone(),
                    categories: entry.categories.clone(),
                },
            );
        }
    }

    /// Apply every cached online answer that is still fresh.
    pub fn apply_online_cache(&mut self, cache: &OnlineCache, now_ms: u64) {
        for (key, entry) in &cache.entries {
            if let Some((_, address)) = key.split_once(':')
                && now_ms.saturating_sub(entry.fetched_at_ms) < ONLINE_TTL.as_millis() as u64
            {
                self.apply_online(address, entry);
            }
        }
    }

    fn apply_user(&mut self) {
        for (address, name) in &self.user.labels {
            self.map
                .insert(address.clone(), Label::new(name, LabelSource::User));
        }
    }
}

/// A pool seen mining this many blocks earns a heuristic "Mining pool" label.
pub const POOL_MIN_BLOCKS: u64 = 25;

/// Longest miner tag (the part of a coinbase's extra data after the node version) kept.
const MAX_MINER_TAG_LEN: usize = 40;

/// Coinbases per payout address, as the chain stream sees them, to label pools:
/// an address that mines [`POOL_MIN_BLOCKS`] blocks is almost certainly a pool
/// (or a very large solo miner), and its coinbase tag often names it.
#[derive(Debug, Default)]
pub struct MinerTally {
    blocks: HashMap<String, (u64, Option<String>)>,
}

impl MinerTally {
    /// Count one coinbase paid to `address` with the miner tag `tag`. Returns the
    /// label to give the address the moment it crosses the threshold, once.
    pub fn observe(&mut self, address: &str, tag: Option<&str>) -> Option<String> {
        let entry = self.blocks.entry(address.to_string()).or_insert((0, None));
        entry.0 += 1;
        if entry.1.is_none()
            && let Some(tag) = tag.map(clean_miner_tag).filter(|t| !t.is_empty())
        {
            entry.1 = Some(tag);
        }
        (entry.0 == POOL_MIN_BLOCKS).then(|| match &entry.1 {
            Some(tag) => format!("Mining pool ({tag})"),
            None => "Mining pool".to_string(),
        })
    }
}

/// Printable, trimmed, capped miner tag.
fn clean_miner_tag(tag: &str) -> String {
    tag.chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .chars()
        .take(MAX_MINER_TAG_LEN)
        .collect()
}

fn load_kaspa_org_cache() -> Result<(Vec<AddressName>, Option<SystemTime>)> {
    let path = kaspa_org_cache_path();
    let text = std::fs::read_to_string(&path)?;
    let saved_at = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    Ok((serde_json::from_str(&text)?, saved_at))
}

/// Whether the cached kaspa.org list is missing or older than [`REFRESH_AFTER`].
pub fn kaspa_org_cache_is_stale() -> bool {
    let saved_at = std::fs::metadata(kaspa_org_cache_path())
        .and_then(|m| m.modified())
        .ok();
    is_stale(saved_at, SystemTime::now())
}

/// Whether a list saved at `saved_at` (none: never) needs fetching again at `now`.
pub fn is_stale(saved_at: Option<SystemTime>, now: SystemTime) -> bool {
    saved_at
        .map(|t| now.duration_since(t).unwrap_or_default())
        .is_none_or(|age| age > REFRESH_AFTER)
}

/// Fetch the kaspa.org list and cache it.
pub async fn fetch_kaspa_org_names() -> Result<Vec<AddressName>> {
    let client = reqwest::Client::builder()
        .user_agent("x4kas")
        .timeout(Duration::from_secs(20))
        .build()?;
    let list: Vec<AddressName> = client
        .get(KASPA_ORG_NAMES_URL)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
        .context("kaspa.org names")?;
    std::fs::create_dir_all(labels_dir())?;
    std::fs::write(kaspa_org_cache_path(), serde_json::to_string(&list)?)?;
    Ok(list)
}

const KAS_FYI_TAG_URL: &str = "https://api.kas.fyi/v1/addresses";
const KNS_ASSETS_URL: &str = "https://api.knsdomains.org/mainnet/api/v1/assets";

/// Ask the enabled online sources about `address` and cache the answers (also the
/// negative ones). Returns what was learned, newest source first.
pub async fn lookup_online(settings: &LabelSettings, address: &str) -> Result<Vec<OnlineEntry>> {
    let client = reqwest::Client::builder()
        .user_agent("x4kas")
        .timeout(Duration::from_secs(15))
        .build()?;
    let now = crate::format::now_ms();
    let mut cache = OnlineCache::load();
    let mut learned = Vec::new();

    if let Some(key) = settings
        .kas_fyi_api_key
        .as_deref()
        .map(str::trim)
        .filter(|k| !k.is_empty())
    {
        if let Some(hit) = cache.get(LabelSource::KasFyi, address, now) {
            learned.push(hit.clone());
        } else {
            let response = client
                .get(format!("{KAS_FYI_TAG_URL}/{address}/tag"))
                .header("x-api-key", key)
                .send()
                .await
                .context("kas.fyi")?;
            let entry = match response.status().as_u16() {
                404 => OnlineEntry {
                    source: LabelSource::KasFyi,
                    name: None,
                    link: None,
                    categories: Vec::new(),
                    fetched_at_ms: now,
                },
                _ => {
                    let body: serde_json::Value = response.error_for_status()?.json().await?;
                    kas_fyi_entry(&body, now)
                }
            };
            cache.put(address, entry.clone());
            learned.push(entry);
        }
    }

    if settings.kns {
        if let Some(hit) = cache.get(LabelSource::Kns, address, now) {
            learned.push(hit.clone());
        } else {
            let body: serde_json::Value = client
                .get(KNS_ASSETS_URL)
                .query(&[("owner", address)])
                .send()
                .await
                .context("KNS")?
                .error_for_status()?
                .json()
                .await?;
            let entry = OnlineEntry {
                source: LabelSource::Kns,
                name: kns_name(&body),
                link: None,
                categories: Vec::new(),
                fetched_at_ms: now,
            };
            cache.put(address, entry.clone());
            learned.push(entry);
        }
    }

    let _ = cache.save();
    Ok(learned)
}

/// A kas.fyi tag response, `{tag: {address, name, link, labels[]}}`.
fn kas_fyi_entry(body: &serde_json::Value, now_ms: u64) -> OnlineEntry {
    let text = |pointer: &str| {
        body.pointer(pointer)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    OnlineEntry {
        source: LabelSource::KasFyi,
        name: text("/tag/name"),
        link: text("/tag/link").filter(|l| l.starts_with("http")),
        categories: body
            .pointer("/tag/labels")
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| s.trim().to_lowercase())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default(),
        fetched_at_ms: now_ms,
    }
}

/// The first `.kas` name in a KNS assets response, whatever field it sits in.
fn kns_name(body: &serde_json::Value) -> Option<String> {
    fn walk(v: &serde_json::Value) -> Option<String> {
        match v {
            serde_json::Value::String(s) if s.ends_with(".kas") => Some(s.clone()),
            serde_json::Value::Array(items) => items.iter().find_map(walk),
            serde_json::Value::Object(map) => map.values().find_map(walk),
            _ => None,
        }
    }
    walk(body.pointer("/data/assets")?)
}

/// Refresh the kaspa.org list in `app.labels` now if the cache is stale, then daily.
/// Independent of the node connection, like market polling.
pub fn start_label_refresh(app: Arc<RwLock<App>>) {
    tokio::spawn(async move {
        loop {
            if kaspa_org_cache_is_stale()
                && let Ok(list) = fetch_kaspa_org_names().await
            {
                let mut app = app.write().await;
                let mut book = (*app.labels).clone();
                book.apply_kaspa_org(&list, SystemTime::now());
                app.labels = Arc::new(book);
                app.mark_dirty();
            }
            tokio::time::sleep(REFRESH_AFTER).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_list_has_known_entities() {
        let book = LabelBook::bundled();
        assert!(book.len() > 100);
        let burn = "kaspa:qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqkx9awp4e";
        assert_eq!(book.name(burn), Some("Burn Address"));
        assert_eq!(book.get(burn).unwrap().source, LabelSource::Bundled);
        assert!(!book.search("bybit").is_empty());
        assert!(book.search("").is_empty());
    }

    #[test]
    fn user_labels_take_precedence_in_memory() {
        let mut book = LabelBook::bundled();
        let burn = "kaspa:qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqkx9awp4e";
        book.user
            .labels
            .insert(burn.to_string(), "Mine".to_string());
        book.apply_user();
        assert_eq!(book.name(burn), Some("Mine"));
        assert_eq!(book.get(burn).unwrap().source, LabelSource::User);
        // A later public refresh doesn't override the user's label.
        book.apply_kaspa_org(
            &[AddressName {
                address: burn.to_string(),
                name: "Burn".to_string(),
            }],
            SystemTime::now(),
        );
        assert_eq!(book.name(burn), Some("Mine"));
    }

    #[test]
    fn online_labels_respect_precedence_and_ttl() {
        let mut book = LabelBook::bundled();
        let burn = "kaspa:qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqkx9awp4e";
        let kns = OnlineEntry {
            source: LabelSource::Kns,
            name: Some("burn.kas".into()),
            link: None,
            categories: Vec::new(),
            fetched_at_ms: 0,
        };
        // KNS is weaker than the bundled list; kas.fyi is stronger.
        book.apply_online(burn, &kns);
        assert_eq!(book.name(burn), Some("Burn Address"));
        book.apply_online(
            burn,
            &OnlineEntry {
                source: LabelSource::KasFyi,
                name: Some("Burn (kas.fyi)".into()),
                link: None,
                categories: Vec::new(),
                fetched_at_ms: 0,
            },
        );
        assert_eq!(book.get(burn).unwrap().source, LabelSource::KasFyi);
        book.apply_online(
            "kaspa:qfresh",
            &OnlineEntry {
                source: LabelSource::Kns,
                name: None,
                link: None,
                categories: Vec::new(),
                fetched_at_ms: 0,
            },
        );
        assert_eq!(book.name("kaspa:qfresh"), None);

        let mut cache = OnlineCache::default();
        cache.put("kaspa:qfresh", kns.clone());
        assert!(cache.get(LabelSource::Kns, "kaspa:qfresh", 1_000).is_some());
        assert!(
            cache
                .get(
                    LabelSource::Kns,
                    "kaspa:qfresh",
                    ONLINE_TTL.as_millis() as u64 + 1
                )
                .is_none()
        );
        let body: serde_json::Value = serde_json::json!({
            "success": true,
            "data": { "assets": [{ "id": 1, "asset": "alice.kas", "owner": "kaspa:q" }] }
        });
        assert_eq!(kns_name(&body).as_deref(), Some("alice.kas"));
        assert_eq!(
            kns_name(&serde_json::json!({ "data": { "assets": [] } })),
            None
        );
        assert!(!LabelSettings::default().any_enabled());
    }

    #[test]
    fn kas_fyi_answers_carry_link_and_categories() {
        let body = serde_json::json!({
            "tag": {
                "address": "kaspa:q",
                "name": "Bybit",
                "link": "https://www.bybit.com",
                "labels": ["Exchange", " cex "]
            }
        });
        let entry = kas_fyi_entry(&body, 7);
        assert_eq!(entry.name.as_deref(), Some("Bybit"));
        assert_eq!(entry.link.as_deref(), Some("https://www.bybit.com"));
        assert_eq!(entry.categories, ["exchange", "cex"]);
        let mut book = LabelBook::bundled();
        book.apply_online("kaspa:q", &entry);
        let label = book.get("kaspa:q").unwrap();
        assert_eq!(label.link.as_deref(), Some("https://www.bybit.com"));
        assert_eq!(label.categories, ["exchange", "cex"]);
        // A cache written before these fields existed still loads.
        let old: OnlineEntry =
            serde_json::from_str(r#"{"source":"Kns","name":"a.kas","fetched_at_ms":1}"#).unwrap();
        assert_eq!(old.link, None);
        assert!(old.categories.is_empty());
    }

    #[test]
    fn heuristics_are_the_weakest_source() {
        let mut book = LabelBook::bundled();
        // The mainnet burn address is on the public list, which wins; the testnet one
        // is only known by heuristic.
        let burn = "kaspa:qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqkx9awp4e";
        assert_eq!(book.get(burn).unwrap().source, LabelSource::Bundled);
        let testnet_burn = emission::burn_address("testnet-10").unwrap().to_string();
        assert_eq!(book.name(&testnet_burn), Some("Burn address"));
        assert_eq!(
            book.get(&testnet_burn).unwrap().source,
            LabelSource::Heuristic
        );

        assert!(book.set_heuristic("kaspa:qpool", "Mining pool (x)"));
        assert_eq!(book.name("kaspa:qpool"), Some("Mining pool (x)"));
        // Nothing changes when the same heuristic is set again, or for a listed address.
        assert!(!book.set_heuristic("kaspa:qpool", "Mining pool (x)"));
        assert!(!book.set_heuristic(burn, "Mining pool"));
        assert_eq!(book.get(burn).unwrap().source, LabelSource::Bundled);
        // Every other source overrides it.
        book.apply_kaspa_org(
            &[AddressName {
                address: "kaspa:qpool".into(),
                name: "Pool X".into(),
            }],
            SystemTime::now(),
        );
        assert_eq!(book.name("kaspa:qpool"), Some("Pool X"));
        assert!(!book.search("burn address").is_empty());
    }

    #[test]
    fn miner_tally_labels_a_pool_once() {
        let mut tally = MinerTally::default();
        for i in 1..POOL_MIN_BLOCKS {
            assert_eq!(tally.observe("kaspa:qa", Some("")), None, "block {i}");
        }
        assert_eq!(
            tally
                .observe("kaspa:qa", Some(" 2miners.com\u{0}"))
                .as_deref(),
            Some("Mining pool (2miners.com)")
        );
        assert_eq!(tally.observe("kaspa:qa", Some("other")), None);
        let mut tally = MinerTally::default();
        let last = (0..POOL_MIN_BLOCKS)
            .filter_map(|_| tally.observe("kaspa:qb", None))
            .last();
        assert_eq!(last.as_deref(), Some("Mining pool"));
        let long = "x".repeat(100);
        assert_eq!(clean_miner_tag(&long).len(), MAX_MINER_TAG_LEN);
    }

    #[test]
    fn kaspa_org_cache_expiry() {
        let now = SystemTime::now();
        assert!(is_stale(None, now));
        assert!(!is_stale(Some(now - Duration::from_secs(60)), now));
        assert!(is_stale(
            Some(now - REFRESH_AFTER - Duration::from_secs(1)),
            now
        ));
        // A file from the future (clock change) is not stale.
        assert!(!is_stale(Some(now + Duration::from_secs(60)), now));
    }

    #[test]
    fn entity_labels_outlive_user_and_kns_names() {
        let mut book = LabelBook::bundled();
        let burn = "kaspa:qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqkx9awp4e";
        assert_eq!(book.entity_name(burn), Some("Burn Address"));
        book.user
            .labels
            .insert(burn.to_string(), "Mine".to_string());
        book.apply_user();
        assert_eq!(book.name(burn), Some("Mine"));
        assert_eq!(book.entity_name(burn), Some("Burn Address"));

        // A .kas name alone is no entity; a kas.fyi tag is, and a stronger one wins.
        let other = "kaspa:qother";
        book.apply_online(
            other,
            &OnlineEntry {
                source: LabelSource::Kns,
                name: Some("other.kas".into()),
                link: None,
                categories: Vec::new(),
                fetched_at_ms: 0,
            },
        );
        assert_eq!(book.name(other), Some("other.kas"));
        assert_eq!(book.entity_name(other), None);
        // The shown label doesn't change (KNS outranks heuristics), the entity does.
        assert!(!book.set_heuristic(other, "Mining pool"));
        assert_eq!(book.name(other), Some("other.kas"));
        assert_eq!(book.entity_name(other), Some("Mining pool"));
        book.apply_online(
            other,
            &OnlineEntry {
                source: LabelSource::KasFyi,
                name: Some("Pool X".into()),
                link: None,
                categories: Vec::new(),
                fetched_at_ms: 0,
            },
        );
        assert_eq!(book.entity_name(other), Some("Pool X"));
        assert!(!LabelSource::User.is_entity());
        assert!(!LabelSource::Kns.is_entity());
        assert!(LabelSource::KaspaOrg.is_entity());
    }

    #[test]
    fn cluster_name_is_the_strongest_then_most_common_label() {
        let mut book = LabelBook::bundled();
        book.apply_kaspa_org(
            &[
                AddressName {
                    address: "kaspa:a".into(),
                    name: "Gate.io".into(),
                },
                AddressName {
                    address: "kaspa:b".into(),
                    name: "Gate.io".into(),
                },
                AddressName {
                    address: "kaspa:c".into(),
                    name: "Bybit".into(),
                },
            ],
            SystemTime::now(),
        );
        let members = ["kaspa:a", "kaspa:b", "kaspa:c", "kaspa:d"];
        assert_eq!(book.name_cluster(members), Some("Gate.io".into()));
        assert_eq!(book.name_cluster(["kaspa:d", "kaspa:e"]), None);
        // The user's own label outranks any count of public ones.
        book.user
            .labels
            .insert("kaspa:d".to_string(), "My cold wallet".to_string());
        book.apply_user();
        assert_eq!(book.name_cluster(members), Some("My cold wallet".into()));
    }

    #[test]
    fn user_labels_file_roundtrip() {
        let mut labels = UserLabels::default();
        labels.labels.insert("kaspa:a".into(), "Alice".into());
        let text = toml::to_string_pretty(&labels).unwrap();
        let loaded: UserLabels = toml::from_str(&text).unwrap();
        assert_eq!(loaded, labels);
    }
}
