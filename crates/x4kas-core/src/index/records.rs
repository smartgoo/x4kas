//! What the address index stores, and how keys and values are laid out.
//!
//! Addresses are interned to `u32` ids so every key is small and fixed-width. Keys are
//! big-endian so byte order is numeric order: an address's transactions sort by time
//! under their `addr_id` prefix, and a reverse prefix scan walks them newest first.

use bincode::Options;
use serde::{Deserialize, Serialize};

use crate::tx_inspect::TransactionProtocol;

/// Value encoding: bincode with variable-length integers, since amounts, scores and
/// counts are mostly small.
pub fn encode<T: Serialize>(value: &T) -> anyhow::Result<Vec<u8>> {
    Ok(bincode::DefaultOptions::new()
        .with_varint_encoding()
        .serialize(value)?)
}

pub fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> anyhow::Result<T> {
    Ok(bincode::DefaultOptions::new()
        .with_varint_encoding()
        .deserialize(bytes)?)
}

/// Interned address id.
pub type AddrId = u32;
/// Transaction id (or block hash) bytes.
pub type Hash32 = [u8; 32];

/// Duration of one time slab: every data keyspace is partitioned by slab, so pruning is a
/// keyspace drop and a 30h retention window spans at most six slabs.
pub const SLAB_MS: u64 = 6 * 60 * 60 * 1000;

/// The slab a timestamp belongs to.
pub fn slab_of(time_ms: u64) -> u64 {
    time_ms / SLAB_MS
}

/// One accepted transaction, as stored under its id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexedTx {
    pub accepting_block: Hash32,
    pub daa_score: u64,
    /// The accepting chain block's timestamp, in unix ms.
    pub time_ms: u64,
    pub inputs: Vec<TxInput>,
    pub outputs: Vec<TxOutput>,
    /// `None` when a spent UTXO's amount wasn't resolved.
    pub fee: Option<u64>,
    pub mass: u64,
    pub is_coinbase: bool,
    pub protocol: Option<TransactionProtocol>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TxInput {
    /// `None` when the spent UTXO's address isn't known (unresolved or non-standard).
    pub addr: Option<AddrId>,
    pub amount: Option<u64>,
    pub prev_txid: Hash32,
    pub prev_index: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TxOutput {
    pub addr: Option<AddrId>,
    pub amount: u64,
    /// How likely this output is the sender's change, 0–100 (see `cluster::change_scores`).
    pub change: u8,
}

impl IndexedTx {
    /// Sompi sent from `addr` in this transaction.
    pub fn sent_by(&self, addr: AddrId) -> u64 {
        self.inputs
            .iter()
            .filter(|i| i.addr == Some(addr))
            .filter_map(|i| i.amount)
            .sum()
    }

    /// Sompi received by `addr` in this transaction.
    pub fn received_by(&self, addr: AddrId) -> u64 {
        self.outputs
            .iter()
            .filter(|o| o.addr == Some(addr))
            .map(|o| o.amount)
            .sum()
    }

    /// Every address touched, each once.
    pub fn addresses(&self) -> Vec<AddrId> {
        let mut ids: Vec<AddrId> = self
            .inputs
            .iter()
            .filter_map(|i| i.addr)
            .chain(self.outputs.iter().filter_map(|o| o.addr))
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }
}

/// An address's activity within one slab. Summed over slabs for the profile, so dropping
/// a slab removes exactly its contribution.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddrStats {
    pub first_seen_ms: u64,
    pub last_seen_ms: u64,
    pub tx_count: u64,
    pub received: u64,
    pub sent: u64,
}

impl AddrStats {
    pub fn merge(&mut self, other: &AddrStats) {
        if other.tx_count == 0 {
            return;
        }
        if self.tx_count == 0 || other.first_seen_ms < self.first_seen_ms {
            self.first_seen_ms = other.first_seen_ms;
        }
        self.last_seen_ms = self.last_seen_ms.max(other.last_seen_ms);
        self.tx_count += other.tx_count;
        self.received += other.received;
        self.sent += other.sent;
    }

    /// Record one transaction. `undo` reverses a previous `apply` for a reorg: counts and
    /// sums are exact, first/last seen stay as they were (they're bounds, not sums).
    pub fn apply(&mut self, time_ms: u64, received: u64, sent: u64, undo: bool) {
        if undo {
            self.tx_count = self.tx_count.saturating_sub(1);
            self.received = self.received.saturating_sub(received);
            self.sent = self.sent.saturating_sub(sent);
            return;
        }
        if self.tx_count == 0 || time_ms < self.first_seen_ms {
            self.first_seen_ms = time_ms;
        }
        self.last_seen_ms = self.last_seen_ms.max(time_ms);
        self.tx_count += 1;
        self.received += received;
        self.sent += sent;
    }
}

/// Flows between an address and one counterparty within a slab, from the address's
/// point of view.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerStats {
    /// Sompi received from the peer.
    pub in_amount: u64,
    /// Sompi sent to the peer.
    pub out_amount: u64,
    pub tx_count: u64,
}

impl PeerStats {
    pub fn merge(&mut self, other: &PeerStats) {
        self.in_amount += other.in_amount;
        self.out_amount += other.out_amount;
        self.tx_count += other.tx_count;
    }

    pub fn apply(&mut self, in_amount: u64, out_amount: u64, undo: bool) {
        if undo {
            self.in_amount = self.in_amount.saturating_sub(in_amount);
            self.out_amount = self.out_amount.saturating_sub(out_amount);
            self.tx_count = self.tx_count.saturating_sub(1);
        } else {
            self.in_amount += in_amount;
            self.out_amount += out_amount;
            self.tx_count += 1;
        }
    }
}

// --- Keys ---

/// Key of an address's transaction entry: `addr_id ‖ time_ms ‖ txid`.
pub fn addr_tx_key(addr: AddrId, time_ms: u64, txid: &Hash32) -> [u8; 44] {
    let mut key = [0u8; 44];
    key[..4].copy_from_slice(&addr.to_be_bytes());
    key[4..12].copy_from_slice(&time_ms.to_be_bytes());
    key[12..].copy_from_slice(txid);
    key
}

/// The `(time_ms, txid)` of an [`addr_tx_key`].
pub fn parse_addr_tx_key(key: &[u8]) -> Option<(u64, Hash32)> {
    if key.len() != 44 {
        return None;
    }
    let time_ms = u64::from_be_bytes(key[4..12].try_into().ok()?);
    let txid: Hash32 = key[12..].try_into().ok()?;
    Some((time_ms, txid))
}

/// Key of a peer entry: `addr_id ‖ peer_id`.
pub fn peer_key(addr: AddrId, peer: AddrId) -> [u8; 8] {
    let mut key = [0u8; 8];
    key[..4].copy_from_slice(&addr.to_be_bytes());
    key[4..].copy_from_slice(&peer.to_be_bytes());
    key
}

pub fn parse_peer_key(key: &[u8]) -> Option<AddrId> {
    Some(AddrId::from_be_bytes(key.get(4..8)?.try_into().ok()?))
}

/// Key of a block's accepted transaction: `block_hash ‖ txid`.
pub fn block_tx_key(block: &Hash32, txid: &Hash32) -> [u8; 64] {
    let mut key = [0u8; 64];
    key[..32].copy_from_slice(block);
    key[32..].copy_from_slice(txid);
    key
}

pub fn addr_key(addr: AddrId) -> [u8; 4] {
    addr.to_be_bytes()
}

pub fn parse_addr_key(key: &[u8]) -> Option<AddrId> {
    Some(AddrId::from_be_bytes(key.get(..4)?.try_into().ok()?))
}

/// The amount an address's balance changed by in a transaction, stored with its
/// [`addr_tx_key`] so balance history needs no transaction lookups.
pub fn encode_delta(delta: i64) -> [u8; 8] {
    delta.to_be_bytes()
}

pub fn decode_delta(value: &[u8]) -> Option<i64> {
    Some(i64::from_be_bytes(value.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addr_tx_keys_sort_by_time_under_the_address() {
        let a = addr_tx_key(7, 100, &[1; 32]);
        let b = addr_tx_key(7, 200, &[0; 32]);
        let c = addr_tx_key(8, 0, &[0; 32]);
        assert!(a < b && b < c);
        assert_eq!(parse_addr_tx_key(&b), Some((200, [0; 32])));
        assert_eq!(parse_addr_tx_key(&b[..10]), None);
    }

    #[test]
    fn stats_apply_and_undo() {
        let mut s = AddrStats::default();
        s.apply(50, 10, 0, false);
        s.apply(20, 0, 5, false);
        assert_eq!((s.first_seen_ms, s.last_seen_ms, s.tx_count), (20, 50, 2));
        s.apply(20, 0, 5, true);
        assert_eq!((s.tx_count, s.received, s.sent), (1, 10, 0));

        let mut total = AddrStats::default();
        total.merge(&s);
        total.merge(&AddrStats {
            first_seen_ms: 5,
            last_seen_ms: 60,
            tx_count: 1,
            received: 1,
            sent: 1,
        });
        assert_eq!((total.first_seen_ms, total.last_seen_ms), (5, 60));
        total.merge(&AddrStats::default());
        assert_eq!(total.first_seen_ms, 5);
    }

    #[test]
    fn slabs_are_six_hours() {
        assert_eq!(slab_of(0), 0);
        assert_eq!(slab_of(SLAB_MS - 1), 0);
        assert_eq!(slab_of(SLAB_MS), 1);
    }
}
