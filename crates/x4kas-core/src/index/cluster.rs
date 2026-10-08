//! Grouping addresses into likely owners.
//!
//! Common-input ownership: every address that signs inputs of one transaction is
//! controlled by one party, so they join one cluster. A change output (an output the
//! heuristics in [`change_scores`] score high enough) joins the inputs' cluster too.
//!
//! Clusters are a persistent union-find over address ids, in three global keyspaces
//! (`cl_parent`: addr → parent, `cl_size`: root → member count, `cl_member`:
//! root ‖ member → ()), unioned by size with path compression. Singletons store nothing.
//! Membership is knowledge, not activity, so it isn't slabbed or undone by reorgs.
//!
//! Guards keep the heuristic from merging strangers: inputs carrying different entity
//! labels (an exchange sweep never merges two exchanges), L2 bridge protocols, and a
//! size cap that treats a runaway cluster as a heuristic failure.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use fjall::{Keyspace, OwnedWriteBatch as WriteBatch};

use super::records::{AddrId, addr_key, parse_addr_key, parse_peer_key, peer_key};
use crate::tx_inspect::TransactionProtocol;

/// A cluster that would grow past this stays as it is.
pub const MAX_CLUSTER: u32 = 50_000;
/// Change outputs scoring at least this join the inputs' cluster.
pub const CHANGE_THRESHOLD: u8 = 70;

/// The three keyspaces.
#[derive(Clone)]
pub struct ClusterKeyspaces {
    pub parent: Keyspace,
    pub size: Keyspace,
    pub member: Keyspace,
}

/// Union-find with a per-batch overlay, flushed into the batch at the end.
pub struct Clusters<'a> {
    ks: &'a ClusterKeyspaces,
    parent: HashMap<AddrId, Option<AddrId>>,
    size: HashMap<AddrId, Option<u32>>,
    member_add: HashSet<(AddrId, AddrId)>,
    member_del: HashSet<(AddrId, AddrId)>,
    /// Unions refused by the size cap.
    pub cap_hits: u64,
}

impl<'a> Clusters<'a> {
    pub fn new(ks: &'a ClusterKeyspaces) -> Self {
        Self {
            ks,
            parent: HashMap::new(),
            size: HashMap::new(),
            member_add: HashSet::new(),
            member_del: HashSet::new(),
            cap_hits: 0,
        }
    }

    fn parent_of(&mut self, id: AddrId) -> Result<Option<AddrId>> {
        if let Some(p) = self.parent.get(&id) {
            return Ok(*p);
        }
        let stored = self
            .ks
            .parent
            .get(addr_key(id))?
            .and_then(|v| parse_addr_key(&v));
        self.parent.insert(id, stored);
        Ok(stored)
    }

    fn size_of(&mut self, root: AddrId) -> Result<u32> {
        if let Some(s) = self.size.get(&root) {
            return Ok(s.unwrap_or(1));
        }
        let stored = self
            .ks
            .size
            .get(addr_key(root))?
            .and_then(|v| v.as_ref().try_into().ok().map(u32::from_be_bytes));
        self.size.insert(root, stored);
        Ok(stored.unwrap_or(1))
    }

    /// The cluster root of `id` (itself when it's in no cluster).
    pub fn find(&mut self, id: AddrId) -> Result<AddrId> {
        let mut path = Vec::new();
        let mut cur = id;
        while let Some(p) = self.parent_of(cur)? {
            path.push(cur);
            cur = p;
        }
        for node in path {
            if node != cur {
                self.parent.insert(node, Some(cur));
            }
        }
        Ok(cur)
    }

    /// Members of the cluster rooted at `root`, the root included.
    fn members(&mut self, root: AddrId) -> Result<Vec<AddrId>> {
        let mut set: HashSet<AddrId> = HashSet::new();
        for guard in self.ks.member.prefix(addr_key(root)) {
            if let Some(m) = parse_peer_key(&guard.key()?) {
                set.insert(m);
            }
        }
        for &(r, m) in &self.member_add {
            if r == root {
                set.insert(m);
            }
        }
        for &(r, m) in &self.member_del {
            if r == root {
                set.remove(&m);
            }
        }
        set.insert(root);
        Ok(set.into_iter().collect())
    }

    /// Merge the clusters of `a` and `b`. Returns false when the size cap refuses.
    pub fn union(&mut self, a: AddrId, b: AddrId) -> Result<bool> {
        let (ra, rb) = (self.find(a)?, self.find(b)?);
        if ra == rb {
            return Ok(true);
        }
        let (sa, sb) = (self.size_of(ra)?, self.size_of(rb)?);
        if sa + sb > MAX_CLUSTER {
            self.cap_hits += 1;
            return Ok(false);
        }
        let (small, big) = if sa <= sb { (ra, rb) } else { (rb, ra) };
        for m in self.members(small)? {
            self.parent.insert(m, Some(big));
            if !self.member_del.insert((small, m)) || self.member_add.contains(&(small, m)) {
                self.member_add.remove(&(small, m));
            }
            self.member_add.insert((big, m));
        }
        // The big root is its own member once it has company.
        self.member_add.insert((big, big));
        self.size.insert(small, None);
        self.size.insert(big, Some(sa + sb));
        Ok(true)
    }

    pub fn flush(self, batch: &mut WriteBatch) {
        for (id, parent) in self.parent {
            if let Some(p) = parent {
                batch.insert(&self.ks.parent, addr_key(id), addr_key(p));
            }
        }
        for (root, size) in self.size {
            match size {
                Some(s) => batch.insert(&self.ks.size, addr_key(root), s.to_be_bytes()),
                None => batch.remove(&self.ks.size, addr_key(root)),
            }
        }
        for (root, m) in self.member_del {
            if !self.member_add.contains(&(root, m)) {
                batch.remove(&self.ks.member, peer_key(root, m));
            }
        }
        for (root, m) in self.member_add {
            batch.insert(&self.ks.member, peer_key(root, m), []);
        }
    }
}

/// Read-only lookups outside a batch.
pub fn root_of(ks: &ClusterKeyspaces, id: AddrId) -> Result<AddrId> {
    let mut cur = id;
    let mut hops = 0;
    while let Some(p) = ks
        .parent
        .get(addr_key(cur))?
        .and_then(|v| parse_addr_key(&v))
    {
        cur = p;
        hops += 1;
        if hops > 64 {
            break;
        }
    }
    Ok(cur)
}

pub fn size_of(ks: &ClusterKeyspaces, root: AddrId) -> Result<u32> {
    Ok(ks
        .size
        .get(addr_key(root))?
        .and_then(|v| v.as_ref().try_into().ok().map(u32::from_be_bytes))
        .unwrap_or(1))
}

/// Up to `limit` members of the cluster rooted at `root` (the root first).
pub fn members_of(ks: &ClusterKeyspaces, root: AddrId, limit: usize) -> Result<Vec<AddrId>> {
    let mut out = vec![root];
    for guard in ks.member.prefix(addr_key(root)) {
        if out.len() >= limit {
            break;
        }
        if let Some(m) = parse_peer_key(&guard.key()?)
            && m != root
        {
            out.push(m);
        }
    }
    Ok(out)
}

/// Whether a transaction's inputs may be unioned: not an L2 bridge protocol (which
/// pools unrelated users' funds), and no two inputs with different entity labels.
pub fn may_union(protocol: Option<TransactionProtocol>, input_labels: &[Option<&str>]) -> bool {
    if matches!(
        protocol,
        Some(TransactionProtocol::Kasplex | TransactionProtocol::Igra)
    ) {
        return false;
    }
    let mut seen: Option<&str> = None;
    for label in input_labels.iter().flatten() {
        match seen {
            Some(s) if s != *label => return false,
            _ => seen = Some(label),
        }
    }
    true
}

/// One output's traits for change detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputTrait {
    /// The address was first seen in this transaction.
    pub fresh: bool,
    /// Address version byte (pubkey vs script hash), to compare with the inputs'.
    pub version: Option<u8>,
    pub amount: u64,
    /// The output goes back to one of the input addresses.
    pub to_input: bool,
}

/// A 0–100 score per output for being the sender's change. Only the classic
/// one-payment-one-change shape (two outputs) can score high; batch payouts don't.
pub fn change_scores(input_version: Option<u8>, outputs: &[OutputTrait]) -> Vec<u8> {
    if outputs.len() < 2 {
        return vec![0; outputs.len()];
    }
    let round = |amount: u64| {
        amount > 0 && (amount.is_multiple_of(100_000_000) || amount.is_multiple_of(10_000_000))
    };
    outputs
        .iter()
        .enumerate()
        .map(|(i, o)| {
            if o.to_input {
                return 100;
            }
            let others = outputs
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, other)| other);
            let mut score: u32 = 0;
            if o.fresh {
                score += 30;
            }
            if !round(o.amount) && others.clone().all(|other| round(other.amount)) {
                score += 25;
            }
            if let (Some(v), Some(iv)) = (o.version, input_version)
                && v == iv
                && others.clone().all(|other| other.version != Some(iv))
            {
                score += 25;
            }
            if o.fresh && others.clone().all(|other| !other.fresh) {
                score += 20;
            }
            if outputs.len() > 2 {
                score = score.min(50);
            }
            score.min(100) as u8
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(fresh: bool, version: u8, amount: u64) -> OutputTrait {
        OutputTrait {
            fresh,
            version: Some(version),
            amount,
            to_input: false,
        }
    }

    #[test]
    fn change_scores_prefer_fresh_odd_same_type_outputs() {
        // Round payment to a known address, odd remainder to a fresh one of the same type.
        let scores = change_scores(
            Some(0),
            &[out(false, 0, 500_000_000), out(true, 0, 123_456)],
        );
        assert_eq!(scores, vec![0, 75]);
        // A script-hash payment makes the version test decisive too.
        let scores = change_scores(
            Some(0),
            &[out(false, 8, 500_000_000), out(true, 0, 123_456)],
        );
        assert_eq!(scores, vec![0, 100]);
        // Batch payouts never score high.
        let scores = change_scores(
            Some(0),
            &[out(false, 0, 100), out(false, 0, 200), out(true, 0, 333)],
        );
        assert!(scores.iter().all(|s| *s <= 50));
        // Back to an input is certain.
        let scores = change_scores(
            Some(0),
            &[
                out(false, 0, 100),
                OutputTrait {
                    to_input: true,
                    ..out(false, 0, 7)
                },
            ],
        );
        assert_eq!(scores[1], 100);
        assert_eq!(change_scores(Some(0), &[out(true, 0, 1)]), vec![0]);
    }

    #[test]
    fn union_guards() {
        assert!(may_union(None, &[Some("Bybit"), None, Some("Bybit")]));
        assert!(!may_union(None, &[Some("Bybit"), Some("Gate.io")]));
        assert!(!may_union(
            Some(TransactionProtocol::Kasplex),
            &[None, None]
        ));
        assert!(may_union(Some(TransactionProtocol::Krc), &[None, None]));
    }
}
