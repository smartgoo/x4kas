//! Turns VSPC v2 responses into index writes: one atomic batch per response, with
//! reorg undo and already-indexed chain blocks skipped, so re-applying a response after
//! a crash or a restart from an older position changes nothing. The same batch carries
//! the analytics engine's changes (`super::analytics`), so the Dashboard's metrics and
//! the index always agree on the position.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::str::FromStr;
use std::sync::Arc;

use anyhow::Result;
use fjall::OwnedWriteBatch as WriteBatch;
use kaspa_addresses::{Prefix, Version};
use kaspa_rpc_core::{
    GetVirtualChainFromBlockV2Response, RpcChainBlockAcceptedTransactions, RpcHash,
    RpcOptionalHeader, RpcOptionalTransaction,
};
use kaspa_wrpc_client::prelude::NetworkId;

use super::cluster::{self, CHANGE_THRESHOLD, Clusters, OutputTrait};
use super::records::{
    AddrId, AddrStats, BlockKind, BlockRecord, Hash32, IndexedTx, PAYLOAD_HEAD, Payout, PeerDelta,
    SCRIPT_UNKNOWN, Subnetwork, TxInput, TxOutput, addr_key, addr_tx_key, block_tx_key, decode,
    encode, encode_delta, peer_delta_key, protocol_tx_key, time_key,
};
use super::{IndexStore, Manifest, Position, Slab, analytics};
use crate::analytics::AnalyticsEngine;
use crate::format::now_ms;
use crate::labels::LabelBook;
use crate::tx_inspect::{
    OpcodeUsage, ScriptClass, detect_protocol, output_script_opcodes, parse_coinbase_payload,
    redeem_script_opcodes, script_address, script_class,
};

/// Interned addresses kept in memory; the map is cleared when it grows past this.
const INTERN_CACHE_MAX: usize = 1_000_000;
/// A transaction with more sender × receiver pairs than this (a huge batch payout from
/// many inputs) gets no counterparty entries: they'd be noise and cost a lot.
const MAX_PEER_PAIRS: usize = 10_000;
/// Removed chain blocks are looked for in this many newest slabs.
const REORG_SLABS: usize = 2;
/// A response is committed in chunks of this many chain blocks (the node sends up to ten
/// mergesets' worth, 2,480 at 10 BPS, which at full blocks is a batch of hundreds of
/// megabytes). Each chunk is atomic and advances the manifest, and a chain block is never
/// split, so a crash between chunks leaves a position to resume from and the skip of
/// already-indexed chain blocks makes the replay harmless.
const COMMIT_BLOCKS: usize = 250;

/// What one applied response did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BatchReport {
    pub chain_blocks: usize,
    pub txs: usize,
    /// Block records created (chain blocks by their header, merged blocks by their
    /// transactions), less those a reorg removed.
    pub blocks: i64,
    /// Chain blocks already in the index (a replayed response).
    pub skipped_blocks: usize,
    pub reorged_blocks: usize,
    /// Removed chain blocks the index didn't have (already pruned, or never indexed).
    pub unresolved_reorgs: Vec<String>,
    /// Unions the cluster size cap refused.
    pub cluster_cap_hits: u64,
    pub newest: Option<Position>,
    /// Removed chain blocks the analytics engine had already finalized into buckets,
    /// which can't be unwound (its counts are then slightly off).
    pub analytics_reorgs: Vec<String>,
}

pub struct IndexWriter {
    store: Arc<IndexStore>,
    manifest: Manifest,
    /// The network's address prefix, for the miner address in coinbase payloads.
    prefix: Prefix,
    intern_cache: HashMap<String, AddrId>,
    /// Entity labels, for the clustering guard.
    labels: Arc<LabelBook>,
    /// The Dashboard's metrics, persisted with every batch.
    analytics: AnalyticsEngine,
}

/// A converted transaction with what clustering needs to know about it.
struct Converted {
    txid: Hash32,
    tx: IndexedTx,
    /// Whether the inputs may be unioned (see `cluster::may_union`).
    may_union: bool,
}

/// Per-commit buffer for stats (read-modify-write: an address touched by many
/// transactions in one response is read and written once) and counterparty deltas
/// (write-only: a transaction's sender × receiver pairs are many, and reading each pair's
/// record back made the writer disk-bound once the counterparty keyspace outgrew the
/// cache; every commit appends one delta per pair instead, and readers sum them).
#[derive(Default)]
struct Overlay {
    stats: HashMap<(u64, AddrId), AddrStats>,
    peers: HashMap<(u64, AddrId, AddrId), PeerDelta>,
    slabs: HashMap<u64, Slab>,
}

impl Overlay {
    fn stats_mut(&mut self, slab: &Slab, addr: AddrId) -> Result<&mut AddrStats> {
        self.slabs.entry(slab.no).or_insert_with(|| slab.clone());
        match self.stats.entry((slab.no, addr)) {
            std::collections::hash_map::Entry::Occupied(e) => Ok(e.into_mut()),
            std::collections::hash_map::Entry::Vacant(e) => {
                let stored = match slab.stats.get(addr_key(addr))? {
                    Some(bytes) => decode(&bytes)?,
                    None => AddrStats::default(),
                };
                Ok(e.insert(stored))
            }
        }
    }

    fn peer_mut(&mut self, slab: &Slab, addr: AddrId, peer: AddrId) -> &mut PeerDelta {
        self.slabs.entry(slab.no).or_insert_with(|| slab.clone());
        self.peers.entry((slab.no, addr, peer)).or_default()
    }

    /// Write everything; the counterparty deltas under the commit's `seq`.
    fn flush(self, batch: &mut WriteBatch, seq: u64) -> Result<()> {
        for ((slab_no, addr), stats) in self.stats {
            let slab = &self.slabs[&slab_no];
            if stats.tx_count == 0 {
                batch.remove(&slab.stats, addr_key(addr));
            } else {
                batch.insert(&slab.stats, addr_key(addr), encode(&stats)?);
            }
        }
        for ((slab_no, addr, peer), delta) in self.peers {
            if delta.is_zero() {
                continue;
            }
            let slab = &self.slabs[&slab_no];
            batch.insert(
                &slab.peers,
                peer_delta_key(addr, peer, seq),
                encode(&delta)?,
            );
        }
        Ok(())
    }
}

/// Per-batch read-modify-write buffer for block records (`blk_<n>`): a chain block's
/// header, its coinbase (which the next chain block brings) and the accepted
/// transactions contained in every block all touch the same record.
#[derive(Default)]
struct BlockOverlay {
    /// Records to write, with whether the store already has them.
    records: HashMap<Hash32, (Slab, BlockRecord, bool)>,
    /// Records to delete (reorged chain blocks), by hash with their slab and time.
    deleted: HashMap<Hash32, (Slab, u64)>,
    /// Records created less records deleted.
    created: i64,
}

impl BlockOverlay {
    /// The record of `hash`, loaded from the slab of `time_ms` or started as `kind`
    /// merged by `merging` if the store has none (or it was deleted in this batch).
    fn get_or_load(
        &mut self,
        store: &IndexStore,
        hash: &Hash32,
        time_ms: u64,
        kind: BlockKind,
        merging: Hash32,
    ) -> Result<&mut BlockRecord> {
        match self.records.entry(*hash) {
            std::collections::hash_map::Entry::Occupied(e) => Ok(&mut e.into_mut().1),
            std::collections::hash_map::Entry::Vacant(e) => {
                let slab = store.slab_for(time_ms)?;
                let stored = if self.deleted.contains_key(hash) {
                    None
                } else {
                    slab.blocks.get(hash)?
                };
                let (record, on_disk) = match stored {
                    Some(bytes) => (decode(&bytes)?, true),
                    None => {
                        self.created += 1;
                        (BlockRecord::new(kind, time_ms, merging), false)
                    }
                };
                Ok(&mut e.insert((slab, record, on_disk)).1)
            }
        }
    }

    /// Forget a block entirely (a reorged chain block).
    fn remove(&mut self, store: &IndexStore, hash: &Hash32, time_ms: u64) -> Result<()> {
        if let Some((slab, record, on_disk)) = self.records.remove(hash) {
            self.created -= 1;
            if on_disk {
                self.deleted.insert(*hash, (slab, record.time_ms));
            }
            return Ok(());
        }
        if self.deleted.contains_key(hash) {
            return Ok(());
        }
        let slab = store.slab_for(time_ms)?;
        if let Some(bytes) = slab.blocks.get(hash)? {
            let record: BlockRecord = decode(&bytes)?;
            self.created -= 1;
            self.deleted.insert(*hash, (slab, record.time_ms));
        }
        Ok(())
    }

    /// Write everything; returns how many records the store gains (negative when a
    /// reorg removed more than the batch created).
    fn flush(self, batch: &mut WriteBatch) -> Result<i64> {
        let mut delta = self.created;
        for (hash, (slab, time_ms)) in self.deleted {
            batch.remove(&slab.blocks, hash);
            batch.remove(&slab.time_blocks, time_key(time_ms, &hash));
        }
        for (hash, (slab, record, on_disk)) in self.records {
            let key = time_key(record.time_ms, &hash);
            if record.is_empty() {
                // A merged block whose transactions were all undone.
                batch.remove(&slab.blocks, hash);
                batch.remove(&slab.time_blocks, key);
                delta -= on_disk as i64;
            } else {
                batch.insert(&slab.blocks, hash, encode(&record)?);
                batch.insert(&slab.time_blocks, key, []);
            }
        }
        Ok(delta)
    }
}

/// Fill a chain block's record from its header (an upsert: a replayed header changes
/// nothing).
fn chain_header(record: &mut BlockRecord, header: &RpcOptionalHeader) {
    record.kind = BlockKind::Chain;
    if let Some(hash) = header.hash {
        record.merging_block = hash.as_bytes();
    }
    if let Some(time_ms) = header.timestamp {
        record.time_ms = time_ms;
    }
    record.version = header.version.or(record.version);
    record.daa_score = header.daa_score.or(record.daa_score);
    record.blue_score = header.blue_score.or(record.blue_score);
    record.blue_work = header
        .blue_work
        .map(|w| w.to_be_bytes())
        .or(record.blue_work);
    record.bits = header.bits.or(record.bits);
    record.nonce = header.nonce.or(record.nonce);
    if let Some(parents) = &header.parents_by_level {
        record.parents = parents
            .get(0)
            .unwrap_or(&[])
            .iter()
            .map(|h| h.as_bytes())
            .collect();
        record.parent_levels = parents.expanded_len().min(u8::MAX as usize) as u8;
    }
    let bytes = |h: Option<RpcHash>| h.map(|h| h.as_bytes());
    record.hash_merkle_root = bytes(header.hash_merkle_root).or(record.hash_merkle_root);
    record.accepted_id_merkle_root =
        bytes(header.accepted_id_merkle_root).or(record.accepted_id_merkle_root);
    record.utxo_commitment = bytes(header.utxo_commitment).or(record.utxo_commitment);
    record.pruning_point = bytes(header.pruning_point).or(record.pruning_point);
}

impl IndexWriter {
    /// A writer on `store`, with the analytics engine the store holds.
    pub fn new(store: Arc<IndexStore>, labels: Arc<LabelBook>) -> Result<Self> {
        let manifest = store.manifest()?;
        let analytics = analytics::load(&store)?;
        let prefix = NetworkId::from_str(store.network())
            .map(Prefix::from)
            .unwrap_or(Prefix::Mainnet);
        Ok(Self {
            store,
            manifest,
            prefix,
            intern_cache: HashMap::new(),
            labels,
            analytics,
        })
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// The analytics engine as of the last applied batch.
    pub fn analytics(&self) -> &AnalyticsEngine {
        &self.analytics
    }

    pub fn store(&self) -> &Arc<IndexStore> {
        &self.store
    }

    /// Use the current label book for the clustering guard (it is replaced as a whole
    /// when labels change, so the writer is handed the latest before each batch).
    pub fn set_labels(&mut self, labels: Arc<LabelBook>) {
        self.labels = labels;
    }

    /// Apply one VSPC v2 response: undo its removed chain blocks, index its added ones,
    /// fold them into the analytics engine, advance the manifest. Committed in chunks of
    /// [`COMMIT_BLOCKS`] chain blocks, each atomic; an error leaves the chunks before it
    /// committed and the writer's state as the store has it.
    pub fn apply(&mut self, response: &GetVirtualChainFromBlockV2Response) -> Result<BatchReport> {
        let result = self.apply_inner(response);
        if result.is_err() {
            // Ids handed out and metrics counted for the failed chunk were never
            // committed: forget them.
            self.intern_cache.clear();
            self.manifest = self.store.manifest()?;
            self.analytics = analytics::load(&self.store)?;
        }
        result
    }

    fn apply_inner(
        &mut self,
        response: &GetVirtualChainFromBlockV2Response,
    ) -> Result<BatchReport> {
        let blocks = &response.chain_block_accepted_transactions[..];
        let mut report = BatchReport::default();
        let mut start = 0;
        loop {
            let end = (start + COMMIT_BLOCKS).min(blocks.len());
            // The removed blocks go first, with the first chunk.
            let removed: &[RpcHash] = if start == 0 {
                &response.removed_chain_block_hashes
            } else {
                &[]
            };
            self.commit(removed, &blocks[start..end], &mut report)?;
            start = end;
            if start >= blocks.len() {
                return Ok(report);
            }
        }
    }

    /// One atomic commit: `removed` undone, `blocks` indexed, both folded into the
    /// analytics engine, the manifest advanced. Adds to `report`.
    fn commit(
        &mut self,
        removed: &[RpcHash],
        blocks: &[RpcChainBlockAcceptedTransactions],
        report: &mut BatchReport,
    ) -> Result<()> {
        let mut batch = self.store.db().batch();
        let mut overlay = Overlay::default();
        let mut block_records = BlockOverlay::default();
        let store = self.store.clone();
        let mut clusters = Clusters::new(store.clusters());
        // Chain blocks already indexed, which analytics must not count twice either.
        let mut seen = HashSet::new();
        let txs_before = report.txs;
        let mut txs_undone = 0;

        for hash in removed {
            match self.undo_block(&mut batch, &mut overlay, &mut block_records, hash)? {
                Some(undone) => {
                    report.reorged_blocks += 1;
                    txs_undone += undone;
                }
                None => report.unresolved_reorgs.push(hash.to_string()),
            }
        }

        for chain_block in blocks {
            let header = &chain_block.chain_block_header;
            let (Some(hash), Some(time_ms)) = (header.hash, header.timestamp) else {
                continue;
            };
            let block = hash.as_bytes();
            let daa_score = header.daa_score.unwrap_or(0);
            let slab = self.store.slab_for(time_ms)?;
            report.newest = Some(Position {
                chain_block: block,
                daa_score,
                time_ms,
            });
            report.chain_blocks += 1;
            if slab.block_tx.prefix(block).next().is_some() {
                report.skipped_blocks += 1;
                seen.insert(hash.to_string());
                continue;
            }
            chain_header(
                block_records.get_or_load(&store, &block, time_ms, BlockKind::Chain, block)?,
                header,
            );
            for rpc_tx in &chain_block.accepted_transactions {
                let Some(converted) =
                    self.convert(&mut batch, rpc_tx, block, daa_score, time_ms)?
                else {
                    continue;
                };
                let Converted {
                    txid,
                    tx,
                    may_union,
                } = converted;
                self.put_tx(&mut batch, &mut overlay, &slab, &block, &txid, &tx)?;
                if let Some(payload) = rpc_tx.payload.as_deref()
                    && payload.len() > PAYLOAD_HEAD
                {
                    batch.insert(&slab.payloads, txid, payload);
                }
                self.touch_block(
                    &mut batch,
                    &mut block_records,
                    &txid,
                    &tx,
                    rpc_tx.payload.as_deref(),
                )?;
                if may_union {
                    link_owners(&mut clusters, &tx)?;
                }
                report.txs += 1;
            }
        }

        let seq = self.manifest.seq + 1;
        overlay.flush(&mut batch, seq)?;
        let blocks_delta = block_records.flush(&mut batch)?;
        report.blocks += blocks_delta;
        report.cluster_cap_hits += clusters.cap_hits;
        clusters.flush(&mut batch);
        let ingest = self
            .analytics
            .ingest_blocks(removed, blocks, &seen, now_ms());
        analytics::write(&mut batch, &store, &self.analytics, &ingest)?;
        report.analytics_reorgs.extend(ingest.unresolved_reorgs);
        let mut manifest = self.manifest;
        if let Some(newest) = report.newest {
            manifest.position = Some(newest);
        }
        manifest.txs_indexed =
            (manifest.txs_indexed + (report.txs - txs_before) as u64).saturating_sub(txs_undone);
        manifest.blocks_indexed = manifest.blocks_indexed.saturating_add_signed(blocks_delta);
        manifest.seq = seq;
        batch.insert(
            self.store.meta_keyspace(),
            "manifest",
            IndexStore::encode_manifest(&manifest)?,
        );
        batch.commit()?;
        self.manifest = manifest;
        Ok(())
    }

    /// Reverse every transaction a removed chain block accepted. Returns how many, or
    /// `None` when the block isn't in the newest slabs.
    fn undo_block(
        &mut self,
        batch: &mut WriteBatch,
        overlay: &mut Overlay,
        blocks: &mut BlockOverlay,
        hash: &RpcHash,
    ) -> Result<Option<u64>> {
        let block = hash.as_bytes();
        let slabs = self.store.slabs();
        for slab in slabs.iter().rev().take(REORG_SLABS) {
            let txids: Vec<Hash32> = slab
                .block_tx
                .prefix(block)
                .map(|guard| {
                    let key = guard.key()?;
                    Ok(key[32..].try_into().expect("block_tx key is 64 bytes"))
                })
                .collect::<fjall::Result<_>>()?;
            if txids.is_empty() {
                continue;
            }
            let mut time_ms = None;
            let mut undone = 0;
            for txid in txids {
                if let Some(bytes) = slab.tx.get(txid)? {
                    let tx: IndexedTx = decode(&bytes)?;
                    time_ms = Some(tx.time_ms);
                    self.remove_tx(batch, overlay, slab, &block, &txid, &tx)?;
                    self.untouch_block(blocks, &tx)?;
                    undone += 1;
                }
            }
            // The chain block's own record goes too; its slab is that of its timestamp,
            // which is the accepting time of what it accepted.
            if let Some(time_ms) = time_ms {
                blocks.remove(&self.store, &block, time_ms)?;
            }
            return Ok(Some(undone));
        }
        Ok(None)
    }

    /// Count an accepted transaction into the block that holds it; a coinbase also
    /// names the block's miner (the block is then a chain block, paid by the next one).
    fn touch_block(
        &mut self,
        batch: &mut WriteBatch,
        blocks: &mut BlockOverlay,
        txid: &Hash32,
        tx: &IndexedTx,
        payload: Option<&[u8]>,
    ) -> Result<()> {
        if tx.block == [0; 32] {
            return Ok(());
        }
        let kind = if tx.is_coinbase {
            BlockKind::Chain
        } else {
            BlockKind::Merged
        };
        let coinbase = payload
            .filter(|_| tx.is_coinbase)
            .and_then(parse_coinbase_payload);
        let miner = match coinbase
            .as_ref()
            .and_then(|c| script_address(c.script, self.prefix))
        {
            Some(address) => Some(self.intern(batch, &address.to_string())?.0),
            None => None,
        };
        let record = blocks.get_or_load(
            &self.store,
            &tx.block,
            tx.block_time_ms,
            kind,
            tx.accepting_block,
        )?;
        record.accepted_txs += 1;
        record.accepted_mass += tx.mass();
        record.accepted_fees += tx.fee.unwrap_or(0);
        if tx.is_coinbase {
            record.kind = BlockKind::Chain;
            record.merging_block = tx.block;
            record.coinbase_txid = Some(*txid);
            record.payouts = tx
                .outputs
                .iter()
                .map(|o| Payout {
                    addr: o.addr,
                    amount: o.amount,
                })
                .collect();
            if let Some(coinbase) = coinbase {
                record.miner = miner;
                record.miner_tag = coinbase.miner_tag();
                record.node_version = Some(coinbase.node_version());
                record.subsidy = Some(coinbase.subsidy);
                record.blue_score.get_or_insert(coinbase.blue_score);
            }
        }
        Ok(())
    }

    /// Reverse [`Self::touch_block`]'s counting (a reorg). The coinbase fields stay: they
    /// describe the block, and a replacement chain block brings the same coinbase.
    fn untouch_block(&mut self, blocks: &mut BlockOverlay, tx: &IndexedTx) -> Result<()> {
        if tx.block == [0; 32] {
            return Ok(());
        }
        let record = blocks.get_or_load(
            &self.store,
            &tx.block,
            tx.block_time_ms,
            BlockKind::Merged,
            tx.accepting_block,
        )?;
        record.accepted_txs = record.accepted_txs.saturating_sub(1);
        record.accepted_mass = record.accepted_mass.saturating_sub(tx.mass());
        record.accepted_fees = record.accepted_fees.saturating_sub(tx.fee.unwrap_or(0));
        Ok(())
    }

    fn put_tx(
        &mut self,
        batch: &mut WriteBatch,
        overlay: &mut Overlay,
        slab: &Slab,
        block: &Hash32,
        txid: &Hash32,
        tx: &IndexedTx,
    ) -> Result<()> {
        batch.insert(&slab.tx, *txid, encode(tx)?);
        batch.insert(&slab.block_tx, block_tx_key(block, txid), []);
        batch.insert(
            &slab.time_tx,
            time_key(tx.time_ms, txid),
            encode(&tx.summary())?,
        );
        if let Some(protocol) = tx.protocol {
            batch.insert(
                &slab.protocol_tx,
                protocol_tx_key(protocol, tx.time_ms, txid),
                [],
            );
        }
        for addr in tx.addresses() {
            let (received, sent) = (tx.received_by(addr), tx.sent_by(addr));
            batch.insert(
                &slab.addr_tx,
                addr_tx_key(addr, tx.time_ms, txid),
                encode_delta(received as i64 - sent as i64),
            );
            overlay
                .stats_mut(slab, addr)?
                .apply(tx.time_ms, received, sent, false);
        }
        for (sender, receiver, amount) in peer_flows(tx) {
            overlay
                .peer_mut(slab, sender, receiver)
                .apply(0, amount, false);
            overlay
                .peer_mut(slab, receiver, sender)
                .apply(amount, 0, false);
        }
        Ok(())
    }

    fn remove_tx(
        &mut self,
        batch: &mut WriteBatch,
        overlay: &mut Overlay,
        slab: &Slab,
        block: &Hash32,
        txid: &Hash32,
        tx: &IndexedTx,
    ) -> Result<()> {
        batch.remove(&slab.tx, *txid);
        if tx.payload_len as usize > PAYLOAD_HEAD {
            batch.remove(&slab.payloads, *txid);
        }
        batch.remove(&slab.block_tx, block_tx_key(block, txid));
        batch.remove(&slab.time_tx, time_key(tx.time_ms, txid));
        if let Some(protocol) = tx.protocol {
            batch.remove(
                &slab.protocol_tx,
                protocol_tx_key(protocol, tx.time_ms, txid),
            );
        }
        for addr in tx.addresses() {
            let (received, sent) = (tx.received_by(addr), tx.sent_by(addr));
            batch.remove(&slab.addr_tx, addr_tx_key(addr, tx.time_ms, txid));
            overlay
                .stats_mut(slab, addr)?
                .apply(tx.time_ms, received, sent, true);
        }
        for (sender, receiver, amount) in peer_flows(tx) {
            overlay
                .peer_mut(slab, sender, receiver)
                .apply(0, amount, true);
            overlay
                .peer_mut(slab, receiver, sender)
                .apply(amount, 0, true);
        }
        Ok(())
    }

    /// The id for `address`, assigning (and writing) a new one if it's unseen; the flag
    /// says whether it was.
    fn intern(&mut self, batch: &mut WriteBatch, address: &str) -> Result<(AddrId, bool)> {
        if let Some(&id) = self.intern_cache.get(address) {
            return Ok((id, false));
        }
        let (id, fresh) = match self.store.lookup(address)? {
            Some(id) => (id, false),
            None => {
                let id = self.manifest.next_addr_id;
                self.manifest.next_addr_id += 1;
                batch.insert(self.store.addr_by_str(), address, addr_key(id));
                batch.insert(self.store.str_by_id(), addr_key(id), address);
                (id, true)
            }
        };
        if self.intern_cache.len() >= INTERN_CACHE_MAX {
            self.intern_cache.clear();
        }
        self.intern_cache.insert(address.to_string(), id);
        Ok((id, fresh))
    }

    /// An RPC transaction as the index stores it; `None` without a transaction id.
    fn convert(
        &mut self,
        batch: &mut WriteBatch,
        tx: &RpcOptionalTransaction,
        accepting_block: Hash32,
        daa_score: u64,
        time_ms: u64,
    ) -> Result<Option<Converted>> {
        let Some(txid) = tx.verbose_data.as_ref().and_then(|v| v.transaction_id) else {
            return Ok(None);
        };

        let mut inputs = Vec::with_capacity(tx.inputs.len());
        let mut input_sum = Some(0u64);
        let mut input_scripts: Vec<&[u8]> = Vec::with_capacity(tx.inputs.len());
        // For clustering: the inputs' address version and entity labels, and whether a
        // script-hash input reveals a covenant (an introspecting redeem script).
        let mut input_version: Option<u8> = None;
        let mut input_labels: Vec<Option<String>> = Vec::new();
        let mut opcodes = OpcodeUsage::default();
        let mut covenant_spent = 0u16;
        let mut sig_ops = 0u32;
        for input in &tx.inputs {
            let utxo = input
                .verbose_data
                .as_ref()
                .and_then(|vd| vd.utxo_entry.as_ref());
            let amount = utxo.and_then(|u| u.amount);
            input_sum = input_sum.zip(amount).map(|(a, b)| a + b);
            let spent_class = utxo
                .and_then(|u| u.script_public_key.as_ref())
                .map(|spk| script_class(spk.script()));
            let addr = match utxo
                .and_then(|u| u.verbose_data.as_ref())
                .and_then(|v| v.script_public_key_address.as_ref())
            {
                Some(a) => {
                    let text = a.to_string();
                    input_version.get_or_insert(a.version as u8);
                    input_labels.push(self.labels.entity_name(&text).map(str::to_string));
                    if a.version == Version::ScriptHash
                        && let Some(script) = input.signature_script.as_deref()
                    {
                        opcodes |= redeem_script_opcodes(script);
                    }
                    Some(self.intern(batch, &text)?.0)
                }
                None => None,
            };
            if let Some(script) = input.signature_script.as_deref() {
                input_scripts.push(script);
            }
            if utxo.is_some_and(|u| u.covenant_id.is_some()) {
                covenant_spent = covenant_spent.saturating_add(1);
            }
            sig_ops += input.sig_op_count.unwrap_or(0) as u32;
            let prev = input.previous_outpoint.as_ref();
            inputs.push(TxInput {
                addr,
                amount,
                prev_txid: prev
                    .and_then(|p| p.transaction_id)
                    .map(|h| h.as_bytes())
                    .unwrap_or_default(),
                prev_index: prev.and_then(|p| p.index).unwrap_or_default(),
                script_class: spent_class.map(|c| c.code()).unwrap_or(SCRIPT_UNKNOWN),
            });
        }
        let covenant = opcodes.introspection;

        let mut outputs = Vec::with_capacity(tx.outputs.len());
        let mut traits = Vec::with_capacity(tx.outputs.len());
        let mut output_sum = 0u64;
        let mut covenant_created = 0u16;
        for output in &tx.outputs {
            let amount = output.value.unwrap_or(0);
            output_sum += amount;
            let mut t = OutputTrait {
                fresh: false,
                version: None,
                amount,
                to_input: false,
            };
            let addr = match output
                .verbose_data
                .as_ref()
                .and_then(|v| v.script_public_key_address.as_ref())
            {
                Some(a) => {
                    let (id, fresh) = self.intern(batch, &a.to_string())?;
                    t.fresh = fresh;
                    t.version = Some(a.version as u8);
                    t.to_input = inputs.iter().any(|i| i.addr == Some(id));
                    Some(id)
                }
                None => None,
            };
            traits.push(t);
            let class = output.script_public_key.as_ref().map(|spk| {
                let class = script_class(spk.script());
                if class == ScriptClass::NonStandard {
                    opcodes |= output_script_opcodes(spk.script());
                }
                class
            });
            let covenant = output.covenant.as_ref().is_some_and(|c| c.0.is_some());
            if covenant {
                covenant_created = covenant_created.saturating_add(1);
            }
            outputs.push(TxOutput {
                addr,
                amount,
                change: 0,
                script_class: class.map(|c| c.code()).unwrap_or(SCRIPT_UNKNOWN),
                covenant,
            });
        }

        let payload = tx.payload.as_deref().unwrap_or(&[]);
        let is_coinbase = tx.inputs.is_empty();
        let protocol = detect_protocol(payload, &input_scripts);
        let has_sender = inputs.iter().any(|i| i.addr.is_some());
        if !is_coinbase && has_sender {
            for (o, score) in outputs
                .iter_mut()
                .zip(cluster::change_scores(input_version, &traits))
            {
                o.change = score;
            }
        }
        let labels: Vec<Option<&str>> = input_labels.iter().map(Option::as_deref).collect();
        let may_union =
            !is_coinbase && has_sender && cluster::may_union(protocol, covenant, &labels);
        let verbose = tx.verbose_data.as_ref();
        let compute_mass = verbose.and_then(|v| v.compute_mass).unwrap_or(0);
        let block = verbose
            .and_then(|v| v.block_hash)
            .map(|h| h.as_bytes())
            .unwrap_or_default();
        let block_time_ms = verbose.and_then(|v| v.block_time).unwrap_or(time_ms);
        Ok(Some(Converted {
            txid: txid.as_bytes(),
            tx: IndexedTx {
                accepting_block,
                daa_score,
                time_ms,
                block,
                block_time_ms,
                inputs,
                outputs,
                fee: if is_coinbase {
                    None
                } else {
                    input_sum.map(|i| i.saturating_sub(output_sum))
                },
                storage_mass: tx.storage_mass.unwrap_or(0),
                compute_mass,
                is_coinbase,
                protocol,
                version: tx.version,
                lock_time: tx.lock_time,
                subnetwork: tx
                    .subnetwork_id
                    .as_ref()
                    .map(|id| Subnetwork::from_bytes(id.as_ref()))
                    .unwrap_or_default(),
                gas: tx.gas,
                payload_len: payload.len().min(u32::MAX as usize) as u32,
                payload_head: payload[..payload.len().min(PAYLOAD_HEAD)].to_vec(),
                opcodes: opcodes.to_bits(),
                covenant_created,
                covenant_spent,
                sig_ops,
            },
            may_union,
        }))
    }
}

/// Common-input ownership plus confident change outputs: everything an owner signed
/// for, and what came back to them, joins one cluster.
fn link_owners(clusters: &mut Clusters<'_>, tx: &IndexedTx) -> Result<()> {
    let mut senders = tx.inputs.iter().filter_map(|i| i.addr);
    let Some(first) = senders.next() else {
        return Ok(());
    };
    for other in senders {
        if other != first {
            clusters.union(first, other)?;
        }
    }
    for output in &tx.outputs {
        if let Some(addr) = output.addr
            && output.change >= CHANGE_THRESHOLD
        {
            clusters.union(first, addr)?;
        }
    }
    Ok(())
}

/// Who paid whom: each receiver's amount split over the senders in proportion to what
/// they put in. Change back to a sender isn't a flow.
pub fn peer_flows(tx: &IndexedTx) -> Vec<(AddrId, AddrId, u64)> {
    let mut senders: BTreeMap<AddrId, u64> = BTreeMap::new();
    for input in &tx.inputs {
        if let (Some(addr), Some(amount)) = (input.addr, input.amount) {
            *senders.entry(addr).or_default() += amount;
        }
    }
    let mut receivers: BTreeMap<AddrId, u64> = BTreeMap::new();
    for output in &tx.outputs {
        if let Some(addr) = output.addr
            && !senders.contains_key(&addr)
        {
            *receivers.entry(addr).or_default() += output.amount;
        }
    }
    let total_in: u64 = senders.values().sum();
    if total_in == 0 || senders.len() * receivers.len() > MAX_PEER_PAIRS {
        return Vec::new();
    }
    let mut flows = Vec::with_capacity(senders.len() * receivers.len());
    for (&receiver, &received) in &receivers {
        for (&sender, &sent) in &senders {
            let share = (received as u128 * sent as u128 / total_in as u128) as u64;
            if share > 0 {
                flows.push((sender, receiver, share));
            }
        }
    }
    flows
}

#[cfg(test)]
pub(crate) mod testing {
    //! Synthetic VSPC v2 responses for tests and benchmarks.

    use std::sync::Arc;

    use kaspa_addresses::{Address, Prefix, Version};
    use kaspa_rpc_core::{
        GetVirtualChainFromBlockV2Response, RpcChainBlockAcceptedTransactions, RpcHash,
        RpcOptionalHeader, RpcOptionalTransaction, RpcOptionalTransactionInput,
        RpcOptionalTransactionInputVerboseData, RpcOptionalTransactionOutpoint,
        RpcOptionalTransactionOutput, RpcOptionalTransactionOutputVerboseData,
        RpcOptionalTransactionVerboseData, RpcOptionalUtxoEntry, RpcOptionalUtxoEntryVerboseData,
    };

    pub fn address(n: u32) -> Address {
        let mut payload = [0u8; 32];
        payload[..4].copy_from_slice(&n.to_be_bytes());
        Address::new(Prefix::Mainnet, Version::PubKey, &payload)
    }

    /// A pay-to-script-hash address, distinct from `address(n)`.
    pub fn p2sh_address(n: u32) -> Address {
        let mut payload = [0u8; 32];
        payload[..4].copy_from_slice(&n.to_be_bytes());
        Address::new(Prefix::Mainnet, Version::ScriptHash, &payload)
    }

    /// Make input `i` of `tx` spend from a script address with this signature script
    /// (its last push is the revealed redeem script).
    pub fn spend_from_script(tx: &mut RpcOptionalTransaction, i: usize, n: u32, sig: Vec<u8>) {
        let input = &mut tx.inputs[i];
        input.signature_script = Some(sig);
        if let Some(utxo) = input
            .verbose_data
            .as_mut()
            .and_then(|vd| vd.utxo_entry.as_mut())
            .and_then(|u| u.verbose_data.as_mut())
        {
            utxo.script_public_key_address = Some(p2sh_address(n));
        }
    }

    pub fn hash(n: u64) -> RpcHash {
        let mut bytes = [0u8; 32];
        bytes[..8].copy_from_slice(&n.to_be_bytes());
        RpcHash::from_bytes(bytes)
    }

    /// A transaction paying `outputs` (address, amount) from `inputs` (address, amount).
    /// No inputs makes a coinbase.
    pub fn tx(id: u64, inputs: &[(u32, u64)], outputs: &[(u32, u64)]) -> RpcOptionalTransaction {
        RpcOptionalTransaction {
            version: None,
            inputs: inputs
                .iter()
                .enumerate()
                .map(|(i, (addr, amount))| RpcOptionalTransactionInput {
                    previous_outpoint: Some(RpcOptionalTransactionOutpoint {
                        transaction_id: Some(hash(id * 1000 + i as u64)),
                        index: Some(i as u32),
                    }),
                    signature_script: Some(vec![]),
                    sequence: None,
                    sig_op_count: None,
                    compute_budget: None,
                    verbose_data: Some(RpcOptionalTransactionInputVerboseData {
                        utxo_entry: Some(RpcOptionalUtxoEntry {
                            amount: Some(*amount),
                            script_public_key: None,
                            block_daa_score: None,
                            is_coinbase: None,
                            verbose_data: Some(RpcOptionalUtxoEntryVerboseData {
                                script_public_key_type: None,
                                script_public_key_address: Some(address(*addr)),
                            }),
                            covenant_id: None,
                        }),
                    }),
                })
                .collect(),
            outputs: outputs
                .iter()
                .map(|(addr, amount)| RpcOptionalTransactionOutput {
                    value: Some(*amount),
                    script_public_key: None,
                    verbose_data: Some(RpcOptionalTransactionOutputVerboseData {
                        script_public_key_type: None,
                        script_public_key_address: Some(address(*addr)),
                    }),
                    covenant: None,
                })
                .collect(),
            lock_time: None,
            subnetwork_id: None,
            gas: None,
            payload: Some(vec![]),
            storage_mass: Some(1),
            verbose_data: Some(RpcOptionalTransactionVerboseData {
                transaction_id: Some(hash(id)),
                hash: None,
                compute_mass: Some(1),
                block_hash: None,
                block_time: None,
            }),
        }
    }

    /// Point input `i` of `tx` at output `index` of the synthetic transaction `prev`
    /// (by default inputs spend unrelated outpoints).
    pub fn with_outpoint(tx: &mut RpcOptionalTransaction, i: usize, prev: u64, index: u32) {
        tx.inputs[i].previous_outpoint = Some(RpcOptionalTransactionOutpoint {
            transaction_id: Some(hash(prev)),
            index: Some(index),
        });
    }

    /// Say which block holds `tx` (the node's verbose `block_hash`/`block_time`).
    pub fn in_block(tx: &mut RpcOptionalTransaction, block: RpcHash, time_ms: u64) {
        if let Some(v) = tx.verbose_data.as_mut() {
            v.block_hash = Some(block);
            v.block_time = Some(time_ms);
        }
    }

    /// The hash of chain block `n` (`chain_block`).
    pub fn chain_hash(n: u64) -> RpcHash {
        hash(1_000_000 + n)
    }

    /// The hash of the synthetic merged block `chain_block(n, …)` stamps on
    /// transactions that don't say which block holds them.
    pub fn merged_hash(n: u64) -> RpcHash {
        hash(2_000_000 + n)
    }

    /// A real coinbase payload: blue score, subsidy, the miner's P2PK script, then
    /// `"<node_version>/<tag>"`.
    pub fn coinbase_payload(
        miner: u32,
        node_version: &str,
        tag: &str,
        blue_score: u64,
        subsidy: u64,
    ) -> Vec<u8> {
        let mut p = Vec::new();
        p.extend_from_slice(&blue_score.to_le_bytes());
        p.extend_from_slice(&subsidy.to_le_bytes());
        p.extend_from_slice(&0u16.to_le_bytes());
        let mut script = vec![0x20];
        script.extend_from_slice(address(miner).payload.as_ref());
        script.push(0xac);
        p.push(script.len() as u8);
        p.extend_from_slice(&script);
        p.extend_from_slice(format!("{node_version}/{tag}").as_bytes());
        p
    }

    /// A coinbase paying `payouts` (address, amount), mined by `miner` (named in the
    /// payload, as a real coinbase does).
    #[allow(clippy::too_many_arguments)]
    pub fn coinbase(
        id: u64,
        miner: u32,
        node_version: &str,
        tag: &str,
        blue_score: u64,
        subsidy: u64,
        payouts: &[(u32, u64)],
    ) -> RpcOptionalTransaction {
        let mut cb = tx(id, &[], payouts);
        cb.payload = Some(coinbase_payload(
            miner,
            node_version,
            tag,
            blue_score,
            subsidy,
        ));
        cb
    }

    /// Chain block `n` at `time_ms` accepting `txs`. A transaction that doesn't say
    /// which block holds it is stamped with the synthetic merged block
    /// [`merged_hash`]`(n)` at `time_ms`.
    pub fn chain_block(
        n: u64,
        time_ms: u64,
        mut txs: Vec<RpcOptionalTransaction>,
    ) -> RpcChainBlockAcceptedTransactions {
        for tx in &mut txs {
            if tx
                .verbose_data
                .as_ref()
                .is_some_and(|v| v.block_hash.is_none())
            {
                in_block(tx, merged_hash(n), time_ms);
            }
        }
        RpcChainBlockAcceptedTransactions {
            chain_block_header: RpcOptionalHeader {
                hash: Some(chain_hash(n)),
                version: None,
                parents_by_level: None,
                hash_merkle_root: None,
                accepted_id_merkle_root: None,
                utxo_commitment: None,
                timestamp: Some(time_ms),
                bits: None,
                nonce: None,
                daa_score: Some(n),
                blue_work: None,
                blue_score: None,
                pruning_point: None,
            },
            accepted_transactions: txs,
        }
    }

    /// [`chain_block`] with every header field set: version 1, bits `0x207fffff`,
    /// nonce `n`, blue score `n + 10`, blue work `n`, the previous chain block as the
    /// only parent, and merkle roots / commitments derived from `n`.
    pub fn chain_block_full(
        n: u64,
        time_ms: u64,
        txs: Vec<RpcOptionalTransaction>,
    ) -> RpcChainBlockAcceptedTransactions {
        let mut block = chain_block(n, time_ms, txs);
        let mut parents = kaspa_rpc_core::RpcCompressedParents::default();
        parents.push(vec![chain_hash(n - 1)]);
        parents.push(vec![hash(3_000_000 + n)]);
        block.chain_block_header = RpcOptionalHeader {
            version: Some(1),
            parents_by_level: Some(parents),
            hash_merkle_root: Some(hash(4_000_000 + n)),
            accepted_id_merkle_root: Some(hash(5_000_000 + n)),
            utxo_commitment: Some(hash(6_000_000 + n)),
            bits: Some(0x207f_ffff),
            nonce: Some(n),
            blue_work: Some(n.into()),
            blue_score: Some(n + 10),
            pruning_point: Some(hash(7_000_000)),
            ..block.chain_block_header
        };
        block
    }

    pub fn response(
        removed: Vec<RpcHash>,
        blocks: Vec<RpcChainBlockAcceptedTransactions>,
    ) -> GetVirtualChainFromBlockV2Response {
        let added = blocks
            .iter()
            .filter_map(|b| b.chain_block_header.hash)
            .collect();
        GetVirtualChainFromBlockV2Response {
            removed_chain_block_hashes: Arc::new(removed),
            added_chain_block_hashes: Arc::new(added),
            chain_block_accepted_transactions: Arc::new(blocks),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Instant;

    use super::testing::*;
    use super::*;
    use crate::index::query;
    use crate::index::records::SLAB_MS;
    use crate::index::{TempStore, temp_store};

    /// A writer on a temporary store; the writer drops before the directory.
    struct TempWriter {
        writer: IndexWriter,
        _store: TempStore,
    }

    fn writer() -> TempWriter {
        writer_with(LabelBook::base())
    }

    fn writer_with(labels: LabelBook) -> TempWriter {
        let store = temp_store();
        let writer = IndexWriter::new(store.store.clone(), Arc::new(labels)).unwrap();
        TempWriter {
            writer,
            _store: store,
        }
    }

    #[test]
    fn indexes_transactions_by_address() {
        let mut tw = writer();
        let w = &mut tw.writer;
        // Address 1 pays 2 and keeps change; a coinbase pays 3.
        let r = response(
            vec![],
            vec![chain_block(
                1,
                1_000,
                vec![
                    tx(1, &[(1, 100)], &[(2, 60), (1, 39)]),
                    tx(2, &[], &[(3, 500)]),
                ],
            )],
        );
        let report = w.apply(&r).unwrap();
        assert_eq!(report.txs, 2);
        assert_eq!(report.chain_blocks, 1);
        assert_eq!(w.manifest().txs_indexed, 2);
        assert_eq!(w.manifest().position.unwrap().daa_score, 1);

        let store = w.store();
        let a1 = store.lookup(&address(1).to_string()).unwrap().unwrap();
        let stats = query::stats(store, a1).unwrap();
        assert_eq!((stats.tx_count, stats.received, stats.sent), (1, 39, 100));
        let a2 = store.lookup(&address(2).to_string()).unwrap().unwrap();
        assert_eq!(query::stats(store, a2).unwrap().received, 60);
        let a3 = store.lookup(&address(3).to_string()).unwrap().unwrap();
        assert_eq!(query::stats(store, a3).unwrap().received, 500);

        let detail = query::transaction(store, &hash(1).as_bytes())
            .unwrap()
            .unwrap();
        assert_eq!(detail.fee, Some(1));
        assert_eq!(
            detail.inputs[0].address.as_deref(),
            Some(address(1).to_string().as_str())
        );

        // Address 1's counterparty is 2 (its change isn't a flow), for the 60 it sent.
        let peers = query::counterparties(store, a1, 10).unwrap();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].address, address(2).to_string());
        assert_eq!(peers[0].stats.out_amount, 60);
        let peers = query::counterparties(store, a2, 10).unwrap();
        assert_eq!(peers[0].stats.in_amount, 60);
    }

    #[test]
    fn replaying_a_response_changes_nothing() {
        let mut tw = writer();
        let w = &mut tw.writer;
        let r = response(
            vec![],
            vec![chain_block(1, 1_000, vec![tx(1, &[(1, 100)], &[(2, 90)])])],
        );
        w.apply(&r).unwrap();
        let report = w.apply(&r).unwrap();
        assert_eq!(report.skipped_blocks, 1);
        assert_eq!(report.txs, 0);
        let a1 = w.store().lookup(&address(1).to_string()).unwrap().unwrap();
        assert_eq!(query::stats(w.store(), a1).unwrap().tx_count, 1);
        assert_eq!(w.manifest().txs_indexed, 1);
    }

    #[test]
    fn analytics_are_written_with_the_batch_and_reloaded() {
        let tw = writer();
        let store = tw._store.store.clone();
        let mut w = tw.writer;
        // Recent enough to stay inside the windows, old enough to be finalized.
        let r = response(
            vec![],
            vec![chain_block(
                1,
                now_ms() - 120_000,
                vec![tx(1, &[(1, 100)], &[(2, 90)]), tx(2, &[], &[(3, 500)])],
            )],
        );
        w.apply(&r).unwrap();
        // A replay counts nothing twice.
        let report = w.apply(&r).unwrap();
        assert_eq!(report.skipped_blocks, 1);
        let ten = &w.analytics().ten_minute_buckets;
        assert_eq!(ten.len(), 1);
        assert_eq!(ten[0].metrics.chain_blocks, 1);
        assert_eq!(ten[0].metrics.tx_count, 1);
        assert_eq!(ten[0].metrics.mined_blocks, 1);

        // A fresh writer on the same store starts from the stored engine.
        drop(w);
        let again = IndexWriter::new(store, Arc::new(LabelBook::base())).unwrap();
        let ten = &again.analytics().ten_minute_buckets;
        assert_eq!(ten.len(), 1);
        assert_eq!(ten[0].metrics.tx_count, 1);
        assert_eq!(again.analytics().minute_buckets.len(), 1);
    }

    #[test]
    fn reorg_undoes_a_chain_block_exactly() {
        let mut tw = writer();
        let w = &mut tw.writer;
        let before = response(
            vec![],
            vec![chain_block(1, 1_000, vec![tx(1, &[(1, 100)], &[(2, 90)])])],
        );
        w.apply(&before).unwrap();
        let a1 = w.store().lookup(&address(1).to_string()).unwrap().unwrap();
        let a2 = w.store().lookup(&address(2).to_string()).unwrap().unwrap();

        let reorged = response(
            vec![],
            vec![chain_block(2, 2_000, vec![tx(2, &[(1, 50)], &[(2, 40)])])],
        );
        w.apply(&reorged).unwrap();
        assert_eq!(query::stats(w.store(), a1).unwrap().tx_count, 2);

        let undo = response(
            vec![hash(1_000_000 + 2)],
            vec![chain_block(3, 2_500, vec![tx(3, &[(1, 50)], &[(2, 45)])])],
        );
        let report = w.apply(&undo).unwrap();
        assert_eq!(report.reorged_blocks, 1);
        let s1 = query::stats(w.store(), a1).unwrap();
        assert_eq!((s1.tx_count, s1.sent), (2, 150));
        let s2 = query::stats(w.store(), a2).unwrap();
        assert_eq!((s2.tx_count, s2.received), (2, 135));
        assert!(
            query::transaction(w.store(), &hash(2).as_bytes())
                .unwrap()
                .is_none()
        );
        let page = query::transactions(w.store(), a1, None, 10).unwrap();
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.items[0].txid, hash(3).to_string());

        let unknown = response(vec![hash(42)], vec![]);
        let report = w.apply(&unknown).unwrap();
        assert_eq!(report.unresolved_reorgs, vec![hash(42).to_string()]);
    }

    #[test]
    fn reorg_undoes_counterparties_and_the_tx_count() {
        let mut tw = writer();
        let w = &mut tw.writer;
        w.apply(&response(
            vec![],
            vec![
                chain_block(1, 1_000, vec![tx(1, &[(1, 100)], &[(2, 90)])]),
                chain_block(2, 2_000, vec![tx(2, &[(1, 50)], &[(2, 40), (3, 5)])]),
            ],
        ))
        .unwrap();
        let store = w.store();
        let id = |n: u32| store.lookup(&address(n).to_string()).unwrap().unwrap();
        let (a1, a3) = (id(1), id(3));
        let peers = query::counterparties(store, a1, 10).unwrap();
        assert_eq!(peers.len(), 2);
        assert_eq!(peers[0].stats.out_amount, 130);
        assert_eq!(peers[0].stats.tx_count, 2);
        assert_eq!(w.manifest().txs_indexed, 2);

        // Block 2 goes: 3 is no counterparty any more, 2 is one of a single transaction.
        w.apply(&response(vec![chain_hash(2)], vec![])).unwrap();
        let store = w.store();
        let peers = query::counterparties(store, a1, 10).unwrap();
        assert_eq!(peers.len(), 1);
        assert_eq!(
            (peers[0].stats.out_amount, peers[0].stats.tx_count),
            (90, 1)
        );
        assert!(query::counterparties(store, a3, 10).unwrap().is_empty());
        assert_eq!(w.manifest().txs_indexed, 1);
        // Every commit stamped its deltas with its own sequence number.
        assert_eq!(w.manifest().seq, 2);
    }

    #[test]
    fn long_payloads_are_kept_whole_and_undone() {
        let mut tw = writer();
        let w = &mut tw.writer;
        // The largest payload consensus allows in a block, give or take the 220 bytes of
        // the rest of the transaction.
        let long: Vec<u8> = (0..249_780u32).map(|i| (i % 251) as u8).collect();
        let mut big = tx(1, &[(1, 100)], &[(2, 90)]);
        big.payload = Some(long.clone());
        let mut short = tx(2, &[(1, 50)], &[(2, 40)]);
        short.payload = Some(b"short".to_vec());
        w.apply(&response(
            vec![],
            vec![chain_block(1, 1_000, vec![big, short])],
        ))
        .unwrap();
        let store = w.store();
        let d = query::transaction(store, &hash(1).as_bytes())
            .unwrap()
            .unwrap();
        assert_eq!(
            (d.payload_len as usize, d.payload.len()),
            (long.len(), long.len())
        );
        assert_eq!(d.payload, long);
        let d = query::transaction(store, &hash(2).as_bytes())
            .unwrap()
            .unwrap();
        assert_eq!(d.payload, b"short");
        // Only the long one is stored beside its record.
        let slab = &store.slabs()[0];
        assert_eq!(slab.payloads.len().unwrap(), 1);

        w.apply(&response(vec![chain_hash(1)], vec![])).unwrap();
        assert!(w.store().slabs()[0].payloads.is_empty().unwrap());
    }

    #[test]
    fn a_large_response_is_committed_in_chunks() {
        let mut tw = writer();
        let w = &mut tw.writer;
        let n = COMMIT_BLOCKS as u64 * 2 + 5;
        let blocks: Vec<_> = (1..=n)
            .map(|i| chain_block(i, i * 100, vec![tx(i, &[(1, 100)], &[(2, 90)])]))
            .collect();
        let r = response(vec![], blocks);
        let report = w.apply(&r).unwrap();
        assert_eq!((report.chain_blocks, report.txs), (n as usize, n as usize));
        // One merged block per chain block plus the chain block itself.
        assert_eq!(report.blocks, 2 * n as i64);
        assert_eq!(w.manifest().seq, 3);
        assert_eq!(w.manifest().txs_indexed, n);
        assert_eq!(w.manifest().position.unwrap().daa_score, n);
        let a1 = w.store().lookup(&address(1).to_string()).unwrap().unwrap();
        assert_eq!(query::stats(w.store(), a1).unwrap().tx_count, n);
        let peers = query::counterparties(w.store(), a1, 10).unwrap();
        assert_eq!(peers[0].stats.tx_count, n);

        // A replay (after a crash between chunks, say) changes nothing.
        let report = w.apply(&r).unwrap();
        assert_eq!(report.skipped_blocks, n as usize);
        assert_eq!(w.manifest().txs_indexed, n);
        assert_eq!(query::stats(w.store(), a1).unwrap().tx_count, n);
    }

    #[test]
    fn protocol_transactions_are_listed_newest_first_and_undone() {
        use crate::tx_inspect::TransactionProtocol;
        let mut tw = writer();
        let w = &mut tw.writer;
        let mut kasia = tx(1, &[(1, 100)], &[(2, 90)]);
        kasia.payload = Some(b"ciph_msg hello".to_vec());
        let mut kasplex = tx(2, &[(1, 100)], &[(2, 90)]);
        kasplex.payload = Some(b"kasplex op".to_vec());
        let mut later = tx(3, &[(1, 100)], &[(2, 90)]);
        later.payload = Some(b"ciph_msg again".to_vec());
        w.apply(&response(
            vec![],
            vec![
                chain_block(1, 1_000, vec![kasia, kasplex, tx(4, &[(1, 5)], &[(2, 4)])]),
                chain_block(2, 2_000, vec![later]),
            ],
        ))
        .unwrap();

        let page =
            query::protocol_transactions(w.store(), TransactionProtocol::Kasia, None, 10).unwrap();
        let ids: Vec<&str> = page.items.iter().map(|r| r.txid.as_str()).collect();
        assert_eq!(ids, vec![hash(3).to_string(), hash(1).to_string()]);
        assert_eq!(page.items[0].output_total, 90);
        assert!(page.next.is_none());
        let page = query::protocol_transactions(w.store(), TransactionProtocol::Kasplex, None, 10)
            .unwrap();
        assert_eq!(page.items.len(), 1);
        assert!(
            query::protocol_transactions(w.store(), TransactionProtocol::Kns, None, 10)
                .unwrap()
                .items
                .is_empty()
        );

        // One row per page: the cursor continues at the older one.
        let first =
            query::protocol_transactions(w.store(), TransactionProtocol::Kasia, None, 1).unwrap();
        assert_eq!(first.items[0].txid, hash(3).to_string());
        let second =
            query::protocol_transactions(w.store(), TransactionProtocol::Kasia, first.next, 1)
                .unwrap();
        assert_eq!(second.items[0].txid, hash(1).to_string());
        assert!(second.next.is_none() || second.items.len() == 1);

        // A reorg of block 2 takes its Kasia transaction out of the list.
        w.apply(&response(
            vec![hash(1_000_000 + 2)],
            vec![chain_block(3, 2_500, vec![tx(5, &[(1, 50)], &[(2, 45)])])],
        ))
        .unwrap();
        let page =
            query::protocol_transactions(w.store(), TransactionProtocol::Kasia, None, 10).unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].txid, hash(1).to_string());
    }

    #[test]
    fn chain_block_record_from_header() {
        let mut tw = writer();
        let w = &mut tw.writer;
        let r = response(
            vec![],
            vec![chain_block_full(
                1,
                1_000,
                vec![tx(1, &[(1, 100)], &[(2, 90)])],
            )],
        );
        let report = w.apply(&r).unwrap();
        // The chain block and the merged block its transaction names.
        assert_eq!(report.blocks, 2);
        assert_eq!(w.manifest().blocks_indexed, 2);
        let store = w.store();
        let chain = query::block(store, &chain_hash(1).as_bytes())
            .unwrap()
            .unwrap();
        assert_eq!(chain.kind, "chain");
        assert_eq!(chain.time_ms, 1_000);
        assert_eq!(chain.merging_block, chain_hash(1).to_string());
        assert_eq!(
            (chain.version, chain.bits, chain.nonce),
            (Some(1), Some(0x207f_ffff), Some(1))
        );
        assert_eq!((chain.daa_score, chain.blue_score), (Some(1), Some(11)));
        assert_eq!(chain.blue_work.as_deref(), Some("1"));
        assert!((chain.difficulty.unwrap() - 1.0).abs() < 1e-6);
        assert_eq!(chain.parents, vec![chain_hash(0).to_string()]);
        assert_eq!(chain.parent_levels, 2);
        assert_eq!(
            chain.hash_merkle_root.as_deref(),
            Some(hash(4_000_001).to_string().as_str())
        );
        assert_eq!(
            chain.utxo_commitment.as_deref(),
            Some(hash(6_000_001).to_string().as_str())
        );
        assert_eq!(
            chain.pruning_point.as_deref(),
            Some(hash(7_000_000).to_string().as_str())
        );
        assert_eq!((chain.accepted_txs, chain.miner), (0, None));

        let merged = query::block(store, &merged_hash(1).as_bytes())
            .unwrap()
            .unwrap();
        assert_eq!(merged.kind, "merged");
        assert_eq!(merged.merging_block, chain_hash(1).to_string());
        assert_eq!(
            (
                merged.accepted_txs,
                merged.accepted_fees,
                merged.accepted_mass
            ),
            (1, 10, 1)
        );
        assert_eq!(merged.version, None);
        assert!(query::block(store, &hash(42).as_bytes()).unwrap().is_none());
    }

    #[test]
    fn coinbase_enriches_the_previous_chain_block() {
        let mut tw = writer();
        let w = &mut tw.writer;
        // Block 1's coinbase comes under block 2, in the same batch.
        let mut cb1 = coinbase(5, 9, "1.2.3", "pool-x", 11, 500, &[(7, 400), (8, 100)]);
        in_block(&mut cb1, chain_hash(1), 1_000);
        w.apply(&response(
            vec![],
            vec![
                chain_block(1, 1_000, vec![tx(1, &[(1, 100)], &[(2, 90)])]),
                chain_block(2, 2_000, vec![cb1]),
            ],
        ))
        .unwrap();
        let store = w.store();
        let b1 = query::block(store, &chain_hash(1).as_bytes())
            .unwrap()
            .unwrap();
        assert_eq!(b1.kind, "chain");
        assert_eq!(b1.miner.as_deref(), Some(address(9).to_string().as_str()));
        assert_eq!(b1.miner_tag.as_deref(), Some("pool-x"));
        assert_eq!(b1.node_version.as_deref(), Some("1.2.3"));
        assert_eq!((b1.subsidy, b1.blue_score), (Some(500), Some(11)));
        assert_eq!(
            b1.coinbase_txid.as_deref(),
            Some(hash(5).to_string().as_str())
        );
        assert_eq!(
            b1.payouts,
            vec![
                (Some(address(7).to_string()), 400),
                (Some(address(8).to_string()), 100)
            ]
        );
        // The coinbase is in block 1, accepted by block 2.
        assert_eq!(b1.accepted_txs, 1);
        let b2 = query::block(store, &chain_hash(2).as_bytes())
            .unwrap()
            .unwrap();
        assert_eq!((b2.kind, b2.accepted_txs, b2.miner), ("chain", 0, None));

        // Block 2's coinbase comes under block 3, in the next batch.
        let mut cb2 = coinbase(6, 10, "1.2.4", "pool-y", 12, 500, &[(9, 500)]);
        in_block(&mut cb2, chain_hash(2), 2_000);
        w.apply(&response(vec![], vec![chain_block(3, 3_000, vec![cb2])]))
            .unwrap();
        let b2 = query::block(w.store(), &chain_hash(2).as_bytes())
            .unwrap()
            .unwrap();
        assert_eq!(b2.miner.as_deref(), Some(address(10).to_string().as_str()));
        assert_eq!(b2.accepted_txs, 1);
        // A header the coinbase found first is kept when a header arrives (none here),
        // and nothing was counted twice: 3 chain blocks, 1 merged.
        assert_eq!(w.manifest().blocks_indexed, 4);
    }

    #[test]
    fn merged_blocks_accumulate_accepted_totals() {
        let mut tw = writer();
        let w = &mut tw.writer;
        let mut a = tx(1, &[(1, 100)], &[(2, 99)]);
        let mut b = tx(2, &[(1, 100)], &[(2, 98)]);
        let c = tx(3, &[(1, 100)], &[(2, 97)]);
        in_block(&mut a, hash(77), 900);
        in_block(&mut b, hash(77), 900);
        w.apply(&response(
            vec![],
            vec![chain_block(1, 1_000, vec![a, b, c])],
        ))
        .unwrap();
        let store = w.store();
        let m = query::block(store, &hash(77).as_bytes()).unwrap().unwrap();
        assert_eq!(
            (m.accepted_txs, m.accepted_fees, m.accepted_mass),
            (2, 3, 2)
        );
        assert_eq!(m.time_ms, 900);
        let other = query::block(store, &merged_hash(1).as_bytes())
            .unwrap()
            .unwrap();
        assert_eq!((other.accepted_txs, other.accepted_fees), (1, 3));
        let detail = query::transaction(store, &hash(1).as_bytes())
            .unwrap()
            .unwrap();
        assert_eq!(detail.block.as_deref(), Some(hash(77).to_string().as_str()));
        assert_eq!(detail.block_time_ms, 900);
    }

    #[test]
    fn reorg_removes_block_records() {
        let mut tw = writer();
        let w = &mut tw.writer;
        let mut cb1 = coinbase(5, 9, "1.0.0", "p", 11, 500, &[(7, 500)]);
        in_block(&mut cb1, chain_hash(1), 1_000);
        w.apply(&response(
            vec![],
            vec![
                chain_block(1, 1_000, vec![tx(1, &[(1, 100)], &[(2, 90)])]),
                chain_block(2, 2_000, vec![cb1, tx(2, &[(1, 50)], &[(2, 40)])]),
            ],
        ))
        .unwrap();
        assert_eq!(w.manifest().blocks_indexed, 4);
        let report = w
            .apply(&response(
                vec![chain_hash(2)],
                vec![chain_block(3, 2_500, vec![tx(3, &[(1, 50)], &[(2, 45)])])],
            ))
            .unwrap();
        assert_eq!(report.reorged_blocks, 1);
        let store = w.store();
        assert!(
            query::block(store, &chain_hash(2).as_bytes())
                .unwrap()
                .is_none()
        );
        assert!(
            query::block(store, &merged_hash(2).as_bytes())
                .unwrap()
                .is_none()
        );
        let b1 = query::block(store, &chain_hash(1).as_bytes())
            .unwrap()
            .unwrap();
        // Its coinbase isn't accepted any more, but what it says about block 1 stays.
        assert_eq!(b1.accepted_txs, 0);
        assert_eq!(b1.miner.as_deref(), Some(address(9).to_string().as_str()));
        assert!(
            query::block(store, &chain_hash(3).as_bytes())
                .unwrap()
                .is_some()
        );
        // 1, merged 1, 3, merged 3.
        assert_eq!(w.manifest().blocks_indexed, 4);
    }

    #[test]
    fn replay_leaves_block_records_unchanged() {
        let mut tw = writer();
        let w = &mut tw.writer;
        let mut cb1 = coinbase(5, 9, "1.0.0", "p", 11, 500, &[(7, 500)]);
        in_block(&mut cb1, chain_hash(1), 1_000);
        let r = response(
            vec![],
            vec![
                chain_block_full(1, 1_000, vec![tx(1, &[(1, 100)], &[(2, 90)])]),
                chain_block_full(2, 2_000, vec![cb1]),
            ],
        );
        w.apply(&r).unwrap();
        let before = query::block(w.store(), &chain_hash(1).as_bytes()).unwrap();
        let report = w.apply(&r).unwrap();
        assert_eq!((report.skipped_blocks, report.blocks), (2, 0));
        assert_eq!(
            query::block(w.store(), &chain_hash(1).as_bytes()).unwrap(),
            before
        );
        assert_eq!(w.manifest().blocks_indexed, 3);
    }

    #[test]
    fn widened_fields_are_stored() {
        use kaspa_rpc_core::{RpcCovenantBinding, RpcNullableCovenantBinding, RpcScriptPublicKey};
        let mut tw = writer();
        let w = &mut tw.writer;
        let mut t = tx(1, &[(1, 100), (2, 50)], &[(3, 140)]);
        t.version = Some(2);
        t.lock_time = Some(77);
        t.subnetwork_id = Some(kaspa_rpc_core::RpcSubnetworkId::from_byte(0));
        t.gas = Some(5);
        t.payload = Some(vec![0x61; 300]);
        t.inputs[0].sig_op_count = Some(1);
        t.inputs[1].sig_op_count = Some(2);
        let mut p2sh = vec![0xaa, 0x20];
        p2sh.extend([0x33; 32]);
        p2sh.push(0x87);
        let mut p2pk = vec![0x20];
        p2pk.extend([0x11; 32]);
        p2pk.push(0xac);
        fn utxo(
            t: &mut RpcOptionalTransaction,
            i: usize,
        ) -> &mut kaspa_rpc_core::RpcOptionalUtxoEntry {
            t.inputs[i]
                .verbose_data
                .as_mut()
                .unwrap()
                .utxo_entry
                .as_mut()
                .unwrap()
        }
        utxo(&mut t, 0).script_public_key = Some(RpcScriptPublicKey::from_vec(0, p2pk.clone()));
        utxo(&mut t, 1).script_public_key = Some(RpcScriptPublicKey::from_vec(0, p2sh.clone()));
        utxo(&mut t, 1).covenant_id = Some(hash(500));
        t.outputs[0].script_public_key = Some(RpcScriptPublicKey::from_vec(0, p2sh));
        t.outputs[0].covenant = Some(RpcNullableCovenantBinding(Some(RpcCovenantBinding::new(
            0,
            hash(501),
        ))));
        w.apply(&response(vec![], vec![chain_block(1, 1_000, vec![t])]))
            .unwrap();
        let d = query::transaction(w.store(), &hash(1).as_bytes())
            .unwrap()
            .unwrap();
        assert_eq!(
            (d.version, d.lock_time, d.gas),
            (Some(2), Some(77), Some(5))
        );
        assert_eq!(d.subnetwork, "native");
        assert_eq!(d.payload_len, 300);
        assert_eq!(d.payload, vec![0x61; 300]);
        assert_eq!(d.sig_ops, 3);
        assert_eq!((d.covenant_created, d.covenant_spent), (1, 1));
        assert_eq!(d.inputs[0].script_class, Some("P2PK"));
        assert_eq!(d.inputs[1].script_class, Some("P2SH"));
        assert_eq!(d.outputs[0].script_class, Some("P2SH"));
        assert!(d.outputs[0].covenant);
        assert_eq!((d.storage_mass, d.compute_mass, d.mass), (1, 1, 1));

        // A transaction the node told less about.
        let d = {
            w.apply(&response(
                vec![],
                vec![chain_block(2, 2_000, vec![tx(2, &[(1, 10)], &[(2, 9)])])],
            ))
            .unwrap();
            query::transaction(w.store(), &hash(2).as_bytes())
                .unwrap()
                .unwrap()
        };
        assert_eq!(d.subnetwork, "unknown");
        assert_eq!(d.inputs[0].script_class, None);
        assert_eq!(d.payload_len, 0);
    }

    #[test]
    fn ttx_lists_newest_first_and_undoes() {
        use crate::index::records::{TxSummary, parse_time_key};
        let mut tw = writer();
        let w = &mut tw.writer;
        w.apply(&response(
            vec![],
            vec![
                chain_block(1, 1_000, vec![tx(1, &[(1, 100)], &[(2, 90)])]),
                chain_block(
                    2,
                    2_000,
                    vec![tx(2, &[(1, 100)], &[(2, 80)]), tx(3, &[], &[(2, 5)])],
                ),
            ],
        ))
        .unwrap();
        let scan = |store: &IndexStore| -> Vec<(u64, u64, Option<u64>)> {
            let mut rows = Vec::new();
            for slab in store.slabs().into_iter().rev() {
                for guard in slab.time_tx.iter().rev() {
                    let (key, value) = guard.into_inner().unwrap();
                    let (time_ms, txid) = parse_time_key(&key).unwrap();
                    let summary: TxSummary = decode(&value).unwrap();
                    rows.push((
                        time_ms,
                        u64::from_be_bytes(txid[..8].try_into().unwrap()),
                        summary.fee,
                    ));
                }
            }
            rows
        };
        let rows = scan(w.store());
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].0, 2_000);
        assert!(rows[0].1 == 2 || rows[0].1 == 3);
        assert_eq!(rows[2], (1_000, 1, Some(10)));
        let coinbase = rows.iter().find(|r| r.1 == 3).unwrap();
        assert_eq!(coinbase.2, None);

        w.apply(&response(vec![chain_hash(2)], vec![])).unwrap();
        assert_eq!(scan(w.store()), vec![(1_000, 1, Some(10))]);
    }

    #[test]
    fn slabs_partition_by_time_and_prune() {
        let mut tw = writer();
        let w = &mut tw.writer;
        let r = response(
            vec![],
            vec![
                chain_block(1, 100, vec![tx(1, &[(1, 100)], &[(2, 90)])]),
                chain_block(2, SLAB_MS + 100, vec![tx(2, &[(1, 100)], &[(2, 90)])]),
            ],
        );
        w.apply(&r).unwrap();
        let store = w.store();
        assert_eq!(store.slabs().len(), 2);
        let a2 = store.lookup(&address(2).to_string()).unwrap().unwrap();
        assert_eq!(query::stats(store, a2).unwrap().received, 180);
        assert_eq!(store.prune_before(SLAB_MS).unwrap(), 1);
        let stats = query::stats(store, a2).unwrap();
        assert_eq!((stats.received, stats.first_seen_ms), (90, SLAB_MS + 100));
    }

    /// Throughput check for the write path (Phase 0 of the address monitoring plan):
    /// `cargo test -p x4kas-core -- --ignored --nocapture bench_ingest`. Ingests a
    /// synthetic chain at a realistic shape (one chain block per 100 ms, ~2 inputs and
    /// ~2 outputs per transaction, addresses drawn from a pool with heavy reuse) and
    /// prints transactions per second, bytes per transaction and query latencies.
    #[test]
    #[ignore]
    fn bench_ingest() {
        let mut tw = writer();
        let w = &mut tw.writer;
        let tx_per_block: u64 = std::env::var("BENCH_TX_PER_BLOCK")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(500); // 5,000 TPS at 10 blocks/s
        let blocks: u64 = std::env::var("BENCH_BLOCKS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(600); // one minute of chain
        let batch_blocks = 100;
        let pool = 2_000_000u32;
        let mut rng = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = |m: u64| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng % m
        };
        let hot = 1u32; // an exchange-like address in a tenth of all transactions

        let start = Instant::now();
        let mut txs = 0u64;
        let mut id = 1u64;
        for batch in 0..blocks / batch_blocks {
            let mut chain = Vec::with_capacity(batch_blocks as usize);
            for b in 0..batch_blocks {
                let n = batch * batch_blocks + b;
                let mut list = Vec::with_capacity(tx_per_block as usize + 1);
                list.push(tx(id, &[], &[(next(pool as u64) as u32, 5_000_000_000)]));
                id += 1;
                for _ in 0..tx_per_block {
                    let a = if next(10) == 0 {
                        hot
                    } else {
                        next(pool as u64) as u32
                    };
                    let b2 = next(pool as u64) as u32;
                    let amount = 1_000_000 + next(10_000_000_000);
                    let inputs = if next(3) == 0 {
                        vec![
                            (a, amount / 2 + 1000),
                            (next(pool as u64) as u32, amount / 2 + 1000),
                        ]
                    } else {
                        vec![(a, amount + 2000)]
                    };
                    list.push(tx(id, &inputs, &[(b2, amount), (a, 1000)]));
                    id += 1;
                }
                txs += list.len() as u64;
                chain.push(chain_block(n, n * 100, list));
            }
            w.apply(&response(vec![], chain)).unwrap();
        }
        let elapsed = start.elapsed();
        let store = w.store();
        store.persist().unwrap();
        // Flush and compact so the size is the tables', not the journal's.
        for slab in store.slabs() {
            for ks in slab.keyspaces() {
                ks.rotate_memtable_and_wait().unwrap();
                ks.major_compact().unwrap();
            }
        }
        for ks in [store.addr_by_str(), store.str_by_id()] {
            ks.rotate_memtable_and_wait().unwrap();
            ks.major_compact().unwrap();
        }
        let bytes: u64 = store
            .slabs()
            .iter()
            .flat_map(Slab::keyspaces)
            .map(|ks| ks.disk_space())
            .sum();
        let intern_bytes = store.addr_by_str().disk_space() + store.str_by_id().disk_space();
        eprintln!(
            "ingest: {txs} txs in {:.1}s = {:.0} tx/s; slabs {} MB = {:.0} B/tx, plus {} MB for {} addresses",
            elapsed.as_secs_f64(),
            txs as f64 / elapsed.as_secs_f64(),
            bytes / (1024 * 1024),
            bytes as f64 / txs as f64,
            intern_bytes / (1024 * 1024),
            w.manifest().next_addr_id,
        );

        let hot_id = store.lookup(&address(hot).to_string()).unwrap().unwrap();
        let t = Instant::now();
        let stats = query::stats(store, hot_id).unwrap();
        eprintln!(
            "profile of hot address ({} txs): {:?}",
            stats.tx_count,
            t.elapsed()
        );
        let t = Instant::now();
        let page = query::transactions(store, hot_id, None, 100).unwrap();
        eprintln!("first page of 100: {:?}", t.elapsed());
        let t = Instant::now();
        let _ = query::transactions(store, hot_id, page.next, 100).unwrap();
        eprintln!("second page: {:?}", t.elapsed());
        let t = Instant::now();
        let peers = query::counterparties(store, hot_id, 25).unwrap();
        eprintln!(
            "top 25 of {} counterparties: {:?}",
            peers.len(),
            t.elapsed()
        );
    }

    #[test]
    fn common_inputs_and_change_form_clusters() {
        let mut tw = writer();
        let w = &mut tw.writer;
        // 1 and 2 spend together, paying a round 5 KAS to 3 and odd change to fresh 4.
        let r = response(
            vec![],
            vec![chain_block(
                1,
                1_000,
                vec![
                    tx(1, &[(3, 1_000)], &[(3, 900)]), // 3 is seen before, so not fresh
                    tx(
                        2,
                        &[(1, 400_000_000), (2, 200_000_000)],
                        &[(3, 500_000_000), (4, 99_990_000)],
                    ),
                ],
            )],
        );
        w.apply(&r).unwrap();
        let store = w.store();
        let id = |n: u32| store.lookup(&address(n).to_string()).unwrap().unwrap();
        let book = LabelBook::base();
        let c = query::cluster(store, &book, id(1), 10).unwrap();
        assert_eq!(c.size, 3);
        let mut members = c.members.clone();
        members.sort();
        let mut expected = vec![
            address(1).to_string(),
            address(2).to_string(),
            address(4).to_string(),
        ];
        expected.sort();
        assert_eq!(members, expected);
        assert_eq!(query::cluster(store, &book, id(3), 10).unwrap().size, 1);
        let detail = query::transaction(store, &hash(2).as_bytes())
            .unwrap()
            .unwrap();
        assert!(detail.outputs[1].change >= CHANGE_THRESHOLD);
        assert_eq!(detail.outputs[0].change, 0);

        // Clusters survive a second batch and merge with new company.
        let r2 = response(
            vec![],
            vec![chain_block(
                2,
                2_000,
                vec![tx(3, &[(4, 10), (5, 10)], &[(6, 15)])],
            )],
        );
        w.apply(&r2).unwrap();
        let store = w.store();
        let id5 = store.lookup(&address(5).to_string()).unwrap().unwrap();
        assert_eq!(query::cluster(store, &book, id5, 10).unwrap().size, 4);
        assert_eq!(
            query::profile(store, &address(2).to_string())
                .unwrap()
                .cluster_size,
            4
        );
    }

    #[test]
    fn guards_keep_strangers_apart() {
        use crate::labels::AddressName;
        let mut book = LabelBook::base();
        let named = |n: u32, name: &str| AddressName {
            address: address(n).to_string(),
            name: name.to_string(),
        };
        book.apply_kaspa_org(
            &[named(1, "Bybit"), named(2, "Gate.io"), named(5, "Bybit")],
            std::time::SystemTime::now(),
        );
        let mut tw = writer_with(book.clone());
        let w = &mut tw.writer;

        // A bridge-protocol spend: its payload names Kasplex.
        let mut bridge = tx(3, &[(3, 10), (4, 10)], &[(9, 15)]);
        bridge.payload = Some(b"kasplex bridge".to_vec());
        // A covenant spend: a script-hash input whose redeem script introspects.
        let mut covenant = tx(4, &[(6, 10), (7, 10)], &[(9, 15)]);
        spend_from_script(
            &mut covenant,
            0,
            6,
            vec![3, 0xaa, 0xbb, 0xcc, 2, 0xb4, 0xac],
        );
        // A plain script-hash spend (a multisig wallet) still clusters.
        let mut multisig = tx(5, &[(10, 10), (11, 10)], &[(9, 15)]);
        spend_from_script(&mut multisig, 0, 10, vec![3, 0xaa, 0xbb, 0xcc, 1, 0xac]);
        let r = response(
            vec![],
            vec![chain_block(
                1,
                1_000,
                vec![
                    // Two exchanges sweeping together never become one owner.
                    tx(1, &[(1, 10), (2, 10)], &[(9, 15)]),
                    // Two Bybit addresses do.
                    tx(2, &[(1, 10), (5, 10)], &[(9, 15)]),
                    bridge,
                    covenant,
                    multisig,
                    // The covenant's other party spends normally later.
                    tx(6, &[(7, 10), (8, 10)], &[(9, 15)]),
                ],
            )],
        );
        let report = w.apply(&r).unwrap();
        assert_eq!(report.cluster_cap_hits, 0);
        let store = w.store();
        let id = |a: &str| store.lookup(a).unwrap().unwrap();
        let size = |a: &str| query::cluster(store, &book, id(a), 10).unwrap().size;
        let plain = |n: u32| address(n).to_string();

        let bybit = query::cluster(store, &book, id(&plain(1)), 10).unwrap();
        assert_eq!(bybit.size, 2);
        assert_eq!(bybit.label.as_deref(), Some("Bybit"));
        assert_eq!(size(&plain(2)), 1);
        assert_eq!(size(&plain(3)), 1);
        assert_eq!(size(&plain(4)), 1);
        assert_eq!(size(&p2sh_address(6).to_string()), 1);
        assert_eq!(size(&p2sh_address(10).to_string()), 2);
        let party = query::cluster(store, &book, id(&plain(7)), 10).unwrap();
        assert_eq!(party.size, 2);
        assert_eq!(party.label, None);
        assert!(party.members.contains(&plain(8)));
    }

    #[test]
    fn peer_flows_split_by_input_share() {
        let input = |addr, amount| TxInput {
            addr: Some(addr),
            amount: Some(amount),
            prev_txid: [0; 32],
            prev_index: 0,
            script_class: 0,
        };
        let output = |addr, amount, change| TxOutput {
            addr: Some(addr),
            amount,
            change,
            script_class: 0,
            covenant: false,
        };
        let tx = IndexedTx {
            inputs: vec![input(1, 75), input(2, 25)],
            outputs: vec![output(3, 40, 0), output(1, 50, 100)],
            fee: Some(10),
            ..IndexedTx::empty([0; 32], 0, 0)
        };
        assert_eq!(peer_flows(&tx), vec![(1, 3, 30), (2, 3, 10)]);
    }
}
