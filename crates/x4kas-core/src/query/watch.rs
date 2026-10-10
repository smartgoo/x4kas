//! Watched saved queries: re-run as the index moves, raising an event for every row
//! that wasn't there the last time. A task started with the chain pipeline
//! (`start_query_watch`) polls the index position every `POLL`; when it moved, every
//! watched query that is due runs over the time since its last run (plus an overlap,
//! so a late batch isn't missed) and its new rows, told apart by their primary value,
//! land in `App.query` as `QueryEvent`s, which the GUI toasts and the CLI prints.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::Serialize;
use tokio::sync::{Notify, RwLock};

use super::exec::{self, Cell, Column, Inputs, ResultSet, RunControl};
use super::saved::SavedQuery;
use super::{Entity, Query, TimeRange};
use crate::app::{App, QueryWatchStatus};
use crate::format::now_ms;
use crate::index::IndexStore;
use crate::polling::PollingHandles;

/// How often the task looks for new data.
pub const POLL: Duration = Duration::from_secs(30);
/// Re-run from this long before the last run, so a batch written late isn't missed.
const OVERLAP_MS: u64 = 5 * 60_000;
/// Rows one run may raise events for.
const WATCH_LIMIT: usize = 500;
/// Primary values remembered per query, to tell new rows from seen ones.
const SEEN_MAX: usize = 5_000;
/// How long one watched run may take.
const RUN_BUDGET: Duration = Duration::from_secs(20);
/// Events kept in `App.query.events`.
pub const MAX_EVENTS: usize = 500;

/// A row a watched query found for the first time. Serializes (the CLI's JSON lines)
/// as `{time_ms, query_id, query_name, row, primary}`, `row` an object keyed by column
/// name as `export::result_json` writes rows; `columns` and `read` aren't written.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryEvent {
    /// When it was found.
    pub time_ms: u64,
    pub query_id: String,
    pub query_name: String,
    pub columns: Vec<Column>,
    pub row: Vec<Cell>,
    /// The row's id (an address, a block hash, a transaction id), as text.
    pub primary: String,
    pub read: bool,
}

impl Serialize for QueryEvent {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut st = s.serialize_struct("QueryEvent", 5)?;
        st.serialize_field("time_ms", &self.time_ms)?;
        st.serialize_field("query_id", &self.query_id)?;
        st.serialize_field("query_name", &self.query_name)?;
        st.serialize_field("row", &row_json(&self.columns, &self.row))?;
        st.serialize_field("primary", &self.primary)?;
        st.end()
    }
}

/// The row as a JSON object, `column name → value` (amounts in sompi, times in
/// milliseconds, hashes in hex).
fn row_json(columns: &[Column], row: &[Cell]) -> serde_json::Value {
    serde_json::Value::Object(
        columns
            .iter()
            .zip(row)
            .map(|(c, cell)| (c.name.clone(), cell.to_json()))
            .collect(),
    )
}

impl QueryEvent {
    /// The row as `name: value` pairs for a line of text.
    pub fn summary(&self) -> String {
        self.columns
            .iter()
            .zip(&self.row)
            .map(|(c, cell)| format!("{}: {}", c.name, cell.text()))
            .collect::<Vec<_>>()
            .join(" · ")
    }
}

/// The primary values a query has already raised, bounded.
#[derive(Debug, Default)]
pub struct Seen {
    set: HashSet<String>,
    order: VecDeque<String>,
}

impl Seen {
    /// Remember `key`; `true` when it is new.
    pub fn insert(&mut self, key: String) -> bool {
        if !self.set.insert(key.clone()) {
            return false;
        }
        self.order.push_back(key);
        while self.order.len() > SEEN_MAX
            && let Some(old) = self.order.pop_front()
        {
            self.set.remove(&old);
        }
        true
    }

    pub fn len(&self) -> usize {
        self.set.len()
    }

    pub fn is_empty(&self) -> bool {
        self.set.is_empty()
    }
}

/// The primary value of a result row, as text, when the result shows one.
fn primary_of(result: &ResultSet, row: &[Cell]) -> Option<String> {
    let cell = row.get(result.primary?)?;
    (!cell.is_null()).then(|| cell.text())
}

/// The rows of `result` not raised before, as events of `query`; `seen` learns them.
pub fn new_events(
    query: &SavedQuery,
    result: &ResultSet,
    seen: &mut Seen,
    now_ms: u64,
) -> Vec<QueryEvent> {
    let mut events = Vec::new();
    for row in &result.rows {
        let Some(primary) = primary_of(result, row) else {
            continue;
        };
        if seen.insert(primary.clone()) {
            events.push(QueryEvent {
                time_ms: now_ms,
                query_id: query.id.clone(),
                query_name: query.name.clone(),
                columns: result.columns.clone(),
                row: row.clone(),
                primary,
                read: false,
            });
        }
    }
    events
}

/// The query a watched run answers: `q` over the time since `last_run_ms` (with the
/// overlap), at most `WATCH_LIMIT` rows. Addresses have no time of their own, so they
/// run as saved and are told apart by address.
pub fn watched_query(q: &Query, last_run_ms: u64, now_ms: u64) -> Query {
    let mut run = q.clone();
    if run.entity.is_timed() {
        run.range = TimeRange::Between(
            last_run_ms.saturating_sub(OVERLAP_MS),
            now_ms.max(last_run_ms + 1),
        );
    }
    run.limit = Some(run.limit.map_or(WATCH_LIMIT, |l| l.min(WATCH_LIMIT)));
    run
}

/// One watched query's bookkeeping.
struct Watched {
    seen: Seen,
    last_run_ms: u64,
    /// The first run of an address query only learns what is there.
    seeded: bool,
}

/// The task's handle: stop it with [`PollingHandles::stop_query_watch`].
pub struct QueryWatchHandle {
    pub task: tokio::task::JoinHandle<()>,
    pub stop: Arc<Notify>,
    pub cancel: Arc<AtomicBool>,
}

/// Start the task on `store`, tracked in `handles.query_watch`.
pub fn start_query_watch(
    store: Arc<IndexStore>,
    app: Arc<RwLock<App>>,
    handles: &mut PollingHandles,
) {
    let stop = Arc::new(Notify::new());
    let cancel = Arc::new(AtomicBool::new(false));
    let task = tokio::spawn(run(store, app, stop.clone(), cancel.clone()));
    handles.query_watch = Some(QueryWatchHandle { task, stop, cancel });
}

async fn run(
    store: Arc<IndexStore>,
    app: Arc<RwLock<App>>,
    stop: Arc<Notify>,
    cancel: Arc<AtomicBool>,
) {
    let mut watched: std::collections::HashMap<String, Watched> = std::collections::HashMap::new();
    let mut last_position = None;
    let mut ticker = tokio::time::interval(POLL);
    loop {
        tokio::select! {
            _ = stop.notified() => return,
            _ = ticker.tick() => {}
        }
        let (position, queries, labels, watchlist, prune_floor, raised, features) = {
            let app = app.read().await;
            let features = app.index_settings;
            (
                app.chain.position.map(|p| p.chain_block),
                app.query.saved.watched().cloned().collect::<Vec<_>>(),
                app.labels.clone(),
                app.watch.list.clone(),
                app.node.pruning_point_timestamp_ms,
                // What was raised before (an earlier connection): not raised again.
                app.query
                    .events
                    .iter()
                    .map(|e| (e.query_id.clone(), e.primary.clone()))
                    .collect::<Vec<_>>(),
                features,
            )
        };
        watched.retain(|id, _| queries.iter().any(|q| q.id == *id));
        if position.is_none() || position == last_position {
            continue;
        }
        last_position = position;
        let now = now_ms();
        for saved in queries {
            let every_ms = saved
                .watch
                .as_ref()
                .map(|w| w.every_secs.max(super::saved::QueryWatch::MIN_EVERY_SECS) * 1000)
                .unwrap_or(POLL.as_millis() as u64);
            let entry = watched.entry(saved.id.clone()).or_insert_with(|| {
                let mut seen = Seen::default();
                for (id, primary) in raised.iter().rev() {
                    if *id == saved.id {
                        seen.insert(primary.clone());
                    }
                }
                Watched {
                    seeded: !seen.is_empty(),
                    seen,
                    last_run_ms: now.saturating_sub(every_ms),
                }
            });
            if now.saturating_sub(entry.last_run_ms) < every_ms {
                continue;
            }
            let query = match saved.query() {
                Ok(q) => q,
                Err(e) => {
                    entry.last_run_ms = now;
                    report(&app, &saved.id, now, Err(format!("doesn't parse: {e}")), 0).await;
                    continue;
                }
            };
            let run_query = watched_query(&query, entry.last_run_ms, now);
            let run_store = store.clone();
            let run_labels = labels.clone();
            let run_watchlist = watchlist.clone();
            let run_cancel = cancel.clone();
            let result = tokio::task::spawn_blocking(move || {
                let inputs = Inputs {
                    store: &run_store,
                    labels: &run_labels,
                    watchlist: &run_watchlist,
                    now_ms: now,
                    prune_floor_ms: prune_floor,
                    features,
                };
                let mut ctl = RunControl::with_budget(RUN_BUDGET);
                ctl.cancel = run_cancel;
                exec::run(&inputs, &run_query, &mut ctl)
            })
            .await;
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            entry.last_run_ms = now;
            let result = match result {
                Ok(Ok(result)) => result,
                Ok(Err(e)) => {
                    report(&app, &saved.id, now, Err(format!("{e:#}")), 0).await;
                    continue;
                }
                Err(e) => {
                    report(&app, &saved.id, now, Err(format!("query task: {e}")), 0).await;
                    continue;
                }
            };
            let events = new_events(&saved, &result, &mut entry.seen, now);
            // An address query's first run only learns which addresses match.
            let notify = query.entity.is_timed() || entry.seeded;
            entry.seeded = true;
            let mut app = app.write().await;
            if notify {
                for event in events {
                    app.query.push_event(event);
                }
            }
            app.query.watch_status.insert(
                saved.id.clone(),
                QueryWatchStatus {
                    last_run_ms: Some(now),
                    last_error: None,
                    last_rows: result.rows.len(),
                },
            );
            app.mark_dirty();
        }
    }
}

/// Record what the watched query `id` just did.
async fn report(app: &RwLock<App>, id: &str, now: u64, outcome: Result<(), String>, rows: usize) {
    let mut app = app.write().await;
    app.query.watch_status.insert(
        id.to_string(),
        QueryWatchStatus {
            last_run_ms: Some(now),
            last_error: outcome.err(),
            last_rows: rows,
        },
    );
    app.mark_dirty();
}

impl PollingHandles {
    /// Stop the watched queries task and wait for it (a run in progress is cancelled),
    /// so the store it reads can be closed.
    pub async fn stop_query_watch(&mut self) {
        if let Some(handle) = self.query_watch.take() {
            handle.cancel.store(true, Ordering::Relaxed);
            handle.stop.notify_one();
            let _ = handle.task.await;
        }
    }
}

/// Whether `entity` rows carry their own time (the watcher windows them).
pub fn windows(entity: Entity) -> bool {
    entity.is_timed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::exec::{ColumnSource, Partial};
    use crate::query::fields::{FieldId, FieldKind};
    use crate::query::text;

    fn result(primary: &[&str]) -> ResultSet {
        ResultSet {
            entity: Entity::Addresses,
            columns: vec![Column {
                name: "address".into(),
                label: "Address".into(),
                kind: FieldKind::Address,
                source: ColumnSource::Field(FieldId::AddrAddress),
            }],
            rows: primary
                .iter()
                .map(|p| vec![Cell::Address((*p).to_string())])
                .collect(),
            matched: primary.len() as u64,
            scanned: 0,
            truncated: false,
            partial: None::<Partial>,
            elapsed: Duration::ZERO,
            window: (0, u64::MAX),
            plan: String::new(),
            primary: Some(0),
            time_column: None,
            balances_pending: false,
            notes: Vec::new(),
        }
    }

    #[test]
    fn diff_emits_only_new_primary_ids() {
        let saved = SavedQuery::new("Whales", "", &text::parse("addresses").unwrap());
        let mut seen = Seen::default();
        let first = new_events(&saved, &result(&["a", "b"]), &mut seen, 1);
        assert_eq!(first.len(), 2);
        assert_eq!(first[0].primary, "a");
        assert_eq!(first[0].query_name, "Whales");
        assert_eq!(first[0].summary(), "address: a");
        let again = new_events(&saved, &result(&["b", "c"]), &mut seen, 2);
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].primary, "c");
        assert_eq!(seen.len(), 3);
        // A row without a primary value raises nothing.
        let mut r = result(&["d"]);
        r.primary = None;
        assert!(new_events(&saved, &r, &mut seen, 3).is_empty());
        let json = serde_json::to_value(&again[0]).unwrap();
        assert_eq!(json["row"]["address"], "c");
        assert_eq!(json["primary"], "c");
        assert_eq!(json["query_name"], "Whales");
        assert!(json.get("columns").is_none() && json.get("read").is_none());
    }

    #[test]
    fn seen_is_bounded() {
        let mut seen = Seen::default();
        for i in 0..SEEN_MAX + 10 {
            assert!(seen.insert(i.to_string()));
        }
        assert_eq!(seen.len(), SEEN_MAX);
        assert!(seen.insert("0".to_string()), "the oldest was forgotten");
        assert!(!seen.insert("1000".to_string()));
        assert!(!seen.is_empty());
    }

    #[test]
    fn watched_query_windows_timed_entities_with_overlap() {
        let q = text::parse("tx last 1d where fee > 1 KAS limit 9000").unwrap();
        let run = watched_query(&q, 1_000_000, 1_060_000);
        assert_eq!(
            run.range,
            TimeRange::Between(1_000_000 - OVERLAP_MS, 1_060_000)
        );
        assert_eq!(run.limit, Some(WATCH_LIMIT));
        assert_eq!(run.filter, q.filter);
        let q = text::parse("addresses order by received desc limit 10").unwrap();
        let run = watched_query(&q, 5, 10);
        assert_eq!(run.range, TimeRange::All);
        assert_eq!(run.limit, Some(10));
        assert!(windows(Entity::Blocks) && !windows(Entity::Addresses));
    }
}
