//! The Explorer tab's model: browser-like sub tabs over block, address and transaction
//! pages, the data those pages show, and the cache the controller fills
//! (`UiCommand::ExplorerLoad`).
//!
//! A sub tab holds a page and its back/forward history. Pages are loaded once into
//! `ExplorerState::cache` (bounded, oldest evicted) and shared by every tab showing them,
//! so going back is instant. A 64-hex query is a `Lookup`: the controller tries it as a
//! block, then as a transaction, and `resolve` turns the tab's page into the one found.

use std::collections::{HashMap, VecDeque};

use anyhow::Result;
use kaspa_addresses::Address;
use kaspa_rpc_core::{RpcBlock, RpcMempoolEntry, RpcTransaction};

use crate::app::AddressView;
use crate::format::shorten_middle;
use crate::index::query::{self, TxDetail};
use crate::index::{IndexStore, parse_hex};
use crate::labels::OnlineEntry;
use crate::rpc::types::BlockRewardInfo;
use crate::tx_inspect::{
    TransactionProtocol, coinbase_miner_tag, coinbase_node_version, detect_protocol, script_class,
};

/// Pages a sub tab can show.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ExplorerPage {
    /// Search, the latest blocks and recently viewed pages.
    Home,
    Block(String),
    Address(String),
    Transaction {
        txid: String,
        /// A block the transaction is known to be in, for one neither the index nor the
        /// mempool has (e.g. opened from a block page).
        block: Option<String>,
    },
    /// A 64-hex id that may be a block hash or a transaction id. The controller finds
    /// out and [`ExplorerState::resolve`] replaces it with the page found.
    Lookup(String),
}

impl ExplorerPage {
    pub fn transaction(txid: &str) -> Self {
        Self::Transaction {
            txid: txid.to_string(),
            block: None,
        }
    }

    /// Short tab title: the page kind and a shortened id.
    pub fn title(&self) -> String {
        match self {
            Self::Home => "Home".to_string(),
            Self::Block(hash) => format!("Block {}", shorten_middle(hash, 11)),
            Self::Address(addr) => shorten_middle(addr, 17),
            Self::Transaction { txid, .. } => format!("Tx {}", shorten_middle(txid, 11)),
            Self::Lookup(id) => shorten_middle(id, 15),
        }
    }

    /// The id the page is about, for the search field.
    pub fn query(&self) -> &str {
        match self {
            Self::Home => "",
            Self::Block(hash) | Self::Address(hash) | Self::Lookup(hash) => hash,
            Self::Transaction { txid, .. } => txid,
        }
    }

    /// Whether the controller has anything to load for it.
    pub fn is_loadable(&self) -> bool {
        !matches!(self, Self::Home)
    }
}

/// What a search field entry is: an address, or a block or transaction id (64 hex
/// characters, with or without `0x`). Anything else is `None` (a label search, say).
pub fn parse_query(input: &str) -> Option<ExplorerPage> {
    let s = input.trim();
    if s.is_empty() {
        return None;
    }
    if Address::try_from(s).is_ok() {
        return Some(ExplorerPage::Address(s.to_string()));
    }
    let hex = s.strip_prefix("0x").unwrap_or(s);
    if hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Some(ExplorerPage::Lookup(hex.to_ascii_lowercase()));
    }
    None
}

/// Back and forward entries kept per tab.
const HISTORY_MAX: usize = 50;
/// The info pane's tab id (it isn't in `ExplorerState::tabs`, so no sub tab has it).
const PANE_TAB_ID: u64 = u64::MAX;

/// One browser-like sub tab: its page and history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExplorerTab {
    pub id: u64,
    pub page: ExplorerPage,
    /// Pages behind the current one, oldest first.
    pub back: Vec<ExplorerPage>,
    /// Pages ahead after going back, the next one last.
    pub forward: Vec<ExplorerPage>,
}

impl ExplorerTab {
    fn new(id: u64, page: ExplorerPage) -> Self {
        Self {
            id,
            page,
            back: Vec::new(),
            forward: Vec::new(),
        }
    }

    /// Go to `page`, pushing the current one onto the back history.
    pub fn navigate(&mut self, page: ExplorerPage) {
        if page == self.page {
            return;
        }
        let previous = std::mem::replace(&mut self.page, page);
        self.back.push(previous);
        if self.back.len() > HISTORY_MAX {
            self.back.remove(0);
        }
        self.forward.clear();
    }

    /// Swap the current page without a history entry (a lookup resolving to its target).
    pub fn replace(&mut self, page: ExplorerPage) {
        self.page = page;
    }

    pub fn go_back(&mut self) -> bool {
        let Some(previous) = self.back.pop() else {
            return false;
        };
        let current = std::mem::replace(&mut self.page, previous);
        self.forward.push(current);
        true
    }

    pub fn go_forward(&mut self) -> bool {
        let Some(next) = self.forward.pop() else {
            return false;
        };
        let current = std::mem::replace(&mut self.page, next);
        self.back.push(current);
        true
    }
}

/// How a page's load is going.
#[derive(Debug, Clone, PartialEq)]
pub enum PageLoad {
    Loading,
    Ready(Box<PageData>),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum PageData {
    Block(BlockView),
    Address(AddressPageData),
    Transaction(TxView),
    /// A lookup that turned out to be this page (which is cached on its own).
    Redirect(ExplorerPage),
}

/// An address page (the info pane's and the Explorer's): the view plus what the page added.
#[derive(Debug, Clone, PartialEq)]
pub struct AddressPageData {
    pub view: AddressView,
    /// An older page of transactions is on its way (`UiCommand::AddressPage`).
    pub loading_more: bool,
    /// What the online label sources said, once asked.
    pub online_result: Option<Vec<OnlineEntry>>,
    /// A later request (more rows, the online lookup) failed.
    pub error: Option<String>,
}

/// Pages kept loaded. The oldest not on show in any tab is evicted beyond this.
pub const CACHE_MAX: usize = 32;
/// Recently viewed pages remembered for the Home page.
pub const RECENT_MAX: usize = 12;

/// The Explorer tab: its sub tabs and the pages loaded for them.
#[derive(Debug, Clone, PartialEq)]
pub struct ExplorerState {
    pub tabs: Vec<ExplorerTab>,
    /// Index into `tabs`.
    pub active: usize,
    next_id: u64,
    /// The info pane: the pane on the right that a click on any address, block hash
    /// or transaction id outside the Explorer opens, on any tab. A tab of its own (not
    /// in `tabs`) with its page and back/forward history; the page is loaded through
    /// the same cache as the tabs' pages, and never evicted while shown.
    pub pane: Option<ExplorerTab>,
    pub cache: HashMap<ExplorerPage, PageLoad>,
    /// Cached pages in order of last visit, oldest first.
    order: VecDeque<ExplorerPage>,
    /// Pages visited, newest first (no Home, no unresolved lookups).
    recent: VecDeque<ExplorerPage>,
}

impl Default for ExplorerState {
    fn default() -> Self {
        Self {
            tabs: vec![ExplorerTab::new(0, ExplorerPage::Home)],
            active: 0,
            next_id: 1,
            pane: None,
            cache: HashMap::new(),
            order: VecDeque::new(),
            recent: VecDeque::new(),
        }
    }
}

impl ExplorerState {
    pub fn active_tab(&self) -> &ExplorerTab {
        &self.tabs[self.active.min(self.tabs.len() - 1)]
    }

    pub fn active_tab_mut(&mut self) -> &mut ExplorerTab {
        let i = self.active.min(self.tabs.len() - 1);
        &mut self.tabs[i]
    }

    /// Open `page` in a new tab after the active one and switch to it. Returns its id.
    pub fn open_tab(&mut self, page: ExplorerPage) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let at = (self.active + 1).min(self.tabs.len());
        self.tabs.insert(at, ExplorerTab::new(id, page.clone()));
        self.active = at;
        self.visited(page);
        id
    }

    /// Close the tab at `index`. The last tab isn't closed but reset to Home.
    pub fn close_tab(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        if self.tabs.len() == 1 {
            self.tabs[0] = ExplorerTab::new(self.tabs[0].id, ExplorerPage::Home);
            return;
        }
        self.tabs.remove(index);
        if self.active > index || self.active >= self.tabs.len() {
            self.active = self.active.saturating_sub(1);
        }
    }

    /// Navigate the active tab to `page`.
    pub fn navigate(&mut self, page: ExplorerPage) {
        self.active_tab_mut().navigate(page.clone());
        self.visited(page);
    }

    /// The page the info pane shows, if it is open.
    pub fn pane_page(&self) -> Option<&ExplorerPage> {
        self.pane.as_ref().map(|pane| &pane.page)
    }

    /// Show `page` in the info pane: opened with it, or navigated to it (the page it
    /// showed goes into the pane's back history).
    pub fn open_pane(&mut self, page: ExplorerPage) {
        match &mut self.pane {
            Some(pane) => pane.navigate(page.clone()),
            None => self.pane = Some(ExplorerTab::new(PANE_TAB_ID, page.clone())),
        }
        self.visited(page);
    }

    /// Close the info pane; its history goes with it.
    pub fn close_pane(&mut self) {
        self.pane = None;
    }

    pub fn pane_back(&mut self) {
        if let Some(pane) = &mut self.pane
            && pane.go_back()
        {
            let page = pane.page.clone();
            self.visited(page);
        }
    }

    pub fn pane_forward(&mut self) {
        if let Some(pane) = &mut self.pane
            && pane.go_forward()
        {
            let page = pane.page.clone();
            self.visited(page);
        }
    }

    pub fn go_back(&mut self) {
        if self.active_tab_mut().go_back() {
            let page = self.active_tab().page.clone();
            self.visited(page);
        }
    }

    pub fn go_forward(&mut self) {
        if self.active_tab_mut().go_forward() {
            let page = self.active_tab().page.clone();
            self.visited(page);
        }
    }

    /// Note a visit: the page moves to the newest end of the cache order and, unless
    /// it is Home or a lookup, to the front of the recent list.
    fn visited(&mut self, page: ExplorerPage) {
        self.order.retain(|p| *p != page);
        self.order.push_back(page.clone());
        if matches!(page, ExplorerPage::Home | ExplorerPage::Lookup(_)) {
            return;
        }
        self.recent.retain(|p| *p != page);
        self.recent.push_front(page);
        self.recent.truncate(RECENT_MAX);
    }

    /// Recently viewed pages, newest first.
    pub fn recent(&self) -> impl Iterator<Item = &ExplorerPage> {
        self.recent.iter()
    }

    pub fn load(&self, page: &ExplorerPage) -> Option<&PageLoad> {
        self.cache.get(page)
    }

    /// Whether the controller should be asked for `page`: it is loadable and nothing is
    /// cached for it (a failed load stays until a reload is asked for).
    pub fn needs_load(&self, page: &ExplorerPage) -> bool {
        page.is_loadable() && !self.cache.contains_key(page)
    }

    /// Mark `page` as loading (the frontend then sends `UiCommand::ExplorerLoad`).
    pub fn start_loading(&mut self, page: ExplorerPage) {
        if !self.order.contains(&page) {
            self.order.push_back(page.clone());
        }
        self.cache.insert(page, PageLoad::Loading);
        self.evict();
    }

    /// Store a load's outcome.
    pub fn set_loaded(&mut self, page: ExplorerPage, result: Result<PageData, String>) {
        let load = match result {
            Ok(data) => PageLoad::Ready(Box::new(data)),
            Err(e) => PageLoad::Failed(e),
        };
        if !self.order.contains(&page) {
            self.order.push_back(page.clone());
        }
        self.cache.insert(page, load);
        self.evict();
    }

    /// A lookup turned out to be `target` with `data`: cache the target, point the
    /// lookup at it, and move every tab (and the pane) showing the lookup to the target.
    pub fn resolve(&mut self, lookup: ExplorerPage, target: ExplorerPage, data: PageData) {
        self.set_loaded(target.clone(), Ok(data));
        self.set_loaded(lookup.clone(), Ok(PageData::Redirect(target.clone())));
        for tab in self.tabs.iter_mut().chain(&mut self.pane) {
            if tab.page == lookup {
                tab.replace(target.clone());
            }
        }
        self.visited(target);
    }

    /// Drop every loaded page (on disconnect); tabs keep their pages and reload.
    pub fn clear_cache(&mut self) {
        self.cache.clear();
        self.order.clear();
    }

    /// The address page's data for `address`, if loaded.
    pub fn address_page_mut(&mut self, address: &str) -> Option<&mut AddressPageData> {
        match self
            .cache
            .get_mut(&ExplorerPage::Address(address.to_string()))
        {
            Some(PageLoad::Ready(data)) => match &mut **data {
                PageData::Address(data) => Some(data),
                _ => None,
            },
            _ => None,
        }
    }

    /// Evict the oldest cached pages neither a tab nor the pane shows until the cache
    /// fits.
    fn evict(&mut self) {
        while self.cache.len() > CACHE_MAX {
            let shown = |p: &ExplorerPage| self.tabs.iter().chain(&self.pane).any(|t| t.page == *p);
            let Some(i) = self.order.iter().position(|p| !shown(p)) else {
                return;
            };
            if let Some(page) = self.order.remove(i) {
                self.cache.remove(&page);
            }
        }
    }
}

// --- Block pages ---

/// What a block page shows, from `get_block` (and `get_block_reward_info`), with the
/// index's acceptance data folded in by [`enrich_block`].
#[derive(Debug, Clone, PartialEq)]
pub struct BlockView {
    pub hash: String,
    pub version: u16,
    pub timestamp_ms: u64,
    pub bits: u32,
    pub nonce: u64,
    pub daa_score: u64,
    pub blue_score: u64,
    /// Hex.
    pub blue_work: String,
    pub difficulty: Option<f64>,
    /// Direct (level 0) parents.
    pub parents: Vec<String>,
    pub parent_levels: usize,
    pub hash_merkle_root: String,
    pub accepted_id_merkle_root: String,
    pub utxo_commitment: String,
    pub pruning_point: String,
    pub selected_parent: Option<String>,
    pub children: Vec<String>,
    pub merge_set_blues: Vec<String>,
    pub merge_set_reds: Vec<String>,
    pub is_chain_block: Option<bool>,
    pub is_header_only: bool,
    pub miner: Option<MinerInfo>,
    pub transactions: Vec<BlockTx>,
    /// Blue/red, confirmations and the reward, when the node answered.
    pub reward: Option<BlockRewardInfo>,
}

/// The miner, from the coinbase: its payout address and what its payload says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MinerInfo {
    pub address: Option<String>,
    pub node_version: Option<String>,
    pub tag: Option<String>,
}

/// One transaction of a block, as its list shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockTx {
    pub txid: String,
    pub is_coinbase: bool,
    pub input_count: usize,
    pub output_count: usize,
    /// Sompi over the outputs.
    pub output_total: u64,
    /// The first output's address.
    pub recipient: Option<String>,
    pub mass: u64,
    pub protocol: Option<TransactionProtocol>,
    /// Filled from the index: the chain block that accepted it and the fee.
    pub accepted: Option<Accepted>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accepted {
    pub block: String,
    pub fee: Option<u64>,
}

/// A transaction's id from its verbose data.
fn txid_of(tx: &RpcTransaction) -> Option<String> {
    tx.verbose_data
        .as_ref()
        .map(|v| v.transaction_id.to_string())
}

fn output_address(tx: &RpcTransaction, i: usize) -> Option<String> {
    tx.outputs
        .get(i)?
        .verbose_data
        .as_ref()
        .map(|v| v.script_public_key_address.to_string())
}

fn protocol_of(tx: &RpcTransaction) -> Option<TransactionProtocol> {
    let scripts: Vec<&[u8]> = tx
        .inputs
        .iter()
        .map(|i| i.signature_script.as_slice())
        .collect();
    detect_protocol(&tx.payload, &scripts)
}

fn mass_of(tx: &RpcTransaction) -> u64 {
    let compute = tx.verbose_data.as_ref().map_or(0, |v| v.compute_mass);
    tx.storage_mass.max(compute)
}

impl BlockView {
    pub fn from_rpc(block: &RpcBlock) -> Self {
        let header = &block.header;
        let verbose = block.verbose_data.as_ref();
        let hashes = |v: &[kaspa_rpc_core::RpcHash]| -> Vec<String> {
            v.iter().map(|h| h.to_string()).collect()
        };
        let transactions = block
            .transactions
            .iter()
            .map(|tx| BlockTx {
                txid: txid_of(tx).unwrap_or_default(),
                is_coinbase: tx.inputs.is_empty(),
                input_count: tx.inputs.len(),
                output_count: tx.outputs.len(),
                output_total: tx.outputs.iter().map(|o| o.value).sum(),
                recipient: output_address(tx, 0),
                mass: mass_of(tx),
                protocol: protocol_of(tx),
                accepted: None,
            })
            .collect();
        let miner = block
            .transactions
            .iter()
            .find(|tx| tx.inputs.is_empty())
            .map(|coinbase| MinerInfo {
                address: output_address(coinbase, 0),
                node_version: coinbase_node_version(&coinbase.payload),
                tag: coinbase_miner_tag(&coinbase.payload),
            });
        Self {
            hash: header.hash.to_string(),
            version: header.version,
            timestamp_ms: header.timestamp,
            bits: header.bits,
            nonce: header.nonce,
            daa_score: header.daa_score,
            blue_score: header.blue_score,
            blue_work: format!("{:x}", header.blue_work),
            difficulty: verbose.map(|v| v.difficulty),
            parents: hashes(header.direct_parents()),
            parent_levels: header.parents_by_level.len(),
            hash_merkle_root: header.hash_merkle_root.to_string(),
            accepted_id_merkle_root: header.accepted_id_merkle_root.to_string(),
            utxo_commitment: header.utxo_commitment.to_string(),
            pruning_point: header.pruning_point.to_string(),
            selected_parent: verbose.map(|v| v.selected_parent_hash.to_string()),
            children: verbose
                .map(|v| hashes(&v.children_hashes))
                .unwrap_or_default(),
            merge_set_blues: verbose
                .map(|v| hashes(&v.merge_set_blues_hashes))
                .unwrap_or_default(),
            merge_set_reds: verbose
                .map(|v| hashes(&v.merge_set_reds_hashes))
                .unwrap_or_default(),
            is_chain_block: verbose.map(|v| v.is_chain_block),
            is_header_only: verbose.is_some_and(|v| v.is_header_only),
            miner,
            transactions,
            reward: None,
        }
    }

    pub fn accepted_count(&self) -> usize {
        self.transactions
            .iter()
            .filter(|t| t.accepted.is_some())
            .count()
    }
}

/// Fill in which of the block's transactions the index saw accepted, and their fees.
pub fn enrich_block(store: &IndexStore, view: &mut BlockView) -> Result<()> {
    for tx in &mut view.transactions {
        let Some(id) = parse_hex(&tx.txid) else {
            continue;
        };
        if let Some(detail) = query::transaction(store, &id)? {
            tx.accepted = Some(Accepted {
                block: detail.accepting_block,
                fee: detail.fee,
            });
        }
    }
    Ok(())
}

// --- Transaction pages ---

/// What a transaction page shows, from the index, the mempool or a block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxView {
    pub txid: String,
    pub status: TxStatus,
    pub inputs: Vec<TxInputView>,
    pub outputs: Vec<TxOutputView>,
    /// Known once every input's amount is (the index, or [`enrich_tx`]).
    pub fee: Option<u64>,
    pub mass: u64,
    pub is_coinbase: bool,
    pub protocol: Option<TransactionProtocol>,
    // The index doesn't keep these; a node's transaction has them.
    pub version: Option<u16>,
    pub lock_time: Option<u64>,
    pub subnetwork_id: Option<String>,
    pub payload: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxStatus {
    /// Accepted by the selected chain, per the index.
    Accepted {
        block: String,
        daa_score: u64,
        time_ms: u64,
    },
    Mempool {
        is_orphan: bool,
    },
    /// In `block`, with no acceptance known to the index (not accepted yet, or outside
    /// the indexed window).
    InBlock {
        block: String,
        time_ms: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxInputView {
    pub prev_txid: String,
    pub prev_index: u32,
    /// The spent output's address and amount, when known.
    pub address: Option<String>,
    pub amount: Option<u64>,
    pub signature_script_len: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxOutputView {
    pub address: Option<String>,
    pub amount: u64,
    /// `tx_inspect::ScriptClass::label`, when the script is known.
    pub script_class: Option<&'static str>,
    /// How likely this is the sender's change, 0–100 (from the index; 0 otherwise).
    pub change: u8,
}

impl TxView {
    pub fn from_index(detail: TxDetail) -> Self {
        Self {
            txid: detail.txid,
            status: TxStatus::Accepted {
                block: detail.accepting_block,
                daa_score: detail.daa_score,
                time_ms: detail.time_ms,
            },
            inputs: detail
                .inputs
                .into_iter()
                .map(|i| TxInputView {
                    prev_txid: i.prev_txid,
                    prev_index: i.prev_index,
                    address: i.address,
                    amount: i.amount,
                    signature_script_len: 0,
                })
                .collect(),
            outputs: detail
                .outputs
                .into_iter()
                .map(|o| TxOutputView {
                    address: o.address,
                    amount: o.amount,
                    script_class: None,
                    change: o.change,
                })
                .collect(),
            fee: detail.fee,
            mass: detail.mass,
            is_coinbase: detail.is_coinbase,
            protocol: detail.protocol,
            version: None,
            lock_time: None,
            subnetwork_id: None,
            payload: None,
        }
    }

    /// A node's transaction with the given status; inputs carry outpoints only until
    /// [`enrich_tx`] resolves them.
    pub fn from_rpc(tx: &RpcTransaction, status: TxStatus, fee: Option<u64>) -> Self {
        let is_coinbase = tx.inputs.is_empty();
        Self {
            txid: txid_of(tx).unwrap_or_default(),
            status,
            inputs: tx
                .inputs
                .iter()
                .map(|i| TxInputView {
                    prev_txid: i.previous_outpoint.transaction_id.to_string(),
                    prev_index: i.previous_outpoint.index,
                    address: None,
                    amount: None,
                    signature_script_len: i.signature_script.len(),
                })
                .collect(),
            outputs: tx
                .outputs
                .iter()
                .enumerate()
                .map(|(i, o)| TxOutputView {
                    address: output_address(tx, i),
                    amount: o.value,
                    script_class: Some(script_class(o.script_public_key.script()).label()),
                    change: 0,
                })
                .collect(),
            fee: if is_coinbase { None } else { fee },
            mass: mass_of(tx),
            is_coinbase,
            protocol: protocol_of(tx),
            version: Some(tx.version),
            lock_time: Some(tx.lock_time),
            subnetwork_id: Some(tx.subnetwork_id.to_string()),
            payload: Some(tx.payload.clone()),
        }
    }

    pub fn from_mempool(entry: &RpcMempoolEntry) -> Self {
        Self::from_rpc(
            &entry.transaction,
            TxStatus::Mempool {
                is_orphan: entry.is_orphan,
            },
            Some(entry.fee),
        )
    }

    /// The transaction `txid` of `block`, if it holds it.
    pub fn from_block(block: &RpcBlock, txid: &str) -> Option<Self> {
        let tx = block
            .transactions
            .iter()
            .find(|tx| txid_of(tx).as_deref() == Some(txid))?;
        Some(Self::from_rpc(
            tx,
            TxStatus::InBlock {
                block: block.header.hash.to_string(),
                time_ms: block.header.timestamp,
            },
            None,
        ))
    }

    pub fn input_total(&self) -> Option<u64> {
        self.inputs.iter().map(|i| i.amount).sum()
    }

    pub fn output_total(&self) -> u64 {
        self.outputs.iter().map(|o| o.amount).sum()
    }

    /// The payload as text, if it is printable ASCII.
    pub fn payload_text(&self) -> Option<String> {
        let payload = self.payload.as_ref()?;
        if payload.is_empty() || !payload.iter().all(|b| b.is_ascii_graphic() || *b == b' ') {
            return None;
        }
        Some(String::from_utf8_lossy(payload).into_owned())
    }
}

/// Resolve a node transaction's inputs (address and amount) from the spent outputs the
/// index holds, then the fee once every amount is known.
pub fn enrich_tx(store: &IndexStore, view: &mut TxView) -> Result<()> {
    for input in &mut view.inputs {
        if input.address.is_some() && input.amount.is_some() {
            continue;
        }
        let Some(prev) = parse_hex(&input.prev_txid) else {
            continue;
        };
        if let Some(detail) = query::transaction(store, &prev)?
            && let Some(output) = detail.outputs.get(input.prev_index as usize)
        {
            input.address = output.address.clone();
            input.amount = Some(output.amount);
        }
    }
    if view.fee.is_none()
        && !view.is_coinbase
        && let Some(total) = view.input_total()
    {
        view.fee = Some(total.saturating_sub(view.output_total()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::query::{TxInputDetail, TxOutputDetail};
    use kaspa_rpc_core::{
        RpcBlockVerboseData, RpcHash, RpcHeader, RpcScriptPublicKey, RpcSubnetworkId,
        RpcTransactionInput, RpcTransactionOutpoint, RpcTransactionOutput,
        RpcTransactionOutputVerboseData, RpcTransactionVerboseData,
    };

    fn h(c: char) -> String {
        c.to_string().repeat(64)
    }

    // --- Pages and tabs ---

    #[test]
    fn parse_query_tells_addresses_and_ids_apart() {
        let addr = crate::index::writer::testing::address(1).to_string();
        assert_eq!(
            parse_query(&format!(" {addr} ")),
            Some(ExplorerPage::Address(addr.clone()))
        );
        assert_eq!(
            parse_query(&format!("  0x{}  ", "A".repeat(64))),
            Some(ExplorerPage::Lookup(h('a')))
        );
        assert_eq!(parse_query("Bybit"), None);
        assert_eq!(parse_query(""), None);
        assert_eq!(parse_query(&"a".repeat(63)), None);
        assert_eq!(parse_query("kaspa:notanaddress"), None);
    }

    #[test]
    fn tab_history_goes_back_and_forward() {
        let mut tab = ExplorerTab::new(0, ExplorerPage::Home);
        tab.navigate(ExplorerPage::Block(h('a')));
        tab.navigate(ExplorerPage::Block(h('a'))); // same page: no entry
        tab.navigate(ExplorerPage::Address("kaspa:x".into()));
        assert_eq!(tab.back.len(), 2);
        assert!(tab.go_back());
        assert_eq!(tab.page, ExplorerPage::Block(h('a')));
        assert_eq!(tab.forward.len(), 1);
        assert!(tab.go_forward());
        assert_eq!(tab.page, ExplorerPage::Address("kaspa:x".into()));
        assert!(!tab.go_forward());
        assert!(tab.go_back());
        // A new navigation drops the forward history.
        tab.navigate(ExplorerPage::transaction(&h('b')));
        assert!(tab.forward.is_empty());
        assert!(tab.go_back() && tab.go_back());
        assert_eq!(tab.page, ExplorerPage::Home);
        assert!(!tab.go_back());
    }

    #[test]
    fn tabs_open_after_the_active_one_and_close_sensibly() {
        let mut state = ExplorerState::default();
        assert_eq!(state.tabs.len(), 1);
        let a = state.open_tab(ExplorerPage::Block(h('a')));
        let b = state.open_tab(ExplorerPage::Block(h('b')));
        assert_ne!(a, b);
        state.active = 1;
        let c = state.open_tab(ExplorerPage::Block(h('c')));
        let ids: Vec<u64> = state.tabs.iter().map(|t| t.id).collect();
        assert_eq!(ids, vec![0, a, c, b]);
        assert_eq!(state.active, 2);

        // Closing a tab before the active one keeps the same tab active.
        state.close_tab(0);
        assert_eq!(state.active, 1);
        assert_eq!(state.active_tab().id, c);
        // Closing the last tab in the row moves to the one before it.
        state.active = 2;
        state.close_tab(2);
        assert_eq!(state.active, 1);
        state.close_tab(1);
        state.close_tab(0);
        // The last tab is reset rather than removed.
        assert_eq!(state.tabs.len(), 1);
        assert_eq!(state.active_tab().page, ExplorerPage::Home);
        state.close_tab(7); // out of range: ignored
        assert_eq!(state.tabs.len(), 1);
    }

    #[test]
    fn cache_loads_resolves_and_evicts() {
        let mut state = ExplorerState::default();
        let page = ExplorerPage::Lookup(h('a'));
        state.navigate(page.clone());
        assert!(state.needs_load(&page));
        assert!(!state.needs_load(&ExplorerPage::Home));
        state.start_loading(page.clone());
        assert_eq!(state.load(&page), Some(&PageLoad::Loading));
        assert!(!state.needs_load(&page));

        let target = ExplorerPage::transaction(&h('a'));
        let tx = TxView::from_index(TxDetail {
            txid: h('a'),
            accepting_block: h('b'),
            daa_score: 1,
            time_ms: 2,
            inputs: vec![],
            outputs: vec![],
            fee: None,
            mass: 0,
            is_coinbase: true,
            protocol: None,
        });
        state.resolve(page.clone(), target.clone(), PageData::Transaction(tx));
        assert_eq!(state.active_tab().page, target);
        assert_eq!(
            state.load(&page),
            Some(&PageLoad::Ready(Box::new(PageData::Redirect(
                target.clone()
            ))))
        );
        assert!(matches!(
            state.load(&target).map(|l| match l {
                PageLoad::Ready(data) => matches!(**data, PageData::Transaction(_)),
                _ => false,
            }),
            Some(true)
        ));
        // Lookups aren't "recent"; the resolved page is.
        assert_eq!(state.recent().collect::<Vec<_>>(), vec![&target]);

        state.set_loaded(ExplorerPage::Block(h('c')), Err("nope".into()));
        assert_eq!(
            state.load(&ExplorerPage::Block(h('c'))),
            Some(&PageLoad::Failed("nope".into()))
        );
        assert!(!state.needs_load(&ExplorerPage::Block(h('c'))));

        // Beyond the cap, the oldest page neither a tab nor the pane shows goes first.
        let paned = ExplorerPage::Block(h('d'));
        state.open_pane(paned.clone());
        state.set_loaded(paned.clone(), Err("x".into()));
        assert_eq!(
            state.recent().next(),
            Some(&paned),
            "a pane view is a visit"
        );
        for i in 0..CACHE_MAX {
            state.set_loaded(ExplorerPage::Block(format!("{i:064x}")), Err("x".into()));
        }
        assert_eq!(state.cache.len(), CACHE_MAX);
        assert!(state.load(&page).is_none(), "the lookup went first");
        assert!(state.load(&target).is_some(), "the shown page stays");
        assert!(state.load(&paned).is_some(), "the pane's page stays");
        state.close_pane();
        assert!(state.pane.is_none());

        state.clear_cache();
        assert!(state.cache.is_empty());
        assert_eq!(state.active_tab().page, target);
    }

    #[test]
    fn pane_has_its_own_history() {
        let mut state = ExplorerState::default();
        let (a, b, c) = (
            ExplorerPage::Block(h('a')),
            ExplorerPage::Address("kaspa:b".to_string()),
            ExplorerPage::transaction(&h('c')),
        );
        state.pane_back();
        assert!(state.pane.is_none(), "nothing to go back to");
        state.open_pane(a.clone());
        state.open_pane(b.clone());
        state.open_pane(c.clone());
        assert_eq!(state.pane_page(), Some(&c));
        state.pane_back();
        assert_eq!(state.pane_page(), Some(&b));
        state.pane_back();
        assert_eq!(state.pane_page(), Some(&a));
        state.pane_back();
        assert_eq!(state.pane_page(), Some(&a), "the history's end");
        state.pane_forward();
        assert_eq!(state.pane_page(), Some(&b));
        assert_eq!(state.recent().next(), Some(&b), "going back is a visit");
        // A new page from there drops the forward history.
        state.open_pane(a.clone());
        state.pane_forward();
        assert_eq!(state.pane_page(), Some(&a));
        assert_eq!(
            state.active_tab().page,
            ExplorerPage::Home,
            "the tabs are untouched"
        );
        // Closing forgets the history.
        state.close_pane();
        state.open_pane(c.clone());
        state.pane_back();
        assert_eq!(state.pane_page(), Some(&c));
    }

    #[test]
    fn page_titles_and_queries() {
        let block = ExplorerPage::Block(h('a'));
        assert_eq!(block.title(), "Block aaaa...aaaa");
        assert_eq!(block.query(), h('a'));
        assert_eq!(ExplorerPage::Home.title(), "Home");
        assert_eq!(ExplorerPage::Home.query(), "");
        assert!(
            ExplorerPage::transaction(&h('b'))
                .title()
                .starts_with("Tx ")
        );
    }

    // --- Views ---

    fn rpc_tx(id: char, inputs: usize, outputs: &[u64]) -> RpcTransaction {
        let mut p2pk = vec![0x20];
        p2pk.extend([0x11; 32]);
        p2pk.push(0xac);
        RpcTransaction {
            version: 0,
            inputs: (0..inputs)
                .map(|i| RpcTransactionInput {
                    previous_outpoint: RpcTransactionOutpoint {
                        transaction_id: RpcHash::from_bytes([0xee; 32]),
                        index: i as u32,
                    },
                    signature_script: vec![1, 2, 3],
                    sequence: 0,
                    sig_op_count: 1,
                    compute_budget: 0,
                    verbose_data: None,
                })
                .collect(),
            outputs: outputs
                .iter()
                .map(|&value| RpcTransactionOutput {
                    value,
                    script_public_key: RpcScriptPublicKey::from_vec(0, p2pk.clone()),
                    verbose_data: Some(RpcTransactionOutputVerboseData {
                        script_public_key_type: kaspa_rpc_core::RpcScriptClass::PubKey,
                        script_public_key_address: crate::index::writer::testing::address(7),
                    }),
                    covenant: None,
                })
                .collect(),
            lock_time: 0,
            subnetwork_id: RpcSubnetworkId::from_byte(0),
            gas: 0,
            payload: vec![],
            storage_mass: 10,
            verbose_data: Some(RpcTransactionVerboseData {
                transaction_id: RpcHash::from_bytes([id as u8; 32]),
                hash: RpcHash::from_bytes([0; 32]),
                compute_mass: 20,
                block_hash: RpcHash::from_bytes([0; 32]),
                block_time: 0,
            }),
        }
    }

    fn rpc_block(txs: Vec<RpcTransaction>) -> RpcBlock {
        let hash = |b: u8| RpcHash::from_bytes([b; 32]);
        RpcBlock {
            header: RpcHeader {
                hash: hash(1),
                version: 1,
                parents_by_level: vec![vec![hash(2), hash(3)], vec![hash(4)]],
                hash_merkle_root: hash(5),
                accepted_id_merkle_root: hash(6),
                utxo_commitment: hash(7),
                timestamp: 1_700_000_000_000,
                bits: 0x1d00ffff,
                nonce: 42,
                daa_score: 100,
                blue_work: 255u64.into(),
                blue_score: 99,
                pruning_point: hash(8),
            },
            transactions: txs,
            verbose_data: Some(RpcBlockVerboseData {
                hash: hash(1),
                difficulty: 12.5,
                selected_parent_hash: hash(2),
                transaction_ids: vec![],
                is_header_only: false,
                blue_score: 99,
                children_hashes: vec![hash(9)],
                merge_set_blues_hashes: vec![hash(2), hash(3)],
                merge_set_reds_hashes: vec![],
                is_chain_block: true,
            }),
        }
    }

    #[test]
    fn block_view_from_rpc() {
        let mut coinbase = rpc_tx('c', 0, &[500]);
        let mut payload = vec![0u8; 18];
        payload.push(34);
        payload.extend([0x20; 34]);
        payload.extend_from_slice(b"1.0.1/pool-x");
        coinbase.payload = payload;
        let block = rpc_block(vec![coinbase, rpc_tx('d', 2, &[100, 50])]);
        let view = BlockView::from_rpc(&block);
        assert_eq!(view.hash, "01".repeat(32));
        assert_eq!(view.parents.len(), 2);
        assert_eq!(view.parent_levels, 2);
        assert_eq!(view.blue_work, "ff");
        assert_eq!(view.difficulty, Some(12.5));
        assert_eq!(view.is_chain_block, Some(true));
        assert_eq!(view.children, vec!["09".repeat(32)]);
        let miner = view.miner.as_ref().unwrap();
        assert_eq!(miner.node_version.as_deref(), Some("1.0.1"));
        assert_eq!(miner.tag.as_deref(), Some("pool-x"));
        assert!(miner.address.as_deref().unwrap().starts_with("kaspa:"));
        assert_eq!(view.transactions.len(), 2);
        assert!(view.transactions[0].is_coinbase);
        let tx = &view.transactions[1];
        assert_eq!(
            (tx.input_count, tx.output_count, tx.output_total),
            (2, 2, 150)
        );
        assert_eq!(tx.mass, 20, "the larger of storage and compute mass");
        assert_eq!(view.accepted_count(), 0);
    }

    #[test]
    fn tx_view_from_rpc_and_index() {
        let tx = rpc_tx('d', 1, &[100, 50]);
        let block = rpc_block(vec![tx.clone()]);
        let view = TxView::from_block(&block, &"64".repeat(32)).unwrap();
        assert_eq!(
            view.status,
            TxStatus::InBlock {
                block: "01".repeat(32),
                time_ms: 1_700_000_000_000
            }
        );
        assert_eq!(view.inputs.len(), 1);
        assert_eq!(view.inputs[0].prev_txid, "ee".repeat(32));
        assert_eq!(view.inputs[0].signature_script_len, 3);
        assert_eq!(view.outputs[0].script_class, Some("P2PK"));
        assert_eq!(view.output_total(), 150);
        assert_eq!(view.input_total(), None);
        assert_eq!(view.fee, None);
        assert!(TxView::from_block(&block, &h('f')).is_none());

        let entry = RpcMempoolEntry::new(7, tx, true);
        let view = TxView::from_mempool(&entry);
        assert_eq!(view.status, TxStatus::Mempool { is_orphan: true });
        assert_eq!(view.fee, Some(7));
        assert_eq!(view.payload_text(), None);

        let view = TxView::from_index(TxDetail {
            txid: h('a'),
            accepting_block: h('b'),
            daa_score: 5,
            time_ms: 6,
            inputs: vec![TxInputDetail {
                address: Some("kaspa:in".into()),
                amount: Some(200),
                prev_txid: h('c'),
                prev_index: 1,
            }],
            outputs: vec![TxOutputDetail {
                address: Some("kaspa:out".into()),
                amount: 150,
                change: 80,
            }],
            fee: Some(50),
            mass: 1,
            is_coinbase: false,
            protocol: None,
        });
        assert!(matches!(
            view.status,
            TxStatus::Accepted { daa_score: 5, .. }
        ));
        assert_eq!(view.input_total(), Some(200));
        assert_eq!(view.outputs[0].change, 80);
        assert_eq!(view.version, None);
    }

    #[test]
    fn payload_text_needs_printable_ascii() {
        let mut view = TxView::from_mempool(&RpcMempoolEntry::new(0, rpc_tx('a', 0, &[1]), false));
        view.payload = Some(b"hello kaspa".to_vec());
        assert_eq!(view.payload_text().as_deref(), Some("hello kaspa"));
        view.payload = Some(vec![0x91, 0x00]);
        assert_eq!(view.payload_text(), None);
    }
}
