//! What can be asked about each entity: every field with its name in the text form,
//! its kind (which decides the operators and the literals it takes), what it costs to
//! evaluate, and a line of help. One table drives validation, the builder's combos, the
//! parser's suggestions and the CLI's `query fields`.

use super::{Entity, Op};
use crate::config::{IndexFeature, IndexSettings};

/// A field of one entity. The variants are prefixed by entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FieldId {
    // --- Transactions ---
    TxTxid,
    TxTime,
    TxDaaScore,
    TxAcceptingBlock,
    TxBlock,
    TxBlockTime,
    TxIsCoinbase,
    TxProtocol,
    TxFee,
    TxFeeRate,
    TxMass,
    TxStorageMass,
    TxComputeMass,
    TxInputCount,
    TxOutputCount,
    TxInputTotal,
    TxOutputTotal,
    TxMaxOutput,
    TxMinOutput,
    TxAddress,
    TxSender,
    TxReceiver,
    TxLabel,
    TxSenderLabel,
    TxReceiverLabel,
    TxChangeMax,
    TxPayloadLen,
    TxPayload,
    TxRedeemScript,
    TxIntrospection,
    TxSeqcommit,
    TxZk,
    TxCovenantCreated,
    TxCovenantSpent,
    TxSigOps,
    TxVersion,
    TxLockTime,
    TxSubnetwork,
    TxGas,
    TxOutputScriptClass,
    TxInputScriptClass,
    TxSelfTransfer,
    TxDistinctAddresses,
    TxIsWatched,
    // --- Blocks ---
    BlockHash,
    BlockTime,
    BlockIsChain,
    BlockMergingBlock,
    BlockDaaScore,
    BlockBlueScore,
    BlockBlueWork,
    BlockBits,
    BlockDifficulty,
    BlockNonce,
    BlockVersion,
    BlockParentCount,
    BlockParents,
    BlockHashMerkleRoot,
    BlockAcceptedIdMerkleRoot,
    BlockUtxoCommitment,
    BlockPruningPoint,
    BlockMiner,
    BlockMinerLabel,
    BlockMinerTag,
    BlockNodeVersion,
    BlockSubsidy,
    BlockReward,
    BlockAcceptedTxs,
    BlockAcceptedMass,
    BlockAcceptedFees,
    BlockMergedBlues,
    BlockPayoutTotal,
    BlockPays,
    // --- Payouts ---
    PayoutTime,
    PayoutBlock,
    PayoutIndex,
    PayoutMiner,
    PayoutMinerLabel,
    PayoutAmount,
    // --- Addresses ---
    AddrAddress,
    AddrLabel,
    AddrLabelSource,
    AddrCategory,
    AddrType,
    AddrFirstSeen,
    AddrLastSeen,
    AddrTxCount,
    AddrReceived,
    AddrSent,
    AddrNet,
    AddrClusterSize,
    AddrClusterLabel,
    AddrPeerCount,
    AddrIsWatched,
    AddrBalance,
}

/// What a field's values are, which decides its operators and literals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    Hash,
    Address,
    Text,
    Int,
    /// Sompi; written with a unit in the text form (`1.5 KAS`, `100 sompi`).
    Amount,
    Float,
    /// Unix milliseconds; written as a UTC time or, relative to now, a duration.
    Time,
    Bool,
    /// One of a fixed set of lowercase names.
    Enum(&'static [&'static str]),
    /// Several hashes (`contains` one of them).
    HashList,
    /// Several addresses (`contains` one of them).
    AddressList,
}

const SCALAR_OPS: &[Op] = &[
    Op::Eq,
    Op::Ne,
    Op::Lt,
    Op::Le,
    Op::Gt,
    Op::Ge,
    Op::Between,
    Op::In,
    Op::NotIn,
    Op::IsNull,
    Op::IsNotNull,
];
const ID_OPS: &[Op] = &[Op::Eq, Op::Ne, Op::In, Op::NotIn, Op::IsNull, Op::IsNotNull];
const TEXT_OPS: &[Op] = &[
    Op::Eq,
    Op::Ne,
    Op::Contains,
    Op::StartsWith,
    Op::In,
    Op::NotIn,
    Op::IsNull,
    Op::IsNotNull,
];
const BOOL_OPS: &[Op] = &[Op::Eq, Op::Ne];
const ENUM_OPS: &[Op] = &[Op::Eq, Op::Ne, Op::In, Op::NotIn, Op::IsNull, Op::IsNotNull];
const LIST_OPS: &[Op] = &[Op::Contains];

impl FieldKind {
    /// The operators a field of this kind takes, in the builder's order.
    pub fn operators(self) -> &'static [Op] {
        match self {
            Self::Hash | Self::Address => ID_OPS,
            Self::Int | Self::Amount | Self::Float | Self::Time => SCALAR_OPS,
            Self::Bool => BOOL_OPS,
            Self::Text => TEXT_OPS,
            Self::Enum(_) => ENUM_OPS,
            Self::HashList | Self::AddressList => LIST_OPS,
        }
    }

    pub fn allows(self, op: Op) -> bool {
        self.operators().contains(&op)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Hash => "a hash",
            Self::Address => "an address",
            Self::Text => "text",
            Self::Int => "a number",
            Self::Amount => "an amount",
            Self::Float => "a number",
            Self::Time => "a time",
            Self::Bool => "true or false",
            Self::Enum(_) => "a name",
            Self::HashList => "hashes",
            Self::AddressList => "addresses",
        }
    }

    /// The label with what the literal looks like, for `query fields` and error
    /// messages: a time field takes a UTC time or how long ago.
    pub fn describe(self) -> &'static str {
        match self {
            Self::Time => {
                "a time (2026-10-01T12:00Z) or how long ago (24h: time > 24h means within the last day)"
            }
            Self::Hash => "a hash (64 hex characters)",
            Self::Address => "an address (kaspa:… or kaspatest:…)",
            Self::Amount => "an amount (1.5 KAS or 100 sompi)",
            other => other.label(),
        }
    }

    pub fn is_list(self) -> bool {
        matches!(self, Self::HashList | Self::AddressList)
    }
}

/// How much answering a condition costs, so the cheap ones are tried first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Cost {
    /// From the time index's summary (`TxSummary`) or the row itself.
    Summary,
    /// Needs the stored record.
    Record,
    /// Needs the label book, the watchlist or another store lookup.
    Lookup,
    /// From the node, for the rows shown only: a column, never a condition.
    Node,
}

pub struct FieldSpec {
    pub id: FieldId,
    /// The name in the text form (snake case).
    pub name: &'static str,
    /// A readable name for the builder and column headers.
    pub label: &'static str,
    pub entity: Entity,
    pub kind: FieldKind,
    pub cost: Cost,
    /// The builder groups fields under these.
    pub category: &'static str,
    pub doc: &'static str,
    /// The field has several values per row (any of them matches `=`, `in`,
    /// `contains`; none of them `!=`, `not in`).
    pub multi: bool,
}

pub const PROTOCOLS: &[&str] = &["krc", "kns", "kasia", "kasplex", "ksocial", "igra"];
pub const SCRIPT_CLASSES: &[&str] = &["p2pk", "p2pk_ecdsa", "p2sh", "non_standard"];
pub const SUBNETWORKS: &[&str] = &["native", "coinbase", "other"];
pub const LABEL_SOURCES: &[&str] = &["user", "kas_fyi", "kaspa_org", "kns", "heuristic"];
pub const ADDRESS_TYPES: &[&str] = &["p2pk", "p2pk_ecdsa", "p2sh"];

macro_rules! field {
    ($id:ident, $name:literal, $label:literal, $entity:ident, $kind:expr, $cost:ident, $cat:literal, $doc:literal) => {
        FieldSpec {
            id: FieldId::$id,
            name: $name,
            label: $label,
            entity: Entity::$entity,
            kind: $kind,
            cost: Cost::$cost,
            category: $cat,
            doc: $doc,
            multi: false,
        }
    };
    ($id:ident, $name:literal, $label:literal, $entity:ident, $kind:expr, $cost:ident, $cat:literal, $doc:literal, multi) => {
        FieldSpec {
            id: FieldId::$id,
            name: $name,
            label: $label,
            entity: Entity::$entity,
            kind: $kind,
            cost: Cost::$cost,
            category: $cat,
            doc: $doc,
            multi: true,
        }
    };
}

use FieldKind::*;

/// Every field, by entity then category, in the order the builder lists them.
pub static CATALOG: &[FieldSpec] = &[
    // --- Transactions: identity ---
    field!(
        TxTxid,
        "txid",
        "Transaction",
        Transactions,
        Hash,
        Summary,
        "Identity",
        "The transaction id."
    ),
    field!(
        TxTime,
        "time",
        "Time",
        Transactions,
        Time,
        Summary,
        "Identity",
        "When the accepting chain block was mined."
    ),
    field!(
        TxDaaScore,
        "daa_score",
        "DAA score",
        Transactions,
        Int,
        Summary,
        "Identity",
        "The accepting chain block's DAA score."
    ),
    field!(
        TxAcceptingBlock,
        "accepting_block",
        "Accepting block",
        Transactions,
        Hash,
        Record,
        "Identity",
        "The chain block that accepted the transaction."
    ),
    field!(
        TxBlock,
        "block",
        "Block",
        Transactions,
        Hash,
        Record,
        "Identity",
        "The block holding the transaction (a merged block, or the chain block itself)."
    ),
    field!(
        TxBlockTime,
        "block_time",
        "Block time",
        Transactions,
        Time,
        Record,
        "Identity",
        "When the holding block was mined."
    ),
    field!(
        TxIsCoinbase,
        "is_coinbase",
        "Coinbase",
        Transactions,
        Bool,
        Summary,
        "Identity",
        "A block reward transaction (no inputs)."
    ),
    field!(
        TxProtocol,
        "protocol",
        "Protocol",
        Transactions,
        Enum(PROTOCOLS),
        Summary,
        "Identity",
        "The protocol the payload or scripts mark: krc, kns, kasia, kasplex, ksocial, igra. Unknown for a plain transfer."
    ),
    // --- Transactions: amounts ---
    field!(
        TxFee,
        "fee",
        "Fee",
        Transactions,
        Amount,
        Summary,
        "Amounts",
        "Inputs minus outputs. Unknown when an input's UTXO wasn't resolved, or for a coinbase."
    ),
    field!(
        TxFeeRate,
        "fee_rate",
        "Fee rate",
        Transactions,
        Float,
        Summary,
        "Amounts",
        "Sompi per gram of mass."
    ),
    field!(
        TxMass,
        "mass",
        "Mass",
        Transactions,
        Int,
        Summary,
        "Amounts",
        "The larger of the storage and compute masses."
    ),
    field!(
        TxStorageMass,
        "storage_mass",
        "Storage mass",
        Transactions,
        Int,
        Record,
        "Amounts",
        "The storage mass."
    ),
    field!(
        TxComputeMass,
        "compute_mass",
        "Compute mass",
        Transactions,
        Int,
        Record,
        "Amounts",
        "The compute mass."
    ),
    field!(
        TxInputCount,
        "input_count",
        "Inputs",
        Transactions,
        Int,
        Summary,
        "Amounts",
        "How many inputs."
    ),
    field!(
        TxOutputCount,
        "output_count",
        "Outputs",
        Transactions,
        Int,
        Summary,
        "Amounts",
        "How many outputs."
    ),
    field!(
        TxInputTotal,
        "input_total",
        "Input total",
        Transactions,
        Amount,
        Summary,
        "Amounts",
        "Sompi over the inputs; unknown when one wasn't resolved."
    ),
    field!(
        TxOutputTotal,
        "output_total",
        "Output total",
        Transactions,
        Amount,
        Summary,
        "Amounts",
        "Sompi over the outputs."
    ),
    field!(
        TxMaxOutput,
        "max_output",
        "Largest output",
        Transactions,
        Amount,
        Summary,
        "Amounts",
        "The largest output."
    ),
    field!(
        TxMinOutput,
        "min_output",
        "Smallest output",
        Transactions,
        Amount,
        Summary,
        "Amounts",
        "The smallest output."
    ),
    // --- Transactions: parties ---
    field!(
        TxAddress,
        "address",
        "Address",
        Transactions,
        Address,
        Record,
        "Parties",
        "Any address the transaction touches, as sender or receiver.",
        multi
    ),
    field!(
        TxSender,
        "sender",
        "Sender",
        Transactions,
        Address,
        Record,
        "Parties",
        "Any input's address.",
        multi
    ),
    field!(
        TxReceiver,
        "receiver",
        "Receiver",
        Transactions,
        Address,
        Record,
        "Parties",
        "Any output's address.",
        multi
    ),
    field!(
        TxLabel,
        "label",
        "Label",
        Transactions,
        Text,
        Lookup,
        "Parties",
        "The label of any address touched (an exchange, a pool, your own).",
        multi
    ),
    field!(
        TxSenderLabel,
        "sender_label",
        "Sender label",
        Transactions,
        Text,
        Lookup,
        "Parties",
        "The label of any sender.",
        multi
    ),
    field!(
        TxReceiverLabel,
        "receiver_label",
        "Receiver label",
        Transactions,
        Text,
        Lookup,
        "Parties",
        "The label of any receiver.",
        multi
    ),
    field!(
        TxChangeMax,
        "change_max",
        "Change score",
        Transactions,
        Int,
        Record,
        "Parties",
        "How likely the most change-like output is the sender's change, 0–100."
    ),
    field!(
        TxSelfTransfer,
        "self_transfer",
        "Self transfer",
        Transactions,
        Bool,
        Summary,
        "Parties",
        "Every output pays an input address back."
    ),
    field!(
        TxDistinctAddresses,
        "distinct_addresses",
        "Distinct addresses",
        Transactions,
        Int,
        Record,
        "Parties",
        "How many different addresses are touched."
    ),
    field!(
        TxIsWatched,
        "is_watched",
        "Watched",
        Transactions,
        Bool,
        Lookup,
        "Parties",
        "Any address touched is on the watchlist."
    ),
    // --- Transactions: scripts ---
    field!(
        TxPayloadLen,
        "payload_len",
        "Payload length",
        Transactions,
        Int,
        Summary,
        "Scripts",
        "The payload's length in bytes."
    ),
    field!(
        TxPayload,
        "payload",
        "Payload",
        Transactions,
        Text,
        Record,
        "Scripts",
        "The payload as text (contains, starts_with; case-insensitive, invalid UTF-8 becomes �). Needs \"Index full payloads\" in Settings."
    ),
    field!(
        TxRedeemScript,
        "redeem_script",
        "Redeem script",
        Transactions,
        Text,
        Record,
        "Scripts",
        "The redeem script of any P2SH spend, in hex (contains, starts_with). Needs \"Index redeem scripts\" in Settings.",
        multi
    ),
    field!(
        TxOutputScriptClass,
        "output_script_class",
        "Output script",
        Transactions,
        Enum(SCRIPT_CLASSES),
        Record,
        "Scripts",
        "The class of any output's script: p2pk, p2pk_ecdsa, p2sh, non_standard.",
        multi
    ),
    field!(
        TxInputScriptClass,
        "input_script_class",
        "Input script",
        Transactions,
        Enum(SCRIPT_CLASSES),
        Record,
        "Scripts",
        "The class of any spent output's script.",
        multi
    ),
    field!(
        TxIntrospection,
        "introspection",
        "Introspection",
        Transactions,
        Bool,
        Summary,
        "Scripts",
        "A script uses transaction introspection opcodes (a covenant)."
    ),
    field!(
        TxSeqcommit,
        "seqcommit",
        "Seqcommit",
        Transactions,
        Bool,
        Summary,
        "Scripts",
        "A script uses OpChainblockSeqcommit."
    ),
    field!(
        TxZk,
        "zk",
        "ZK precompile",
        Transactions,
        Bool,
        Summary,
        "Scripts",
        "A script calls the ZK precompile (Groth16, R0 succinct or unknown)."
    ),
    field!(
        TxCovenantCreated,
        "covenant_created",
        "Covenants created",
        Transactions,
        Int,
        Record,
        "Scripts",
        "Outputs that create a covenant."
    ),
    field!(
        TxCovenantSpent,
        "covenant_spent",
        "Covenants spent",
        Transactions,
        Int,
        Record,
        "Scripts",
        "Inputs that spend a covenant output."
    ),
    field!(
        TxSigOps,
        "sig_ops",
        "Signature ops",
        Transactions,
        Int,
        Record,
        "Scripts",
        "Signature operations over the inputs."
    ),
    // --- Transactions: fields ---
    field!(
        TxVersion,
        "version",
        "Version",
        Transactions,
        Int,
        Record,
        "Fields",
        "The transaction version."
    ),
    field!(
        TxLockTime,
        "lock_time",
        "Lock time",
        Transactions,
        Int,
        Record,
        "Fields",
        "The lock time (0 for none)."
    ),
    field!(
        TxSubnetwork,
        "subnetwork",
        "Subnetwork",
        Transactions,
        Enum(SUBNETWORKS),
        Record,
        "Fields",
        "native, coinbase or other."
    ),
    field!(
        TxGas,
        "gas",
        "Gas",
        Transactions,
        Int,
        Record,
        "Fields",
        "The gas field."
    ),
    // --- Blocks: identity ---
    field!(
        BlockHash,
        "hash",
        "Block",
        Blocks,
        Hash,
        Record,
        "Identity",
        "The block hash."
    ),
    field!(
        BlockTime,
        "time",
        "Time",
        Blocks,
        Time,
        Record,
        "Identity",
        "When the block was mined."
    ),
    field!(
        BlockIsChain,
        "is_chain",
        "Chain block",
        Blocks,
        Bool,
        Record,
        "Identity",
        "A selected-chain block (with a header and a miner); otherwise a block a chain block merged, known only by the transactions it holds."
    ),
    field!(
        BlockMergingBlock,
        "merging_block",
        "Merging block",
        Blocks,
        Hash,
        Record,
        "Identity",
        "The chain block that merged it (itself for a chain block)."
    ),
    // --- Blocks: header ---
    field!(
        BlockDaaScore,
        "daa_score",
        "DAA score",
        Blocks,
        Int,
        Record,
        "Header",
        "The DAA score (chain blocks)."
    ),
    field!(
        BlockBlueScore,
        "blue_score",
        "Blue score",
        Blocks,
        Int,
        Record,
        "Header",
        "The blue score (chain blocks)."
    ),
    field!(
        BlockBlueWork,
        "blue_work",
        "Blue work",
        Blocks,
        Text,
        Record,
        "Header",
        "The blue work, in hex (chain blocks)."
    ),
    field!(
        BlockBits,
        "bits",
        "Bits",
        Blocks,
        Int,
        Record,
        "Header",
        "The compact difficulty target (chain blocks)."
    ),
    field!(
        BlockDifficulty,
        "difficulty",
        "Difficulty",
        Blocks,
        Float,
        Record,
        "Header",
        "The difficulty the bits encode (chain blocks)."
    ),
    field!(
        BlockNonce,
        "nonce",
        "Nonce",
        Blocks,
        Int,
        Record,
        "Header",
        "The nonce (chain blocks)."
    ),
    field!(
        BlockVersion,
        "version",
        "Version",
        Blocks,
        Int,
        Record,
        "Header",
        "The header version (chain blocks)."
    ),
    field!(
        BlockParentCount,
        "parent_count",
        "Parents",
        Blocks,
        Int,
        Record,
        "Header",
        "How many direct parents (chain blocks)."
    ),
    field!(
        BlockParents,
        "parents",
        "Parent hashes",
        Blocks,
        HashList,
        Record,
        "Header",
        "The direct parents (chain blocks).",
        multi
    ),
    field!(
        BlockHashMerkleRoot,
        "hash_merkle_root",
        "Hash merkle root",
        Blocks,
        Hash,
        Record,
        "Header",
        "The transactions' merkle root (chain blocks)."
    ),
    field!(
        BlockAcceptedIdMerkleRoot,
        "accepted_id_merkle_root",
        "Accepted id merkle root",
        Blocks,
        Hash,
        Record,
        "Header",
        "The accepted ids' merkle root (chain blocks)."
    ),
    field!(
        BlockUtxoCommitment,
        "utxo_commitment",
        "UTXO commitment",
        Blocks,
        Hash,
        Record,
        "Header",
        "The UTXO set commitment (chain blocks)."
    ),
    field!(
        BlockPruningPoint,
        "pruning_point",
        "Pruning point",
        Blocks,
        Hash,
        Record,
        "Header",
        "The pruning point the block names (chain blocks)."
    ),
    // --- Blocks: mining ---
    field!(
        BlockMiner,
        "miner",
        "Miner",
        Blocks,
        Address,
        Record,
        "Mining",
        "Who mined the block, from its coinbase payload (chain blocks; known once the next chain block arrived)."
    ),
    field!(
        BlockMinerLabel,
        "miner_label",
        "Miner label",
        Blocks,
        Text,
        Lookup,
        "Mining",
        "The miner's label (a pool)."
    ),
    field!(
        BlockMinerTag,
        "miner_tag",
        "Miner tag",
        Blocks,
        Text,
        Record,
        "Mining",
        "The tag in the coinbase payload (pool and miner software names)."
    ),
    field!(
        BlockNodeVersion,
        "node_version",
        "Node version",
        Blocks,
        Text,
        Record,
        "Mining",
        "The miner's node version from the coinbase payload."
    ),
    field!(
        BlockSubsidy,
        "subsidy",
        "Subsidy",
        Blocks,
        Amount,
        Record,
        "Mining",
        "The block subsidy from the coinbase payload."
    ),
    field!(
        BlockReward,
        "reward",
        "Reward",
        Blocks,
        Amount,
        Record,
        "Mining",
        "Subsidy plus the fees of the accepted transactions it holds."
    ),
    field!(
        BlockMergedBlues,
        "merged_blues",
        "Merged blues",
        Blocks,
        Int,
        Record,
        "Mining",
        "Mergeset blue blocks the coinbase paid (chain blocks)."
    ),
    field!(
        BlockPayoutTotal,
        "payout_total",
        "Payout total",
        Blocks,
        Amount,
        Record,
        "Mining",
        "Sompi over the coinbase's outputs (chain blocks)."
    ),
    field!(
        BlockPays,
        "pays",
        "Pays",
        Blocks,
        AddressList,
        Record,
        "Mining",
        "The addresses the coinbase pays: the mergeset blues' miners (chain blocks).",
        multi
    ),
    // --- Blocks: contents ---
    field!(
        BlockAcceptedTxs,
        "accepted_txs",
        "Accepted txs",
        Blocks,
        Int,
        Record,
        "Contents",
        "Accepted transactions the block holds."
    ),
    field!(
        BlockAcceptedMass,
        "accepted_mass",
        "Accepted mass",
        Blocks,
        Int,
        Record,
        "Contents",
        "Their mass."
    ),
    field!(
        BlockAcceptedFees,
        "accepted_fees",
        "Accepted fees",
        Blocks,
        Amount,
        Record,
        "Contents",
        "Their fees."
    ),
    // --- Payouts ---
    field!(
        PayoutTime,
        "time",
        "Time",
        Payouts,
        Time,
        Record,
        "Payout",
        "When the paying chain block was mined."
    ),
    field!(
        PayoutBlock,
        "block",
        "Block",
        Payouts,
        Hash,
        Record,
        "Payout",
        "The chain block whose coinbase pays."
    ),
    field!(
        PayoutIndex,
        "index",
        "Output",
        Payouts,
        Int,
        Record,
        "Payout",
        "The coinbase output's index (0 pays the selected parent's miner)."
    ),
    field!(
        PayoutMiner,
        "miner",
        "Miner",
        Payouts,
        Address,
        Record,
        "Payout",
        "The address paid: the rewarded block's miner."
    ),
    field!(
        PayoutMinerLabel,
        "miner_label",
        "Miner label",
        Payouts,
        Text,
        Lookup,
        "Payout",
        "The paid address's label (a pool)."
    ),
    field!(
        PayoutAmount,
        "amount",
        "Amount",
        Payouts,
        Amount,
        Record,
        "Payout",
        "Sompi paid."
    ),
    // --- Addresses ---
    field!(
        AddrAddress,
        "address",
        "Address",
        Addresses,
        Address,
        Record,
        "Identity",
        "The address."
    ),
    field!(
        AddrLabel,
        "label",
        "Label",
        Addresses,
        Text,
        Lookup,
        "Identity",
        "The address's label (shown first: yours, then kas.fyi, kaspa.org, KNS, heuristics)."
    ),
    field!(
        AddrLabelSource,
        "label_source",
        "Label source",
        Addresses,
        Enum(LABEL_SOURCES),
        Lookup,
        "Identity",
        "Where the label comes from: user, kas_fyi, kaspa_org, kns, heuristic."
    ),
    field!(
        AddrCategory,
        "category",
        "Category",
        Addresses,
        Text,
        Lookup,
        "Identity",
        "Any category the label's source gives (exchange, pool, …).",
        multi
    ),
    field!(
        AddrType,
        "address_type",
        "Type",
        Addresses,
        Enum(ADDRESS_TYPES),
        Record,
        "Identity",
        "p2pk, p2pk_ecdsa or p2sh, from the address itself."
    ),
    field!(
        AddrFirstSeen,
        "first_seen",
        "First seen",
        Addresses,
        Time,
        Record,
        "Activity",
        "The first indexed transaction in the window."
    ),
    field!(
        AddrLastSeen,
        "last_seen",
        "Last seen",
        Addresses,
        Time,
        Record,
        "Activity",
        "The last indexed transaction in the window."
    ),
    field!(
        AddrTxCount,
        "tx_count",
        "Transactions",
        Addresses,
        Int,
        Record,
        "Activity",
        "Indexed transactions in the window."
    ),
    field!(
        AddrReceived,
        "received",
        "Received",
        Addresses,
        Amount,
        Record,
        "Activity",
        "Sompi received in the window."
    ),
    field!(
        AddrSent,
        "sent",
        "Sent",
        Addresses,
        Amount,
        Record,
        "Activity",
        "Sompi sent in the window."
    ),
    field!(
        AddrNet,
        "net",
        "Net",
        Addresses,
        Amount,
        Record,
        "Activity",
        "Received minus sent in the window."
    ),
    field!(
        AddrClusterSize,
        "cluster_size",
        "Cluster size",
        Addresses,
        Int,
        Lookup,
        "Owner",
        "Addresses in the same likely-owner cluster, this one included."
    ),
    field!(
        AddrClusterLabel,
        "cluster_label",
        "Cluster label",
        Addresses,
        Text,
        Lookup,
        "Owner",
        "The label that names the cluster."
    ),
    field!(
        AddrPeerCount,
        "peer_count",
        "Counterparties",
        Addresses,
        Int,
        Lookup,
        "Owner",
        "Distinct counterparties in the window."
    ),
    field!(
        AddrIsWatched,
        "is_watched",
        "Watched",
        Addresses,
        Bool,
        Lookup,
        "Owner",
        "On the watchlist."
    ),
    field!(
        AddrBalance,
        "balance",
        "Balance",
        Addresses,
        Amount,
        Node,
        "Activity",
        "The balance now, from the node, for the rows shown (not a filter)."
    ),
];

impl FieldId {
    pub fn spec(self) -> &'static FieldSpec {
        CATALOG
            .iter()
            .find(|f| f.id == self)
            .expect("every field is in the catalog")
    }

    pub fn name(self) -> &'static str {
        self.spec().name
    }

    pub fn label(self) -> &'static str {
        self.spec().label
    }

    pub fn kind(self) -> FieldKind {
        self.spec().kind
    }

    pub fn entity(self) -> Entity {
        self.spec().entity
    }

    pub fn cost(self) -> Cost {
        self.spec().cost
    }

    pub fn is_multi(self) -> bool {
        self.spec().multi
    }

    /// The opt-in index feature the field reads, if any: without it the index doesn't
    /// keep the field's data, so the field can't be used.
    pub fn feature(self) -> Option<IndexFeature> {
        match self {
            Self::TxPayload => Some(IndexFeature::FullPayloads),
            Self::TxRedeemScript => Some(IndexFeature::RedeemScripts),
            _ => None,
        }
    }

    /// Why the field can't be used with `settings`, if it can't.
    pub fn unavailable(self, settings: &IndexSettings) -> Option<String> {
        self.feature()
            .filter(|f| !settings.enabled(*f))
            .map(IndexFeature::off_reason)
    }
}

/// The fields of `entity`, in catalog order.
pub fn for_entity(entity: Entity) -> impl Iterator<Item = &'static FieldSpec> {
    CATALOG.iter().filter(move |f| f.entity == entity)
}

/// The field of `entity` named `name` (case-insensitive).
pub fn by_name(entity: Entity, name: &str) -> Option<FieldId> {
    let name = name.to_ascii_lowercase();
    for_entity(entity).find(|f| f.name == name).map(|f| f.id)
}

/// Names people reach for that aren't fields, and the field they mean. Only
/// [`suggest`] uses them: they never resolve, so saved queries stay canonical. An alias
/// applies to any entity that has the target (`hash` is a real field of blocks, so the
/// alias only fires for transactions).
const ALIASES: &[(&str, &str)] = &[
    ("amount", "output_total"),
    ("value", "output_total"),
    ("total", "output_total"),
    ("inputs", "input_count"),
    ("outputs", "output_count"),
    ("hash", "txid"),
    ("id", "txid"),
    ("coinbase", "is_coinbase"),
    ("accepted_tx_count", "accepted_txs"),
    ("tx_count", "accepted_txs"),
    ("is_self_transfer", "self_transfer"),
    ("watched", "is_watched"),
    ("from", "sender"),
    ("to", "receiver"),
    ("timestamp", "time"),
    ("date", "time"),
];

/// The field name of `entity` closest to `name`, when close enough to be a slip: an
/// alias from [`ALIASES`], else the nearest name by edit distance, within two edits for
/// names of four characters or more, or within 40% of the name's length (so `fees`
/// suggests `fee`, but `x` doesn't suggest `zk`), or a name containing it.
pub fn suggest(entity: Entity, name: &str) -> Option<&'static str> {
    let name = name.to_ascii_lowercase();
    if let Some((_, target)) = ALIASES.iter().find(|(alias, _)| *alias == name)
        && by_name(entity, target).is_some()
    {
        return Some(target);
    }
    for_entity(entity)
        .map(|f| (edit_distance(&name, f.name), f.name))
        .filter(|(d, candidate)| close_enough(*d, candidate, &name))
        .min_by_key(|(d, candidate)| (*d, candidate.len()))
        .map(|(_, candidate)| candidate)
}

/// Whether `candidate` at edit distance `d` is a plausible slip of `name`.
fn close_enough(d: usize, candidate: &str, name: &str) -> bool {
    let len = candidate.len().max(name.len());
    (d <= 2 && len >= 4) || d * 10 <= len * 4 || (candidate.contains(name) && name.len() >= 3)
}

fn edit_distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur.push((prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// The columns a query of `entity` shows unless it says otherwise.
pub fn default_columns(entity: Entity) -> Vec<FieldId> {
    use FieldId::*;
    match entity {
        Entity::Transactions => vec![
            TxTime,
            TxTxid,
            TxProtocol,
            TxOutputTotal,
            TxFee,
            TxInputCount,
            TxOutputCount,
        ],
        Entity::Blocks => vec![
            BlockTime,
            BlockHash,
            BlockIsChain,
            BlockBlueScore,
            BlockMiner,
            BlockAcceptedTxs,
            BlockAcceptedFees,
        ],
        Entity::Payouts => vec![PayoutTime, PayoutBlock, PayoutMiner, PayoutAmount],
        Entity::Addresses => vec![
            AddrAddress,
            AddrLabel,
            AddrTxCount,
            AddrReceived,
            AddrSent,
            AddrLastSeen,
            AddrBalance,
        ],
    }
}

/// The categories of `entity`'s fields, in catalog order.
pub fn categories(entity: Entity) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    for f in for_entity(entity) {
        if !out.contains(&f.category) {
            out.push(f.category);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::labels::LabelSource;
    use crate::tx_inspect::{ScriptClass, TransactionProtocol};

    #[test]
    fn every_field_has_a_unique_name_and_allowed_ops() {
        for entity in Entity::ALL {
            let names: Vec<&str> = for_entity(entity).map(|f| f.name).collect();
            let mut sorted = names.clone();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(names.len(), sorted.len(), "{entity:?} names repeat");
            for f in for_entity(entity) {
                assert_eq!(by_name(entity, f.name), Some(f.id));
                assert_eq!(by_name(entity, &f.name.to_uppercase()), Some(f.id));
                assert!(!f.kind.operators().is_empty());
                assert!(!f.doc.is_empty() && !f.label.is_empty());
                assert_eq!(f.id.spec().name, f.name);
                assert!(f.multi == f.kind.is_list() || f.multi);
            }
            for c in default_columns(entity) {
                assert_eq!(c.entity(), entity);
            }
            assert!(!categories(entity).is_empty());
        }
        assert_eq!(
            CATALOG.len(),
            Entity::ALL
                .iter()
                .map(|e| for_entity(*e).count())
                .sum::<usize>()
        );
    }

    #[test]
    fn enum_tables_match_their_types() {
        let protocols: Vec<&str> = TransactionProtocol::ALL.iter().map(|p| p.slug()).collect();
        assert_eq!(PROTOCOLS, protocols.as_slice());
        let classes: Vec<&str> = ScriptClass::ALL.iter().map(|c| c.slug()).collect();
        assert_eq!(SCRIPT_CLASSES, classes.as_slice());
        for source in [
            LabelSource::User,
            LabelSource::KasFyi,
            LabelSource::KaspaOrg,
            LabelSource::Kns,
            LabelSource::Heuristic,
        ] {
            assert!(LABEL_SOURCES.contains(&label_source_name(source)));
        }
    }

    /// The text-form name of a label source (`exec` has the same mapping).
    fn label_source_name(source: LabelSource) -> &'static str {
        match source {
            LabelSource::User => "user",
            LabelSource::KasFyi => "kas_fyi",
            LabelSource::KaspaOrg => "kaspa_org",
            LabelSource::Kns => "kns",
            LabelSource::Heuristic => "heuristic",
        }
    }

    #[test]
    fn balance_is_a_column_only() {
        assert_eq!(FieldId::AddrBalance.cost(), Cost::Node);
        assert!(default_columns(Entity::Addresses).contains(&FieldId::AddrBalance));
    }

    #[test]
    fn suggestions_catch_slips() {
        assert_eq!(suggest(Entity::Transactions, "fees"), Some("fee"));
        assert_eq!(
            suggest(Entity::Transactions, "ouput_total"),
            Some("output_total")
        );
        assert_eq!(suggest(Entity::Transactions, "miner"), None);
        assert_eq!(suggest(Entity::Blocks, "bluescore"), Some("blue_score"));
        assert_eq!(suggest(Entity::Addresses, "recv"), None);
        assert_eq!(suggest(Entity::Addresses, "receive"), Some("received"));
        assert_eq!(edit_distance("kitten", "sitting"), 3);
        // Short names don't get far-fetched suggestions.
        assert_eq!(suggest(Entity::Transactions, "x"), None);
        assert_eq!(suggest(Entity::Transactions, "ab"), None);
        assert_eq!(suggest(Entity::Transactions, "zkk"), Some("zk"));
        assert_eq!(suggest(Entity::Payouts, "amt"), None);
    }

    #[test]
    fn aliases_suggest_but_never_resolve() {
        assert_eq!(
            suggest(Entity::Transactions, "amount"),
            Some("output_total")
        );
        assert_eq!(suggest(Entity::Transactions, "value"), Some("output_total"));
        assert_eq!(suggest(Entity::Transactions, "inputs"), Some("input_count"));
        assert_eq!(
            suggest(Entity::Transactions, "outputs"),
            Some("output_count")
        );
        assert_eq!(suggest(Entity::Transactions, "hash"), Some("txid"));
        assert_eq!(
            suggest(Entity::Transactions, "coinbase"),
            Some("is_coinbase")
        );
        assert_eq!(
            suggest(Entity::Transactions, "is_self_transfer"),
            Some("self_transfer")
        );
        assert_eq!(
            suggest(Entity::Blocks, "accepted_tx_count"),
            Some("accepted_txs")
        );
        // `hash` is a real field of blocks; `amount` one of payouts.
        assert_eq!(by_name(Entity::Blocks, "hash"), Some(FieldId::BlockHash));
        assert_eq!(
            by_name(Entity::Payouts, "amount"),
            Some(FieldId::PayoutAmount)
        );
        for (alias, target) in ALIASES {
            for entity in Entity::ALL {
                if by_name(entity, alias).is_none() && by_name(entity, target).is_some() {
                    assert_eq!(suggest(entity, alias), Some(*target), "{alias}");
                }
            }
        }
        assert_eq!(by_name(Entity::Transactions, "amount"), None);
    }

    #[test]
    fn time_kind_describes_its_literals() {
        assert!(FieldKind::Time.describe().contains("2026-10-01T12:00Z"));
        assert!(FieldKind::Time.describe().contains("24h"));
        assert_eq!(FieldKind::Int.describe(), FieldKind::Int.label());
    }

    #[test]
    fn kinds_decide_operators() {
        assert!(FieldKind::Amount.allows(Op::Between));
        assert!(!FieldKind::Amount.allows(Op::Contains));
        assert!(FieldKind::Text.allows(Op::Contains));
        assert!(!FieldKind::Bool.allows(Op::IsNull));
        assert!(FieldKind::HashList.allows(Op::Contains));
        assert!(!FieldKind::HashList.allows(Op::Eq));
        assert!(FieldKind::Enum(PROTOCOLS).allows(Op::In));
    }
}
