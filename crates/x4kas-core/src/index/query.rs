//! Read side of the address index: profiles, transaction pages, counterparties and
//! balance history. Plain structs, serializable for the CLI. Synchronous; call inside
//! `spawn_blocking` from async code.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::str::FromStr;

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};

use super::cluster;
use super::peel::{self, PeelChain};
use super::records::{
    AddrId, AddrStats, BlockKind, BlockRecord, Hash32, IndexedTx, PeerStats, addr_key, addr_tx_key,
    decode, decode_delta, parse_addr_tx_key, parse_peer_key, parse_protocol_tx_key,
    protocol_tx_key,
};
use super::{IndexStore, Slab, hex, parse_hex};
use crate::labels::LabelBook;
use crate::tx_inspect::{OpcodeUsage, ScriptClass, TransactionProtocol, difficulty_from_bits};

/// An address's totals over everything the index holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddressProfile {
    pub address: String,
    /// `None` when the index has never seen the address.
    pub id: Option<AddrId>,
    pub stats: AddrStats,
    /// The time span the index covers, `(from_ms, to_ms)`.
    pub coverage: Option<(u64, u64)>,
    /// Addresses in the same likely-owner cluster, this one included.
    pub cluster_size: u32,
    /// The peel chain this address is a link of, if any (`peel::peel_chain`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peel_chain: Option<PeelChain>,
}

impl AddressProfile {
    /// A profile for an address no index was asked about (no store, e.g. with the
    /// resolver): nothing indexed.
    pub fn unindexed(address: &str) -> Self {
        Self {
            address: address.to_string(),
            id: None,
            stats: AddrStats::default(),
            coverage: None,
            cluster_size: 0,
            peel_chain: None,
        }
    }
}

/// An address's likely-owner cluster.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterInfo {
    pub root: String,
    pub size: u32,
    /// Up to the requested number of members, the root first.
    pub members: Vec<String>,
    /// The likely owner: the strongest label among the sampled members (the most
    /// common one at that strength; `LabelBook::name_cluster`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Where a transaction page continues: the oldest entry shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    pub time_ms: u64,
    pub txid: Hash32,
}

impl fmt::Display for Cursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.time_ms, hex(&self.txid))
    }
}

impl FromStr for Cursor {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let (time, txid) = s
            .split_once(':')
            .ok_or_else(|| anyhow!("cursor must be <time_ms>:<txid>"))?;
        Ok(Self {
            time_ms: time.parse()?,
            txid: parse_hex(txid).ok_or_else(|| anyhow!("bad txid in cursor"))?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    /// Pass as `before` to get the next (older) page; `None` at the end.
    pub next: Option<Cursor>,
}

impl<T> Page<T> {
    /// Append an older page's rows, skipping the rows already here by `id` (a page
    /// asked for twice, with the same cursor, must not double its rows), and continue
    /// from where it ends.
    pub fn append(&mut self, more: Page<T>, id: impl Fn(&T) -> &str) {
        let have: HashSet<String> = self.items.iter().map(|t| id(t).to_string()).collect();
        self.items
            .extend(more.items.into_iter().filter(|t| !have.contains(id(t))));
        self.next = more.next;
    }
}

/// One row of a protocol's transaction list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolTxRow {
    pub txid: String,
    pub time_ms: u64,
    pub daa_score: u64,
    pub accepting_block: String,
    /// Sompi over the outputs.
    pub output_total: u64,
    pub fee: Option<u64>,
    pub input_count: usize,
    pub output_count: usize,
    /// The first output's address.
    pub recipient: Option<String>,
}

/// One row of an address's transaction list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TxRow {
    pub txid: String,
    pub time_ms: u64,
    pub daa_score: u64,
    pub accepting_block: String,
    /// How the address's balance changed, in sompi.
    pub delta: i64,
    pub fee: Option<u64>,
    pub is_coinbase: bool,
    pub protocol: Option<TransactionProtocol>,
    pub input_count: usize,
    pub output_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TxInputDetail {
    pub address: Option<String>,
    pub amount: Option<u64>,
    pub prev_txid: String,
    pub prev_index: u32,
    /// The spent output's script class (`ScriptClass::label`), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script_class: Option<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TxOutputDetail {
    pub address: Option<String>,
    pub amount: u64,
    /// Likelihood this is the sender's change, 0–100.
    pub change: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script_class: Option<&'static str>,
    #[serde(default)]
    pub covenant: bool,
}

/// A stored transaction with its addresses resolved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TxDetail {
    pub txid: String,
    pub accepting_block: String,
    pub daa_score: u64,
    pub time_ms: u64,
    /// The block holding the transaction (`None` when the node didn't say).
    pub block: Option<String>,
    pub block_time_ms: u64,
    pub inputs: Vec<TxInputDetail>,
    pub outputs: Vec<TxOutputDetail>,
    pub fee: Option<u64>,
    /// The larger of the storage and compute masses.
    pub mass: u64,
    pub storage_mass: u64,
    pub compute_mass: u64,
    pub is_coinbase: bool,
    pub protocol: Option<TransactionProtocol>,
    pub version: Option<u16>,
    pub lock_time: Option<u64>,
    /// `native`, `coinbase`, `other` or `unknown`.
    pub subnetwork: &'static str,
    pub gas: Option<u64>,
    pub payload_len: u32,
    /// The first `records::PAYLOAD_HEAD` bytes of the payload.
    #[serde(with = "serde_bytes_hex")]
    pub payload_head: Vec<u8>,
    pub opcodes: OpcodeUsageDetail,
    pub covenant_created: u16,
    pub covenant_spent: u16,
    pub sig_ops: u32,
}

impl TxDetail {
    /// A coinbase-less, input-less transaction, for tests.
    #[cfg(test)]
    pub(crate) fn minimal(txid: &str, accepting_block: &str, daa_score: u64, time_ms: u64) -> Self {
        Self {
            txid: txid.to_string(),
            accepting_block: accepting_block.to_string(),
            daa_score,
            time_ms,
            block: None,
            block_time_ms: time_ms,
            inputs: Vec::new(),
            outputs: Vec::new(),
            fee: None,
            mass: 0,
            storage_mass: 0,
            compute_mass: 0,
            is_coinbase: false,
            protocol: None,
            version: None,
            lock_time: None,
            subnetwork: "unknown",
            gas: None,
            payload_len: 0,
            payload_head: Vec::new(),
            opcodes: OpcodeUsageDetail::default(),
            covenant_created: 0,
            covenant_spent: 0,
            sig_ops: 0,
        }
    }
}

/// Payload bytes as hex in JSON.
mod serde_bytes_hex {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&bytes.iter().map(|b| format!("{b:02x}")).collect::<String>())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let hex = String::deserialize(d)?;
        (0..hex.len())
            .step_by(2)
            .map(|i| {
                u8::from_str_radix(hex.get(i..i + 2).unwrap_or("zz"), 16)
                    .map_err(serde::de::Error::custom)
            })
            .collect()
    }
}

/// The covenant-era opcodes a transaction used, spelled out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct OpcodeUsageDetail {
    pub introspection: bool,
    pub chainblock_seqcommit: bool,
    pub zk_groth16: bool,
    pub zk_r0succinct: bool,
    pub zk_unknown: bool,
}

impl From<OpcodeUsage> for OpcodeUsageDetail {
    fn from(u: OpcodeUsage) -> Self {
        Self {
            introspection: u.introspection,
            chainblock_seqcommit: u.chainblock_seqcommit,
            zk_groth16: u.zk_groth16,
            zk_r0succinct: u.zk_r0succinct,
            zk_unknown: u.zk_unknown,
        }
    }
}

/// A stored block with its addresses resolved.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BlockDetail {
    pub hash: String,
    /// `chain` or `merged`.
    pub kind: &'static str,
    pub time_ms: u64,
    pub merging_block: String,
    pub version: Option<u16>,
    pub daa_score: Option<u64>,
    pub blue_score: Option<u64>,
    /// Hex.
    pub blue_work: Option<String>,
    pub bits: Option<u32>,
    pub difficulty: Option<f64>,
    pub nonce: Option<u64>,
    pub parents: Vec<String>,
    pub parent_levels: u8,
    pub hash_merkle_root: Option<String>,
    pub accepted_id_merkle_root: Option<String>,
    pub utxo_commitment: Option<String>,
    pub pruning_point: Option<String>,
    pub coinbase_txid: Option<String>,
    pub miner: Option<String>,
    pub miner_tag: Option<String>,
    pub node_version: Option<String>,
    pub subsidy: Option<u64>,
    /// The coinbase's outputs: `(address, sompi)`.
    pub payouts: Vec<(Option<String>, u64)>,
    pub accepted_txs: u32,
    pub accepted_mass: u64,
    pub accepted_fees: u64,
}

impl BlockKind {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Chain => "chain",
            Self::Merged => "merged",
        }
    }
}

/// The stored block `hash`, if the index has it.
pub fn block(store: &IndexStore, hash: &Hash32) -> Result<Option<BlockDetail>> {
    for slab in store.slabs().into_iter().rev() {
        let Some(bytes) = slab.blocks.get(hash)? else {
            continue;
        };
        let record: BlockRecord = decode(&bytes)?;
        return Ok(Some(block_detail(store, hash, &record)?));
    }
    Ok(None)
}

/// A [`BlockDetail`] of a stored record, its addresses resolved.
pub fn block_detail(
    store: &IndexStore,
    hash: &Hash32,
    record: &BlockRecord,
) -> Result<BlockDetail> {
    let h = |o: &Option<Hash32>| o.as_ref().map(hex);
    let mut payouts = Vec::with_capacity(record.payouts.len());
    for p in &record.payouts {
        let address = match p.addr {
            Some(id) => store.address_of(id)?,
            None => None,
        };
        payouts.push((address, p.amount));
    }
    Ok(BlockDetail {
        hash: hex(hash),
        kind: record.kind.label(),
        time_ms: record.time_ms,
        merging_block: hex(&record.merging_block),
        version: record.version,
        daa_score: record.daa_score,
        blue_score: record.blue_score,
        blue_work: record
            .blue_work
            .map(|w| w.iter().map(|b| format!("{b:02x}")).collect::<String>())
            .map(|s| s.trim_start_matches('0').to_string())
            .map(|s| if s.is_empty() { "0".to_string() } else { s }),
        bits: record.bits,
        difficulty: record.bits.map(difficulty_from_bits),
        nonce: record.nonce,
        parents: record.parents.iter().map(hex).collect(),
        parent_levels: record.parent_levels,
        hash_merkle_root: h(&record.hash_merkle_root),
        accepted_id_merkle_root: h(&record.accepted_id_merkle_root),
        utxo_commitment: h(&record.utxo_commitment),
        pruning_point: h(&record.pruning_point),
        coinbase_txid: h(&record.coinbase_txid),
        miner: match record.miner {
            Some(id) => store.address_of(id)?,
            None => None,
        },
        miner_tag: record.miner_tag.clone(),
        node_version: record.node_version.clone(),
        subsidy: record.subsidy,
        payouts,
        accepted_txs: record.accepted_txs,
        accepted_mass: record.accepted_mass,
        accepted_fees: record.accepted_fees,
    })
}

/// A counterparty of an address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Peer {
    pub address: String,
    pub id: AddrId,
    pub stats: PeerStats,
}

impl Peer {
    pub fn volume(&self) -> u64 {
        self.stats.in_amount + self.stats.out_amount
    }
}

/// A node of a flow graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlowNode {
    pub id: AddrId,
    pub address: String,
    /// Sompi over the node's edges in the graph.
    pub volume: u64,
    /// Distance from the roots.
    pub hop: u8,
}

/// Money moved from `from` to `to`, aggregated over the indexed window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlowEdge {
    pub from: AddrId,
    pub to: AddrId,
    /// Sompi; over a collapsed chain, the least any link carried.
    pub amount: u64,
    pub tx_count: u64,
    /// The pass-through addresses a collapsed edge skips, in order
    /// (`FlowGraph::collapse_chains`); empty for a direct flow.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub via: Vec<String>,
}

impl FlowEdge {
    /// Direct flows this edge stands for: 1, or more for a collapsed chain.
    pub fn hops(&self) -> usize {
        self.via.len() + 1
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlowGraph {
    pub nodes: Vec<FlowNode>,
    pub edges: Vec<FlowEdge>,
}

impl FlowGraph {
    /// Add another graph's nodes and edges (an expansion), keeping the first hop
    /// number seen for a node.
    pub fn merge(&mut self, other: FlowGraph) {
        for node in other.nodes {
            match self.nodes.iter_mut().find(|n| n.id == node.id) {
                Some(existing) => existing.hop = existing.hop.min(node.hop),
                None => self.nodes.push(node),
            }
        }
        for edge in other.edges {
            if !self
                .edges
                .iter()
                .any(|e| e.from == edge.from && e.to == edge.to)
            {
                self.edges.push(edge);
            }
        }
        self.recount();
    }

    fn recount(&mut self) {
        for node in &mut self.nodes {
            node.volume = self
                .edges
                .iter()
                .filter(|e| e.from == node.id || e.to == node.id)
                .map(|e| e.amount)
                .sum();
        }
    }

    /// The same graph with every chain of pass-through addresses (one flow in, one flow
    /// out, not a root) folded into a single edge that lists them in `FlowEdge::via`:
    /// a peel chain or any other relay shows as one hop-counted arrow.
    pub fn collapse_chains(&self) -> FlowGraph {
        let roots: HashSet<AddrId> = self
            .nodes
            .iter()
            .filter(|n| n.hop == 0)
            .map(|n| n.id)
            .collect();
        let mut ins: HashMap<AddrId, Vec<usize>> = HashMap::new();
        let mut outs: HashMap<AddrId, Vec<usize>> = HashMap::new();
        for (i, e) in self.edges.iter().enumerate() {
            ins.entry(e.to).or_default().push(i);
            outs.entry(e.from).or_default().push(i);
        }
        let pass_through = |id: AddrId| -> bool {
            if roots.contains(&id) {
                return false;
            }
            match (ins.get(&id), outs.get(&id)) {
                (Some(i), Some(o)) if i.len() == 1 && o.len() == 1 => {
                    let (into, out) = (&self.edges[i[0]], &self.edges[o[0]]);
                    into.from != id && out.to != id && into.from != out.to
                }
                _ => false,
            }
        };
        let name = |id: AddrId| -> String {
            self.nodes
                .iter()
                .find(|n| n.id == id)
                .map(|n| n.address.clone())
                .unwrap_or_default()
        };
        let mut used = vec![false; self.edges.len()];
        let mut edges = Vec::with_capacity(self.edges.len());
        for (i, first) in self.edges.iter().enumerate() {
            if used[i] || pass_through(first.from) {
                continue;
            }
            used[i] = true;
            let mut last = first;
            let mut via = Vec::new();
            let (mut amount, mut tx_count) = (first.amount, first.tx_count);
            while pass_through(last.to) {
                let next = outs[&last.to][0];
                if used[next] {
                    break;
                }
                used[next] = true;
                via.push(name(last.to));
                last = &self.edges[next];
                amount = amount.min(last.amount);
                tx_count = tx_count.min(last.tx_count);
            }
            edges.push(FlowEdge {
                from: first.from,
                to: last.to,
                amount,
                tx_count,
                via,
            });
        }
        // Rings of pass-through addresses have no head to start from; keep them as is.
        for (i, e) in self.edges.iter().enumerate() {
            if !used[i] {
                edges.push(e.clone());
            }
        }
        let kept: HashSet<AddrId> = edges.iter().flat_map(|e| [e.from, e.to]).collect();
        let mut graph = FlowGraph {
            nodes: self
                .nodes
                .iter()
                .filter(|n| kept.contains(&n.id) || !pass_through(n.id))
                .cloned()
                .collect(),
            edges,
        };
        graph.edges.sort_by_key(|e| (e.from, e.to));
        graph.recount();
        graph
    }
}

/// Follow the money `hops` counterparties out from `roots`, taking the `top` peers by
/// volume at each node.
pub fn flows(store: &IndexStore, roots: &[AddrId], hops: u8, top: usize) -> Result<FlowGraph> {
    let mut graph = FlowGraph::default();
    let mut seen: HashMap<AddrId, u8> = HashMap::new();
    let mut frontier: Vec<AddrId> = Vec::new();
    for &root in roots {
        if seen.insert(root, 0).is_none() {
            graph.nodes.push(FlowNode {
                id: root,
                address: store.address_of(root)?.unwrap_or_default(),
                volume: 0,
                hop: 0,
            });
            frontier.push(root);
        }
    }
    let mut edges: HashMap<(AddrId, AddrId), (u64, u64)> = HashMap::new();
    for hop in 1..=hops {
        let mut next = Vec::new();
        for addr in frontier {
            for peer in counterparties(store, addr, top)? {
                if peer.stats.out_amount > 0 {
                    edges.insert(
                        (addr, peer.id),
                        (peer.stats.out_amount, peer.stats.tx_count),
                    );
                }
                if peer.stats.in_amount > 0 {
                    edges.insert((peer.id, addr), (peer.stats.in_amount, peer.stats.tx_count));
                }
                if seen.insert(peer.id, hop).is_none() {
                    graph.nodes.push(FlowNode {
                        id: peer.id,
                        address: peer.address,
                        volume: 0,
                        hop,
                    });
                    next.push(peer.id);
                }
            }
        }
        frontier = next;
    }
    graph.edges = edges
        .into_iter()
        .map(|((from, to), (amount, tx_count))| FlowEdge {
            from,
            to,
            amount,
            tx_count,
            via: Vec::new(),
        })
        .collect();
    graph.edges.sort_by_key(|e| (e.from, e.to));
    graph.recount();
    Ok(graph)
}

/// Totals for `id`, summed over every slab.
pub fn stats(store: &IndexStore, id: AddrId) -> Result<AddrStats> {
    let mut total = AddrStats::default();
    for slab in store.slabs() {
        if let Some(bytes) = slab.stats.get(addr_key(id))? {
            total.merge(&decode(&bytes)?);
        }
    }
    Ok(total)
}

pub fn profile(store: &IndexStore, address: &str) -> Result<AddressProfile> {
    let id = store.lookup(address)?;
    let (stats, cluster_size, peel_chain) = match id {
        Some(id) => (
            stats(store, id)?,
            cluster::size_of(store.clusters(), cluster::root_of(store.clusters(), id)?)?,
            peel::peel_chain(store, id)?,
        ),
        None => (AddrStats::default(), 0, None),
    };
    Ok(AddressProfile {
        address: address.to_string(),
        id,
        stats,
        coverage: store.coverage(),
        cluster_size,
        peel_chain,
    })
}

/// The cluster `id` belongs to, with up to `limit` members, named by `labels`.
pub fn cluster(
    store: &IndexStore,
    labels: &LabelBook,
    id: AddrId,
    limit: usize,
) -> Result<ClusterInfo> {
    let ks = store.clusters();
    let root = cluster::root_of(ks, id)?;
    let members = cluster::members_of(ks, root, limit)?
        .into_iter()
        .map(|m| Ok(store.address_of(m)?.unwrap_or_default()))
        .collect::<Result<Vec<_>>>()?;
    let label = labels.name_cluster(members.iter().map(String::as_str));
    Ok(ClusterInfo {
        root: store.address_of(root)?.unwrap_or_default(),
        size: cluster::size_of(ks, root)?,
        members,
        label,
    })
}

/// `limit` transactions of `id`, newest first, older than `before` if given.
pub fn transactions(
    store: &IndexStore,
    id: AddrId,
    before: Option<Cursor>,
    limit: usize,
) -> Result<Page<TxRow>> {
    let mut items = Vec::with_capacity(limit);
    let limit = limit.max(1);
    let start = addr_key(id);
    'slabs: for slab in store.slabs().into_iter().rev() {
        if before.is_some_and(|c| slab.start_ms() > c.time_ms) {
            continue;
        }
        let end = match before {
            Some(c) if c.time_ms < slab.end_ms() => addr_tx_key(id, c.time_ms, &c.txid).to_vec(),
            _ => addr_tx_key(id, u64::MAX, &[0xff; 32]).to_vec(),
        };
        for guard in slab.addr_tx.range(start.to_vec()..end).rev() {
            let (key, value) = guard.into_inner()?;
            let Some((time_ms, txid)) = parse_addr_tx_key(&key) else {
                continue;
            };
            let delta = decode_delta(&value).unwrap_or(0);
            let Some(bytes) = slab.tx.get(txid)? else {
                continue;
            };
            let tx: IndexedTx = decode(&bytes)?;
            items.push(TxRow {
                txid: hex(&txid),
                time_ms,
                daa_score: tx.daa_score,
                accepting_block: hex(&tx.accepting_block),
                delta,
                fee: tx.fee,
                is_coinbase: tx.is_coinbase,
                protocol: tx.protocol,
                input_count: tx.inputs.len(),
                output_count: tx.outputs.len(),
            });
            if items.len() == limit {
                break 'slabs;
            }
        }
    }
    let next = (items.len() == limit)
        .then(|| items.last())
        .flatten()
        .map(|row| Cursor {
            time_ms: row.time_ms,
            txid: parse_hex(&row.txid).unwrap_or_default(),
        });
    Ok(Page { items, next })
}

/// `limit` transactions of `protocol`, newest first, older than `before` if given.
pub fn protocol_transactions(
    store: &IndexStore,
    protocol: TransactionProtocol,
    before: Option<Cursor>,
    limit: usize,
) -> Result<Page<ProtocolTxRow>> {
    let mut items = Vec::with_capacity(limit);
    let limit = limit.max(1);
    let start = [protocol.code()];
    'slabs: for slab in store.slabs().into_iter().rev() {
        if before.is_some_and(|c| slab.start_ms() > c.time_ms) {
            continue;
        }
        let end = match before {
            Some(c) if c.time_ms < slab.end_ms() => {
                protocol_tx_key(protocol, c.time_ms, &c.txid).to_vec()
            }
            _ => protocol_tx_key(protocol, u64::MAX, &[0xff; 32]).to_vec(),
        };
        for guard in slab.protocol_tx.range(start.to_vec()..end).rev() {
            let (key, _) = guard.into_inner()?;
            let Some((time_ms, txid)) = parse_protocol_tx_key(&key) else {
                continue;
            };
            let Some(bytes) = slab.tx.get(txid)? else {
                continue;
            };
            let tx: IndexedTx = decode(&bytes)?;
            let recipient = match tx.outputs.first().and_then(|o| o.addr) {
                Some(id) => store.address_of(id)?,
                None => None,
            };
            items.push(ProtocolTxRow {
                txid: hex(&txid),
                time_ms,
                daa_score: tx.daa_score,
                accepting_block: hex(&tx.accepting_block),
                output_total: tx.outputs.iter().map(|o| o.amount).sum(),
                fee: tx.fee,
                input_count: tx.inputs.len(),
                output_count: tx.outputs.len(),
                recipient,
            });
            if items.len() == limit {
                break 'slabs;
            }
        }
    }
    let next = (items.len() == limit)
        .then(|| items.last())
        .flatten()
        .map(|row| Cursor {
            time_ms: row.time_ms,
            txid: parse_hex(&row.txid).unwrap_or_default(),
        });
    Ok(Page { items, next })
}

/// How many of an address's transactions a spender search reads before giving up on
/// an output: a busy address (an exchange) would otherwise cost a scan of everything
/// it did since.
pub const SPEND_SCAN_MAX: usize = 2_000;

/// For each output of `txid`, the id of the indexed transaction that spent it, or
/// `None` when nothing in the indexed window did (or the output's address isn't
/// known, or the address was too busy to search: [`SPEND_SCAN_MAX`]). An empty vector
/// when the index doesn't have `txid`. A spend is found by reading the output
/// address's transactions from the spent transaction's time on and matching an
/// input's outpoint.
pub fn spenders(store: &IndexStore, txid: &Hash32) -> Result<Vec<Option<String>>> {
    let mut found: Option<(IndexedTx, Slab)> = None;
    for slab in store.slabs().into_iter().rev() {
        if let Some(bytes) = slab.tx.get(txid)? {
            found = Some((decode(&bytes)?, slab));
            break;
        }
    }
    let Some((tx, _)) = found else {
        return Ok(Vec::new());
    };
    let mut spenders = vec![None; tx.outputs.len()];
    // One scan per distinct address, matching every output it holds.
    let mut addrs: Vec<AddrId> = tx.outputs.iter().filter_map(|o| o.addr).collect();
    addrs.sort_unstable();
    addrs.dedup();
    for addr in addrs {
        let wanted: Vec<usize> = tx
            .outputs
            .iter()
            .enumerate()
            .filter(|(_, o)| o.addr == Some(addr))
            .map(|(i, _)| i)
            .collect();
        let mut left = wanted.len();
        let mut read = 0;
        let start = addr_tx_key(addr, tx.time_ms, &[0; 32]).to_vec();
        let end = addr_tx_key(addr, u64::MAX, &[0xff; 32]).to_vec();
        'slabs: for slab in store.slabs() {
            if slab.end_ms() <= tx.time_ms {
                continue;
            }
            for guard in slab.addr_tx.range(start.clone()..end.clone()) {
                let (key, _) = guard.into_inner()?;
                let Some((_, candidate)) = parse_addr_tx_key(&key) else {
                    continue;
                };
                if candidate == *txid {
                    continue;
                }
                read += 1;
                if read > SPEND_SCAN_MAX {
                    break 'slabs;
                }
                let Some(bytes) = slab.tx.get(candidate)? else {
                    continue;
                };
                let spend: IndexedTx = decode(&bytes)?;
                for input in &spend.inputs {
                    if input.prev_txid == *txid
                        && let Some(&i) = wanted.iter().find(|&&i| i == input.prev_index as usize)
                        && spenders[i].is_none()
                    {
                        spenders[i] = Some(hex(&candidate));
                        left -= 1;
                    }
                }
                if left == 0 {
                    break 'slabs;
                }
            }
        }
    }
    Ok(spenders)
}

/// The stored transaction `txid`, if the index has it.
pub fn transaction(store: &IndexStore, txid: &Hash32) -> Result<Option<TxDetail>> {
    for slab in store.slabs().into_iter().rev() {
        let Some(bytes) = slab.tx.get(txid)? else {
            continue;
        };
        let tx: IndexedTx = decode(&bytes)?;
        let mut names: HashMap<AddrId, Option<String>> = HashMap::new();
        let mut resolve = |id: Option<AddrId>| -> Result<Option<String>> {
            let Some(id) = id else { return Ok(None) };
            if let Some(name) = names.get(&id) {
                return Ok(name.clone());
            }
            let name = store.address_of(id)?;
            names.insert(id, name.clone());
            Ok(name)
        };
        let class = |code: u8| ScriptClass::from_code(code).map(|c| c.label());
        let mut inputs = Vec::with_capacity(tx.inputs.len());
        for input in &tx.inputs {
            inputs.push(TxInputDetail {
                address: resolve(input.addr)?,
                amount: input.amount,
                prev_txid: hex(&input.prev_txid),
                prev_index: input.prev_index,
                script_class: class(input.script_class),
            });
        }
        let mut outputs = Vec::with_capacity(tx.outputs.len());
        for output in &tx.outputs {
            outputs.push(TxOutputDetail {
                address: resolve(output.addr)?,
                amount: output.amount,
                change: output.change,
                script_class: class(output.script_class),
                covenant: output.covenant,
            });
        }
        return Ok(Some(TxDetail {
            txid: hex(txid),
            accepting_block: hex(&tx.accepting_block),
            daa_score: tx.daa_score,
            time_ms: tx.time_ms,
            block: (tx.block != [0; 32]).then(|| hex(&tx.block)),
            block_time_ms: tx.block_time_ms,
            inputs,
            outputs,
            fee: tx.fee,
            mass: tx.mass(),
            storage_mass: tx.storage_mass,
            compute_mass: tx.compute_mass,
            is_coinbase: tx.is_coinbase,
            protocol: tx.protocol,
            version: tx.version,
            lock_time: tx.lock_time,
            subnetwork: tx.subnetwork.label(),
            gas: tx.gas,
            payload_len: tx.payload_len,
            payload_head: tx.payload_head.clone(),
            opcodes: OpcodeUsage::from_bits(tx.opcodes).into(),
            covenant_created: tx.covenant_created,
            covenant_spent: tx.covenant_spent,
            sig_ops: tx.sig_ops,
        }));
    }
    Ok(None)
}

/// The `top` counterparties of `id` by volume, over every slab.
pub fn counterparties(store: &IndexStore, id: AddrId, top: usize) -> Result<Vec<Peer>> {
    let mut merged: HashMap<AddrId, PeerStats> = HashMap::new();
    for slab in store.slabs() {
        for guard in slab.peers.prefix(addr_key(id)) {
            let (key, value) = guard.into_inner()?;
            let Some(peer) = parse_peer_key(&key) else {
                continue;
            };
            let stats: PeerStats = decode(&value)?;
            merged.entry(peer).or_default().merge(&stats);
        }
    }
    let mut peers: Vec<(AddrId, PeerStats)> = merged.into_iter().collect();
    peers.sort_unstable_by_key(|(id, s)| (std::cmp::Reverse(s.in_amount + s.out_amount), *id));
    peers.truncate(top);
    peers
        .into_iter()
        .map(|(peer, stats)| {
            Ok(Peer {
                address: store.address_of(peer)?.unwrap_or_default(),
                id: peer,
                stats,
            })
        })
        .collect()
}

/// Balance changes of `id` since `from_ms`, oldest first, as `(time_ms, delta)`.
pub fn balance_deltas(store: &IndexStore, id: AddrId, from_ms: u64) -> Result<Vec<(u64, i64)>> {
    let mut deltas = Vec::new();
    for slab in store.slabs() {
        if slab.end_ms() <= from_ms {
            continue;
        }
        let start = addr_tx_key(id, from_ms, &[0; 32]);
        let end = addr_tx_key(id, u64::MAX, &[0xff; 32]);
        for guard in slab.addr_tx.range(start.to_vec()..end.to_vec()) {
            let (key, value) = guard.into_inner()?;
            if let (Some((time_ms, _)), Some(delta)) =
                (parse_addr_tx_key(&key), decode_delta(&value))
            {
                deltas.push((time_ms, delta));
            }
        }
    }
    Ok(deltas)
}

/// Absolute balances from deltas (oldest first) and the balance now: each point is the
/// balance right after its transaction. Without a known balance the curve starts at 0.
pub fn balance_curve(deltas: &[(u64, i64)], now_balance: Option<u64>) -> Vec<(u64, i64)> {
    let total: i64 = deltas.iter().map(|(_, d)| d).sum();
    let mut running = match now_balance {
        Some(b) => b as i64 - total,
        None => 0,
    };
    deltas
        .iter()
        .map(|&(t, d)| {
            running += d;
            (t, running)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_append_skips_rows_already_shown() {
        let row = |n: u32| n.to_string();
        let mut page = Page {
            items: vec![row(3), row(2)],
            next: Some(Cursor {
                time_ms: 2,
                txid: [0; 32],
            }),
        };
        page.append(
            Page {
                items: vec![row(2), row(1)],
                next: None,
            },
            |t| t.as_str(),
        );
        assert_eq!(page.items, vec![row(3), row(2), row(1)]);
        assert!(page.next.is_none());
    }

    #[test]
    fn spenders_finds_the_transaction_that_spent_each_output() {
        use std::sync::Arc;

        use crate::index::temp_store;
        use crate::index::writer::IndexWriter;
        use crate::index::writer::testing::{chain_block, hash, response, tx, with_outpoint};
        use crate::labels::LabelBook;

        let store = temp_store();
        let mut writer =
            IndexWriter::new(store.store.clone(), Arc::new(LabelBook::base())).unwrap();
        // The coinbase pays 1, 2 and 3; 1 spends its output a block later, 3 spends
        // its in the same block; 2's stays unspent.
        let mut t1 = tx(1, &[(1, 100)], &[(4, 90)]);
        with_outpoint(&mut t1, 0, 0, 0);
        let mut t3 = tx(3, &[(3, 50)], &[(5, 40)]);
        with_outpoint(&mut t3, 0, 0, 2);
        let r = response(
            vec![],
            vec![
                chain_block(
                    1,
                    1_000,
                    vec![tx(0, &[], &[(1, 100), (2, 70), (3, 50)]), t3],
                ),
                chain_block(2, 2_000, vec![t1]),
            ],
        );
        writer.apply(&r).unwrap();
        let id = |n: u64| hash(n).as_bytes();
        let found = spenders(&store.store, &id(0)).unwrap();
        assert_eq!(found, vec![Some(hex(&id(1))), None, Some(hex(&id(3)))]);
        // A transaction the index doesn't have has no outputs to speak of.
        assert!(spenders(&store.store, &id(9)).unwrap().is_empty());
    }

    #[test]
    fn cursor_roundtrip() {
        let c = Cursor {
            time_ms: 42,
            txid: [7; 32],
        };
        assert_eq!(c.to_string().parse::<Cursor>().unwrap(), c);
        assert!("nope".parse::<Cursor>().is_err());
    }

    fn node(id: AddrId, hop: u8) -> FlowNode {
        FlowNode {
            id,
            address: format!("kaspa:{id}"),
            volume: 0,
            hop,
        }
    }

    fn edge(from: AddrId, to: AddrId, amount: u64) -> FlowEdge {
        FlowEdge {
            from,
            to,
            amount,
            tx_count: 1,
            via: Vec::new(),
        }
    }

    #[test]
    fn collapse_folds_pass_through_chains_into_one_edge() {
        // Root 1 → 2 → 3 → 4, and 1 → 5 directly; 4 also pays 6 and 7, so it stays.
        let mut graph = FlowGraph {
            nodes: vec![
                node(1, 0),
                node(2, 1),
                node(3, 2),
                node(4, 3),
                node(5, 1),
                node(6, 4),
                node(7, 4),
            ],
            edges: vec![
                edge(1, 2, 100),
                edge(2, 3, 90),
                edge(3, 4, 80),
                edge(1, 5, 7),
                edge(4, 6, 40),
                edge(4, 7, 30),
            ],
        };
        graph.recount();
        let c = graph.collapse_chains();
        let ids: Vec<AddrId> = c.nodes.iter().map(|n| n.id).collect();
        assert_eq!(ids, vec![1, 4, 5, 6, 7]);
        let chain = c.edges.iter().find(|e| e.from == 1 && e.to == 4).unwrap();
        assert_eq!(chain.via, vec!["kaspa:2", "kaspa:3"]);
        assert_eq!((chain.hops(), chain.amount), (3, 80));
        assert_eq!(c.edges.len(), 4);
        assert!(c.edges.iter().all(|e| e.via.is_empty() || e.to == 4));
        // Volumes follow the collapsed edges.
        assert_eq!(c.nodes[0].volume, 87);

        // A root is never folded, even in the middle of a chain, and a two-way pair isn't a chain.
        let mut graph = FlowGraph {
            nodes: vec![node(1, 1), node(2, 0), node(3, 1), node(4, 2)],
            edges: vec![edge(1, 2, 5), edge(2, 3, 5), edge(3, 4, 5), edge(4, 3, 5)],
        };
        graph.recount();
        let c = graph.collapse_chains();
        assert_eq!(c.nodes.len(), 4);
        assert_eq!(c.edges.len(), 4);
        assert!(c.edges.iter().all(|e| e.via.is_empty()));
    }

    #[test]
    fn balance_curve_ends_at_the_known_balance() {
        let deltas = [(1, 100), (2, -30), (3, 10)];
        let curve = balance_curve(&deltas, Some(500));
        assert_eq!(curve, vec![(1, 520), (2, 490), (3, 500)]);
        assert_eq!(
            balance_curve(&deltas, None),
            vec![(1, 100), (2, 70), (3, 80)]
        );
    }
}
