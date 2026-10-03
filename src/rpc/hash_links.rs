//! Find block hashes in pretty-printed JSON RPC responses so the result viewer can link
//! them to `get_block`. Works line by line on `serde_json::to_string_pretty` output and
//! uses the field name (and its ancestors) to tell block hashes from transaction hashes.

/// A block hash in a response, with the char index of the end of its line.
#[derive(Debug, Clone, PartialEq)]
pub struct HashLink {
    pub hash: String,
    pub line_end_char: usize,
}

/// Fields whose 64-hex values are always block hashes.
const BLOCK_HASH_KEYS: &[&str] = &[
    "acceptingBlockHash",
    "addedChainBlockHashes",
    "blockHash",
    "blockHashes",
    "childrenHashes",
    "finalityBlockHash",
    "lowHash",
    "mergeSetBluesHashes",
    "mergeSetRedsHashes",
    "mergingChainBlockHash",
    "parentsByLevel",
    "pruningPoint",
    "pruningPointHash",
    "removedChainBlockHashes",
    "selectedParentHash",
    "sink",
    "startHash",
    "tip",
    "tipHashes",
    "violatingBlockHash",
    "virtualParentHashes",
];

/// `hash` is a block hash unless it sits inside a transaction.
const TRANSACTION_KEYS: &[&str] = &["transaction", "transactions", "acceptedTransactions"];

pub fn block_hash_links(text: &str) -> Vec<HashLink> {
    let mut links = Vec::new();
    // Field name of each open object/array; anonymous elements inherit their parent's.
    let mut stack: Vec<Option<&str>> = Vec::new();
    let mut char_pos = 0;

    for line in text.split('\n') {
        let line_end_char = char_pos + line.chars().count();
        char_pos = line_end_char + 1;

        let t = line.trim();
        let (key, rest) = match t.strip_prefix('"').and_then(|s| s.split_once("\": ")) {
            Some((k, r)) => (Some(k), r),
            None => (None, t),
        };
        let rest = rest.trim_end_matches(',');

        if rest.ends_with('{') || rest.ends_with('[') {
            stack.push(key.or_else(|| stack.last().copied().flatten()));
        } else if rest.starts_with('}') || rest.starts_with(']') {
            stack.pop();
        } else if let Some(hash) = rest.strip_prefix('"').and_then(|s| s.strip_suffix('"'))
            && is_hash(hash)
            && let Some(key) = key.or_else(|| stack.last().copied().flatten())
            && is_block_hash_field(key, &stack)
        {
            links.push(HashLink {
                hash: hash.to_string(),
                line_end_char,
            });
        }
    }
    links
}

fn is_hash(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn is_block_hash_field(key: &str, ancestors: &[Option<&str>]) -> bool {
    match key {
        "hash" => !ancestors
            .iter()
            .flatten()
            .any(|a| TRANSACTION_KEYS.contains(a)),
        _ => BLOCK_HASH_KEYS.contains(&key),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(c: char) -> String {
        c.to_string().repeat(64)
    }

    fn hashes(text: &str) -> Vec<String> {
        block_hash_links(text).into_iter().map(|l| l.hash).collect()
    }

    #[test]
    fn finds_block_hashes_but_not_transaction_hashes() {
        let json = serde_json::json!({
            "header": { "hash": h('a'), "parentsByLevel": [[h('b'), h('c')]], "hashMerkleRoot": h('d') },
            "transactions": [
                { "verboseData": { "transactionId": h('e'), "hash": h('f'), "blockHash": h('1') } }
            ],
            "verboseData": { "hash": h('2'), "childrenHashes": [h('3')] },
            "sink": h('4'),
        });
        let text = serde_json::to_string_pretty(&json).unwrap();
        let mut found = hashes(&text);
        found.sort();
        let mut expected = vec![h('a'), h('b'), h('c'), h('1'), h('2'), h('3'), h('4')];
        expected.sort();
        assert_eq!(found, expected);
    }

    #[test]
    fn mempool_transaction_hash_is_not_linked() {
        let json = serde_json::json!([
            { "fee": 1, "transaction": { "verboseData": { "hash": h('a') } } }
        ]);
        assert!(hashes(&serde_json::to_string_pretty(&json).unwrap()).is_empty());
    }

    #[test]
    fn ignores_empty_containers_and_non_hashes() {
        let json = serde_json::json!({ "tipHashes": [], "sink": "abc", "blockHash": h('a') });
        assert_eq!(
            hashes(&serde_json::to_string_pretty(&json).unwrap()),
            vec![h('a')]
        );
    }

    #[test]
    fn line_end_char_points_at_end_of_line() {
        let text = format!("{{\n  \"sink\": \"{}\"\n}}", h('a'));
        let links = block_hash_links(&text);
        assert_eq!(links.len(), 1);
        let line_start = 2;
        let line = format!("  \"sink\": \"{}\"", h('a'));
        assert_eq!(links[0].line_end_char, line_start + line.chars().count());
    }
}
