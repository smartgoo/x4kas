//! Turns VSPC v2 responses into index writes: one atomic batch per response, with
//! reorg undo and already-indexed chain blocks skipped, so re-applying a response after
//! a crash or a restart from an older position changes nothing.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use anyhow::Result;
use fjall::OwnedWriteBatch as WriteBatch;
use kaspa_rpc_core::{GetVirtualChainFromBlockV2Response, RpcHash, RpcOptionalTransaction};

use super::cluster::{self, CHANGE_THRESHOLD, Clusters, OutputTrait};
use super::records::{
    AddrId, AddrStats, Hash32, IndexedTx, PeerStats, TxInput, TxOutput, addr_key, addr_tx_key,
    block_tx_key, decode, encode, encode_delta, peer_key,
};
use super::{IndexStore, Manifest, Position, Slab};
use crate::labels::LabelBook;
use crate::tx_inspect::detect_protocol;

/// Interned addresses kept in memory; the map is cleared when it grows past this.
const INTERN_CACHE_MAX: usize = 1_000_000;
/// A transaction with more sender × receiver pairs than this (a huge batch payout from
/// many inputs) gets no counterparty entries: they'd be noise and cost a lot.
const MAX_PEER_PAIRS: usize = 10_000;
/// Removed chain blocks are looked for in this many newest slabs.
const REORG_SLABS: usize = 2;

/// What one applied response did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BatchReport {
    pub chain_blocks: usize,
    pub txs: usize,
    /// Chain blocks already in the index (a replayed response).
    pub skipped_blocks: usize,
    pub reorged_blocks: usize,
    /// Removed chain blocks the index didn't have (already pruned, or never indexed).
    pub unresolved_reorgs: Vec<String>,
    /// Unions the cluster size cap refused.
    pub cluster_cap_hits: u64,
    pub newest: Option<Position>,
}

pub struct IndexWriter {
    store: Arc<IndexStore>,
    manifest: Manifest,
    intern_cache: HashMap<String, AddrId>,
    /// Entity labels, for the clustering guard.
    labels: Arc<LabelBook>,
}

/// A converted transaction with what clustering needs to know about it.
struct Converted {
    txid: Hash32,
    tx: IndexedTx,
    /// Whether the inputs may be unioned (see `cluster::may_union`).
    may_union: bool,
}

/// Per-batch read-modify-write buffer for stats and peers, so an address touched by
/// many transactions in one response is read and written once.
#[derive(Default)]
struct Overlay {
    stats: HashMap<(u64, AddrId), AddrStats>,
    peers: HashMap<(u64, AddrId, AddrId), PeerStats>,
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

    fn peer_mut(&mut self, slab: &Slab, addr: AddrId, peer: AddrId) -> Result<&mut PeerStats> {
        self.slabs.entry(slab.no).or_insert_with(|| slab.clone());
        match self.peers.entry((slab.no, addr, peer)) {
            std::collections::hash_map::Entry::Occupied(e) => Ok(e.into_mut()),
            std::collections::hash_map::Entry::Vacant(e) => {
                let stored = match slab.peers.get(peer_key(addr, peer))? {
                    Some(bytes) => decode(&bytes)?,
                    None => PeerStats::default(),
                };
                Ok(e.insert(stored))
            }
        }
    }

    fn flush(self, batch: &mut WriteBatch) -> Result<()> {
        for ((slab_no, addr), stats) in self.stats {
            let slab = &self.slabs[&slab_no];
            if stats.tx_count == 0 {
                batch.remove(&slab.stats, addr_key(addr));
            } else {
                batch.insert(&slab.stats, addr_key(addr), encode(&stats)?);
            }
        }
        for ((slab_no, addr, peer), stats) in self.peers {
            let slab = &self.slabs[&slab_no];
            if stats.tx_count == 0 {
                batch.remove(&slab.peers, peer_key(addr, peer));
            } else {
                batch.insert(&slab.peers, peer_key(addr, peer), encode(&stats)?);
            }
        }
        Ok(())
    }
}

impl IndexWriter {
    pub fn new(store: Arc<IndexStore>, labels: Arc<LabelBook>) -> Result<Self> {
        let manifest = store.manifest()?;
        Ok(Self {
            store,
            manifest,
            intern_cache: HashMap::new(),
            labels,
        })
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    pub fn store(&self) -> &Arc<IndexStore> {
        &self.store
    }

    /// Apply one VSPC v2 response atomically: undo its removed chain blocks, index its
    /// added ones, advance the manifest.
    pub fn apply(&mut self, response: &GetVirtualChainFromBlockV2Response) -> Result<BatchReport> {
        let result = self.apply_inner(response);
        if result.is_err() {
            // Ids handed out for this batch were never committed: forget them.
            self.intern_cache.clear();
            self.manifest = self.store.manifest()?;
        }
        result
    }

    fn apply_inner(
        &mut self,
        response: &GetVirtualChainFromBlockV2Response,
    ) -> Result<BatchReport> {
        let mut batch = self.store.db().batch();
        let mut overlay = Overlay::default();
        let store = self.store.clone();
        let mut clusters = Clusters::new(store.clusters());
        let mut report = BatchReport::default();

        for hash in response.removed_chain_block_hashes.iter() {
            if self.undo_block(&mut batch, &mut overlay, hash)? {
                report.reorged_blocks += 1;
            } else {
                report.unresolved_reorgs.push(hash.to_string());
            }
        }

        for chain_block in response.chain_block_accepted_transactions.iter() {
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
                continue;
            }
            for tx in &chain_block.accepted_transactions {
                let Some(converted) = self.convert(&mut batch, tx, block, daa_score, time_ms)?
                else {
                    continue;
                };
                let Converted {
                    txid,
                    tx,
                    may_union,
                } = converted;
                self.put_tx(&mut batch, &mut overlay, &slab, &block, &txid, &tx)?;
                if may_union {
                    link_owners(&mut clusters, &tx)?;
                }
                report.txs += 1;
            }
        }

        overlay.flush(&mut batch)?;
        report.cluster_cap_hits = clusters.cap_hits;
        clusters.flush(&mut batch);
        if let Some(newest) = report.newest {
            self.manifest.position = Some(newest);
        }
        self.manifest.txs_indexed += report.txs as u64;
        batch.insert(
            self.store.meta_keyspace(),
            "manifest",
            IndexStore::encode_manifest(&self.manifest)?,
        );
        batch.commit()?;
        Ok(report)
    }

    /// Reverse every transaction a removed chain block accepted. Returns false when the
    /// block isn't in the newest slabs.
    fn undo_block(
        &mut self,
        batch: &mut WriteBatch,
        overlay: &mut Overlay,
        hash: &RpcHash,
    ) -> Result<bool> {
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
            for txid in txids {
                if let Some(bytes) = slab.tx.get(txid)? {
                    let tx: IndexedTx = decode(&bytes)?;
                    self.remove_tx(batch, overlay, slab, &block, &txid, &tx)?;
                }
            }
            return Ok(true);
        }
        Ok(false)
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
                .peer_mut(slab, sender, receiver)?
                .apply(0, amount, false);
            overlay
                .peer_mut(slab, receiver, sender)?
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
        batch.remove(&slab.block_tx, block_tx_key(block, txid));
        for addr in tx.addresses() {
            let (received, sent) = (tx.received_by(addr), tx.sent_by(addr));
            batch.remove(&slab.addr_tx, addr_tx_key(addr, tx.time_ms, txid));
            overlay
                .stats_mut(slab, addr)?
                .apply(tx.time_ms, received, sent, true);
        }
        for (sender, receiver, amount) in peer_flows(tx) {
            overlay
                .peer_mut(slab, sender, receiver)?
                .apply(0, amount, true);
            overlay
                .peer_mut(slab, receiver, sender)?
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
        block: Hash32,
        daa_score: u64,
        time_ms: u64,
    ) -> Result<Option<Converted>> {
        let Some(txid) = tx.verbose_data.as_ref().and_then(|v| v.transaction_id) else {
            return Ok(None);
        };

        let mut inputs = Vec::with_capacity(tx.inputs.len());
        let mut input_sum = Some(0u64);
        let mut input_scripts: Vec<&[u8]> = Vec::with_capacity(tx.inputs.len());
        // For clustering: the inputs' address version and entity labels.
        let mut input_version: Option<u8> = None;
        let mut input_labels: Vec<Option<String>> = Vec::new();
        for input in &tx.inputs {
            let utxo = input
                .verbose_data
                .as_ref()
                .and_then(|vd| vd.utxo_entry.as_ref());
            let amount = utxo.and_then(|u| u.amount);
            input_sum = input_sum.zip(amount).map(|(a, b)| a + b);
            let addr = match utxo
                .and_then(|u| u.verbose_data.as_ref())
                .and_then(|v| v.script_public_key_address.as_ref())
            {
                Some(a) => {
                    let text = a.to_string();
                    input_version.get_or_insert(a.version as u8);
                    input_labels.push(self.labels.name(&text).map(str::to_string));
                    Some(self.intern(batch, &text)?.0)
                }
                None => None,
            };
            if let Some(script) = input.signature_script.as_deref() {
                input_scripts.push(script);
            }
            let prev = input.previous_outpoint.as_ref();
            inputs.push(TxInput {
                addr,
                amount,
                prev_txid: prev
                    .and_then(|p| p.transaction_id)
                    .map(|h| h.as_bytes())
                    .unwrap_or_default(),
                prev_index: prev.and_then(|p| p.index).unwrap_or_default(),
            });
        }

        let mut outputs = Vec::with_capacity(tx.outputs.len());
        let mut traits = Vec::with_capacity(tx.outputs.len());
        let mut output_sum = 0u64;
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
            outputs.push(TxOutput {
                addr,
                amount,
                change: 0,
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
        let may_union = !is_coinbase && has_sender && cluster::may_union(protocol, &labels);
        let compute_mass = tx
            .verbose_data
            .as_ref()
            .and_then(|v| v.compute_mass)
            .unwrap_or(0);
        Ok(Some(Converted {
            txid: txid.as_bytes(),
            tx: IndexedTx {
                accepting_block: block,
                daa_score,
                time_ms,
                inputs,
                outputs,
                fee: if is_coinbase {
                    None
                } else {
                    input_sum.map(|i| i.saturating_sub(output_sum))
                },
                mass: tx.storage_mass.unwrap_or(0).max(compute_mass),
                is_coinbase,
                protocol,
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

    pub fn chain_block(
        n: u64,
        time_ms: u64,
        txs: Vec<RpcOptionalTransaction>,
    ) -> RpcChainBlockAcceptedTransactions {
        RpcChainBlockAcceptedTransactions {
            chain_block_header: RpcOptionalHeader {
                hash: Some(hash(1_000_000 + n)),
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
        let store = temp_store();
        let writer = IndexWriter::new(store.store.clone(), Arc::new(LabelBook::bundled())).unwrap();
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
            for ks in [
                &slab.tx,
                &slab.addr_tx,
                &slab.block_tx,
                &slab.stats,
                &slab.peers,
            ] {
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
            .flat_map(|s| [&s.tx, &s.addr_tx, &s.block_tx, &s.stats, &s.peers])
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
        let c = query::cluster(store, id(1), 10).unwrap();
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
        assert_eq!(query::cluster(store, id(3), 10).unwrap().size, 1);
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
        assert_eq!(query::cluster(store, id5, 10).unwrap().size, 4);
        assert_eq!(
            query::profile(store, &address(2).to_string())
                .unwrap()
                .cluster_size,
            4
        );
    }

    #[test]
    fn peer_flows_split_by_input_share() {
        let tx = IndexedTx {
            accepting_block: [0; 32],
            daa_score: 0,
            time_ms: 0,
            inputs: vec![
                TxInput {
                    addr: Some(1),
                    amount: Some(75),
                    prev_txid: [0; 32],
                    prev_index: 0,
                },
                TxInput {
                    addr: Some(2),
                    amount: Some(25),
                    prev_txid: [0; 32],
                    prev_index: 0,
                },
            ],
            outputs: vec![
                TxOutput {
                    addr: Some(3),
                    amount: 40,
                    change: 0,
                },
                TxOutput {
                    addr: Some(1),
                    amount: 50,
                    change: 100,
                },
            ],
            fee: Some(10),
            mass: 0,
            is_coinbase: false,
            protocol: None,
        };
        assert_eq!(peer_flows(&tx), vec![(1, 3, 30), (2, 3, 10)]);
    }
}
