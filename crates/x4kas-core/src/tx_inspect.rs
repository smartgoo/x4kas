//! Per-transaction classification for analytics: protocol tags, output script
//! classes, covenant / introspection opcode usage, and the node version in a
//! coinbase payload. Pure functions over raw bytes; the RPC adapter lives in
//! `analytics::summarize_chain_blocks`.
//!
//! The rules mirror Kaspalytics so the numbers are comparable.

use kaspa_addresses::{Address, Prefix, Version};
use serde::{Deserialize, Serialize};

// --- Protocols ---

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TransactionProtocol {
    Krc,
    Kns,
    Kasia,
    Kasplex,
    KSocial,
    Igra,
}

impl TransactionProtocol {
    /// Display order, as on the Kaspalytics home page.
    pub const ALL: [Self; 6] = [
        Self::Krc,
        Self::Kns,
        Self::Kasia,
        Self::Kasplex,
        Self::KSocial,
        Self::Igra,
    ];

    pub fn label(&self) -> &'static str {
        match self {
            Self::Krc => "KRC Inscriptions",
            Self::Kns => "KNS",
            Self::Kasia => "Kasia",
            Self::Kasplex => "Kasplex L2",
            Self::KSocial => "K Social",
            Self::Igra => "Igra L2",
        }
    }

    /// A short lowercase name, for the Explorer's search field (`protocol:<slug>`) and
    /// the CLI.
    pub fn slug(&self) -> &'static str {
        match self {
            Self::Krc => "krc",
            Self::Kns => "kns",
            Self::Kasia => "kasia",
            Self::Kasplex => "kasplex",
            Self::KSocial => "ksocial",
            Self::Igra => "igra",
        }
    }

    pub fn from_slug(slug: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.slug() == slug)
    }

    /// The protocol by its display name ([`label`](Self::label)).
    pub fn from_label(label: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.label() == label)
    }

    /// A stable one-byte code, the prefix of the address index's per-protocol
    /// transaction keys. Never renumber: bump `index::FORMAT_VERSION` instead.
    pub fn code(&self) -> u8 {
        match self {
            Self::Krc => 1,
            Self::Kns => 2,
            Self::Kasia => 3,
            Self::Kasplex => 4,
            Self::KSocial => 5,
            Self::Igra => 6,
        }
    }

    pub fn from_code(code: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.code() == code)
    }
}

/// Detect the protocol of a transaction from its payload and input signature scripts.
/// Returns `None` for standard (non-protocol) transactions. First match wins.
pub fn detect_protocol(payload: &[u8], input_scripts: &[&[u8]]) -> Option<TransactionProtocol> {
    if !payload.is_empty() {
        // Byte-per-char so binary payloads still match their ASCII markers.
        let s: String = payload.iter().map(|&b| b as char).collect();
        if s.contains("ciph_msg") {
            return Some(TransactionProtocol::Kasia);
        }
        if s.contains("kasplex") {
            return Some(TransactionProtocol::Kasplex);
        }
        if s.starts_with("k:") {
            return Some(TransactionProtocol::KSocial);
        }
        // Igra: first byte upper nibble 0x9, lower nibble 0x1..=0x7
        if matches!(payload.first(), Some(b) if (0x91..=0x97).contains(b)) {
            return Some(TransactionProtocol::Igra);
        }
    }

    input_scripts
        .iter()
        .find_map(|script| scan_script_for_inscription(script, 0))
}

/// Look for an inscription marker push (`kasplex`/`kspr` → KRC, `kns` → KNS).
/// The marker sits in the redeem script, which the signature script pushes with
/// OP_PUSHDATA1/2, so those pushes are scanned recursively.
fn scan_script_for_inscription(script: &[u8], depth: u8) -> Option<TransactionProtocol> {
    for push in Pushes::new(script) {
        match push.data {
            b"kasplex" | b"kspr" => return Some(TransactionProtocol::Krc),
            b"kns" => return Some(TransactionProtocol::Kns),
            data if push.extended && depth < 2 => {
                if let Some(proto) = scan_script_for_inscription(data, depth + 1) {
                    return Some(proto);
                }
            }
            _ => {}
        }
    }
    None
}

// --- Script parsing ---

/// One step of a script walk: an opcode, or a data push.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Token<'a> {
    Op(u8),
    /// `extended`: pushed with OP_PUSHDATA1/2/4 rather than a direct push.
    Push {
        data: &'a [u8],
        extended: bool,
    },
}

/// Walks a script, skipping over pushed data so it is never read as opcodes.
/// Stops at a truncated push.
struct Tokens<'a> {
    script: &'a [u8],
    cursor: usize,
}

impl<'a> Tokens<'a> {
    fn new(script: &'a [u8]) -> Self {
        Self { script, cursor: 0 }
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.cursor.checked_add(n)?;
        let bytes = self.script.get(self.cursor..end)?;
        self.cursor = end;
        Some(bytes)
    }
}

impl<'a> Iterator for Tokens<'a> {
    type Item = Token<'a>;

    fn next(&mut self) -> Option<Token<'a>> {
        let op = *self.script.get(self.cursor)?;
        self.cursor += 1;
        let (len, extended) = match op {
            0x01..=0x4b => (op as usize, false),
            0x4c => (self.take(1)?[0] as usize, true),
            0x4d => {
                let b = self.take(2)?;
                (u16::from_le_bytes([b[0], b[1]]) as usize, true)
            }
            0x4e => {
                let b = self.take(4)?;
                (u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize, true)
            }
            _ => return Some(Token::Op(op)),
        };
        let data = self.take(len)?;
        Some(Token::Push { data, extended })
    }
}

struct Push<'a> {
    data: &'a [u8],
    extended: bool,
}

/// Only the data pushes of a script.
struct Pushes<'a>(Tokens<'a>);

impl<'a> Pushes<'a> {
    fn new(script: &'a [u8]) -> Self {
        Self(Tokens::new(script))
    }
}

impl<'a> Iterator for Pushes<'a> {
    type Item = Push<'a>;

    fn next(&mut self) -> Option<Push<'a>> {
        loop {
            if let Token::Push { data, extended } = self.0.next()? {
                return Some(Push { data, extended });
            }
        }
    }
}

// --- Script classes ---

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptClass {
    PubKey,
    PubKeyEcdsa,
    ScriptHash,
    NonStandard,
}

impl ScriptClass {
    pub const ALL: [Self; 4] = [
        Self::PubKey,
        Self::PubKeyEcdsa,
        Self::ScriptHash,
        Self::NonStandard,
    ];

    pub fn label(&self) -> &'static str {
        match self {
            Self::PubKey => "P2PK",
            Self::PubKeyEcdsa => "P2PK (ECDSA)",
            Self::ScriptHash => "P2SH",
            Self::NonStandard => "non-standard",
        }
    }

    /// A short lowercase name, for queries and the CLI.
    pub fn slug(&self) -> &'static str {
        match self {
            Self::PubKey => "p2pk",
            Self::PubKeyEcdsa => "p2pk_ecdsa",
            Self::ScriptHash => "p2sh",
            Self::NonStandard => "non_standard",
        }
    }

    pub fn from_slug(slug: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.slug() == slug)
    }

    /// A stable one-byte code, stored by the address index. Never renumber: bump
    /// `index::FORMAT_VERSION` instead.
    pub fn code(&self) -> u8 {
        match self {
            Self::PubKey => 0,
            Self::PubKeyEcdsa => 1,
            Self::ScriptHash => 2,
            Self::NonStandard => 3,
        }
    }

    pub fn from_code(code: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.code() == code)
    }
}

/// The address a standard script public key pays, on the network of `prefix`: P2PK,
/// ECDSA P2PK or P2SH. `None` for a non-standard script.
pub fn script_address(script: &[u8], prefix: Prefix) -> Option<Address> {
    let (version, payload) = match script_class(script) {
        ScriptClass::PubKey => (Version::PubKey, &script[1..33]),
        ScriptClass::PubKeyEcdsa => (Version::PubKeyECDSA, &script[1..34]),
        ScriptClass::ScriptHash => (Version::ScriptHash, &script[2..34]),
        ScriptClass::NonStandard => return None,
    };
    Some(Address::new(prefix, version, payload))
}

// --- Difficulty ---

/// The proof-of-work limit as a float: `MAX_DIFFICULTY_TARGET_AS_F64` in rusty-kaspa.
const MAX_TARGET_F64: f64 = 5.78960446186581e76;

/// A block's difficulty from its header's compact target `bits`, as the node reports it
/// in a block's verbose data (`max target / target`). Zero for an invalid encoding.
pub fn difficulty_from_bits(bits: u32) -> f64 {
    // The compact encoding: a 24-bit mantissa and a byte-shift exponent minus 3.
    let unshifted_expt = bits >> 24;
    let (mant, expt) = if unshifted_expt <= 3 {
        ((bits & 0xff_ffff) >> (8 * (3 - unshifted_expt)), 0)
    } else {
        (bits & 0xff_ffff, 8 * (unshifted_expt - 3))
    };
    if mant == 0 || mant > 0x7f_ffff {
        return 0.0;
    }
    let target = mant as f64 * 2f64.powi(expt as i32);
    MAX_TARGET_F64 / target
}

const OP_DATA_32: u8 = 0x20;
const OP_DATA_33: u8 = 0x21;
const OP_EQUAL: u8 = 0x87;
const OP_BLAKE2B: u8 = 0xaa;
const OP_CHECKSIG_ECDSA: u8 = 0xab;
const OP_CHECKSIG: u8 = 0xac;

/// Classify a script public key the way `kaspa_txscript::ScriptClass::from_script` does.
pub fn script_class(script: &[u8]) -> ScriptClass {
    match script {
        [OP_DATA_32, key @ .., OP_CHECKSIG] if key.len() == 32 => ScriptClass::PubKey,
        [OP_DATA_33, key @ .., OP_CHECKSIG_ECDSA] if key.len() == 33 => ScriptClass::PubKeyEcdsa,
        [OP_BLAKE2B, OP_DATA_32, hash @ .., OP_EQUAL] if hash.len() == 32 => {
            ScriptClass::ScriptHash
        }
        _ => ScriptClass::NonStandard,
    }
}

// --- Covenant / introspection opcodes ---

const OP_ZK_PRECOMPILE: u8 = 0xa6;
/// `OpTxVersion..=OpOutputAuthorizingInput`, minus the holes below.
const INTROSPECTION_MIN: u8 = 0xb2;
const INTROSPECTION_MAX: u8 = 0xd6;
/// `OpUnknown202`, `OpNum2Bin`, `OpBin2Num`: inside the range but not introspection.
const INTROSPECTION_HOLES: [u8; 3] = [0xca, 0xcd, 0xce];
const OP_CHAINBLOCK_SEQCOMMIT: u8 = 0xd4;
const ZK_TAG_GROTH16: u8 = 0x20;
const ZK_TAG_R0SUCCINCT: u8 = 0x21;

fn is_introspection_opcode(op: u8) -> bool {
    (INTROSPECTION_MIN..=INTROSPECTION_MAX).contains(&op) && !INTROSPECTION_HOLES.contains(&op)
}

/// Covenant-era opcodes found in a script. The ZK flags are overlapping subsets
/// of "uses `OpZkPrecompile`"; seqcommit is a subset of introspection.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct OpcodeUsage {
    pub introspection: bool,
    pub chainblock_seqcommit: bool,
    pub zk_groth16: bool,
    pub zk_r0succinct: bool,
    pub zk_unknown: bool,
}

impl OpcodeUsage {
    pub fn zk_precompile(&self) -> bool {
        self.zk_groth16 || self.zk_r0succinct || self.zk_unknown
    }

    /// Packed into one byte for the address index (stable: bump `index::FORMAT_VERSION`
    /// to change the layout).
    pub fn to_bits(self) -> u8 {
        (self.introspection as u8)
            | (self.chainblock_seqcommit as u8) << 1
            | (self.zk_groth16 as u8) << 2
            | (self.zk_r0succinct as u8) << 3
            | (self.zk_unknown as u8) << 4
    }

    pub fn from_bits(bits: u8) -> Self {
        Self {
            introspection: bits & 1 != 0,
            chainblock_seqcommit: bits & 2 != 0,
            zk_groth16: bits & 4 != 0,
            zk_r0succinct: bits & 8 != 0,
            zk_unknown: bits & 16 != 0,
        }
    }
}

impl std::ops::BitOrAssign for OpcodeUsage {
    fn bitor_assign(&mut self, rhs: Self) {
        self.introspection |= rhs.introspection;
        self.chainblock_seqcommit |= rhs.chainblock_seqcommit;
        self.zk_groth16 |= rhs.zk_groth16;
        self.zk_r0succinct |= rhs.zk_r0succinct;
        self.zk_unknown |= rhs.zk_unknown;
    }
}

/// Scan a script body. `stack_top` is the value on the stack when the script
/// starts: `OpZkPrecompile` pops a 1-byte proof-system tag, which a P2SH spend
/// usually supplies as the push right before the redeem script.
fn scan_opcodes(script: &[u8], stack_top: Option<&[u8]>) -> OpcodeUsage {
    let mut usage = OpcodeUsage::default();
    let mut last_push = stack_top;
    for token in Tokens::new(script) {
        match token {
            Token::Push { data, .. } => last_push = Some(data),
            Token::Op(OP_ZK_PRECOMPILE) => {
                match last_push {
                    Some([ZK_TAG_GROTH16]) => usage.zk_groth16 = true,
                    Some([ZK_TAG_R0SUCCINCT]) => usage.zk_r0succinct = true,
                    _ => usage.zk_unknown = true,
                }
                last_push = None;
            }
            Token::Op(op) => {
                usage.introspection |= is_introspection_opcode(op);
                usage.chainblock_seqcommit |= op == OP_CHAINBLOCK_SEQCOMMIT;
                last_push = None;
            }
        }
    }
    usage
}

/// Opcodes used directly in a (non-standard) script public key.
pub fn output_script_opcodes(script: &[u8]) -> OpcodeUsage {
    scan_opcodes(script, None)
}

/// Opcodes in the redeem script revealed by a P2SH spend: the signature
/// script's final push. Only meaningful when the spent output is P2SH.
/// The redeem script a P2SH spend reveals: the signature script's last push.
pub fn redeem_script(signature_script: &[u8]) -> Option<&[u8]> {
    Pushes::new(signature_script).last().map(|push| push.data)
}

pub fn redeem_script_opcodes(signature_script: &[u8]) -> OpcodeUsage {
    let mut prev = None;
    let mut last = None;
    for push in Pushes::new(signature_script) {
        prev = last;
        last = Some(push.data);
    }
    match last {
        Some(redeem) => scan_opcodes(redeem, prev),
        None => OpcodeUsage::default(),
    }
}

// --- Coinbase ---

/// Longest node version string kept; longer ones are miner junk.
const MAX_VERSION_LEN: usize = 32;

/// A coinbase payload, decoded. Layout: blue score (u64 LE), subsidy (u64 LE), the
/// miner's script public key version (u16 LE) and length (u8), the script itself, then
/// extra data `"<node version>/<miner info>"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoinbasePayload<'a> {
    pub blue_score: u64,
    pub subsidy: u64,
    pub script_version: u16,
    /// The script public key of the block's own miner (who gets the reds' reward, and
    /// whose block is paid by the next chain block's coinbase).
    pub script: &'a [u8],
    pub extra: &'a [u8],
}

impl CoinbasePayload<'_> {
    /// The node version, e.g. `"1.0.1"`; `""` if the miner didn't set one.
    pub fn node_version(&self) -> String {
        let extra = self.extra_text();
        let version = extra.split('/').next().unwrap_or_default().trim();
        version.chars().take(MAX_VERSION_LEN).collect()
    }

    /// The miner's tag: the extra data after the node version, e.g. `"kaspa-miner/pool"`
    /// for `"1.0.1/kaspa-miner/pool"`. Pools and miner software put their names here.
    pub fn miner_tag(&self) -> Option<String> {
        let extra = self.extra_text();
        let (_, tag) = extra.split_once('/')?;
        let tag: String = tag.chars().filter(|c| !c.is_control()).collect();
        Some(tag.trim().to_string()).filter(|t| !t.is_empty())
    }

    /// The extra data byte per char, so binary junk doesn't hide an ASCII tag.
    fn extra_text(&self) -> String {
        self.extra.iter().map(|&b| b as char).collect()
    }
}

/// Decode a coinbase payload; `None` if `payload` isn't one (too short, or the script
/// isn't a miner's pay-to-pubkey script).
pub fn parse_coinbase_payload(payload: &[u8]) -> Option<CoinbasePayload<'_>> {
    let blue_score = u64::from_le_bytes(payload.get(..8)?.try_into().ok()?);
    let subsidy = u64::from_le_bytes(payload.get(8..16)?.try_into().ok()?);
    let script_version = u16::from_le_bytes(payload.get(16..18)?.try_into().ok()?);
    let script_len = *payload.get(18)? as usize;
    let script = payload.get(19..19 + script_len)?;
    if script.first().is_none_or(|&b| b == OP_BLAKE2B) {
        return None;
    }
    Some(CoinbasePayload {
        blue_score,
        subsidy,
        script_version,
        script,
        extra: &payload[19 + script_len..],
    })
}

/// The node version from a coinbase payload, e.g. `"1.0.1"`; `""` if the miner
/// didn't set one. `None` if the payload isn't a coinbase payload.
pub fn coinbase_node_version(payload: &[u8]) -> Option<String> {
    Some(parse_coinbase_payload(payload)?.node_version())
}

/// The miner's tag from a coinbase payload (`CoinbasePayload::miner_tag`). `None` if
/// the payload isn't a coinbase payload or there is no tag.
pub fn coinbase_miner_tag(payload: &[u8]) -> Option<String> {
    parse_coinbase_payload(payload)?.miner_tag()
}

/// The address of the block's own miner from its coinbase payload, on the network of
/// `prefix`. `None` if the payload isn't a coinbase payload or the script isn't standard.
pub fn coinbase_miner_address(payload: &[u8], prefix: Prefix) -> Option<Address> {
    script_address(parse_coinbase_payload(payload)?.script, prefix)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push(data: &[u8]) -> Vec<u8> {
        let mut s = vec![data.len() as u8];
        s.extend_from_slice(data);
        s
    }

    /// A P2SH signature script: `<witness pushes…> <redeem>`.
    fn p2sh_sig(witness: &[&[u8]], redeem: &[u8]) -> Vec<u8> {
        let mut sig: Vec<u8> = witness.iter().flat_map(|w| push(w)).collect();
        sig.extend(push(redeem));
        sig
    }

    // --- Protocols ---

    #[test]
    fn detects_payload_protocols() {
        let cases: [(&[u8], TransactionProtocol); 4] = [
            (b"some ciph_msg data", TransactionProtocol::Kasia),
            (b"kasplex operation", TransactionProtocol::Kasplex),
            (b"k:post data", TransactionProtocol::KSocial),
            (&[0x93, 0x01, 0x02], TransactionProtocol::Igra),
        ];
        for (payload, expected) in cases {
            assert_eq!(detect_protocol(payload, &[]), Some(expected));
        }
    }

    #[test]
    fn detects_marker_in_binary_payload() {
        let mut payload = vec![0xff, 0xfe];
        payload.extend_from_slice(b"ciph_msg");
        assert_eq!(
            detect_protocol(&payload, &[]),
            Some(TransactionProtocol::Kasia)
        );
    }

    #[test]
    fn igra_boundary_values() {
        assert_eq!(
            detect_protocol(&[0x91], &[]),
            Some(TransactionProtocol::Igra)
        );
        assert_eq!(
            detect_protocol(&[0x97], &[]),
            Some(TransactionProtocol::Igra)
        );
        assert_eq!(detect_protocol(&[0x90], &[]), None);
        assert_eq!(detect_protocol(&[0x98], &[]), None);
    }

    #[test]
    fn kasia_wins_over_kasplex() {
        assert_eq!(
            detect_protocol(b"ciph_msg kasplex data", &[]),
            Some(TransactionProtocol::Kasia)
        );
    }

    #[test]
    fn detects_inscription_markers() {
        let krc = push(b"kasplex");
        let kspr = push(b"kspr");
        let kns = push(b"kns");
        assert_eq!(
            detect_protocol(&[], &[&krc]),
            Some(TransactionProtocol::Krc)
        );
        assert_eq!(
            detect_protocol(&[], &[&kspr]),
            Some(TransactionProtocol::Krc)
        );
        assert_eq!(
            detect_protocol(&[], &[&kns]),
            Some(TransactionProtocol::Kns)
        );
    }

    #[test]
    fn detects_inscription_inside_redeem_script() {
        // Real inscriptions: <sig> OP_PUSHDATA1 <redeem: <pubkey> OP_CHECKSIG OP_0 OP_IF "kasplex" … OP_ENDIF>
        let mut redeem = push(&[0x11; 32]);
        redeem.extend([OP_CHECKSIG, 0x00, 0x63]);
        redeem.extend(push(b"kasplex"));
        redeem.extend(push(br#"{"p":"krc-20","op":"mint","tick":"TEST"}"#));
        redeem.push(0x68);
        let mut sig = push(&[0x22; 65]);
        sig.extend([0x4c, redeem.len() as u8]);
        sig.extend(&redeem);
        assert_eq!(
            detect_protocol(&[], &[&sig]),
            Some(TransactionProtocol::Krc)
        );
    }

    #[test]
    fn marker_must_be_an_exact_push() {
        let script = push(b"not kasplex");
        assert_eq!(detect_protocol(&[], &[&script]), None);
    }

    #[test]
    fn op_pushdata2_inscription() {
        let mut script = vec![0x4d, 3, 0];
        script.extend_from_slice(b"kns");
        assert_eq!(
            detect_protocol(&[], &[&script]),
            Some(TransactionProtocol::Kns)
        );
    }

    #[test]
    fn protocol_names_and_codes_round_trip() {
        for p in TransactionProtocol::ALL {
            assert_eq!(TransactionProtocol::from_slug(p.slug()), Some(p));
            assert_eq!(TransactionProtocol::from_label(p.label()), Some(p));
            assert_eq!(TransactionProtocol::from_code(p.code()), Some(p));
        }
        assert_eq!(TransactionProtocol::from_slug("KNS"), None);
        assert_eq!(TransactionProtocol::from_code(0), None);
        let mut codes: Vec<u8> = TransactionProtocol::ALL.iter().map(|p| p.code()).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), TransactionProtocol::ALL.len());
    }

    #[test]
    fn standard_tx_has_no_protocol() {
        assert_eq!(detect_protocol(&[], &[]), None);
        assert_eq!(detect_protocol(b"random data", &[]), None);
    }

    // --- Script classes ---

    #[test]
    fn classifies_standard_scripts() {
        let mut p2pk = vec![OP_DATA_32];
        p2pk.extend([0x11; 32]);
        p2pk.push(OP_CHECKSIG);
        let mut ecdsa = vec![OP_DATA_33];
        ecdsa.extend([0x22; 33]);
        ecdsa.push(OP_CHECKSIG_ECDSA);
        let mut p2sh = vec![OP_BLAKE2B, OP_DATA_32];
        p2sh.extend([0x33; 32]);
        p2sh.push(OP_EQUAL);

        assert_eq!(script_class(&p2pk), ScriptClass::PubKey);
        assert_eq!(script_class(&ecdsa), ScriptClass::PubKeyEcdsa);
        assert_eq!(script_class(&p2sh), ScriptClass::ScriptHash);
        assert_eq!(script_class(&[0x00]), ScriptClass::NonStandard);
        assert_eq!(script_class(&p2pk[..33]), ScriptClass::NonStandard);
    }

    #[test]
    fn script_class_names_and_codes_round_trip() {
        for c in ScriptClass::ALL {
            assert_eq!(ScriptClass::from_slug(c.slug()), Some(c));
            assert_eq!(ScriptClass::from_code(c.code()), Some(c));
        }
        assert_eq!(ScriptClass::from_code(9), None);
    }

    #[test]
    fn script_address_for_standard_classes() {
        let mut p2pk = vec![OP_DATA_32];
        p2pk.extend([0x11; 32]);
        p2pk.push(OP_CHECKSIG);
        let a = script_address(&p2pk, Prefix::Mainnet).unwrap();
        assert_eq!((a.prefix, a.version), (Prefix::Mainnet, Version::PubKey));
        assert_eq!(a.payload.as_ref(), &[0x11; 32]);
        assert_eq!(
            a,
            Address::new(Prefix::Mainnet, Version::PubKey, &[0x11; 32])
        );

        let mut ecdsa = vec![OP_DATA_33];
        ecdsa.extend([0x22; 33]);
        ecdsa.push(OP_CHECKSIG_ECDSA);
        let a = script_address(&ecdsa, Prefix::Testnet).unwrap();
        assert_eq!(
            (a.prefix, a.version),
            (Prefix::Testnet, Version::PubKeyECDSA)
        );
        assert_eq!(a.payload.len(), 33);

        let mut p2sh = vec![OP_BLAKE2B, OP_DATA_32];
        p2sh.extend([0x33; 32]);
        p2sh.push(OP_EQUAL);
        let a = script_address(&p2sh, Prefix::Mainnet).unwrap();
        assert_eq!(a.version, Version::ScriptHash);
        assert_eq!(a.payload.as_ref(), &[0x33; 32]);

        assert_eq!(script_address(&[0x51], Prefix::Mainnet), None);
    }

    // --- Difficulty ---

    #[test]
    fn difficulty_of_the_pow_limit_is_one() {
        // The mainnet proof-of-work limit, `0x207fffff`: a target of 2^255 - 1.
        let d = difficulty_from_bits(0x207f_ffff);
        assert!((d - 1.0).abs() < 1e-6, "{d}");
        // Halving the target doubles the difficulty.
        let d2 = difficulty_from_bits(0x203f_ffff);
        assert!((d2 - 2.0).abs() < 1e-6, "{d2}");
        // The smallest target, 1 (exponent 3, mantissa 1), is the hardest; a smaller
        // exponent shifts the mantissa away entirely.
        assert!(difficulty_from_bits(0x0300_0001) > 1e76);
        assert_eq!(difficulty_from_bits(0x0100_0001), 0.0);
        assert_eq!(difficulty_from_bits(0), 0.0);
        assert_eq!(difficulty_from_bits(0x2080_0000), 0.0);
    }

    #[test]
    fn difficulty_from_bits_is_max_target_over_target() {
        // The node's formula (`MAX_DIFFICULTY_TARGET_AS_F64 / target.as_f64()`) on a
        // typical mainnet encoding: mantissa 0x3a2e9c, exponent 0x1c.
        let d = difficulty_from_bits(0x1c3a_2e9c);
        let target = 0x3a2e9c as f64 * 2f64.powi(8 * (0x1c - 3));
        assert!((d - 5.78960446186581e76 / target).abs() / d < 1e-12);
        assert!((d - 9_448_887_500.97).abs() < 1.0, "{d}");
    }

    // --- Opcodes ---

    #[test]
    fn introspection_range_and_holes() {
        for op in [0xb2, 0xb4, 0xc9, 0xcb, 0xcf, 0xd6] {
            assert!(output_script_opcodes(&[op]).introspection, "{op:#x}");
        }
        for op in [0xca, 0xcd, 0xce, 0xd7, 0xda, 0xb1] {
            assert!(!output_script_opcodes(&[op]).introspection, "{op:#x}");
        }
    }

    #[test]
    fn opcode_bytes_inside_pushes_are_ignored() {
        assert_eq!(
            output_script_opcodes(&[0x02, 0xb4, 0xd4, OP_CHECKSIG]),
            OpcodeUsage::default()
        );
    }

    #[test]
    fn seqcommit_is_also_introspection() {
        let usage = output_script_opcodes(&[OP_CHAINBLOCK_SEQCOMMIT, OP_CHECKSIG]);
        assert!(usage.chainblock_seqcommit);
        assert!(usage.introspection);
        assert!(!usage.zk_precompile());
    }

    #[test]
    fn redeem_reveal_detects_introspection() {
        let sig = p2sh_sig(&[&[0xaa, 0xbb, 0xcc]], &[0xb4, OP_CHECKSIG]);
        assert!(redeem_script_opcodes(&sig).introspection);
        let plain = p2sh_sig(&[], &[OP_CHECKSIG]);
        assert!(!redeem_script_opcodes(&plain).introspection);
    }

    #[test]
    fn zk_tag_from_witness_push() {
        let groth = p2sh_sig(&[&[0xaa], &[ZK_TAG_GROTH16]], &[OP_ZK_PRECOMPILE]);
        let r0 = p2sh_sig(&[&[0xaa], &[ZK_TAG_R0SUCCINCT]], &[OP_ZK_PRECOMPILE]);
        let untagged = p2sh_sig(&[&[0xaa, 0xbb]], &[OP_ZK_PRECOMPILE]);

        let g = redeem_script_opcodes(&groth);
        assert!(g.zk_groth16 && !g.zk_r0succinct && !g.zk_unknown);
        assert!(!g.introspection);
        assert!(redeem_script_opcodes(&r0).zk_r0succinct);
        assert!(redeem_script_opcodes(&untagged).zk_unknown);
    }

    #[test]
    fn zk_tag_inline_in_redeem() {
        let sig = p2sh_sig(&[&[0xaa]], &[0x01, ZK_TAG_GROTH16, OP_ZK_PRECOMPILE]);
        let usage = redeem_script_opcodes(&sig);
        assert!(usage.zk_groth16 && !usage.zk_unknown);
    }

    // --- Coinbase ---

    fn coinbase_payload(script: &[u8], extra: &[u8]) -> Vec<u8> {
        let mut p = vec![0u8; 18];
        p.push(script.len() as u8);
        p.extend_from_slice(script);
        p.extend_from_slice(extra);
        p
    }

    #[test]
    fn parses_node_version() {
        let script = [OP_DATA_32; 34];
        let payload = coinbase_payload(&script, b"1.0.1/kaspa-miner/pool");
        assert_eq!(coinbase_node_version(&payload).as_deref(), Some("1.0.1"));
        assert_eq!(
            coinbase_miner_tag(&payload).as_deref(),
            Some("kaspa-miner/pool")
        );
        assert_eq!(
            coinbase_miner_tag(&coinbase_payload(&script, b"1.0.1/ \x00")),
            None
        );
        let bare = coinbase_payload(&script, b"0.17.2");
        assert_eq!(coinbase_miner_tag(&bare), None);
        assert_eq!(coinbase_node_version(&bare).as_deref(), Some("0.17.2"));
        let empty = coinbase_payload(&script, b"");
        assert_eq!(coinbase_node_version(&empty).as_deref(), Some(""));
    }

    #[test]
    fn coinbase_payload_fields_and_miner_address() {
        let mut p = Vec::new();
        p.extend_from_slice(&1234u64.to_le_bytes());
        p.extend_from_slice(&50_000_000_000u64.to_le_bytes());
        p.extend_from_slice(&0u16.to_le_bytes());
        let mut script = vec![OP_DATA_32];
        script.extend([0x44; 32]);
        script.push(OP_CHECKSIG);
        p.push(script.len() as u8);
        p.extend_from_slice(&script);
        p.extend_from_slice(b"1.2.3/pool-x");
        let parsed = parse_coinbase_payload(&p).unwrap();
        assert_eq!((parsed.blue_score, parsed.subsidy), (1234, 50_000_000_000));
        assert_eq!(parsed.script_version, 0);
        assert_eq!(parsed.script, &script[..]);
        assert_eq!(parsed.node_version(), "1.2.3");
        assert_eq!(parsed.miner_tag().as_deref(), Some("pool-x"));
        assert_eq!(
            coinbase_miner_address(&p, Prefix::Mainnet),
            Some(Address::new(Prefix::Mainnet, Version::PubKey, &[0x44; 32]))
        );
        assert_eq!(coinbase_miner_address(&[0u8; 10], Prefix::Mainnet), None);
    }

    #[test]
    fn opcode_usage_bits_round_trip() {
        let usage = OpcodeUsage {
            introspection: true,
            chainblock_seqcommit: false,
            zk_groth16: true,
            zk_r0succinct: false,
            zk_unknown: true,
        };
        assert_eq!(usage.to_bits(), 0b10101);
        assert_eq!(OpcodeUsage::from_bits(usage.to_bits()), usage);
        assert_eq!(OpcodeUsage::from_bits(0), OpcodeUsage::default());
    }

    #[test]
    fn rejects_malformed_coinbase_payloads() {
        assert_eq!(coinbase_node_version(&[0u8; 10]), None);
        // Script length runs past the end
        let mut short = vec![0u8; 18];
        short.push(40);
        assert_eq!(coinbase_node_version(&short), None);
        // P2SH miner script
        let p2sh = coinbase_payload(&[OP_BLAKE2B, 0x20], b"1.0.0");
        assert_eq!(coinbase_node_version(&p2sh), None);
    }
}
