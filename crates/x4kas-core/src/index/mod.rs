//! The address index: every accepted transaction the connected node has served, keyed
//! by address, for the retention window. Built from the VSPC v2 chain stream (see
//! `chain_stream`), stored on disk with fjall (an LSM-tree, so random-key inserts at
//! thousands of transactions per second stay cheap) under `~/.x4kas/index/<network>/`.
//!
//! Every data keyspace is partitioned into six-hour slabs (`tx_<n>`, `atx_<n>`, …), so
//! pruning behind the node's pruning point drops whole keyspaces instead of scanning, and
//! an address's totals are the sum of at most a few per-slab records. Only the address
//! interning tables, the clusters, the Dashboard's analytics buckets (`analytics`, see
//! [`analytics`]) and the manifest are global.
//!
//! The store is synchronous: the writer runs on its own blocking thread, and queries are
//! called inside `spawn_blocking`. Nothing here touches the GUI thread.

pub mod analytics;
pub mod cluster;
pub mod export;
pub mod peel;
pub mod query;
pub mod records;
pub mod task;
pub mod writer;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::RwLock;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use fjall::{Database, Keyspace, KeyspaceCreateOptions};
use serde::{Deserialize, Serialize};

use crate::config;
use records::{AddrId, Hash32, SLAB_MS, addr_key, decode, encode, parse_addr_key, slab_of};

/// Bumped when the on-disk layout or the meaning of stored data changes; an index with
/// another format is discarded and rebuilt from the node.
pub const FORMAT_VERSION: u32 = 3;

/// Block cache shared by all keyspaces.
const CACHE_BYTES: u64 = 256 * 1024 * 1024;

/// How long [`IndexStore::open`] keeps retrying while another process (or a writer that
/// is still shutting down) holds the directory lock.
const LOCK_RETRY: Duration = Duration::from_secs(5);

/// Where the index for `network` lives.
pub fn index_dir(network: &str) -> PathBuf {
    config::data_dir().join("index").join(network)
}

/// Delete the index of `network` from disk (nothing to do when there is none), so the
/// next [`IndexStore::open`] starts from scratch. Close the store first: a writer
/// holding it would keep writing into files that no longer have a directory.
pub fn discard(network: &str) -> Result<()> {
    let path = index_dir(network);
    match std::fs::remove_dir_all(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("discard {}", path.display())),
    }
}

/// Where the chain stream should resume: the last indexed chain block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    pub chain_block: Hash32,
    pub daa_score: u64,
    pub time_ms: u64,
}

/// Index-wide counters, persisted in the manifest with every batch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub position: Option<Position>,
    pub next_addr_id: AddrId,
    pub txs_indexed: u64,
}

/// The keyspaces of one six-hour slab.
#[derive(Clone)]
pub struct Slab {
    pub no: u64,
    /// `txid → IndexedTx`
    pub tx: Keyspace,
    /// `addr_id ‖ time_ms ‖ txid → balance delta`
    pub addr_tx: Keyspace,
    /// `block_hash ‖ txid → ()`, for reorg undo
    pub block_tx: Keyspace,
    /// `addr_id → AddrStats`
    pub stats: Keyspace,
    /// `addr_id ‖ peer_id → PeerStats`
    pub peers: Keyspace,
}

impl Slab {
    pub fn start_ms(&self) -> u64 {
        self.no * SLAB_MS
    }

    pub fn end_ms(&self) -> u64 {
        (self.no + 1) * SLAB_MS
    }

    fn keyspaces(&self) -> [&Keyspace; 5] {
        [
            &self.tx,
            &self.addr_tx,
            &self.block_tx,
            &self.stats,
            &self.peers,
        ]
    }
}

const SLAB_PREFIXES: [&str; 5] = ["tx", "atx", "btx", "ast", "apr"];

pub struct IndexStore {
    db: Database,
    /// `format`, `network`, `manifest`
    meta: Keyspace,
    /// `address → addr_id`
    addr_by_str: Keyspace,
    /// `addr_id → address`
    str_by_id: Keyspace,
    clusters: cluster::ClusterKeyspaces,
    /// The analytics engine's buckets and recent blocks (`analytics`).
    analytics: Keyspace,
    slabs: RwLock<BTreeMap<u64, Slab>>,
}

impl IndexStore {
    /// Open (or create) the index for `network` in the default data directory.
    pub fn open(network: &str) -> Result<Self> {
        Self::open_at(index_dir(network), network)
    }

    /// Open the index at `path`, discarding it first when its format or network doesn't
    /// match. Waits a few seconds for the directory lock if another process holds it.
    pub fn open_at(path: PathBuf, network: &str) -> Result<Self> {
        let store = Self::open_dir(&path)?;
        let format: Option<u32> = store.meta_get("format")?;
        let stored_network: Option<String> = store.meta_get("network")?;
        if format.is_none() {
            store.meta_put("format", &FORMAT_VERSION)?;
            store.meta_put("network", &network.to_string())?;
            return Ok(store);
        }
        if format == Some(FORMAT_VERSION) && stored_network.as_deref() == Some(network) {
            return Ok(store);
        }
        // Outdated or foreign: start over. Release the lock before removing the files.
        drop(store);
        std::fs::remove_dir_all(&path).with_context(|| format!("reset {}", path.display()))?;
        let store = Self::open_dir(&path)?;
        store.meta_put("format", &FORMAT_VERSION)?;
        store.meta_put("network", &network.to_string())?;
        Ok(store)
    }

    fn open_dir(path: &PathBuf) -> Result<Self> {
        std::fs::create_dir_all(path)?;
        let deadline = std::time::Instant::now() + LOCK_RETRY;
        let db = loop {
            match Database::builder(path).cache_size(CACHE_BYTES).open() {
                Ok(db) => break db,
                Err(fjall::Error::Locked) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(200));
                }
                Err(fjall::Error::Locked) => {
                    return Err(anyhow!(
                        "the address index at {} is in use by another x4kas process",
                        path.display()
                    ));
                }
                Err(e) => return Err(e).context("open address index"),
            }
        };
        let meta = db.keyspace("meta", KeyspaceCreateOptions::default)?;
        let addr_by_str = db.keyspace("addr_by_str", KeyspaceCreateOptions::default)?;
        let str_by_id = db.keyspace("str_by_id", KeyspaceCreateOptions::default)?;
        let clusters = cluster::ClusterKeyspaces {
            parent: db.keyspace("cl_parent", KeyspaceCreateOptions::default)?,
            size: db.keyspace("cl_size", KeyspaceCreateOptions::default)?,
            member: db.keyspace("cl_member", KeyspaceCreateOptions::default)?,
        };
        let analytics = db.keyspace("analytics", KeyspaceCreateOptions::default)?;

        // Reopen the slabs that exist on disk.
        let mut slabs = BTreeMap::new();
        for name in db.list_keyspace_names() {
            if let Some(no) = name.strip_prefix("tx_").and_then(|n| n.parse::<u64>().ok()) {
                slabs.insert(no, open_slab(&db, no)?);
            }
        }
        Ok(Self {
            db,
            meta,
            addr_by_str,
            str_by_id,
            clusters,
            analytics,
            slabs: RwLock::new(slabs),
        })
    }

    pub fn db(&self) -> &Database {
        &self.db
    }

    pub fn disk_space(&self) -> u64 {
        self.db.disk_space().unwrap_or(0)
    }

    // --- Manifest ---

    pub fn manifest(&self) -> Result<Manifest> {
        Ok(self.meta_get("manifest")?.unwrap_or_default())
    }

    /// The manifest's bytes, for writing it in a batch.
    pub fn encode_manifest(manifest: &Manifest) -> Result<Vec<u8>> {
        encode(manifest)
    }

    pub fn meta_keyspace(&self) -> &Keyspace {
        &self.meta
    }

    fn meta_get<T: serde::de::DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        match self.meta.get(key)? {
            Some(bytes) => Ok(Some(decode(&bytes)?)),
            None => Ok(None),
        }
    }

    fn meta_put<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
        self.meta.insert(key, encode(value)?)?;
        Ok(())
    }

    // --- Interning ---

    pub fn addr_by_str(&self) -> &Keyspace {
        &self.addr_by_str
    }

    pub fn str_by_id(&self) -> &Keyspace {
        &self.str_by_id
    }

    pub fn clusters(&self) -> &cluster::ClusterKeyspaces {
        &self.clusters
    }

    pub fn analytics_keyspace(&self) -> &Keyspace {
        &self.analytics
    }

    /// The id of an address already in the index.
    pub fn lookup(&self, address: &str) -> Result<Option<AddrId>> {
        Ok(self
            .addr_by_str
            .get(address)?
            .and_then(|v| parse_addr_key(&v)))
    }

    /// The address with `id`.
    pub fn address_of(&self, id: AddrId) -> Result<Option<String>> {
        Ok(self
            .str_by_id
            .get(addr_key(id))?
            .map(|v| String::from_utf8_lossy(&v).into_owned()))
    }

    /// Number of distinct addresses seen (approximate, from the manifest).
    pub fn address_count(&self) -> Result<u64> {
        Ok(self.manifest()?.next_addr_id as u64)
    }

    // --- Slabs ---

    /// Open slabs, oldest first.
    pub fn slabs(&self) -> Vec<Slab> {
        self.slabs
            .read()
            .expect("slab map poisoned")
            .values()
            .cloned()
            .collect()
    }

    /// The slab for `time_ms`, creating it if needed.
    pub fn slab_for(&self, time_ms: u64) -> Result<Slab> {
        let no = slab_of(time_ms);
        if let Some(slab) = self.slabs.read().expect("slab map poisoned").get(&no) {
            return Ok(slab.clone());
        }
        let mut slabs = self.slabs.write().expect("slab map poisoned");
        if let Some(slab) = slabs.get(&no) {
            return Ok(slab.clone());
        }
        let slab = open_slab(&self.db, no)?;
        slabs.insert(no, slab.clone());
        Ok(slab)
    }

    /// The slab for `time_ms` if it exists.
    pub fn existing_slab(&self, time_ms: u64) -> Option<Slab> {
        self.slabs
            .read()
            .expect("slab map poisoned")
            .get(&slab_of(time_ms))
            .cloned()
    }

    /// Drop every slab that ends at or before `floor_ms`. Returns how many were dropped.
    pub fn prune_before(&self, floor_ms: u64) -> Result<usize> {
        let old: Vec<Slab> = self
            .slabs
            .read()
            .expect("slab map poisoned")
            .values()
            .filter(|s| s.end_ms() <= floor_ms)
            .cloned()
            .collect();
        for slab in &old {
            for ks in slab.keyspaces() {
                self.db.delete_keyspace(ks.clone())?;
            }
            self.slabs
                .write()
                .expect("slab map poisoned")
                .remove(&slab.no);
        }
        Ok(old.len())
    }

    /// The oldest and newest timestamps the index covers, from its slabs.
    pub fn coverage(&self) -> Option<(u64, u64)> {
        let slabs = self.slabs.read().expect("slab map poisoned");
        let first = slabs.values().next()?;
        let last = slabs.values().next_back()?;
        Some((first.start_ms(), last.end_ms()))
    }

    /// Flush the journal so everything written so far survives a crash.
    pub fn persist(&self) -> Result<()> {
        self.db.persist(fjall::PersistMode::SyncAll)?;
        Ok(())
    }
}

fn open_slab(db: &Database, no: u64) -> Result<Slab> {
    let mut ks = SLAB_PREFIXES
        .iter()
        .map(|p| db.keyspace(&format!("{p}_{no}"), KeyspaceCreateOptions::default));
    let mut next = || ks.next().expect("five slab keyspaces");
    Ok(Slab {
        no,
        tx: next()?,
        addr_tx: next()?,
        block_tx: next()?,
        stats: next()?,
        peers: next()?,
    })
}

/// Hex of a hash or transaction id.
pub fn hex(hash: &Hash32) -> String {
    hash.iter().map(|b| format!("{b:02x}")).collect()
}

/// A 32-byte hash from hex.
pub fn parse_hex(s: &str) -> Option<Hash32> {
    let s = s.trim();
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

/// A store in a temporary directory. The store is dropped before the directory (struct
/// fields drop in order), as the engine's shutdown needs its files.
#[cfg(test)]
pub(crate) struct TempStore {
    pub store: std::sync::Arc<IndexStore>,
    _dir: tempfile::TempDir,
}

#[cfg(test)]
pub(crate) fn temp_store() -> TempStore {
    let dir = tempfile::tempdir().unwrap();
    let store = IndexStore::open_at(dir.path().join("index"), "mainnet").unwrap();
    TempStore {
        store: std::sync::Arc::new(store),
        _dir: dir,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip() {
        let h = [0xabu8; 32];
        assert_eq!(parse_hex(&hex(&h)), Some(h));
        assert_eq!(parse_hex("abc"), None);
    }

    #[test]
    fn opens_slabs_on_demand_and_reopens_them() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index");
        {
            let store = IndexStore::open_at(path.clone(), "mainnet").unwrap();
            store.slab_for(0).unwrap();
            store.slab_for(SLAB_MS * 3).unwrap();
            assert_eq!(store.slabs().len(), 2);
            assert_eq!(store.coverage(), Some((0, SLAB_MS * 4)));
        }
        let store = IndexStore::open_at(path.clone(), "mainnet").unwrap();
        assert_eq!(store.slabs().len(), 2);
        assert_eq!(store.prune_before(SLAB_MS).unwrap(), 1);
        assert_eq!(store.slabs().len(), 1);
        assert_eq!(store.existing_slab(0).map(|s| s.no), None);
        assert_eq!(store.existing_slab(SLAB_MS * 3).map(|s| s.no), Some(3));
    }

    #[test]
    fn another_network_resets_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index");
        {
            let store = IndexStore::open_at(path.clone(), "mainnet").unwrap();
            store.slab_for(0).unwrap();
        }
        let store = IndexStore::open_at(path, "testnet-10").unwrap();
        assert!(store.slabs().is_empty());
    }
}
