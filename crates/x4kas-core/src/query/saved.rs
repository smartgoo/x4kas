//! The user's saved queries, `~/.x4kas/queries.toml`, and the built-in templates.
//!
//! A saved query is its text form (`text::print`) with a name and some bookkeeping: the
//! text is the canonical form, readable and editable in the file, and a query that no
//! longer parses (a hand edit, an older grammar) is kept with its error rather than
//! dropped, so the user can fix it in the editor.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::text;
use super::{Query, QueryError};
use crate::config;
use crate::format::now_ms;

/// The file's format, for a future migration.
const FORMAT: u32 = 1;

/// Saved queries are watched (re-run as new data is indexed, raising events on new
/// matches) when this is set and enabled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct QueryWatch {
    pub enabled: bool,
    /// How often to re-run, at most this often.
    pub every_secs: u64,
    /// Show a toast for every new match.
    pub notify: bool,
}

impl Default for QueryWatch {
    fn default() -> Self {
        Self {
            enabled: true,
            every_secs: Self::MIN_EVERY_SECS,
            notify: true,
        }
    }
}

impl QueryWatch {
    pub const MIN_EVERY_SECS: u64 = 30;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedQuery {
    /// A short random id, stable across renames.
    pub id: String,
    pub name: String,
    pub description: String,
    /// The query in its text form.
    pub text: String,
    pub pinned: bool,
    pub created_ms: u64,
    pub updated_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub watch: Option<QueryWatch>,
}

impl Default for SavedQuery {
    fn default() -> Self {
        Self {
            id: new_id(),
            name: String::new(),
            description: String::new(),
            text: String::new(),
            pinned: false,
            created_ms: 0,
            updated_ms: 0,
            watch: None,
        }
    }
}

impl SavedQuery {
    /// A new saved query of `query`, named `name`, stamped now.
    pub fn new(name: &str, description: &str, query: &Query) -> Self {
        let now = now_ms();
        Self {
            id: new_id(),
            name: name.trim().to_string(),
            description: description.trim().to_string(),
            text: text::print(query),
            pinned: false,
            created_ms: now,
            updated_ms: now,
            watch: None,
        }
    }

    /// The query, parsed from its text.
    pub fn query(&self) -> Result<Query, QueryError> {
        text::parse_valid(&self.text)
    }

    /// Replace the query, stamping the change.
    pub fn set_query(&mut self, query: &Query) {
        self.text = text::print(query);
        self.updated_ms = now_ms();
    }

    /// Whether this is a built-in template rather than the user's.
    pub fn is_preset(&self) -> bool {
        self.id.starts_with(PRESET_PREFIX)
    }

    /// Whether the query is watched and enabled.
    pub fn is_watched(&self) -> bool {
        self.watch.as_ref().is_some_and(|w| w.enabled)
    }
}

const PRESET_PREFIX: &str = "preset:";

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A 12-hex-character id from the clock and a counter (no randomness needed: ids only
/// have to differ within one user's file).
pub fn new_id() -> String {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mixed = now_ms()
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(u64::from(n).wrapping_mul(0x85EB_CA6B));
    format!("{:012x}", mixed & 0xffff_ffff_ffff)
}

/// The persisted list, `~/.x4kas/queries.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedQueries {
    pub format: u32,
    pub queries: Vec<SavedQuery>,
}

impl Default for SavedQueries {
    fn default() -> Self {
        Self {
            format: FORMAT,
            queries: Vec::new(),
        }
    }
}

impl SavedQueries {
    pub fn path() -> PathBuf {
        config::data_dir().join("queries.toml")
    }

    pub fn load() -> Result<Self> {
        Self::load_from(&Self::path())
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        Ok(toml::from_str(&std::fs::read_to_string(path)?)?)
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::path())
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, toml::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<&SavedQuery> {
        self.queries.iter().find(|q| q.id == id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut SavedQuery> {
        self.queries.iter_mut().find(|q| q.id == id)
    }

    /// The saved query named `name` (case-insensitive), the user's before the presets.
    pub fn by_name(&self, name: &str) -> Option<&SavedQuery> {
        self.queries
            .iter()
            .find(|q| q.name.eq_ignore_ascii_case(name.trim()))
    }

    /// The query `key` names, for the CLI: one of the user's by name or id, else a
    /// template by name, by id (`preset:<slug>`) or by its bare slug.
    pub fn find(&self, key: &str) -> Option<SavedQuery> {
        let key = key.trim();
        if let Some(q) = self.by_name(key).or_else(|| self.get(key)) {
            return Some(q.clone());
        }
        let as_id = format!("{PRESET_PREFIX}{}", key.to_ascii_lowercase());
        presets()
            .into_iter()
            .find(|p| p.name.eq_ignore_ascii_case(key) || p.id == key || p.id == as_id)
    }

    /// Add or replace (by id) a query.
    pub fn upsert(&mut self, query: SavedQuery) {
        match self.queries.iter_mut().find(|q| q.id == query.id) {
            Some(slot) => *slot = query,
            None => self.queries.push(query),
        }
    }

    pub fn remove(&mut self, id: &str) -> Option<SavedQuery> {
        let i = self.queries.iter().position(|q| q.id == id)?;
        Some(self.queries.remove(i))
    }

    /// Pinned first, then by name.
    pub fn sorted(&self) -> Vec<&SavedQuery> {
        let mut out: Vec<&SavedQuery> = self.queries.iter().collect();
        out.sort_by(|a, b| {
            b.pinned
                .cmp(&a.pinned)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        out
    }

    /// The watched queries.
    pub fn watched(&self) -> impl Iterator<Item = &SavedQuery> {
        self.queries.iter().filter(|q| q.is_watched())
    }
}

/// Built-in templates: useful questions, ready to run or to start from. Never saved;
/// "Save as…" copies one into the user's list.
pub fn presets() -> Vec<SavedQuery> {
    let preset = |slug: &str, name: &str, description: &str, text: &str| SavedQuery {
        id: format!("{PRESET_PREFIX}{slug}"),
        name: name.to_string(),
        description: description.to_string(),
        text: text.to_string(),
        pinned: false,
        created_ms: 0,
        updated_ms: 0,
        watch: None,
    };
    vec![
        preset(
            "large-transfers",
            "Large transfers",
            "Transactions moving at least 100,000 KAS in the last day, largest first.",
            "tx last 1d where output_total >= 100000 KAS and not is_coinbase order by output_total desc limit 200",
        ),
        preset(
            "krc-inscriptions",
            "KRC inscriptions",
            "KRC-20 inscription transactions of the last day.",
            "tx last 1d where protocol = krc limit 500",
        ),
        preset(
            "fee-outliers",
            "Fee outliers",
            "Transactions paying more than 10 sompi per gram in the last 6 hours.",
            "tx last 6h where fee_rate > 10.0 order by fee_rate desc limit 200",
        ),
        preset(
            "self-transfers",
            "Self transfers",
            "Transactions whose outputs all go back to the sender.",
            "tx last 1d where self_transfer and not is_coinbase limit 500",
        ),
        preset(
            "covenants",
            "Covenant and ZK scripts",
            "Transactions using introspection, the ZK precompile or covenants in the last week.",
            "tx last 7d where zk or introspection or covenant_created > 0 or covenant_spent > 0 limit 500",
        ),
        preset(
            "dust-fanouts",
            "Dust fan-outs",
            "Transactions with 50 outputs or more, none over 1 KAS.",
            "tx last 1d where output_count >= 50 and max_output < 1 KAS order by output_count desc limit 200",
        ),
        preset(
            "mining-share",
            "Mining share",
            "Blocks rewarded per miner over the last day, from the chain blocks' coinbases.",
            "payouts last 1d count, sum(amount) by miner order by count desc limit 50",
        ),
        preset(
            "node-versions",
            "Node versions",
            "Chain blocks per miner node version over the last day.",
            "blocks last 1d where is_chain count by node_version order by count desc",
        ),
        preset(
            "whales",
            "Whales by received",
            "Addresses that received the most over the indexed window.",
            "addresses order by received desc limit 100",
        ),
        preset(
            "busiest-addresses",
            "Busiest addresses",
            "Addresses with the most transactions in the indexed window.",
            "addresses order by tx_count desc limit 100",
        ),
        preset(
            "new-large-addresses",
            "New addresses with large receipts",
            "Addresses first seen in the last day that received at least 10,000 KAS in a few transactions.",
            "addresses where first_seen > 1d and received >= 10000 KAS and tx_count <= 3 order by received desc limit 100",
        ),
        preset(
            "block-sizes",
            "Fullest chain blocks",
            "Chain blocks accepting the most transactions in the last hour.",
            "blocks last 1h where is_chain order by accepted_txs desc limit 100",
        ),
        preset(
            "watchlist-activity",
            "Watchlist activity",
            "Transactions of the last day touching an address on the watchlist.",
            "tx last 1d where is_watched limit 500",
        ),
        preset(
            "hourly-throughput",
            "Hourly throughput",
            "Transactions and fees per hour over the last day.",
            "tx last 1d count, sum(fee) by time(1h) order by bucket",
        ),
        preset(
            "labelled-inflows",
            "Inflows to labelled addresses",
            "Transfers of the last day from unlabelled addresses to labelled ones (exchanges, pools), largest first.",
            "tx last 1d where receiver_label is not null and sender_label is null and not is_coinbase order by output_total desc limit 200",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::Entity;

    #[test]
    fn presets_have_unique_ids_and_parse() {
        let presets = presets();
        let mut ids: Vec<&str> = presets.iter().map(|p| p.id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), presets.len());
        for p in &presets {
            assert!(p.is_preset());
            let q = p.query().unwrap_or_else(|e| panic!("{}: {e}", p.name));
            assert_eq!(text::print(&q), p.text, "{} isn't canonical", p.name);
        }
        assert!(
            presets
                .iter()
                .any(|p| p.query().unwrap().entity == Entity::Payouts)
        );
        assert!(presets.iter().any(|p| p.id == "preset:krc-inscriptions"));
        assert!(presets.iter().any(|p| p.id == "preset:busiest-addresses"));
    }

    #[test]
    fn find_takes_names_ids_and_preset_slugs() {
        let mut list = SavedQueries::default();
        let mine = SavedQuery::new("Mine", "", &Query::default_for(Entity::Blocks));
        list.upsert(mine.clone());
        assert_eq!(list.find("mine").unwrap().id, mine.id);
        assert_eq!(list.find(&mine.id).unwrap().name, "Mine");
        assert_eq!(
            list.find("KRC inscriptions").unwrap().id,
            "preset:krc-inscriptions"
        );
        assert_eq!(
            list.find("preset:krc-inscriptions").unwrap().name,
            "KRC inscriptions"
        );
        assert_eq!(
            list.find("krc-inscriptions").unwrap().name,
            "KRC inscriptions"
        );
        assert_eq!(
            list.find(" Hourly-Throughput ").unwrap().name,
            "Hourly throughput"
        );
        assert!(list.find("nothing").is_none());
        // The user's query shadows a template of the same name.
        let mut shadow = SavedQuery::new(
            "Whales by received",
            "",
            &Query::default_for(Entity::Blocks),
        );
        shadow.id = "mine2".into();
        list.upsert(shadow);
        assert_eq!(list.find("whales by received").unwrap().id, "mine2");
    }

    #[test]
    fn load_save_roundtrip_in_tempdir() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queries.toml");
        assert_eq!(
            SavedQueries::load_from(&path).unwrap(),
            SavedQueries::default()
        );
        let mut list = SavedQueries::default();
        let q = text::parse("tx where fee > 1 KAS last 24h").unwrap();
        let mut saved = SavedQuery::new("  Big fees ", "", &q);
        saved.pinned = true;
        saved.watch = Some(QueryWatch {
            enabled: true,
            every_secs: 60,
            notify: false,
        });
        list.upsert(saved.clone());
        list.upsert(SavedQuery::new(
            "Other",
            "desc",
            &Query::default_for(Entity::Blocks),
        ));
        list.save_to(&path).unwrap();
        let loaded = SavedQueries::load_from(&path).unwrap();
        assert_eq!(loaded, list);
        assert_eq!(loaded.by_name("big fees").unwrap().id, saved.id);
        assert_eq!(loaded.sorted()[0].name, "Big fees");
        assert_eq!(loaded.watched().count(), 1);
        assert_eq!(loaded.get(&saved.id).unwrap().query().unwrap(), q);

        let mut list = loaded;
        let id = list.queries[1].id.clone();
        list.get_mut(&id).unwrap().set_query(&q);
        assert_eq!(list.get(&id).unwrap().text, text::print(&q));
        assert!(list.remove(&id).is_some());
        assert!(list.remove(&id).is_none());
        assert_eq!(list.queries.len(), 1);
    }

    #[test]
    fn broken_text_is_kept_with_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queries.toml");
        std::fs::write(
            &path,
            "format = 1\n[[queries]]\nid = \"abc\"\nname = \"Broken\"\ntext = \"tx where fees > 1\"\n",
        )
        .unwrap();
        let list = SavedQueries::load_from(&path).unwrap();
        assert_eq!(list.queries.len(), 1);
        let err = list.queries[0].query().unwrap_err();
        assert!(err.msg.contains("did you mean fee?"), "{err}");
        assert!(!list.queries[0].pinned);
    }

    #[test]
    fn ids_differ() {
        let a = new_id();
        let b = new_id();
        assert_ne!(a, b);
        assert_eq!(a.len(), 12);
    }
}
