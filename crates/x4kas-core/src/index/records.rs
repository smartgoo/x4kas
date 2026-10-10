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

/// How many bytes of a transaction's payload are kept (`IndexedTx::payload_head`):
/// enough for a protocol marker or a short message, not an inscription's body.
pub const PAYLOAD_HEAD: usize = 128;

/// `TxInput::script_class`/`TxOutput::script_class` when the script wasn't in the
/// response (an unresolved input).
pub const SCRIPT_UNKNOWN: u8 = 0xff;

/// One accepted transaction, as stored under its id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexedTx {
    pub accepting_block: Hash32,
    pub daa_score: u64,
    /// The accepting chain block's timestamp, in unix ms.
    pub time_ms: u64,
    /// The block the transaction is in (a merged block, or the chain block itself);
    /// zero when the node didn't say.
    pub block: Hash32,
    /// `block`'s timestamp, in unix ms (the accepting block's when the node didn't say).
    pub block_time_ms: u64,
    pub inputs: Vec<TxInput>,
    pub outputs: Vec<TxOutput>,
    /// `None` when a spent UTXO's amount wasn't resolved.
    pub fee: Option<u64>,
    pub storage_mass: u64,
    pub compute_mass: u64,
    pub is_coinbase: bool,
    pub protocol: Option<TransactionProtocol>,
    /// Transaction version, lock time, subnetwork and gas (Full verbosity; `None` or
    /// `Unknown` when the node didn't send them).
    pub version: Option<u16>,
    pub lock_time: Option<u64>,
    pub subnetwork: Subnetwork,
    pub gas: Option<u64>,
    pub payload_len: u32,
    /// The first [`PAYLOAD_HEAD`] bytes of the payload.
    pub payload_head: Vec<u8>,
    /// Covenant-era opcodes used by the outputs' scripts and the redeem scripts the
    /// inputs revealed (`tx_inspect::OpcodeUsage::to_bits`).
    pub opcodes: u8,
    /// Outputs that create a covenant.
    pub covenant_created: u16,
    /// Inputs that spend a covenant output (a lower bound: only resolved inputs count).
    pub covenant_spent: u16,
    /// Signature operations over every input.
    pub sig_ops: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TxInput {
    /// `None` when the spent UTXO's address isn't known (unresolved or non-standard).
    pub addr: Option<AddrId>,
    pub amount: Option<u64>,
    pub prev_txid: Hash32,
    pub prev_index: u32,
    /// The spent output's script class (`tx_inspect::ScriptClass::code`), or
    /// [`SCRIPT_UNKNOWN`].
    pub script_class: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TxOutput {
    pub addr: Option<AddrId>,
    pub amount: u64,
    /// How likely this output is the sender's change, 0–100 (see `cluster::change_scores`).
    pub change: u8,
    /// The script's class (`tx_inspect::ScriptClass::code`), or [`SCRIPT_UNKNOWN`].
    pub script_class: u8,
    /// The output creates a covenant.
    pub covenant: bool,
}

/// A transaction's subnetwork, as far as the index cares: native, the coinbase
/// subnetwork, or something else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Subnetwork {
    /// The node didn't send it (a response below Full verbosity).
    #[default]
    Unknown,
    Native,
    Coinbase,
    Other([u8; 20]),
}

impl Subnetwork {
    /// From the 20 id bytes: all zero is native, `[1, 0, …]` is the coinbase subnetwork.
    pub fn from_bytes(id: &[u8]) -> Self {
        let Ok(bytes) = <[u8; 20]>::try_from(id) else {
            return Self::Unknown;
        };
        if bytes.iter().all(|&b| b == 0) {
            Self::Native
        } else if bytes[0] == 1 && bytes[1..].iter().all(|&b| b == 0) {
            Self::Coinbase
        } else {
            Self::Other(bytes)
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Native => "native",
            Self::Coinbase => "coinbase",
            Self::Other(_) => "other",
        }
    }
}

/// `TxSummary::flags` bits.
pub const SUMMARY_COINBASE: u8 = 1;
pub const SUMMARY_HAS_PAYLOAD: u8 = 2;
/// Every output pays an input address back (nothing leaves the sender).
pub const SUMMARY_SELF_TRANSFER: u8 = 4;

/// What the time-ordered transaction index (`ttx_<n>`) stores with each entry: enough
/// to filter and sort on the scalar properties of a transaction while scanning
/// sequentially, so the full record is read only for the matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TxSummary {
    pub daa_score: u64,
    pub fee: Option<u64>,
    pub mass: u64,
    pub input_total: Option<u64>,
    pub output_total: u64,
    pub max_output: u64,
    pub min_output: u64,
    pub inputs: u16,
    pub outputs: u16,
    pub flags: u8,
    /// `TransactionProtocol::code`, or 0.
    pub protocol: u8,
    pub opcodes: u8,
    pub payload_len: u32,
}

impl IndexedTx {
    /// A transaction with nothing in it, accepted by `accepting_block`; the block it is
    /// in is unknown.
    pub fn empty(accepting_block: Hash32, daa_score: u64, time_ms: u64) -> Self {
        Self {
            accepting_block,
            daa_score,
            time_ms,
            block: [0; 32],
            block_time_ms: time_ms,
            inputs: Vec::new(),
            outputs: Vec::new(),
            fee: None,
            storage_mass: 0,
            compute_mass: 0,
            is_coinbase: false,
            protocol: None,
            version: None,
            lock_time: None,
            subnetwork: Subnetwork::Unknown,
            gas: None,
            payload_len: 0,
            payload_head: Vec::new(),
            opcodes: 0,
            covenant_created: 0,
            covenant_spent: 0,
            sig_ops: 0,
        }
    }

    /// The mass the node charged: the larger of the storage and compute masses.
    pub fn mass(&self) -> u64 {
        self.storage_mass.max(self.compute_mass)
    }

    /// Sompi over the inputs, when every input's amount is known.
    pub fn input_total(&self) -> Option<u64> {
        self.inputs.iter().map(|i| i.amount).sum()
    }

    /// Sompi over the outputs.
    pub fn output_total(&self) -> u64 {
        self.outputs.iter().map(|o| o.amount).sum()
    }

    /// Every output pays an address that also signed an input (and there is one).
    pub fn self_transfer(&self) -> bool {
        if self.is_coinbase || self.outputs.is_empty() {
            return false;
        }
        self.outputs.iter().all(|o| {
            o.addr
                .is_some_and(|addr| self.inputs.iter().any(|i| i.addr == Some(addr)))
        })
    }

    pub fn summary(&self) -> TxSummary {
        let mut flags = 0;
        if self.is_coinbase {
            flags |= SUMMARY_COINBASE;
        }
        if self.payload_len > 0 {
            flags |= SUMMARY_HAS_PAYLOAD;
        }
        if self.self_transfer() {
            flags |= SUMMARY_SELF_TRANSFER;
        }
        TxSummary {
            daa_score: self.daa_score,
            fee: self.fee,
            mass: self.mass(),
            input_total: self.input_total(),
            output_total: self.output_total(),
            max_output: self.outputs.iter().map(|o| o.amount).max().unwrap_or(0),
            min_output: self.outputs.iter().map(|o| o.amount).min().unwrap_or(0),
            inputs: self.inputs.len().min(u16::MAX as usize) as u16,
            outputs: self.outputs.len().min(u16::MAX as usize) as u16,
            flags,
            protocol: self.protocol.map(|p| p.code()).unwrap_or(0),
            opcodes: self.opcodes,
            payload_len: self.payload_len,
        }
    }

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

/// What kind of block a [`BlockRecord`] describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlockKind {
    /// A selected-chain block: the stream brings its header, and its own coinbase one
    /// chain block later (under its child).
    Chain,
    /// A block a chain block merged: known only through the accepted transactions it
    /// holds (no header, no miner; red blocks' transactions are accepted too).
    Merged,
}

/// One output of a chain block's coinbase: the reward of one mergeset blue block, paid
/// to that block's miner (the last output may instead pay the reds' rewards to the
/// chain block's own miner).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Payout {
    pub addr: Option<AddrId>,
    pub amount: u64,
}

/// A block, as stored under its hash in `blk_<n>` (the slab of its own timestamp).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockRecord {
    pub kind: BlockKind,
    pub time_ms: u64,
    /// The chain block that merged it (itself for a chain block).
    pub merging_block: Hash32,

    // --- Header: chain blocks only ---
    pub version: Option<u16>,
    pub daa_score: Option<u64>,
    pub blue_score: Option<u64>,
    /// Big-endian.
    pub blue_work: Option<[u8; 24]>,
    pub bits: Option<u32>,
    pub nonce: Option<u64>,
    /// Direct (level 0) parents.
    pub parents: Vec<Hash32>,
    pub parent_levels: u8,
    pub hash_merkle_root: Option<Hash32>,
    pub accepted_id_merkle_root: Option<Hash32>,
    pub utxo_commitment: Option<Hash32>,
    pub pruning_point: Option<Hash32>,

    // --- Own coinbase: chain blocks only, written when the next chain block arrives ---
    pub coinbase_txid: Option<Hash32>,
    /// From the coinbase payload's script: who mined this block.
    pub miner: Option<AddrId>,
    pub miner_tag: Option<String>,
    pub node_version: Option<String>,
    pub subsidy: Option<u64>,
    pub payouts: Vec<Payout>,

    // --- Accepted transactions contained in this block (both kinds) ---
    pub accepted_txs: u32,
    pub accepted_mass: u64,
    pub accepted_fees: u64,
}

impl BlockRecord {
    /// An empty record of `kind` at `time_ms`, merged by `merging_block`.
    pub fn new(kind: BlockKind, time_ms: u64, merging_block: Hash32) -> Self {
        Self {
            kind,
            time_ms,
            merging_block,
            version: None,
            daa_score: None,
            blue_score: None,
            blue_work: None,
            bits: None,
            nonce: None,
            parents: Vec::new(),
            parent_levels: 0,
            hash_merkle_root: None,
            accepted_id_merkle_root: None,
            utxo_commitment: None,
            pruning_point: None,
            coinbase_txid: None,
            miner: None,
            miner_tag: None,
            node_version: None,
            subsidy: None,
            payouts: Vec::new(),
            accepted_txs: 0,
            accepted_mass: 0,
            accepted_fees: 0,
        }
    }

    /// Sompi over the coinbase's outputs.
    pub fn payout_total(&self) -> u64 {
        self.payouts.iter().map(|p| p.amount).sum()
    }

    /// Nothing is known about the block beyond its existence: a merged record whose
    /// transactions were all undone.
    pub fn is_empty(&self) -> bool {
        self.kind == BlockKind::Merged && self.accepted_txs == 0
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

/// Flows between an address and one counterparty, from the address's point of view: the
/// sum of its [`PeerDelta`]s.
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

    /// The stats a summed delta amounts to (a reorg undone before its transaction was
    /// ever indexed can leave a negative sum, which counts as nothing).
    pub fn from_delta(delta: &PeerDelta) -> Self {
        Self {
            in_amount: delta.in_amount.max(0) as u64,
            out_amount: delta.out_amount.max(0) as u64,
            tx_count: delta.tx_count.max(0) as u64,
        }
    }
}

/// What one commit changed about the flows between an address and a peer, stored under
/// [`peer_delta_key`]: the writer never reads a counterparty record back, it appends a
/// delta (an LSM insert), and readers sum the deltas of a pair. Signed, so a reorg's
/// undo is a delta too.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerDelta {
    pub in_amount: i64,
    pub out_amount: i64,
    pub tx_count: i64,
}

impl PeerDelta {
    pub fn merge(&mut self, other: &PeerDelta) {
        self.in_amount += other.in_amount;
        self.out_amount += other.out_amount;
        self.tx_count += other.tx_count;
    }

    pub fn apply(&mut self, in_amount: u64, out_amount: u64, undo: bool) {
        let sign = if undo { -1 } else { 1 };
        self.in_amount += sign * in_amount as i64;
        self.out_amount += sign * out_amount as i64;
        self.tx_count += sign;
    }

    pub fn is_zero(&self) -> bool {
        *self == Self::default()
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

/// Key of a protocol's transaction entry: `protocol code ‖ time_ms ‖ txid`
/// ([`TransactionProtocol::code`]), so a protocol's transactions sort by time under
/// its one-byte prefix like an address's do.
pub fn protocol_tx_key(protocol: TransactionProtocol, time_ms: u64, txid: &Hash32) -> [u8; 41] {
    let mut key = [0u8; 41];
    key[0] = protocol.code();
    key[1..9].copy_from_slice(&time_ms.to_be_bytes());
    key[9..].copy_from_slice(txid);
    key
}

/// The `(time_ms, txid)` of a [`protocol_tx_key`].
pub fn parse_protocol_tx_key(key: &[u8]) -> Option<(u64, Hash32)> {
    if key.len() != 41 {
        return None;
    }
    let time_ms = u64::from_be_bytes(key[1..9].try_into().ok()?);
    let txid: Hash32 = key[9..].try_into().ok()?;
    Some((time_ms, txid))
}

/// Key of a time-ordered entry: `time_ms ‖ hash`, the transactions of a slab by time
/// (`ttx_<n>`, with a [`TxSummary`] value) and its blocks by time (`tbk_<n>`).
pub fn time_key(time_ms: u64, hash: &Hash32) -> [u8; 40] {
    let mut key = [0u8; 40];
    key[..8].copy_from_slice(&time_ms.to_be_bytes());
    key[8..].copy_from_slice(hash);
    key
}

/// The `(time_ms, hash)` of a [`time_key`].
pub fn parse_time_key(key: &[u8]) -> Option<(u64, Hash32)> {
    if key.len() != 40 {
        return None;
    }
    let time_ms = u64::from_be_bytes(key[..8].try_into().ok()?);
    let hash: Hash32 = key[8..].try_into().ok()?;
    Some((time_ms, hash))
}

/// Key of a pair entry: `addr_id ‖ peer_id` (a cluster's members, `cl_member`).
pub fn peer_key(addr: AddrId, peer: AddrId) -> [u8; 8] {
    let mut key = [0u8; 8];
    key[..4].copy_from_slice(&addr.to_be_bytes());
    key[4..].copy_from_slice(&peer.to_be_bytes());
    key
}

/// Key of a counterparty delta (`apr_<n>`): `addr_id ‖ peer_id ‖ seq`, the commit's
/// [`crate::index::Manifest::seq`], so an address's deltas sort by peer under its prefix
/// and every commit's delta for a pair is its own entry.
pub fn peer_delta_key(addr: AddrId, peer: AddrId, seq: u64) -> [u8; 16] {
    let mut key = [0u8; 16];
    key[..8].copy_from_slice(&peer_key(addr, peer));
    key[8..].copy_from_slice(&seq.to_be_bytes());
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
    fn protocol_tx_keys_sort_by_time_under_the_protocol() {
        let a = protocol_tx_key(TransactionProtocol::Krc, 100, &[1; 32]);
        let b = protocol_tx_key(TransactionProtocol::Krc, 200, &[0; 32]);
        let c = protocol_tx_key(TransactionProtocol::Kns, 0, &[0; 32]);
        assert!(a < b && b < c);
        assert_eq!(parse_protocol_tx_key(&b), Some((200, [0; 32])));
        assert_eq!(parse_protocol_tx_key(&b[..10]), None);
    }

    #[test]
    fn time_keys_sort_by_time() {
        let a = time_key(100, &[9; 32]);
        let b = time_key(200, &[0; 32]);
        assert!(a < b);
        assert_eq!(parse_time_key(&b), Some((200, [0; 32])));
        assert_eq!(parse_time_key(&b[..10]), None);
    }

    fn sample_tx() -> IndexedTx {
        IndexedTx {
            accepting_block: [1; 32],
            daa_score: 7,
            time_ms: 1_000,
            block: [2; 32],
            block_time_ms: 900,
            inputs: vec![TxInput {
                addr: Some(1),
                amount: Some(100),
                prev_txid: [0; 32],
                prev_index: 0,
                script_class: 0,
            }],
            outputs: vec![
                TxOutput {
                    addr: Some(2),
                    amount: 60,
                    change: 0,
                    script_class: 0,
                    covenant: false,
                },
                TxOutput {
                    addr: Some(1),
                    amount: 39,
                    change: 90,
                    script_class: 0,
                    covenant: false,
                },
            ],
            fee: Some(1),
            storage_mass: 10,
            compute_mass: 20,
            is_coinbase: false,
            protocol: Some(TransactionProtocol::Kns),
            version: Some(0),
            lock_time: Some(0),
            subnetwork: Subnetwork::Native,
            gas: Some(0),
            payload_len: 3,
            payload_head: b"kns".to_vec(),
            opcodes: 0b101,
            covenant_created: 0,
            covenant_spent: 0,
            sig_ops: 1,
        }
    }

    #[test]
    fn tx_summary_matches_record() {
        let tx = sample_tx();
        let s = tx.summary();
        assert_eq!(tx.mass(), 20);
        assert_eq!((s.mass, s.fee, s.daa_score), (20, Some(1), 7));
        assert_eq!((s.input_total, s.output_total), (Some(100), 99));
        assert_eq!((s.max_output, s.min_output), (60, 39));
        assert_eq!((s.inputs, s.outputs), (1, 2));
        assert_eq!(s.protocol, TransactionProtocol::Kns.code());
        assert_eq!((s.opcodes, s.payload_len), (0b101, 3));
        assert_eq!(s.flags, SUMMARY_HAS_PAYLOAD);

        // Everything back to the sender is a self transfer; a coinbase never is.
        let mut tx = sample_tx();
        tx.outputs[0].addr = Some(1);
        assert!(tx.self_transfer());
        assert_eq!(
            tx.summary().flags & SUMMARY_SELF_TRANSFER,
            SUMMARY_SELF_TRANSFER
        );
        tx.is_coinbase = true;
        tx.inputs.clear();
        assert!(!tx.self_transfer());
        assert_eq!(tx.summary().flags & SUMMARY_COINBASE, SUMMARY_COINBASE);
        tx.inputs.push(TxInput {
            addr: None,
            amount: None,
            prev_txid: [0; 32],
            prev_index: 0,
            script_class: SCRIPT_UNKNOWN,
        });
        assert_eq!(tx.input_total(), None);
    }

    #[test]
    fn subnetwork_from_bytes() {
        assert_eq!(Subnetwork::from_bytes(&[0; 20]), Subnetwork::Native);
        let mut coinbase = [0u8; 20];
        coinbase[0] = 1;
        assert_eq!(Subnetwork::from_bytes(&coinbase), Subnetwork::Coinbase);
        assert_eq!(Subnetwork::from_bytes(&[7; 20]), Subnetwork::Other([7; 20]));
        assert_eq!(Subnetwork::from_bytes(&[0; 3]), Subnetwork::Unknown);
        assert_eq!(Subnetwork::Coinbase.label(), "coinbase");
    }

    #[test]
    fn block_record_defaults_and_payout_total() {
        let mut b = BlockRecord::new(BlockKind::Merged, 5, [1; 32]);
        assert!(b.is_empty());
        b.accepted_txs = 1;
        assert!(!b.is_empty());
        let mut c = BlockRecord::new(BlockKind::Chain, 5, [1; 32]);
        assert!(!c.is_empty());
        c.payouts = vec![
            Payout {
                addr: Some(1),
                amount: 10,
            },
            Payout {
                addr: None,
                amount: 5,
            },
        ];
        assert_eq!(c.payout_total(), 15);
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
    fn peer_deltas_sum_and_clamp() {
        let mut d = PeerDelta::default();
        d.apply(10, 0, false);
        d.apply(0, 5, false);
        assert_eq!((d.in_amount, d.out_amount, d.tx_count), (10, 5, 2));
        d.apply(10, 0, true);
        d.merge(&PeerDelta {
            in_amount: -3,
            out_amount: 1,
            tx_count: -1,
        });
        assert_eq!((d.in_amount, d.out_amount, d.tx_count), (-3, 6, 0));
        let s = PeerStats::from_delta(&d);
        assert_eq!((s.in_amount, s.out_amount, s.tx_count), (0, 6, 0));
        assert!(!d.is_zero());
        assert!(PeerDelta::default().is_zero());

        let a = peer_delta_key(7, 9, 1);
        let b = peer_delta_key(7, 9, 2);
        let c = peer_delta_key(7, 10, 0);
        assert!(a < b && b < c);
        assert_eq!(parse_peer_key(&c), Some(10));
    }

    #[test]
    fn slabs_are_six_hours() {
        assert_eq!(slab_of(0), 0);
        assert_eq!(slab_of(SLAB_MS - 1), 0);
        assert_eq!(slab_of(SLAB_MS), 1);
    }
}
