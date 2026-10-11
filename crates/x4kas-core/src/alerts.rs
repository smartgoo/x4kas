//! The alert history: the watchlist's events (`App.watch.events`) and the watched
//! queries' (`App.query.events`), with their read marks, kept per network in
//! `~/.x4kas/alerts/<network>.json` so they outlive a restart and a connection switch.
//! The controller loads a network's file when it connects to it and writes the events
//! back whenever they changed (`AlertHistory::revision`).

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::app::App;
use crate::config;
use crate::index::{hex, parse_hex};
use crate::query::Entity;
use crate::query::exec::{Cell, Column, ColumnSource};
use crate::query::fields::{self, FieldKind};
use crate::query::watch::QueryEvent;
use crate::watch::AddressEvent;

/// The events of one network, newest first, as `App` holds them, and the rows the
/// watched address queries know (`QueryState::known`).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct AlertHistory {
    pub watch: VecDeque<AddressEvent>,
    pub queries: VecDeque<QueryEvent>,
    pub known: HashMap<String, Vec<String>>,
}

/// A history file that couldn't be read.
#[derive(Debug)]
pub enum LoadError {
    /// Moved aside to the given path, so a save can't overwrite it.
    SetAside(anyhow::Error, PathBuf),
    /// Still in place: nothing may be saved over it.
    Kept(anyhow::Error),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SetAside(e, to) => write!(f, "{e:#} (the file was moved to {})", to.display()),
            Self::Kept(e) => write!(f, "{e:#} (not saving over it)"),
        }
    }
}

impl AlertHistory {
    pub fn path(network: &str) -> PathBuf {
        config::data_dir()
            .join("alerts")
            .join(format!("{network}.json"))
    }

    /// The network's history (empty when there is none). A file that doesn't read is
    /// moved aside, so the history saved next doesn't replace it.
    pub fn load(network: &str) -> Result<Self, LoadError> {
        Self::load_from(&Self::path(network))
    }

    fn load_from(path: &Path) -> Result<Self, LoadError> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let read = || -> Result<Self> {
            let file: HistoryFile = serde_json::from_str(&std::fs::read_to_string(path)?)?;
            Ok(file.into())
        };
        read().map_err(|e| {
            let aside = path.with_extension(format!("json.bad-{}", crate::format::now_ms()));
            match std::fs::rename(path, &aside) {
                Ok(()) => LoadError::SetAside(e, aside),
                Err(_) => LoadError::Kept(e),
            }
        })
    }

    pub fn save(&self, network: &str) -> Result<()> {
        self.save_to(&Self::path(network))
    }

    /// Write to a temporary file and rename it over the file, so a reader never sees
    /// it half written.
    fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(&HistoryFile::from(self))?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// The events `app` holds now, and what its watched queries know (of the queries
    /// still saved).
    pub fn of(app: &App) -> Self {
        Self {
            watch: app.watch.events.clone(),
            queries: app.query.events.clone(),
            known: app
                .query
                .known
                .iter()
                .filter(|(id, _)| app.query.saved.watched().any(|q| q.id == **id))
                .map(|(id, known)| (id.clone(), known.clone()))
                .collect(),
        }
    }

    /// Changes whenever either list of events does (an event pushed, a read mark, a
    /// clear), so a writer can tell whether there is anything to save.
    pub fn revision(app: &App) -> u64 {
        app.watch.events_rev.wrapping_add(app.query.events_rev)
    }

    /// Show these events in `app` in place of the ones there. Not counted as raised,
    /// so nothing is toasted again.
    pub fn restore(self, app: &mut App) {
        app.watch.events = self.watch;
        app.query.events = self.queries;
        app.query.known = self.known;
        app.watch.events_rev = app.watch.events_rev.wrapping_add(1);
        app.query.events_rev = app.query.events_rev.wrapping_add(1);
        app.mark_dirty();
    }
}

#[derive(Serialize, Deserialize, Default)]
struct HistoryFile {
    #[serde(default)]
    watch: Vec<StoredAddressEvent>,
    #[serde(default)]
    queries: Vec<StoredQueryEvent>,
    #[serde(default)]
    known: BTreeMap<String, Vec<String>>,
}

/// An `AddressEvent` with its read mark, which the CLI's JSON lines leave out.
#[derive(Serialize, Deserialize)]
struct StoredAddressEvent {
    #[serde(flatten)]
    event: AddressEvent,
    #[serde(default)]
    read: bool,
}

/// A `QueryEvent` with its columns by field name (all of a watched query's columns are
/// fields: a grouped query can't be watched) under the entity they belong to.
#[derive(Serialize, Deserialize)]
struct StoredQueryEvent {
    time_ms: u64,
    query_id: String,
    query_name: String,
    entity: String,
    columns: Vec<String>,
    row: Vec<StoredCell>,
    primary: String,
    #[serde(default)]
    read: bool,
}

/// A `Cell` as JSON: hashes in hex, enum values by name.
#[derive(Serialize, Deserialize)]
#[serde(tag = "t", content = "v", rename_all = "snake_case")]
enum StoredCell {
    Null,
    Bool(bool),
    Int(i64),
    Amount(i64),
    Float(f64),
    Time(u64),
    Hash(String),
    Txid(String),
    Address(String),
    Text(String),
    Enum(String),
    List(Vec<StoredCell>),
}

impl From<&Cell> for StoredCell {
    fn from(cell: &Cell) -> Self {
        match cell {
            Cell::Null => Self::Null,
            Cell::Bool(b) => Self::Bool(*b),
            Cell::Int(n) => Self::Int(*n),
            Cell::Amount(n) => Self::Amount(*n),
            // JSON has no infinity or NaN (they would be written as null, which then
            // fails to load as a float).
            Cell::Float(f) if f.is_finite() => Self::Float(*f),
            Cell::Float(_) => Self::Null,
            Cell::Time(ms) => Self::Time(*ms),
            Cell::Hash(h) => Self::Hash(hex(h)),
            Cell::Txid(h) => Self::Txid(hex(h)),
            Cell::Address(a) => Self::Address(a.clone()),
            Cell::Text(t) => Self::Text(t.clone()),
            Cell::Enum(e) => Self::Enum((*e).to_string()),
            Cell::List(items) => Self::List(items.iter().map(Self::from).collect()),
        }
    }
}

impl StoredCell {
    /// The cell back, under a column of `kind`: an enum value becomes the field's own
    /// name for it (text when the field no longer has it), a hash that doesn't parse text.
    fn into_cell(self, kind: FieldKind) -> Cell {
        match self {
            Self::Null => Cell::Null,
            Self::Bool(b) => Cell::Bool(b),
            Self::Int(n) => Cell::Int(n),
            Self::Amount(n) => Cell::Amount(n),
            Self::Float(f) => Cell::Float(f),
            Self::Time(ms) => Cell::Time(ms),
            Self::Hash(h) => parse_hex(&h).map_or(Cell::Text(h), Cell::Hash),
            Self::Txid(h) => parse_hex(&h).map_or(Cell::Text(h), Cell::Txid),
            Self::Address(a) => Cell::Address(a),
            Self::Text(t) => Cell::Text(t),
            Self::Enum(e) => match kind {
                FieldKind::Enum(values) => values
                    .iter()
                    .find(|v| **v == e)
                    .map_or(Cell::Text(e), |v| Cell::Enum(v)),
                _ => Cell::Text(e),
            },
            Self::List(items) => Cell::List(items.into_iter().map(|c| c.into_cell(kind)).collect()),
        }
    }
}

impl From<&QueryEvent> for StoredQueryEvent {
    fn from(e: &QueryEvent) -> Self {
        let entity = e
            .columns
            .iter()
            .find_map(|c| match c.source {
                ColumnSource::Field(f) => Some(f.entity()),
                _ => None,
            })
            .unwrap_or(Entity::Transactions);
        Self {
            time_ms: e.time_ms,
            query_id: e.query_id.clone(),
            query_name: e.query_name.clone(),
            entity: entity.name().to_string(),
            columns: e.columns.iter().map(|c| c.name.clone()).collect(),
            row: e.row.iter().map(StoredCell::from).collect(),
            primary: e.primary.clone(),
            read: e.read,
        }
    }
}

impl StoredQueryEvent {
    /// The event back; `None` when one of its columns is no longer a field.
    fn into_event(self) -> Option<QueryEvent> {
        let entity = Entity::ALL.into_iter().find(|e| e.name() == self.entity)?;
        let columns = self
            .columns
            .iter()
            .map(|name| {
                let f = fields::by_name(entity, name)?;
                Some(Column {
                    name: f.name().to_string(),
                    label: f.label().to_string(),
                    kind: f.kind(),
                    source: ColumnSource::Field(f),
                })
            })
            .collect::<Option<Vec<_>>>()?;
        if columns.len() != self.row.len() {
            return None;
        }
        let row = self
            .row
            .into_iter()
            .zip(&columns)
            .map(|(cell, column)| cell.into_cell(column.kind))
            .collect();
        Some(QueryEvent {
            time_ms: self.time_ms,
            query_id: self.query_id,
            query_name: self.query_name,
            columns,
            row,
            primary: self.primary,
            read: self.read,
        })
    }
}

impl From<&AlertHistory> for HistoryFile {
    fn from(h: &AlertHistory) -> Self {
        Self {
            watch: h
                .watch
                .iter()
                .map(|e| StoredAddressEvent {
                    event: e.clone(),
                    read: e.read,
                })
                .collect(),
            queries: h.queries.iter().map(StoredQueryEvent::from).collect(),
            known: h
                .known
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        }
    }
}

impl From<HistoryFile> for AlertHistory {
    fn from(f: HistoryFile) -> Self {
        Self {
            watch: f
                .watch
                .into_iter()
                .map(|s| AddressEvent {
                    read: s.read,
                    ..s.event
                })
                .take(crate::watch::MAX_EVENTS)
                .collect(),
            queries: f
                .queries
                .into_iter()
                .filter_map(StoredQueryEvent::into_event)
                .take(crate::query::watch::MAX_EVENTS)
                .collect(),
            known: f.known.into_iter().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::fields::FieldId;
    use crate::watch::EventKind;

    fn column(f: FieldId) -> Column {
        Column {
            name: f.name().to_string(),
            label: f.label().to_string(),
            kind: f.kind(),
            source: ColumnSource::Field(f),
        }
    }

    #[test]
    fn history_round_trips_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("alerts").join("mainnet.json");
        assert_eq!(
            AlertHistory::load_from(&path).unwrap(),
            AlertHistory::default()
        );
        let known = HashMap::from([("q2".to_string(), vec!["kaspa:a".to_string()])]);

        let protocol = fields::by_name(Entity::Transactions, "protocol").unwrap();
        let FieldKind::Enum(values) = protocol.kind() else {
            panic!("protocol is an enum");
        };
        let columns = vec![
            column(fields::by_name(Entity::Transactions, "time").unwrap()),
            column(fields::by_name(Entity::Transactions, "txid").unwrap()),
            column(fields::by_name(Entity::Transactions, "fee").unwrap()),
            column(protocol),
        ];
        let history = AlertHistory {
            watch: VecDeque::from([AddressEvent {
                time_ms: 5,
                address: "kaspa:x".into(),
                kind: EventKind::Sent,
                amount: 7,
                txid: Some("ab".into()),
                is_coinbase: false,
                balance_after: Some(3),
                alerts: vec!["sent 7 KAS".into()],
                read: true,
            }]),
            queries: VecDeque::from([QueryEvent {
                time_ms: 9,
                query_id: "q1".into(),
                query_name: "Big fees".into(),
                columns,
                row: vec![
                    Cell::Time(1_000),
                    Cell::Txid([7; 32]),
                    Cell::Amount(12),
                    Cell::Enum(values[0]),
                ],
                primary: hex(&[7; 32]),
                read: false,
            }]),
            known,
        };
        history.save_to(&path).unwrap();
        assert_eq!(AlertHistory::load_from(&path).unwrap(), history);
    }

    #[test]
    fn an_event_whose_column_is_gone_is_dropped() {
        let stored = StoredQueryEvent {
            time_ms: 0,
            query_id: "q".into(),
            query_name: "q".into(),
            entity: "tx".into(),
            columns: vec!["no_such_field".into()],
            row: vec![StoredCell::Int(1)],
            primary: "x".into(),
            read: false,
        };
        assert!(stored.into_event().is_none());
    }

    #[test]
    fn a_non_finite_float_is_saved_as_null() {
        let cell = StoredCell::from(&Cell::Float(f64::INFINITY));
        let json = serde_json::to_string(&cell).unwrap();
        let back: StoredCell = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, StoredCell::Null));
    }

    #[test]
    fn an_unreadable_file_is_set_aside() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mainnet.json");
        std::fs::write(&path, "{ not json").unwrap();
        let Err(LoadError::SetAside(_, aside)) = AlertHistory::load_from(&path) else {
            panic!("set aside");
        };
        assert!(!path.exists());
        assert_eq!(std::fs::read_to_string(aside).unwrap(), "{ not json");
        assert_eq!(
            AlertHistory::load_from(&path).unwrap(),
            AlertHistory::default()
        );
    }
}
