//! Address labels: who an address belongs to. Sources, by precedence: the user's own
//! labels (`~/.x4kas/labels/user.toml`), the public list at `api.kaspa.org`
//! (exchanges, pools, funds, bridges; refreshed daily and cached at
//! `~/.x4kas/labels/kaspa_org.json`), and a snapshot of that list bundled with the app
//! so labels show offline. The kaspa.org list is fetched in bulk, so looking up an
//! address never tells anyone which one.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::app::App;
use crate::config;

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
}

impl LabelSource {
    pub fn label(&self) -> &'static str {
        match self {
            Self::User => "your label",
            Self::KasFyi => "kas.fyi",
            Self::KaspaOrg => "api.kaspa.org",
            Self::Kns => "KNS",
            Self::Bundled => "bundled list",
        }
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
pub struct Label {
    pub name: String,
    pub source: LabelSource,
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
    user: UserLabels,
    /// When the kaspa.org list was last fetched (from the cache file's age on load).
    pub kaspa_org_refreshed: Option<SystemTime>,
}

impl Default for LabelBook {
    fn default() -> Self {
        Self::bundled()
    }
}

impl LabelBook {
    /// The bundled snapshot only.
    pub fn bundled() -> Self {
        let mut book = Self {
            map: HashMap::new(),
            user: UserLabels::default(),
            kaspa_org_refreshed: None,
        };
        if let Ok(list) = serde_json::from_str::<Vec<AddressName>>(BUNDLED) {
            book.apply_list(&list, LabelSource::Bundled);
        }
        book
    }

    /// The bundled snapshot, the cached kaspa.org list if any, and the user's labels.
    pub fn load() -> Self {
        let mut book = Self::bundled();
        if let Ok((list, saved_at)) = load_kaspa_org_cache() {
            book.apply_list(&list, LabelSource::KaspaOrg);
            book.kaspa_org_refreshed = saved_at;
        }
        book.apply_online_cache(&OnlineCache::load(), crate::format::now_ms());
        book.user = UserLabels::load().unwrap_or_default();
        book.apply_user();
        book
    }

    pub fn get(&self, address: &str) -> Option<&Label> {
        self.map.get(address)
    }

    pub fn name(&self, address: &str) -> Option<&str> {
        self.get(address).map(|l| l.name.as_str())
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
        let mut rebuilt = Self::bundled();
        if let Ok((list, saved_at)) = load_kaspa_org_cache() {
            rebuilt.apply_list(&list, LabelSource::KaspaOrg);
            rebuilt.kaspa_org_refreshed = saved_at;
        }
        rebuilt.apply_online_cache(&OnlineCache::load(), crate::format::now_ms());
        rebuilt.user = std::mem::take(&mut self.user);
        rebuilt.apply_user();
        *self = rebuilt;
        Ok(())
    }

    /// Apply a freshly fetched kaspa.org list.
    pub fn apply_kaspa_org(&mut self, list: &[AddressName], fetched_at: SystemTime) {
        self.apply_list(list, LabelSource::KaspaOrg);
        self.kaspa_org_refreshed = Some(fetched_at);
        self.apply_user();
    }

    fn apply_list(&mut self, list: &[AddressName], source: LabelSource) {
        for entry in list {
            self.apply_one(&entry.address, &entry.name, source);
        }
    }

    /// Set a label unless a stronger source already names the address.
    fn apply_one(&mut self, address: &str, name: &str, source: LabelSource) {
        if self
            .map
            .get(address)
            .is_some_and(|l| l.source.rank() > source.rank())
        {
            return;
        }
        self.map.insert(
            address.to_string(),
            Label {
                name: name.to_string(),
                source,
            },
        );
    }

    /// Apply an online answer (kas.fyi or KNS).
    pub fn apply_online(&mut self, address: &str, entry: &OnlineEntry) {
        if let Some(name) = &entry.name {
            self.apply_one(address, name, entry.source);
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
            self.map.insert(
                address.clone(),
                Label {
                    name: name.clone(),
                    source: LabelSource::User,
                },
            );
        }
    }
}

fn load_kaspa_org_cache() -> Result<(Vec<AddressName>, Option<SystemTime>)> {
    let path = kaspa_org_cache_path();
    let text = std::fs::read_to_string(&path)?;
    let saved_at = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    Ok((serde_json::from_str(&text)?, saved_at))
}

/// Whether the cached kaspa.org list is missing or older than [`REFRESH_AFTER`].
pub fn kaspa_org_cache_is_stale() -> bool {
    std::fs::metadata(kaspa_org_cache_path())
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
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
            let name = match response.status().as_u16() {
                404 => None,
                _ => {
                    let body: serde_json::Value = response.error_for_status()?.json().await?;
                    body.pointer("/tag/name")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                }
            };
            let entry = OnlineEntry {
                source: LabelSource::KasFyi,
                name,
                fetched_at_ms: now,
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
                fetched_at_ms: now,
            };
            cache.put(address, entry.clone());
            learned.push(entry);
        }
    }

    let _ = cache.save();
    Ok(learned)
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
                fetched_at_ms: 0,
            },
        );
        assert_eq!(book.get(burn).unwrap().source, LabelSource::KasFyi);
        book.apply_online(
            "kaspa:qfresh",
            &OnlineEntry {
                source: LabelSource::Kns,
                name: None,
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
    fn user_labels_file_roundtrip() {
        let mut labels = UserLabels::default();
        labels.labels.insert("kaspa:a".into(), "Alice".into());
        let text = toml::to_string_pretty(&labels).unwrap();
        let loaded: UserLabels = toml::from_str(&text).unwrap();
        assert_eq!(loaded, labels);
    }
}
