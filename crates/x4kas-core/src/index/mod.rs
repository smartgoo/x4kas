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
use fjall::config::PinningPolicy;
use fjall::{Database, Keyspace, KeyspaceCreateOptions};
use serde::{Deserialize, Serialize};

use crate::config;
use records::{
    AddrId, Hash32, IndexedTx, SLAB_MS, addr_key, decode, encode, parse_addr_key, slab_of,
};

/// Bumped when the on-disk layout or the meaning of stored data changes; an index with
/// another format is discarded and rebuilt from the node.
pub const FORMAT_VERSION: u32 = 7;

/// Block cache shared by all keyspaces.
const CACHE_BYTES: u64 = 256 * 1024 * 1024;

/// Memtable size of the small keyspaces (the manifest, cluster sizes, analytics): fjall's
/// 64 MiB default is for the data keyspaces.
const SMALL_MEMTABLE_BYTES: u64 = 8 * 1024 * 1024;

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
    /// Block records held (chain blocks and the merged blocks their transactions name).
    #[serde(default)]
    pub blocks_indexed: u64,
    /// Commits so far; the writer stamps the counterparty deltas of each commit with it
    /// (`records::peer_delta_key`), so no commit overwrites another's.
    #[serde(default)]
    pub seq: u64,
    /// The time of the first chain block indexed with each opt-in feature on since it
    /// was last turned on (`config::IndexFeature`); `None` while it is off. Queries over
    /// earlier transactions say they can't see what wasn't kept.
    #[serde(default)]
    pub full_payloads_from_ms: Option<u64>,
    #[serde(default)]
    pub redeem_scripts_from_ms: Option<u64>,
}

impl Manifest {
    /// Since when `feature` has been indexed, if it is.
    pub fn feature_from_ms(&self, feature: crate::config::IndexFeature) -> Option<u64> {
        match feature {
            crate::config::IndexFeature::FullPayloads => self.full_payloads_from_ms,
            crate::config::IndexFeature::RedeemScripts => self.redeem_scripts_from_ms,
        }
    }

    fn feature_from_mut(&mut self, feature: crate::config::IndexFeature) -> &mut Option<u64> {
        match feature {
            crate::config::IndexFeature::FullPayloads => &mut self.full_payloads_from_ms,
            crate::config::IndexFeature::RedeemScripts => &mut self.redeem_scripts_from_ms,
        }
    }

    /// Record which features this commit indexed with: a feature turned on starts at
    /// `first_ms` (the commit's first chain block), or at 0 (everything) when this is the
    /// index's first commit (`fresh`); one turned off forgets its start.
    pub fn track_features(
        &mut self,
        settings: &crate::config::IndexSettings,
        first_ms: u64,
        fresh: bool,
    ) {
        for feature in crate::config::IndexFeature::ALL {
            let from = self.feature_from_mut(feature);
            if !settings.enabled(feature) {
                *from = None;
            } else if from.is_none() {
                *from = Some(if fresh { 0 } else { first_ms });
            }
        }
    }
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
    /// `addr_id ‖ peer_id ‖ seq → PeerDelta`, every commit's change to the flows between
    /// an address and a counterparty (summed on read)
    pub peers: Keyspace,
    /// `protocol code ‖ time_ms ‖ txid → ()`, the transactions of each protocol
    pub protocol_tx: Keyspace,
    /// `time_ms ‖ txid → TxSummary`, every transaction by time
    pub time_tx: Keyspace,
    /// `block_hash → BlockRecord`, the blocks whose own timestamp falls in the slab
    pub blocks: Keyspace,
    /// `time_ms ‖ block_hash → ()`, those blocks by time
    pub time_blocks: Keyspace,
    /// `txid → payload`, the whole payload of a transaction whose payload is longer than
    /// the record's head (`records::PAYLOAD_HEAD`), while `IndexFeature::FullPayloads` is on
    pub payloads: Keyspace,
    /// `txid ‖ input → redeem script`, what each P2SH spend revealed, while
    /// `IndexFeature::RedeemScripts` is on
    pub redeem_scripts: Keyspace,
}

impl Slab {
    pub fn start_ms(&self) -> u64 {
        self.no * SLAB_MS
    }

    pub fn end_ms(&self) -> u64 {
        (self.no + 1) * SLAB_MS
    }

    /// Every keyspace of the slab.
    pub fn keyspaces(&self) -> [&Keyspace; 11] {
        [
            &self.tx,
            &self.addr_tx,
            &self.block_tx,
            &self.stats,
            &self.peers,
            &self.protocol_tx,
            &self.time_tx,
            &self.blocks,
            &self.time_blocks,
            &self.payloads,
            &self.redeem_scripts,
        ]
    }

    /// The redeem scripts `txid` revealed, by input, oldest input first.
    pub fn redeem_scripts_of(&self, txid: &Hash32) -> Result<Vec<(u32, Vec<u8>)>> {
        let mut out = Vec::new();
        for guard in self.redeem_scripts.prefix(txid) {
            let (key, value) = guard.into_inner()?;
            if let Some(index) = key.get(32..36) {
                let index = u32::from_be_bytes(index.try_into().expect("four bytes"));
                out.push((index, value.to_vec()));
            }
        }
        Ok(out)
    }

    /// The whole payload of `tx` (`txid`'s record in this slab): its head when that is
    /// all of it, else the stored payload (the head if that is missing).
    pub fn payload(&self, txid: &Hash32, tx: &IndexedTx) -> Result<Vec<u8>> {
        if tx.payload_len as usize <= tx.payload_head.len() {
            return Ok(tx.payload_head.clone());
        }
        Ok(match self.payloads.get(txid)? {
            Some(bytes) => bytes.to_vec(),
            None => tx.payload_head.clone(),
        })
    }

    /// Whether the slab holds anything timestamped within `from_ms..to_ms`.
    pub fn overlaps(&self, from_ms: u64, to_ms: u64) -> bool {
        self.start_ms() < to_ms && self.end_ms() > from_ms
    }
}

pub struct IndexStore {
    db: Database,
    network: String,
    /// The format of the index that was on disk and had to be discarded, if any.
    rebuilt_from: Option<u32>,
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
        let store = Self::open_dir(&path, network)?;
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
        let mut store = Self::open_dir(&path, network)?;
        store.meta_put("format", &FORMAT_VERSION)?;
        store.meta_put("network", &network.to_string())?;
        store.rebuilt_from = format.filter(|_| stored_network.as_deref() == Some(network));
        Ok(store)
    }

    /// Open the index of `network` as it is on disk, to read it: for a CLI command that
    /// must never change it. Unlike [`IndexStore::open`] it fails at once, without
    /// waiting for the lock, when another x4kas process holds the index, when there is
    /// none yet, or when its format or network isn't this build's (`open` would discard
    /// and rebuild it); it deletes nothing.
    pub fn open_existing(network: &str) -> Result<Self> {
        Self::open_existing_at(index_dir(network), network)
    }

    /// [`IndexStore::open_existing`] at `path`.
    pub fn open_existing_at(path: PathBuf, network: &str) -> Result<Self> {
        if !path.is_dir() {
            return Err(anyhow!(
                "no address index for {network} at {}: build it in the GUI with a direct \
                 node or with `x4kas-cli index run --url …`",
                path.display()
            ));
        }
        let db = match Database::builder(&path).cache_size(CACHE_BYTES).open() {
            Ok(db) => db,
            Err(fjall::Error::Locked) => {
                return Err(anyhow!(
                    "the address index at {} is in use by another x4kas process",
                    path.display()
                ));
            }
            Err(e) => return Err(e).context("open address index"),
        };
        let store = Self::from_db(db, network)?;
        let format: Option<u32> = store.meta_get("format")?;
        let stored_network: Option<String> = store.meta_get("network")?;
        if format != Some(FORMAT_VERSION) {
            return Err(anyhow!(
                "the address index at {} has format {}, this build reads format \
                 {FORMAT_VERSION}: open it in the GUI or run `x4kas-cli index run --url …` \
                 to rebuild it",
                path.display(),
                format.map_or("none".to_string(), |f| f.to_string())
            ));
        }
        if stored_network.as_deref() != Some(network) {
            return Err(anyhow!(
                "the address index at {} is of {}, not {network}",
                path.display(),
                stored_network.unwrap_or_default()
            ));
        }
        Ok(store)
    }

    fn open_dir(path: &PathBuf, network: &str) -> Result<Self> {
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
        Self::from_db(db, network)
    }

    /// The store over an open database: its global keyspaces and the slabs on disk.
    fn from_db(db: Database, network: &str) -> Result<Self> {
        let meta = db.keyspace("meta", small_keyspace)?;
        let addr_by_str = db.keyspace("addr_by_str", point_read_keyspace)?;
        let str_by_id = db.keyspace("str_by_id", point_read_keyspace)?;
        let clusters = cluster::ClusterKeyspaces {
            parent: db.keyspace("cl_parent", point_read_keyspace)?,
            size: db.keyspace("cl_size", small_point_read_keyspace)?,
            member: db.keyspace("cl_member", KeyspaceCreateOptions::default)?,
        };
        let analytics = db.keyspace("analytics", small_keyspace)?;

        // Reopen the slabs that exist on disk.
        let mut slabs = BTreeMap::new();
        for name in db.list_keyspace_names() {
            if let Some(no) = name.strip_prefix("tx_").and_then(|n| n.parse::<u64>().ok()) {
                slabs.insert(no, open_slab(&db, no)?);
            }
        }
        Ok(Self {
            db,
            network: network.to_string(),
            rebuilt_from: None,
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

    /// The network the index is of (`mainnet`, `testnet-10`, …).
    pub fn network(&self) -> &str {
        &self.network
    }

    /// The format of the index this open discarded (an older x4kas wrote it), so the
    /// rebuild from the node can be explained.
    pub fn rebuilt_from(&self) -> Option<u32> {
        self.rebuilt_from
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

    /// The slabs holding anything timestamped within `from_ms..to_ms`, oldest first.
    pub fn slabs_in(&self, from_ms: u64, to_ms: u64) -> Vec<Slab> {
        self.slabs
            .read()
            .expect("slab map poisoned")
            .values()
            .filter(|s| s.overlaps(from_ms, to_ms))
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

/// A keyspace the writer looks keys up in while indexing (interning, stats, cluster
/// parents, block records): its filter blocks stay in memory at every level, so a miss
/// (a fresh address, most of the time) costs no disk read. fjall pins them for the first
/// level only.
fn point_read_keyspace() -> KeyspaceCreateOptions {
    KeyspaceCreateOptions::default().filter_block_pinning_policy(PinningPolicy::new([true]))
}

fn small_keyspace() -> KeyspaceCreateOptions {
    KeyspaceCreateOptions::default().max_memtable_size(SMALL_MEMTABLE_BYTES)
}

fn small_point_read_keyspace() -> KeyspaceCreateOptions {
    point_read_keyspace().max_memtable_size(SMALL_MEMTABLE_BYTES)
}

fn open_slab(db: &Database, no: u64) -> Result<Slab> {
    let open = |p: &str, options: fn() -> KeyspaceCreateOptions| {
        db.keyspace(&format!("{p}_{no}"), options)
    };
    Ok(Slab {
        no,
        tx: open("tx", point_read_keyspace)?,
        addr_tx: open("atx", KeyspaceCreateOptions::default)?,
        block_tx: open("btx", point_read_keyspace)?,
        stats: open("ast", point_read_keyspace)?,
        peers: open("apr", KeyspaceCreateOptions::default)?,
        protocol_tx: open("ptx", KeyspaceCreateOptions::default)?,
        time_tx: open("ttx", KeyspaceCreateOptions::default)?,
        blocks: open("blk", point_read_keyspace)?,
        time_blocks: open("tbk", KeyspaceCreateOptions::default)?,
        payloads: open("pay", KeyspaceCreateOptions::default)?,
        redeem_scripts: open("rds", KeyspaceCreateOptions::default)?,
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
            assert_eq!(store.network(), "mainnet");
            let nos = |slabs: Vec<Slab>| slabs.iter().map(|s| s.no).collect::<Vec<_>>();
            assert_eq!(nos(store.slabs_in(0, 1)), vec![0]);
            assert_eq!(nos(store.slabs_in(SLAB_MS, SLAB_MS * 3)), Vec::<u64>::new());
            assert_eq!(
                nos(store.slabs_in(SLAB_MS - 1, SLAB_MS * 3 + 1)),
                vec![0, 3]
            );
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
        assert_eq!(store.rebuilt_from(), None);
    }

    #[test]
    fn an_older_format_resets_the_store_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index");
        {
            let store = IndexStore::open_at(path.clone(), "mainnet").unwrap();
            store.slab_for(0).unwrap();
            store.meta_put("format", &(FORMAT_VERSION - 1)).unwrap();
        }
        let store = IndexStore::open_at(path.clone(), "mainnet").unwrap();
        assert!(store.slabs().is_empty());
        assert_eq!(store.rebuilt_from(), Some(FORMAT_VERSION - 1));
        drop(store);
        let store = IndexStore::open_at(path, "mainnet").unwrap();
        assert_eq!(store.rebuilt_from(), None);
    }

    #[test]
    fn open_existing_never_resets_or_creates() {
        fn refused(path: &std::path::Path, network: &str) -> String {
            match IndexStore::open_existing_at(path.to_path_buf(), network) {
                Ok(_) => panic!("opened {network} at {}", path.display()),
                Err(e) => format!("{e:#}"),
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index");
        assert!(refused(&path, "mainnet").contains("no address index"));
        assert!(!path.exists());
        {
            let store = IndexStore::open_at(path.clone(), "mainnet").unwrap();
            store.slab_for(0).unwrap();
            // Held open: a second opener fails at once.
            assert!(refused(&path, "mainnet").contains("in use"));
        }
        let store = IndexStore::open_existing_at(path.clone(), "mainnet").unwrap();
        assert_eq!(store.slabs().len(), 1);
        drop(store);
        assert!(refused(&path, "testnet-10").contains("not testnet-10"));
        let store = IndexStore::open_existing_at(path.clone(), "mainnet").unwrap();
        store.meta_put("format", &(FORMAT_VERSION - 1)).unwrap();
        drop(store);
        assert!(refused(&path, "mainnet").contains("has format"));
        // Nothing was discarded: the slab is still there for a normal open to reset.
        let store = IndexStore::open_at(path, "mainnet").unwrap();
        assert_eq!(store.rebuilt_from(), Some(FORMAT_VERSION - 1));
    }
}
