//! Read side of the address index: profiles, transaction pages, counterparties and
//! balance history. Plain structs, serializable for the CLI. Synchronous; call inside
//! `spawn_blocking` from async code.

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};

use super::cluster;
use super::records::{
    AddrId, AddrStats, Hash32, IndexedTx, PeerStats, addr_key, addr_tx_key, decode, decode_delta,
    parse_addr_tx_key, parse_peer_key,
};
use super::{IndexStore, hex, parse_hex};
use crate::labels::LabelBook;
use crate::tx_inspect::TransactionProtocol;

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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TxOutputDetail {
    pub address: Option<String>,
    pub amount: u64,
    /// Likelihood this is the sender's change, 0–100.
    pub change: u8,
}

/// A stored transaction with its addresses resolved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TxDetail {
    pub txid: String,
    pub accepting_block: String,
    pub daa_score: u64,
    pub time_ms: u64,
    pub inputs: Vec<TxInputDetail>,
    pub outputs: Vec<TxOutputDetail>,
    pub fee: Option<u64>,
    pub mass: u64,
    pub is_coinbase: bool,
    pub protocol: Option<TransactionProtocol>,
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
    pub amount: u64,
    pub tx_count: u64,
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
    let (stats, cluster_size) = match id {
        Some(id) => (
            stats(store, id)?,
            cluster::size_of(store.clusters(), cluster::root_of(store.clusters(), id)?)?,
        ),
        None => (AddrStats::default(), 0),
    };
    Ok(AddressProfile {
        address: address.to_string(),
        id,
        stats,
        coverage: store.coverage(),
        cluster_size,
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
        let mut inputs = Vec::with_capacity(tx.inputs.len());
        for input in &tx.inputs {
            inputs.push(TxInputDetail {
                address: resolve(input.addr)?,
                amount: input.amount,
                prev_txid: hex(&input.prev_txid),
                prev_index: input.prev_index,
            });
        }
        let mut outputs = Vec::with_capacity(tx.outputs.len());
        for output in &tx.outputs {
            outputs.push(TxOutputDetail {
                address: resolve(output.addr)?,
                amount: output.amount,
                change: output.change,
            });
        }
        return Ok(Some(TxDetail {
            txid: hex(txid),
            accepting_block: hex(&tx.accepting_block),
            daa_score: tx.daa_score,
            time_ms: tx.time_ms,
            inputs,
            outputs,
            fee: tx.fee,
            mass: tx.mass,
            is_coinbase: tx.is_coinbase,
            protocol: tx.protocol,
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
    fn cursor_roundtrip() {
        let c = Cursor {
            time_ms: 42,
            txid: [7; 32],
        };
        assert_eq!(c.to_string().parse::<Cursor>().unwrap(), c);
        assert!("nope".parse::<Cursor>().is_err());
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
