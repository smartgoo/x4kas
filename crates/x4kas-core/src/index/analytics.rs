//! The analytics engine's buckets as a keyspace of the index store, written in the same
//! atomic batch as the transactions they count, so the engine and the index can never
//! disagree about the position. Every bucket is its own key and only the buckets a batch
//! changed are written, plus the last minute of blocks (`recent`), which changes with
//! nearly every batch. The keyspace isn't slabbed: the engine prunes by its own windows
//! (at most 24h), well inside the node's retention.

use std::path::Path;

use anyhow::Result;
use fjall::OwnedWriteBatch as WriteBatch;

use super::records::{decode, encode};
use super::{IndexStore, Manifest, Position, parse_hex};
use crate::analytics::{
    AnalyticsEngine, BucketKey, BucketWidth, Ingest, TimeBucket, load_legacy_cache,
};

/// The last minute of blocks, not yet in any bucket.
const RECENT_KEY: &[u8] = b"recent";
/// Bucket keys: `b` ‖ width ‖ start_ms (big-endian), so a prefix scan walks them in order.
const BUCKET_PREFIX: u8 = b'b';

fn bucket_key(width: BucketWidth, start_ms: u64) -> [u8; 10] {
    let mut key = [0u8; 10];
    key[0] = BUCKET_PREFIX;
    key[1] = match width {
        BucketWidth::Minute => 1,
        BucketWidth::TenMinute => 10,
    };
    key[2..].copy_from_slice(&start_ms.to_be_bytes());
    key
}

fn parse_bucket_key(key: &[u8]) -> Option<BucketKey> {
    if key.len() != 10 || key[0] != BUCKET_PREFIX {
        return None;
    }
    let width = match key[1] {
        1 => BucketWidth::Minute,
        10 => BucketWidth::TenMinute,
        _ => return None,
    };
    Some((width, u64::from_be_bytes(key[2..].try_into().ok()?)))
}

/// The engine as the store holds it: every bucket and the recent blocks.
pub fn load(store: &IndexStore) -> Result<AnalyticsEngine> {
    let ks = store.analytics_keyspace();
    let mut engine = AnalyticsEngine::default();
    for guard in ks.prefix([BUCKET_PREFIX]) {
        let (key, value) = guard.into_inner()?;
        let Some((width, _)) = parse_bucket_key(&key) else {
            continue;
        };
        let bucket: TimeBucket = decode(&value)?;
        engine.insert_bucket(width, bucket);
    }
    if let Some(bytes) = ks.get(RECENT_KEY)? {
        engine.recent_blocks = decode(&bytes)?;
    }
    Ok(engine)
}

/// Put what `ingest` changed in `engine` into `batch`.
pub fn write(
    batch: &mut WriteBatch,
    store: &IndexStore,
    engine: &AnalyticsEngine,
    ingest: &Ingest,
) -> Result<()> {
    let ks = store.analytics_keyspace();
    for &(width, start) in &ingest.removed {
        batch.remove(ks, bucket_key(width, start));
    }
    for &(width, start) in &ingest.touched {
        if let Some(bucket) = engine.bucket(width, start) {
            batch.insert(ks, bucket_key(width, start), encode(bucket)?);
        }
    }
    if ingest.recent_changed {
        batch.insert(ks, RECENT_KEY, encode(&engine.recent_blocks)?);
    }
    Ok(())
}

/// Put a whole engine into `batch` (an import).
fn write_all(batch: &mut WriteBatch, store: &IndexStore, engine: &AnalyticsEngine) -> Result<()> {
    let ks = store.analytics_keyspace();
    for width in BucketWidth::ALL {
        for bucket in engine.buckets(width) {
            batch.insert(
                ks,
                bucket_key(width, bucket.bucket_start_ms),
                encode(bucket)?,
            );
        }
    }
    batch.insert(ks, RECENT_KEY, encode(&engine.recent_blocks)?);
    Ok(())
}

/// Whether the store holds any analytics.
pub fn is_empty(store: &IndexStore) -> bool {
    store
        .analytics_keyspace()
        .prefix([BUCKET_PREFIX])
        .next()
        .is_none()
}

/// Import the analytics cache file of earlier versions (`analytics::legacy_cache_path`)
/// into a store that has no chain data yet, and remove the file either way. The stream
/// then resumes from the cache's last chain block, as the index did before. Returns
/// whether anything was imported.
pub fn import_legacy_cache(store: &IndexStore, path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let saved_ms = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64);
    let loaded = load_legacy_cache(path);
    // One shot: a cache that can't be read, or isn't needed, is just stale.
    let _ = std::fs::remove_file(path);
    let (engine, last_chain_block) = loaded?;

    let manifest = store.manifest()?;
    if manifest.position.is_some() || !is_empty(store) {
        return Ok(false);
    }
    let mut batch = store.db().batch();
    write_all(&mut batch, store, &engine)?;
    if let Some(chain_block) = last_chain_block.as_deref().and_then(parse_hex) {
        let manifest = Manifest {
            position: Some(Position {
                chain_block,
                daa_score: 0,
                time_ms: saved_ms.unwrap_or(0),
            }),
            ..manifest
        };
        batch.insert(
            store.meta_keyspace(),
            "manifest",
            IndexStore::encode_manifest(&manifest)?,
        );
    }
    batch.commit()?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashSet};

    use super::*;
    use crate::analytics::{BlockSummary, Metrics, save_legacy_cache};
    use crate::index::temp_store;

    fn block(hash: &str, timestamp_ms: u64, tx_count: u64) -> BlockSummary {
        BlockSummary {
            hash: hash.to_string(),
            timestamp_ms,
            metrics: Metrics {
                chain_blocks: 1,
                tx_count,
                ..Default::default()
            },
        }
    }

    #[test]
    fn bucket_keys_round_trip_in_order() {
        let a = bucket_key(BucketWidth::Minute, 60_000);
        let b = bucket_key(BucketWidth::Minute, 120_000);
        let c = bucket_key(BucketWidth::TenMinute, 0);
        assert!(a < b && b < c);
        assert_eq!(parse_bucket_key(&a), Some((BucketWidth::Minute, 60_000)));
        assert_eq!(parse_bucket_key(&c), Some((BucketWidth::TenMinute, 0)));
        assert_eq!(parse_bucket_key(b"recent"), None);
    }

    #[test]
    fn writes_only_what_changed_and_loads_it_back() {
        let ts = temp_store();
        let store = &ts.store;
        assert!(is_empty(store));
        let now = 3_600_000 * 10;

        let mut engine = AnalyticsEngine::default();
        engine.add_block(block("old", now - 300_000, 4));
        engine.add_block(block("new", now, 1));
        let mut ingest = Ingest {
            recent_changed: true,
            ..Default::default()
        };
        ingest.touched = engine.finalize_old_blocks(now);
        let mut batch = store.db().batch();
        write(&mut batch, store, &engine, &ingest).unwrap();
        batch.commit().unwrap();

        let loaded = load(store).unwrap();
        assert_eq!(loaded.minute_buckets.len(), 1);
        assert_eq!(loaded.ten_minute_buckets.len(), 1);
        assert_eq!(loaded.minute_buckets[0].metrics.tx_count, 4);
        assert_eq!(loaded.recent_blocks.len(), 1);
        assert!(loaded.recent_blocks.contains_key("new"));

        // Pruning removes keys; an unchanged recent set isn't rewritten.
        let removed = engine.prune_buckets(now + 2 * 3_600_000);
        let ingest = Ingest {
            removed,
            ..Default::default()
        };
        let mut batch = store.db().batch();
        write(&mut batch, store, &engine, &ingest).unwrap();
        batch.commit().unwrap();
        let loaded = load(store).unwrap();
        assert!(loaded.minute_buckets.is_empty());
        assert_eq!(loaded.ten_minute_buckets.len(), 1);
        assert_eq!(loaded.recent_blocks.len(), 1);
    }

    #[test]
    fn ingest_skips_seen_blocks_and_reports_changes() {
        let now = 3_600_000 * 10;
        let mut engine = AnalyticsEngine::default();
        let r = crate::index::writer::testing::response(
            vec![],
            vec![
                crate::index::writer::testing::chain_block(1, now - 120_000, vec![]),
                crate::index::writer::testing::chain_block(2, now - 120_000, vec![]),
            ],
        );
        // The builder numbers chain block hashes from a million.
        let skip: HashSet<String> =
            HashSet::from([crate::index::writer::testing::hash(1_000_002).to_string()]);
        let ingest = engine.ingest(&r, &skip, now);
        assert!(ingest.recent_changed);
        assert_eq!(ingest.touched.len(), 2);
        assert_eq!(ingest.removed, BTreeSet::new());
        assert_eq!(engine.minute_buckets[0].metrics.chain_blocks, 1);

        // Nothing new: nothing changes.
        let all: HashSet<String> = [1_000_001, 1_000_002]
            .map(|n| crate::index::writer::testing::hash(n).to_string())
            .into();
        let ingest = engine.ingest(&r, &all, now);
        assert_eq!(ingest, Ingest::default());
    }

    #[test]
    fn imports_the_legacy_cache_once() {
        let ts = temp_store();
        let store = &ts.store;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("analytics_cache.bin");

        let mut engine = AnalyticsEngine::default();
        engine.add_block(block("b1", 1_000, 3));
        engine.finalize_old_blocks(3_600_000);
        engine.add_block(block("b2", 2_000, 1));
        let last = "ab".repeat(32);
        save_legacy_cache(&path, &engine, Some(last.clone())).unwrap();

        assert!(import_legacy_cache(store, &path).unwrap());
        assert!(!path.exists(), "the file is consumed");
        let loaded = load(store).unwrap();
        assert_eq!(loaded.minute_buckets.len(), 1);
        assert_eq!(loaded.recent_blocks.len(), 1);
        let position = store.manifest().unwrap().position.unwrap();
        assert_eq!(position.chain_block, parse_hex(&last).unwrap());

        // A store with data keeps it; the file still goes.
        save_legacy_cache(&path, &AnalyticsEngine::default(), None).unwrap();
        assert!(!import_legacy_cache(store, &path).unwrap());
        assert!(!path.exists());
        assert_eq!(load(store).unwrap().minute_buckets.len(), 1);

        // No file: nothing to do.
        assert!(!import_legacy_cache(store, &path).unwrap());
    }
}
