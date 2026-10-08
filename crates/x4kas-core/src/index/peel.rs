//! Peel chains: a run of single-input, two-output spends in which each link spends the
//! previous link's remainder and pays a slice off to the side (a deposit, a payment).
//! Mixers, launderers and some exchange hot wallets move funds this way; a profile
//! notes which link of such a chain an address is, and the flow graph collapses the
//! pass-through addresses (`FlowGraph::collapse_chains`).
//!
//! The walk follows outpoints, so it is exact where the index has both transactions:
//! backwards through each link's spent `prev_txid`, forwards by finding the link-shaped
//! spend of either output. Only the last remainder, which nothing has spent yet, is
//! picked by its change score (`cluster::CHANGE_THRESHOLD`).

use std::collections::HashSet;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::cluster::CHANGE_THRESHOLD;
use super::records::{AddrId, Hash32, IndexedTx, addr_key, decode, parse_addr_tx_key};
use super::{IndexStore, hex};

/// Links followed in either direction from an address, at most.
pub const MAX_PEEL_LINKS: usize = 64;
/// Chains shorter than this are ordinary payments with change, not peel chains.
pub const MIN_PEEL_LINKS: usize = 2;
/// An address with more transactions than this is a wallet in use, not a link.
const LINK_MAX_TXS: usize = 8;

/// One spend of a peel chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeelLink {
    pub txid: String,
    pub time_ms: u64,
    /// The address spent: the previous link's remainder.
    pub from: String,
    /// Where the remainder went on to.
    pub to: String,
    /// The remainder, in sompi.
    pub carried: u64,
    /// The address paid off the chain, and what it got, in sompi.
    pub peeled_to: String,
    pub peeled: u64,
}

/// The chain an address belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeelChain {
    /// Oldest first.
    pub links: Vec<PeelLink>,
    /// Which link the address spends in, 1-based; `links.len() + 1` when it holds the
    /// last remainder.
    pub position: u32,
    /// Sompi paid off the chain over every link.
    pub peeled_total: u64,
}

impl PeelChain {
    pub fn len(&self) -> usize {
        self.links.len()
    }

    pub fn is_empty(&self) -> bool {
        self.links.is_empty()
    }
}

/// A transaction with its id, as loaded from the store.
struct Link {
    txid: Hash32,
    tx: IndexedTx,
    /// Which output carries the remainder on.
    remainder: usize,
}

/// The peel chain `id` is a link of, if it is one.
pub fn peel_chain(store: &IndexStore, id: AddrId) -> Result<Option<PeelChain>> {
    let Some(txids) = txids_of(store, id)? else {
        return Ok(None);
    };
    let mut loaded = Vec::with_capacity(txids.len());
    for txid in txids {
        if let Some(tx) = load(store, &txid)? {
            loaded.push((txid, tx));
        }
    }

    // The link `id` spends in, and the link that created it.
    let spend = loaded
        .iter()
        .find(|(_, tx)| is_link(tx) && tx.inputs[0].addr == Some(id))
        .map(|(txid, tx)| (*txid, tx.clone()));
    let fund = match &spend {
        Some((_, spend_tx)) => {
            let input = &spend_tx.inputs[0];
            load(store, &input.prev_txid)?
                .filter(is_link)
                .filter(|tx| {
                    tx.outputs
                        .get(input.prev_index as usize)
                        .is_some_and(|o| o.addr == Some(id))
                })
                .map(|tx| Link {
                    txid: input.prev_txid,
                    tx,
                    remainder: input.prev_index as usize,
                })
        }
        None => {
            // Unspent: `id` is the chain's end only if it holds the remainder. If the
            // other output was carried on, `id` is a peel target, not a link.
            let mut fund = None;
            for (txid, tx) in &loaded {
                if !is_link(tx) {
                    continue;
                }
                let Some(mine) = tx.outputs.iter().position(|o| o.addr == Some(id)) else {
                    continue;
                };
                let other = 1 - mine;
                if spend_of(store, tx.outputs[other].addr, txid)?.is_some() {
                    return Ok(None);
                }
                if tx.outputs[mine].change >= CHANGE_THRESHOLD {
                    fund = Some(Link {
                        txid: *txid,
                        tx: tx.clone(),
                        remainder: mine,
                    });
                    break;
                }
            }
            fund
        }
    };

    let mut seen: HashSet<Hash32> = HashSet::new();
    // Backwards from the funding link through the spent outpoints.
    let mut behind: Vec<Link> = Vec::new();
    if let Some(fund) = fund {
        seen.insert(fund.txid);
        let mut cur = fund;
        loop {
            let input = &cur.tx.inputs[0];
            let prev = if behind.len() + 1 < MAX_PEEL_LINKS && !seen.contains(&input.prev_txid) {
                load(store, &input.prev_txid)?
                    .filter(is_link)
                    .map(|tx| Link {
                        txid: input.prev_txid,
                        tx,
                        remainder: input.prev_index as usize,
                    })
            } else {
                None
            };
            behind.push(cur);
            match prev {
                Some(prev) => {
                    seen.insert(prev.txid);
                    cur = prev;
                }
                None => break,
            }
        }
    }
    // Forwards from the spend, following whichever output is spent by a link.
    let mut ahead: Vec<Link> = Vec::new();
    if let Some((txid, tx)) = spend {
        seen.insert(txid);
        let mut cur = (txid, tx);
        while ahead.len() < MAX_PEEL_LINKS {
            let (txid, tx) = cur;
            let mut next = None;
            for (i, output) in tx.outputs.iter().enumerate() {
                if let Some(found) = spend_of(store, output.addr, &txid)?
                    && !seen.contains(&found.0)
                {
                    let better =
                        next.as_ref()
                            .is_none_or(|(_, _, j): &(Hash32, IndexedTx, usize)| {
                                tx.outputs[i].change > tx.outputs[*j].change
                            });
                    if better {
                        next = Some((found.0, found.1, i));
                    }
                }
            }
            match next {
                Some((next_txid, next_tx, remainder)) => {
                    ahead.push(Link {
                        txid,
                        tx,
                        remainder,
                    });
                    seen.insert(next_txid);
                    cur = (next_txid, next_tx);
                }
                None => {
                    // Nothing spent either output yet: the likelier change carries on.
                    let remainder = likely_remainder(&tx);
                    ahead.push(Link {
                        txid,
                        tx,
                        remainder,
                    });
                    break;
                }
            }
        }
    }

    let position = behind.len() + 1;
    let links: Vec<Link> = behind.into_iter().rev().chain(ahead).collect();
    if links.len() < MIN_PEEL_LINKS {
        return Ok(None);
    }
    let mut out = Vec::with_capacity(links.len());
    for link in &links {
        out.push(describe(store, link)?);
    }
    Ok(Some(PeelChain {
        peeled_total: out.iter().map(|l| l.peeled).sum(),
        links: out,
        position: position as u32,
    }))
}

fn describe(store: &IndexStore, link: &Link) -> Result<PeelLink> {
    let name = |id: Option<AddrId>| -> Result<String> {
        Ok(match id {
            Some(id) => store.address_of(id)?.unwrap_or_default(),
            None => String::new(),
        })
    };
    let carried = &link.tx.outputs[link.remainder];
    let peeled = &link.tx.outputs[1 - link.remainder];
    Ok(PeelLink {
        txid: hex(&link.txid),
        time_ms: link.tx.time_ms,
        from: name(link.tx.inputs[0].addr)?,
        to: name(carried.addr)?,
        carried: carried.amount,
        peeled_to: name(peeled.addr)?,
        peeled: peeled.amount,
    })
}

/// One input from a known address, two outputs to known addresses.
fn is_link(tx: &IndexedTx) -> bool {
    !tx.is_coinbase
        && tx.inputs.len() == 1
        && tx.inputs[0].addr.is_some()
        && tx.outputs.len() == 2
        && tx.outputs.iter().all(|o| o.addr.is_some())
}

/// The output of an unspent link that most looks like the remainder: the higher change
/// score, then the larger amount.
fn likely_remainder(tx: &IndexedTx) -> usize {
    let (a, b) = (&tx.outputs[0], &tx.outputs[1]);
    if (b.change, b.amount) > (a.change, a.amount) {
        1
    } else {
        0
    }
}

/// The link-shaped transaction in which `addr` spends an output of `txid`.
fn spend_of(
    store: &IndexStore,
    addr: Option<AddrId>,
    txid: &Hash32,
) -> Result<Option<(Hash32, IndexedTx)>> {
    let Some(addr) = addr else { return Ok(None) };
    let Some(txids) = txids_of(store, addr)? else {
        return Ok(None);
    };
    for candidate in txids {
        if let Some(tx) = load(store, &candidate)?
            && is_link(&tx)
            && tx.inputs[0].addr == Some(addr)
            && tx.inputs[0].prev_txid == *txid
        {
            return Ok(Some((candidate, tx)));
        }
    }
    Ok(None)
}

/// The transaction ids of `id`, newest first, or `None` once there are more than a link
/// would have.
fn txids_of(store: &IndexStore, id: AddrId) -> Result<Option<Vec<Hash32>>> {
    let mut txids = Vec::new();
    for slab in store.slabs().into_iter().rev() {
        for guard in slab.addr_tx.prefix(addr_key(id)).rev() {
            let (key, _) = guard.into_inner()?;
            if let Some((_, txid)) = parse_addr_tx_key(&key) {
                txids.push(txid);
                if txids.len() > LINK_MAX_TXS {
                    return Ok(None);
                }
            }
        }
    }
    Ok(Some(txids))
}

fn load(store: &IndexStore, txid: &Hash32) -> Result<Option<IndexedTx>> {
    for slab in store.slabs().into_iter().rev() {
        if let Some(bytes) = slab.tx.get(txid)? {
            return Ok(Some(decode(&bytes)?));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::index::temp_store;
    use crate::index::writer::IndexWriter;
    use crate::index::writer::testing::{address, chain_block, hash, response, tx, with_outpoint};
    use crate::labels::LabelBook;

    const KAS: u64 = 100_000_000;

    #[test]
    fn follows_a_chain_in_both_directions() {
        let store = temp_store();
        let mut writer =
            IndexWriter::new(store.store.clone(), Arc::new(LabelBook::base())).unwrap();
        // 1 gets 100 KAS; 10 is an exchange deposit address seen before the chain.
        // Then 1 → 2 → 3 → 4, peeling 10 KAS to 10 each time; 5 pays 6 with change
        // once, which is no chain; 7 batches to 8 and 9.
        let mut t1 = tx(1, &[(1, 100 * KAS)], &[(10, 10 * KAS), (2, 89 * KAS + 7)]);
        with_outpoint(&mut t1, 0, 0, 0);
        let mut t2 = tx(
            2,
            &[(2, 89 * KAS + 7)],
            &[(10, 10 * KAS), (3, 78 * KAS + 9)],
        );
        with_outpoint(&mut t2, 0, 1, 1);
        let mut t3 = tx(
            3,
            &[(3, 78 * KAS + 9)],
            &[(4, 67 * KAS + 3), (10, 10 * KAS)],
        );
        with_outpoint(&mut t3, 0, 2, 1);
        let mut t4 = tx(4, &[(5, 50 * KAS)], &[(10, 20 * KAS), (6, 29 * KAS + 1)]);
        with_outpoint(&mut t4, 0, 0, 2);
        let mut t5 = tx(
            5,
            &[(7, 50 * KAS)],
            &[(8, 20 * KAS), (9, 20 * KAS), (10, 9 * KAS)],
        );
        with_outpoint(&mut t5, 0, 0, 3);
        let r = response(
            vec![],
            vec![chain_block(
                1,
                1_000,
                vec![
                    tx(
                        0,
                        &[],
                        &[(1, 100 * KAS), (10, 5), (5, 50 * KAS), (7, 50 * KAS)],
                    ),
                    t1,
                    t2,
                    t3,
                    t4,
                    t5,
                ],
            )],
        );
        writer.apply(&r).unwrap();
        let store = &store.store;
        let id = |n: u32| store.lookup(&address(n).to_string()).unwrap().unwrap();

        // The middle link sees the whole chain.
        let chain = peel_chain(store, id(2)).unwrap().unwrap();
        assert_eq!(chain.len(), 3);
        assert_eq!(chain.position, 2);
        assert_eq!(chain.peeled_total, 30 * KAS);
        let txids: Vec<&str> = chain.links.iter().map(|l| l.txid.as_str()).collect();
        assert_eq!(
            txids,
            vec![
                hex(&hash(1).as_bytes()),
                hex(&hash(2).as_bytes()),
                hex(&hash(3).as_bytes())
            ]
        );
        assert_eq!(chain.links[0].from, address(1).to_string());
        assert_eq!(chain.links[0].to, address(2).to_string());
        assert_eq!(chain.links[0].peeled_to, address(10).to_string());
        assert_eq!(chain.links[2].to, address(4).to_string());
        assert_eq!(chain.links[2].carried, 67 * KAS + 3);

        // The first link, and the unspent end, see the same chain.
        let first = peel_chain(store, id(1)).unwrap().unwrap();
        assert_eq!((first.len(), first.position), (3, 1));
        let end = peel_chain(store, id(4)).unwrap().unwrap();
        assert_eq!((end.len(), end.position), (3, 4));
        assert_eq!(end.links, chain.links);

        // The deposit address is a peel target, a lone payment with change is no chain,
        // and a batch payout isn't link-shaped.
        assert_eq!(peel_chain(store, id(10)).unwrap(), None);
        assert_eq!(peel_chain(store, id(5)).unwrap(), None);
        assert_eq!(peel_chain(store, id(6)).unwrap(), None);
        assert_eq!(peel_chain(store, id(8)).unwrap(), None);
        drop(writer);
    }
}
