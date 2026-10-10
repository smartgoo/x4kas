//! The Query tab: build a query over the index (`x4kas_core::query`) in a structured
//! builder or as text, run it, read the result in a table, export it, and keep it in
//! the saved queries. The builder edits a draft of the model and prints the text; the
//! text, once edited, parses back into the draft. The run itself happens in the
//! controller (`UiCommand::QueryRun`), which reports in `app.query`.

use eframe::egui::containers::menu::{MenuButton, MenuConfig};
use eframe::egui::{
    self, Button, ComboBox, Key, Modifiers, PopupCloseBehavior, RichText, TextEdit, Ui,
};
use egui_extras::Column;

use super::address::export_status;
use super::monitoring::alert_dot;
use super::theme;
use super::toasts::primary_link;
use super::widgets::{
    CARD_GAP, address, block_hash, card_overhead, card_with_header, copy_value,
    direct_node_placeholder, field_label, kv_grid, label_search_popup, modal_window, page_table,
    placeholder, primary_button, request_explorer, request_pane, section_title, table_row_height,
    transaction_id,
};
use x4kas_core::app::{App, ChainPhase, ExportOrigin};
use x4kas_core::config::IndexSettings;
use x4kas_core::controller::{CommandSender, ExportRequest, UiCommand};
use x4kas_core::explorer::ExplorerPage;
use x4kas_core::format::{
    format_duration, format_duration_ms, format_kas, format_number, format_sompi_exact, format_utc,
    now_ms, parse_utc,
};
use x4kas_core::index::export::ExportFormat;
use x4kas_core::index::hex;
use x4kas_core::query::exec::{Cell, ColumnSource, MAX_RESULT_ROWS, ResultSet};
use x4kas_core::query::fields::{self, Cost, FieldSpec};
use x4kas_core::query::saved::{QueryWatch, SavedQueries, SavedQuery, presets};
use x4kas_core::query::text;
use x4kas_core::query::watch::QueryEvent;
use x4kas_core::query::{
    Arity, Condition, Dir, Entity, FieldId, FieldKind, Filter, GroupBy, GroupKey, Metric, Op,
    OrderKey, Query, QueryError, TimeRange, Value,
};

const SIDEBAR_WIDTH: f32 = 260.0;
const SIDEBAR_WIDTH_RANGE: std::ops::RangeInclusive<f32> = 200.0..=440.0;
/// The matches-over-time strip above the table.
const HISTOGRAM_HEIGHT: f32 = 32.0;
const HISTOGRAM_BUCKETS: usize = 48;
/// Order keys the builder offers at most.
const MAX_ORDER: usize = 3;
/// Alerts the sidebar lists at most.
const ALERTS_SHOWN: usize = 8;
/// A second right-click within this long of "Delete…" offers "Delete for good".
const CONFIRM_SECS: f64 = 3.0;
/// The results table is at least this tall.
const MIN_TABLE_HEIGHT: f32 = 360.0;
/// Text and list columns stop growing at this width (the full value is on hover).
const MAX_TEXT_COLUMN: f32 = 360.0;
/// List cells show this many values before "+n".
const LIST_PREVIEW: usize = 2;

/// Where the Query tab's time ranges come from.
const RANGES: [(&str, Option<u64>); 6] = [
    ("Last hour", Some(3_600_000)),
    ("Last 6 hours", Some(6 * 3_600_000)),
    ("Last 24 hours", Some(24 * 3_600_000)),
    ("Last 7 days", Some(7 * 24 * 3_600_000)),
    ("Last 30 days", Some(30 * 24 * 3_600_000)),
    ("Indexed window", None),
];

/// A node of the builder's filter tree, mirroring `Filter` with editable text.
enum Node {
    Group {
        key: u64,
        /// `any of` (or) rather than `all of` (and).
        any: bool,
        not: bool,
        children: Vec<Node>,
    },
    Cond {
        key: u64,
        not: bool,
        field: FieldId,
        op: Op,
        /// The value as typed; the second one for `between`.
        value: String,
        value2: String,
        /// Amounts are typed in this unit.
        kas: bool,
        /// Why the value doesn't parse, shown on the row.
        error: Option<String>,
        /// The label matches popup under an address value is showing.
        search_open: bool,
    },
}

impl Node {
    fn key(&self) -> u64 {
        match self {
            Node::Group { key, .. } | Node::Cond { key, .. } => *key,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MetricKind {
    Count,
    CountDistinct,
    Sum,
    Avg,
    Min,
    Max,
}

impl MetricKind {
    const ALL: [Self; 6] = [
        Self::Count,
        Self::CountDistinct,
        Self::Sum,
        Self::Avg,
        Self::Min,
        Self::Max,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Count => "count",
            Self::CountDistinct => "count distinct",
            Self::Sum => "sum",
            Self::Avg => "average",
            Self::Min => "min",
            Self::Max => "max",
        }
    }

    fn takes_field(self) -> bool {
        self != Self::Count
    }
}

struct MetricDraft {
    key: u64,
    kind: MetricKind,
    field: FieldId,
}

struct KeyDraft {
    key: u64,
    /// A time bucket rather than a field.
    bucket: bool,
    field: FieldId,
    width: String,
}

struct OrderDraft {
    key: u64,
    target: OrderKey,
    desc: bool,
}

/// The builder's editable copy of a query.
struct Draft {
    entity: Entity,
    root: Node,
    /// Index into `RANGES`, or `None` for a custom range.
    range: Option<usize>,
    custom_from: String,
    custom_to: String,
    grouped: bool,
    keys: Vec<KeyDraft>,
    metrics: Vec<MetricDraft>,
    order: Vec<OrderDraft>,
    limit: String,
    columns: Option<Vec<FieldId>>,
}

/// The first field of `entity` a new condition starts with.
/// A field in a picker, selected or not: greyed out, with why on hover, while the index
/// doesn't keep the field's data (`FieldId::unavailable`). Returns whether it was picked.
fn field_option(ui: &mut Ui, selected: bool, spec: &FieldSpec, features: &IndexSettings) -> bool {
    let why = spec.id.unavailable(features);
    let response = ui
        .add_enabled(why.is_none(), Button::selectable(selected, spec.label))
        .on_hover_text(spec.doc);
    match why {
        Some(why) => response.on_disabled_hover_text(why).clicked(),
        None => response.clicked(),
    }
}

fn first_field(entity: Entity) -> FieldId {
    fields::for_entity(entity)
        .find(|f| f.cost != Cost::Node)
        .map(|f| f.id)
        .expect("every entity has a field")
}

/// A numeric field of `entity` for a new metric.
fn first_numeric(entity: Entity) -> FieldId {
    fields::for_entity(entity)
        .find(|f| {
            matches!(
                f.kind,
                FieldKind::Int | FieldKind::Amount | FieldKind::Float
            ) && f.cost != Cost::Node
        })
        .map(|f| f.id)
        .unwrap_or_else(|| first_field(entity))
}

impl Draft {
    fn new(entity: Entity, keys: &mut u64) -> Self {
        Self::from_query(&Query::default_for(entity), keys)
    }

    fn from_query(q: &Query, keys: &mut u64) -> Self {
        let mut next = || {
            *keys += 1;
            *keys
        };
        let root = match &q.filter {
            Some(f) => {
                let node = node_from_filter(f, &mut next);
                match node {
                    group @ Node::Group { .. } => group,
                    cond => Node::Group {
                        key: next(),
                        any: false,
                        not: false,
                        children: vec![cond],
                    },
                }
            }
            None => Node::Group {
                key: next(),
                any: false,
                not: false,
                children: Vec::new(),
            },
        };
        let (range, custom_from, custom_to) = match q.range {
            TimeRange::All => (Some(RANGES.len() - 1), String::new(), String::new()),
            TimeRange::Last(ms) => match RANGES.iter().position(|(_, r)| *r == Some(ms)) {
                Some(i) => (Some(i), String::new(), String::new()),
                None => (None, format_duration_ms(ms), String::new()),
            },
            TimeRange::Since(from) => (None, text::format_time(from), String::new()),
            TimeRange::Between(from, to) => (None, text::format_time(from), text::format_time(to)),
        };
        let (grouped, keys_d, metrics) = match &q.group {
            Some(g) => (
                true,
                g.keys
                    .iter()
                    .map(|k| match k {
                        GroupKey::Field(f) => KeyDraft {
                            key: next(),
                            bucket: false,
                            field: *f,
                            width: "1h".to_string(),
                        },
                        GroupKey::TimeBucket(ms) => KeyDraft {
                            key: next(),
                            bucket: true,
                            field: first_field(q.entity),
                            width: format_duration_ms(*ms),
                        },
                    })
                    .collect(),
                g.metrics
                    .iter()
                    .map(|m| MetricDraft {
                        key: next(),
                        kind: match m {
                            Metric::Count => MetricKind::Count,
                            Metric::CountDistinct(_) => MetricKind::CountDistinct,
                            Metric::Sum(_) => MetricKind::Sum,
                            Metric::Avg(_) => MetricKind::Avg,
                            Metric::Min(_) => MetricKind::Min,
                            Metric::Max(_) => MetricKind::Max,
                        },
                        field: m.field().unwrap_or_else(|| first_numeric(q.entity)),
                    })
                    .collect(),
            ),
            None => (false, Vec::new(), Vec::new()),
        };
        Self {
            entity: q.entity,
            root,
            range,
            custom_from,
            custom_to,
            grouped,
            keys: keys_d,
            metrics,
            order: q
                .order
                .iter()
                .map(|(target, dir)| OrderDraft {
                    key: next(),
                    target: *target,
                    desc: *dir == Dir::Desc,
                })
                .collect(),
            limit: q.limit.map(|l| l.to_string()).unwrap_or_default(),
            columns: q.columns.clone(),
        }
    }

    /// The query the draft describes; the first thing wrong with it otherwise (the
    /// conditions keep their own errors for the rows). A condition whose value is
    /// still empty is left out unless `strict` (a run, a save), which reports it.
    fn build(&mut self, strict: bool) -> Result<Query, String> {
        let filter = node_to_filter(&mut self.root, strict)?;
        let range = match self.range {
            Some(i) => match RANGES[i].1 {
                Some(ms) => TimeRange::Last(ms),
                None => TimeRange::All,
            },
            None => {
                let from = strip_ago(self.custom_from.trim());
                let to = strip_ago(self.custom_to.trim());
                if from.is_empty() && to.is_empty() {
                    TimeRange::All
                } else if to.is_empty() {
                    match text::classify_word(from).map_err(|e| e.msg)? {
                        Value::Duration(ms) => TimeRange::Last(ms),
                        Value::Time(t) => TimeRange::Since(t),
                        _ => return Err(format!("{from} isn't a time or a duration")),
                    }
                } else {
                    let from = parse_utc(from).ok_or_else(|| format!("{from} isn't a UTC time"))?;
                    let to = parse_utc(to).ok_or_else(|| format!("{to} isn't a UTC time"))?;
                    TimeRange::Between(from, to)
                }
            }
        };
        let group = if self.grouped {
            let mut keys = Vec::new();
            for k in &self.keys {
                keys.push(if k.bucket {
                    let ms = x4kas_core::format::parse_duration_ms(&k.width)
                        .ok_or_else(|| format!("{} isn't a bucket width (1h, 10m)", k.width))?;
                    GroupKey::TimeBucket(ms)
                } else {
                    GroupKey::Field(k.field)
                });
            }
            let metrics = self
                .metrics
                .iter()
                .map(|m| match m.kind {
                    MetricKind::Count => Metric::Count,
                    MetricKind::CountDistinct => Metric::CountDistinct(m.field),
                    MetricKind::Sum => Metric::Sum(m.field),
                    MetricKind::Avg => Metric::Avg(m.field),
                    MetricKind::Min => Metric::Min(m.field),
                    MetricKind::Max => Metric::Max(m.field),
                })
                .collect();
            Some(GroupBy { keys, metrics })
        } else {
            None
        };
        let limit = match self.limit.trim() {
            "" => None,
            s => Some(
                s.parse::<usize>()
                    .map_err(|_| format!("{s} isn't a row count"))?,
            ),
        };
        let q = Query {
            entity: self.entity,
            filter,
            range,
            group,
            order: self
                .order
                .iter()
                .map(|o| (o.target, if o.desc { Dir::Desc } else { Dir::Asc }))
                .collect(),
            limit,
            columns: if self.grouped {
                None
            } else {
                self.columns.clone()
            },
        };
        q.validate().map_err(|e| e.msg)?;
        Ok(q)
    }
}

fn node_from_filter(filter: &Filter, next: &mut impl FnMut() -> u64) -> Node {
    match filter {
        Filter::And(items) | Filter::Or(items) => Node::Group {
            key: next(),
            any: matches!(filter, Filter::Or(_)),
            not: false,
            children: items.iter().map(|f| node_from_filter(f, next)).collect(),
        },
        Filter::Not(inner) => {
            let mut node = node_from_filter(inner, next);
            match &mut node {
                Node::Group { not, .. } | Node::Cond { not, .. } => *not = !*not,
            }
            node
        }
        Filter::Cond(c) => {
            let kind = c.field.kind();
            let unquoted = |v: &Value| match v {
                Value::Text(t) => t.clone(),
                Value::Amount(sompi) => format_sompi_exact(*sompi),
                other => text::format_value(other),
            };
            let (value, value2) = match (&c.value, c.op.arity()) {
                (Value::List(items), Arity::Two) if items.len() == 2 => {
                    (unquoted(&items[0]), unquoted(&items[1]))
                }
                (Value::List(items), _) => (
                    items.iter().map(unquoted).collect::<Vec<_>>().join(", "),
                    String::new(),
                ),
                (Value::Null, _) => (String::new(), String::new()),
                (v, _) => (unquoted(v), String::new()),
            };
            let _ = kind;
            Node::Cond {
                key: next(),
                not: false,
                field: c.field,
                op: c.op,
                value,
                value2,
                kas: true,
                error: None,
                search_open: false,
            }
        }
    }
}

/// `24h ago` as `24h`: a time typed as "this long ago" may say so.
fn strip_ago(s: &str) -> &str {
    s.strip_suffix(" ago")
        .or_else(|| s.strip_suffix("ago"))
        .map_or(s, str::trim)
}

/// Whether a value typed for `kind` is a 64-hex id (the builder reads `ago` off times,
/// nothing else).
fn tidy_value(kind: FieldKind, s: &str) -> String {
    let s = s.trim();
    if kind == FieldKind::Time {
        strip_ago(s).to_string()
    } else {
        s.to_string()
    }
}

/// One typed value, in the unit chosen for amounts.
fn parse_typed(kind: FieldKind, s: &str, kas: bool) -> Result<Value, QueryError> {
    let s = &tidy_value(kind, s);
    if kind == FieldKind::Amount
        && !s.is_empty()
        && !s.to_ascii_lowercase().ends_with("kas")
        && !s.to_ascii_lowercase().ends_with("sompi")
    {
        let unit = if kas { "KAS" } else { "sompi" };
        return text::parse_value(kind, &format!("{s} {unit}"));
    }
    text::parse_value(kind, s)
}

fn node_to_filter(node: &mut Node, strict: bool) -> Result<Option<Filter>, String> {
    match node {
        Node::Group {
            any, not, children, ..
        } => {
            let mut items = Vec::new();
            let mut first_error = None;
            for child in children.iter_mut() {
                match node_to_filter(child, strict) {
                    Ok(Some(f)) => items.push(f),
                    Ok(None) => {}
                    Err(e) => {
                        first_error.get_or_insert(e);
                    }
                }
            }
            if let Some(e) = first_error {
                return Err(e);
            }
            let filter = match items.len() {
                0 => return Ok(None),
                1 => items.pop().unwrap(),
                _ if *any => Filter::Or(items),
                _ => Filter::And(items),
            };
            Ok(Some(if *not {
                Filter::Not(Box::new(filter))
            } else {
                filter
            }))
        }
        Node::Cond {
            not,
            field,
            op,
            value,
            value2,
            kas,
            error,
            ..
        } => {
            let kind = field.kind();
            // Nothing typed yet: not a condition (and not an error) until it matters.
            if !strict && op.arity() != Arity::None && value.trim().is_empty() {
                *error = None;
                return Ok(None);
            }
            let parsed = match op.arity() {
                Arity::None => Ok(Value::Null),
                Arity::One => parse_typed(kind, value, *kas),
                Arity::Two => parse_typed(kind, value, *kas).and_then(|lo| {
                    parse_typed(kind, value2, *kas).map(|hi| Value::List(vec![lo, hi]))
                }),
                Arity::List => value
                    .split(',')
                    .filter(|s| !s.trim().is_empty())
                    .map(|s| parse_typed(kind, s, *kas))
                    .collect::<Result<Vec<_>, _>>()
                    .map(Value::List),
            };
            let cond = parsed.and_then(|value| {
                let c = Condition::new(*field, *op, value);
                let probe = Query {
                    filter: Some(Filter::Cond(c.clone())),
                    ..Query::default_for(field.entity())
                };
                probe.validate().map(|_| c)
            });
            match cond {
                Ok(c) => {
                    *error = None;
                    Ok(Some(if *not {
                        Filter::Not(Box::new(Filter::Cond(c)))
                    } else {
                        Filter::Cond(c)
                    }))
                }
                Err(e) => {
                    *error = Some(e.msg.clone());
                    Err(format!("{}: {}", field.label(), e.msg))
                }
            }
        }
    }
}

/// What the save dialog does with its name.
#[derive(Clone, PartialEq, Eq)]
enum SaveMode {
    /// A new saved query of the draft.
    New,
    /// Replace the query (and name) of a saved one.
    Update(String),
    /// Only the name and description of a saved one.
    Rename(String),
}

struct SaveDialog {
    mode: SaveMode,
    name: String,
    description: String,
    focus: bool,
    /// Watch the query: re-run it as the index moves and raise its new rows.
    watch: bool,
    every_secs: String,
    notify: bool,
    /// Why the query can't be watched (a grouped one has no row ids), if it can't.
    unwatchable: Option<&'static str>,
    /// The query is over addresses: the first watched run only learns the matches.
    addresses: bool,
}

impl SaveDialog {
    fn new(
        mode: SaveMode,
        name: String,
        description: String,
        watch: Option<&QueryWatch>,
        query: Option<&Query>,
    ) -> Self {
        Self {
            mode,
            name,
            description,
            focus: true,
            watch: watch.is_some_and(|w| w.enabled),
            every_secs: watch
                .map(|w| w.every_secs)
                .unwrap_or(QueryWatch::MIN_EVERY_SECS)
                .to_string(),
            notify: watch.is_none_or(|w| w.notify),
            unwatchable: query.and_then(|q| {
                q.group
                    .is_some()
                    .then_some("Alerts need a row id: a grouped query has none")
            }),
            addresses: query.is_some_and(|q| q.entity == Entity::Addresses),
        }
    }

    /// The interval typed, when it is a number of seconds of at least the minimum.
    fn every(&self) -> Option<u64> {
        self.every_secs
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|s| *s >= QueryWatch::MIN_EVERY_SECS)
    }

    /// The dialog can be saved: a name, and a valid interval if watching.
    fn valid(&self) -> bool {
        !self.name.trim().is_empty() && (!self.watch || self.every().is_some())
    }

    fn watch_setting(&self) -> Option<QueryWatch> {
        (self.watch && self.unwatchable.is_none()).then(|| QueryWatch {
            enabled: true,
            every_secs: self.every().unwrap_or(QueryWatch::MIN_EVERY_SECS),
            notify: self.notify,
        })
    }
}

pub struct QueryTab {
    /// The saved queries pane on the left is open (remembered across launches).
    pub sidebar_open: bool,
    sidebar_search: String,
    draft: Draft,
    text: String,
    /// The text was edited and not yet applied: builder edits don't overwrite it.
    text_dirty: bool,
    text_error: Option<QueryError>,
    /// The builder's query is wrong (shown by the Run button).
    draft_error: Option<String>,
    /// The saved query the builder shows, if any.
    loaded: Option<String>,
    save_dialog: Option<SaveDialog>,
    /// A saved query whose Delete was clicked once, and when.
    confirm_delete: Option<(String, f64)>,
    next_key: u64,
    /// The result column the table is sorted by, if not the result's own order, and
    /// the result (`QueryState::result_seq`) it was sorted for.
    sort: Option<(usize, bool)>,
    sorted_result: u64,
    /// Run on the next frame (Ctrl+Enter, a query handed in).
    run_requested: bool,
    /// A query was loaded: the result of another one is cleared on the next frame.
    loaded_text: Option<String>,
    /// The builder card's height last frame, so the Results card can fill the rest.
    builder_height: f32,
    /// The draft differs from the loaded saved query (computed once per frame).
    modified: bool,
    /// The Alerts section is folded.
    alerts_folded: bool,
    /// The index's opt-in features this frame: fields they gate are greyed out.
    features: IndexSettings,
}

impl Default for QueryTab {
    fn default() -> Self {
        let mut keys = 0;
        let draft = Draft::new(Entity::Transactions, &mut keys);
        let mut tab = Self {
            sidebar_open: true,
            sidebar_search: String::new(),
            draft,
            text: String::new(),
            text_dirty: false,
            text_error: None,
            draft_error: None,
            loaded: None,
            save_dialog: None,
            confirm_delete: None,
            next_key: keys,
            sort: None,
            sorted_result: 0,
            run_requested: false,
            loaded_text: None,
            builder_height: 0.0,
            modified: false,
            alerts_folded: false,
            features: IndexSettings::default(),
        };
        tab.sync_text();
        tab
    }
}

impl QueryTab {
    /// The save dialog is on show.
    pub fn modal_open(&self) -> bool {
        self.save_dialog.is_some()
    }

    /// Show `query` in the builder (a saved query or template, a query handed in).
    fn load(&mut self, query: &Query, loaded: Option<String>) {
        self.draft = Draft::from_query(query, &mut self.next_key);
        self.loaded = loaded;
        self.text_dirty = false;
        self.text_error = None;
        self.draft_error = None;
        self.loaded_text = Some(query.to_text());
        self.sync_text();
    }

    /// Print the draft into the text line, unless the text is being edited.
    fn sync_text(&mut self) {
        if self.text_dirty {
            return;
        }
        match self.draft.build(false) {
            Ok(q) => {
                self.text = q.to_text();
                self.draft_error = None;
            }
            Err(e) => self.draft_error = Some(e),
        }
    }

    /// Parse the text line into the builder. With `print`, the text is also
    /// rewritten in its canonical form (Enter); while typing it is left alone.
    fn apply_text(&mut self, print: bool) {
        match text::parse_valid(&self.text) {
            Ok(q) => {
                self.draft = Draft::from_query(&q, &mut self.next_key);
                self.text_error = None;
                self.draft_error = None;
                if print {
                    self.text_dirty = false;
                    self.text = q.to_text();
                }
            }
            Err(e) => self.text_error = Some(e),
        }
    }

    /// Drop the text line's edit and show the builder's query again.
    fn revert_text(&mut self) {
        self.text_dirty = false;
        self.text_error = None;
        self.sync_text();
    }

    /// The query to run or save: the text when it was edited, else the builder's.
    fn current_query(&mut self) -> Result<Query, String> {
        if self.text_dirty {
            self.apply_text(true);
            if let Some(e) = &self.text_error {
                return Err(e.to_string());
            }
        }
        self.draft.build(true)
    }

    /// The saved query or template `loaded` names, if any.
    fn loaded_query(&self, app: &App) -> Option<SavedQuery> {
        let id = self.loaded.as_deref()?;
        app.query
            .saved
            .get(id)
            .cloned()
            .or_else(|| presets().into_iter().find(|p| p.id == id))
    }

    /// Queries can run: a direct node whose index is open. Else why not.
    fn ready(app: &App) -> Result<(), String> {
        if !app.connection.is_direct() {
            return Err(direct_node_placeholder(
                app,
                "Queries read the address index, which needs a direct node connection",
            )
            .to_string());
        }
        match &app.chain.phase {
            ChainPhase::Error(e) => Err(format!("The address index isn't open: {e}")),
            ChainPhase::Idle | ChainPhase::Opening => {
                Err("The address index is still opening".to_string())
            }
            _ => Ok(()),
        }
    }

    /// Why the builder's query can't run with the index's features: a field it reads
    /// whose data isn't kept (from a saved query or the text line; the pickers grey them).
    fn feature_block(&mut self) -> Option<String> {
        let q = self.draft.build(false).ok()?;
        q.fields_used().into_iter().find_map(|f| {
            f.unavailable(&self.features)
                .map(|why| format!("{}: {why}", f.label()))
        })
    }

    fn run(&mut self, app: &App, cmd_tx: &CommandSender) {
        if let Err(why) = Self::ready(app).and_then(|()| match self.feature_block() {
            Some(why) => Err(why),
            None => Ok(()),
        }) {
            self.draft_error = Some(why);
            return;
        }
        match self.current_query() {
            Ok(q) => {
                let name = self.loaded_query(app).map(|s| s.name);
                let _ = cmd_tx.send(UiCommand::QueryRun { query: q, name });
            }
            Err(e) => self.draft_error = Some(e),
        }
    }

    /// `close`: this frame's Esc is for the save dialog. `modal`: a window is open
    /// (the dialog, Help, …), so the tab's shortcuts stay off.
    pub fn show(
        &mut self,
        ui: &mut Ui,
        app: &mut App,
        cmd_tx: &CommandSender,
        close: bool,
        modal: bool,
    ) {
        self.features = app.index_settings;
        if let Some(q) = app.query.preload.take() {
            let id = app.query.preload_id.take();
            self.load(&q, id);
            self.run_requested = true;
        }
        // Loading a query blanks the Results of another one (and drops its run).
        if let Some(text) = self.loaded_text.take() {
            let shows_other = |q: Option<&Query>| q.is_some_and(|q| q.to_text() != text);
            if shows_other(app.query.result_query.as_ref())
                || shows_other(app.query.run.as_ref().map(|r| &r.query))
                || shows_other(app.query.cancelled.as_ref().map(|c| &c.query))
                || app.query.error.is_some()
            {
                app.query.clear_result();
            }
        }
        // Run with Cmd/Ctrl+Enter, even from a text field (it types nothing), unless a
        // window is open over the tab.
        if !modal
            && ui
                .ctx()
                .input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Enter))
        {
            self.run_requested = true;
        }
        if std::mem::take(&mut self.run_requested) {
            self.run(app, cmd_tx);
        }
        self.modified = match self.loaded_query(app) {
            Some(saved) => {
                self.text_dirty
                    || self
                        .draft
                        .build(false)
                        .map(|q| q.to_text() != saved.text)
                        .unwrap_or(true)
            }
            None => false,
        };
        // The Results card fills what the builder leaves of the tab, measured outside
        // the page's scroll area so it doesn't change with the scroll position.
        // What the Results card adds around its table: the gap above it, its own frame
        // and the table's header row.
        let header =
            CARD_GAP + card_overhead(ui) + theme::ROW_HEIGHT + 4.0 + ui.spacing().item_spacing.y;
        let table_height =
            (ui.available_height() - self.builder_height - header).max(MIN_TABLE_HEIGHT);

        if self.sidebar_open {
            egui::SidePanel::left("query_sidebar")
                .resizable(true)
                // The card's border is the edge; a panel line beside it would be a stray.
                .show_separator_line(false)
                .default_width(SIDEBAR_WIDTH)
                .width_range(SIDEBAR_WIDTH_RANGE)
                .frame(egui::Frame::NONE.inner_margin(egui::Margin {
                    right: CARD_GAP as i8,
                    ..Default::default()
                }))
                .show_inside(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .auto_shrink(false)
                        .show(ui, |ui| self.sidebar(ui, app, cmd_tx));
                });
        }
        egui::ScrollArea::vertical()
            .id_salt("query_page")
            .auto_shrink(false)
            .show(ui, |ui| {
                let top = ui.cursor().top();
                self.builder(ui, app, cmd_tx);
                self.builder_height = ui.cursor().top() - top;
                ui.add_space(CARD_GAP);
                self.results(ui, app, cmd_tx, table_height);
            });
        self.save_dialog_ui(ui.ctx(), app, cmd_tx, close);
    }

    // --- Sidebar ---

    fn sidebar(&mut self, ui: &mut Ui, app: &mut App, cmd_tx: &CommandSender) {
        card_with_header(
            ui,
            "Queries",
            self,
            |ui, tab| {
                if ui
                    .small_button("+ New")
                    .on_hover_text("Start a new query")
                    .clicked()
                {
                    let entity = tab.draft.entity;
                    tab.load(&Query::default_for(entity), None);
                }
            },
            |ui, tab| {
                ui.horizontal(|ui| {
                    let clear = !tab.sidebar_search.is_empty();
                    let width = ui.available_width()
                        - if clear {
                            ui.spacing().interact_size.x + ui.spacing().item_spacing.x
                        } else {
                            0.0
                        };
                    let field = ui.add(
                        TextEdit::singleline(&mut tab.sidebar_search)
                            .hint_text("Find a query")
                            .desired_width(width),
                    );
                    if field.lost_focus() && ui.input(|i| i.key_pressed(Key::Escape)) {
                        tab.sidebar_search.clear();
                    }
                    if clear && ui.small_button("×").on_hover_text("Clear").clicked() {
                        tab.sidebar_search.clear();
                    }
                });
                let needle = tab.sidebar_search.trim().to_lowercase();
                let matches = |q: &SavedQuery| {
                    needle.is_empty()
                        || q.name.to_lowercase().contains(&needle)
                        || q.description.to_lowercase().contains(&needle)
                        || q.text.to_lowercase().contains(&needle)
                };
                let saved = app.query.saved.clone();
                let sorted = saved.sorted();
                let pinned: Vec<&SavedQuery> = sorted
                    .iter()
                    .copied()
                    .filter(|q| q.pinned && matches(q))
                    .collect();
                let others: Vec<&SavedQuery> = sorted
                    .iter()
                    .copied()
                    .filter(|q| !q.pinned && matches(q))
                    .collect();
                let templates = presets();
                let templates: Vec<&SavedQuery> = templates.iter().filter(|q| matches(q)).collect();
                if let Some(e) = &app.query.save_error {
                    ui.label(RichText::new(e).color(theme::ERROR));
                }
                alerts_section(ui, app, &mut tab.alerts_folded);
                // A "Delete…" is forgotten once its moment passes.
                let now = ui.input(|i| i.time);
                if tab
                    .confirm_delete
                    .as_ref()
                    .is_some_and(|(_, at)| now - at >= CONFIRM_SECS)
                {
                    tab.confirm_delete = None;
                }
                let mut changed: Option<SavedQueries> = None;
                let none = pinned.is_empty() && others.is_empty() && templates.is_empty();
                for (title, list) in [
                    ("Pinned", pinned),
                    ("Saved", others),
                    ("Templates", templates),
                ] {
                    if list.is_empty() && (title != "Saved" || !needle.is_empty()) {
                        continue;
                    }
                    ui.add_space(4.0);
                    section_title(ui, title);
                    if list.is_empty() {
                        placeholder(ui, "No saved queries");
                    }
                    for q in list {
                        tab.saved_row(ui, app, cmd_tx, q, &saved, &mut changed);
                    }
                }
                if none {
                    ui.add_space(4.0);
                    placeholder(ui, "No query matches.");
                }
                if let Some(list) = changed {
                    let _ = cmd_tx.send(UiCommand::QueriesSet(list));
                }
            },
        );
    }

    /// One saved query (or template) in the sidebar: a click loads it, a right-click
    /// offers more. Edits to the list go into `changed`.
    fn saved_row(
        &mut self,
        ui: &mut Ui,
        app: &App,
        cmd_tx: &CommandSender,
        q: &SavedQuery,
        list: &SavedQueries,
        changed: &mut Option<SavedQueries>,
    ) {
        let selected = self.loaded.as_deref() == Some(q.id.as_str());
        let parsed = q.query();
        let unread = app.query.unread_of(&q.id);
        let status = app.query.watch_status.get(&q.id);
        let failing = status.is_some_and(|s| s.last_error.is_some());
        let modified = selected && self.modified;
        let mut name = q.name.clone();
        if modified {
            name.push_str(" •");
        }
        let title = match (&parsed, q.is_watched()) {
            (Err(_), _) => RichText::new(name).color(theme::ERROR),
            (Ok(_), true) if failing => RichText::new(format!("◉ {name}")).color(theme::ERROR),
            (Ok(_), true) if unread > 0 => {
                RichText::new(format!("◉ {name} ({unread})")).color(theme::WARN)
            }
            (Ok(_), true) => RichText::new(format!("◉ {name}")),
            (Ok(_), false) => RichText::new(name),
        };
        let response = ui.selectable_label(selected, title).on_hover_ui(|ui| {
            if !q.description.is_empty() {
                ui.label(&q.description);
            }
            ui.label(RichText::new(&q.text).weak());
            if let Err(e) = &parsed {
                ui.label(RichText::new(format!("Doesn't parse: {e}")).color(theme::ERROR));
            }
            if modified {
                ui.label(RichText::new("Changed in the builder; Save keeps it").weak());
            }
            if q.is_watched() {
                match status {
                    Some(s) => {
                        let when = s
                            .last_run_ms
                            .map(|t| format!("{} ago", ago(t)))
                            .unwrap_or_else(|| "not yet".to_string());
                        match &s.last_error {
                            Some(e) => {
                                ui.label(
                                    RichText::new(format!("Watch failed ({when}): {e}"))
                                        .color(theme::ERROR),
                                );
                            }
                            None => {
                                ui.label(
                                    RichText::new(format!(
                                        "Watched: last run {when}, {} row{}",
                                        s.last_rows,
                                        if s.last_rows == 1 { "" } else { "s" }
                                    ))
                                    .weak(),
                                );
                            }
                        }
                    }
                    None => {
                        ui.label(
                            RichText::new(
                                "Watched: runs while connected to a direct node, once the \
                                 index moves",
                            )
                            .weak(),
                        );
                    }
                }
            }
            ui.label(
                RichText::new("Click to load, double-click to run")
                    .weak()
                    .small(),
            );
        });
        if response.clicked() {
            match &parsed {
                Ok(query) => self.load(query, Some(q.id.clone())),
                Err(e) => {
                    // Show the text so it can be fixed.
                    self.loaded = Some(q.id.clone());
                    self.text = q.text.clone();
                    self.text_dirty = true;
                    self.text_error = Some(e.clone());
                }
            }
        }
        let mut run = response.double_clicked() && parsed.is_ok();
        egui::Popup::context_menu(&response)
            .close_behavior(PopupCloseBehavior::CloseOnClickOutside)
            .show(|ui| {
                if ui.add_enabled(parsed.is_ok(), Button::new("Run")).clicked() {
                    run = true;
                    ui.close();
                }
                if q.is_preset() {
                    if ui.button("Save a copy").clicked() {
                        self.save_dialog = Some(SaveDialog::new(
                            SaveMode::New,
                            q.name.clone(),
                            q.description.clone(),
                            None,
                            parsed.as_ref().ok(),
                        ));
                        if let Ok(query) = &parsed {
                            self.load(query, Some(q.id.clone()));
                        }
                        ui.close();
                    }
                    return;
                }
                if ui.button("Rename / watch…").clicked() {
                    self.save_dialog = Some(SaveDialog::new(
                        SaveMode::Rename(q.id.clone()),
                        q.name.clone(),
                        q.description.clone(),
                        q.watch.as_ref(),
                        parsed.as_ref().ok(),
                    ));
                    ui.close();
                }
                if ui.button("Duplicate").clicked() {
                    let mut copy = q.clone();
                    copy.id = x4kas_core::query::saved::new_id();
                    copy.name = format!("{} (copy)", q.name);
                    copy.pinned = false;
                    copy.created_ms = now_ms();
                    copy.updated_ms = copy.created_ms;
                    let mut list = list.clone();
                    list.upsert(copy);
                    *changed = Some(list);
                    ui.close();
                }
                if ui.button(if q.pinned { "Unpin" } else { "Pin" }).clicked() {
                    let mut list = list.clone();
                    if let Some(entry) = list.get_mut(&q.id) {
                        entry.pinned = !entry.pinned;
                    }
                    *changed = Some(list);
                    ui.close();
                }
                ui.separator();
                let now = ui.input(|i| i.time);
                let armed = self
                    .confirm_delete
                    .as_ref()
                    .is_some_and(|(id, at)| *id == q.id && now - at < CONFIRM_SECS);
                if armed {
                    if ui
                        .button(RichText::new("Delete for good").color(theme::ERROR))
                        .on_hover_text("Gone from ~/.x4kas/queries.toml; there is no undo")
                        .clicked()
                    {
                        let mut list = list.clone();
                        list.remove(&q.id);
                        *changed = Some(list);
                        self.confirm_delete = None;
                        if self.loaded.as_deref() == Some(q.id.as_str()) {
                            self.loaded = None;
                        }
                        ui.close();
                    }
                } else if ui
                    .button("Delete…")
                    .on_hover_text("Asks once more")
                    .clicked()
                {
                    self.confirm_delete = Some((q.id.clone(), now));
                }
            });
        if run && let Ok(query) = parsed {
            self.load(&query, Some(q.id.clone()));
            self.run(app, cmd_tx);
        }
    }

    // --- Builder ---

    fn builder(&mut self, ui: &mut Ui, app: &App, cmd_tx: &CommandSender) {
        card_with_header(
            ui,
            "Query",
            self,
            |ui, tab| {
                ui.toggle_value(&mut tab.sidebar_open, "☰")
                    .on_hover_text("Saved queries");
            },
            |ui, tab| {
                let mut changed = false;
                // Entity.
                ui.horizontal(|ui| {
                    field_label(ui, "Rows");
                    for entity in Entity::ALL {
                        if ui
                            .selectable_label(tab.draft.entity == entity, entity.label())
                            .on_hover_text(if tab.draft.entity == entity {
                                "The rows the query is over".to_string()
                            } else {
                                format!(
                                    "Query {} instead: keeps the time range and limit, \
                                     clears the conditions",
                                    entity.label().to_lowercase()
                                )
                            })
                            .clicked()
                            && tab.draft.entity != entity
                        {
                            let mut keys = tab.next_key;
                            let mut draft = Draft::new(entity, &mut keys);
                            if entity.is_timed() == tab.draft.entity.is_timed() {
                                draft.range = tab.draft.range;
                                draft.custom_from = tab.draft.custom_from.clone();
                                draft.custom_to = tab.draft.custom_to.clone();
                            }
                            draft.limit = tab.draft.limit.clone();
                            tab.draft = draft;
                            tab.next_key = keys;
                            tab.loaded = None;
                            tab.text_dirty = false;
                            changed = true;
                        }
                    }
                });
                ui.add_space(4.0);
                // Conditions.
                let entity = tab.draft.entity;
                let mut next_key = tab.next_key;
                let mut unused = None;
                let features = tab.features;
                changed |= group_ui(
                    ui,
                    &mut tab.draft.root,
                    entity,
                    &features,
                    &mut next_key,
                    true,
                    &mut unused,
                );
                tab.next_key = next_key;
                ui.add_space(6.0);
                changed |= tab.range_ui(ui);
                changed |= tab.group_ui(ui);
                changed |= tab.order_ui(ui);
                ui.horizontal(|ui| {
                    field_label(ui, "Limit");
                    let response = ui.add(
                        TextEdit::singleline(&mut tab.draft.limit)
                            .id_salt("qlimit")
                            .hint_text(format!("{MAX_RESULT_ROWS} at most"))
                            .desired_width(80.0),
                    );
                    changed |= response.changed();
                    ui.add_space(12.0);
                    if !tab.draft.grouped {
                        changed |= tab.columns_menu(ui);
                    }
                });
                if changed {
                    // A text edit that parses was applied as it was typed, so the
                    // builder is on top of it; one that doesn't is kept with its error
                    // until it is fixed or reverted (Esc).
                    if !(tab.text_dirty && tab.text_error.is_some()) {
                        tab.text_dirty = false;
                        tab.sync_text();
                    }
                }
                ui.add_space(6.0);
                tab.text_line(ui);
                ui.add_space(4.0);
                tab.buttons(ui, app, cmd_tx);
            },
        );
    }

    fn range_ui(&mut self, ui: &mut Ui) -> bool {
        let mut changed = false;
        ui.horizontal(|ui| {
            field_label(ui, "Time");
            let current = match self.draft.range {
                Some(i) => RANGES[i].0,
                None => "Custom",
            };
            ComboBox::from_id_salt("query_range")
                .selected_text(current)
                .show_ui(ui, |ui| {
                    for (i, (label, _)) in RANGES.iter().enumerate() {
                        if !self.draft.entity.is_timed() && i != RANGES.len() - 1 {
                            continue;
                        }
                        if ui
                            .selectable_value(&mut self.draft.range, Some(i), *label)
                            .changed()
                        {
                            changed = true;
                        }
                    }
                    if self.draft.entity.is_timed()
                        && ui
                            .selectable_value(&mut self.draft.range, None, "Custom")
                            .changed()
                    {
                        changed = true;
                    }
                });
            if self.draft.range.is_none() {
                changed |= ui
                    .add(
                        TextEdit::singleline(&mut self.draft.custom_from)
                            .id_salt("qfrom")
                            .hint_text("from: 2026-10-01T12:00Z, or 36h ago")
                            .desired_width(230.0),
                    )
                    .changed();
                changed |= ui
                    .add(
                        TextEdit::singleline(&mut self.draft.custom_to)
                            .id_salt("qto")
                            .hint_text("to (optional)")
                            .desired_width(160.0),
                    )
                    .changed();
            }
            if !self.draft.entity.is_timed() {
                ui.label(
                    RichText::new(
                        "(the window the index covers; addresses have no time of their own)",
                    )
                    .weak(),
                );
            }
        });
        changed
    }

    fn group_ui(&mut self, ui: &mut Ui) -> bool {
        let mut changed = false;
        let entity = self.draft.entity;
        let features = self.features;
        ui.horizontal(|ui| {
            field_label(ui, "Group");
            if ui
                .checkbox(&mut self.draft.grouped, "Aggregate rows")
                .on_hover_text(
                    "Count or sum the matching rows by a field or a time bucket (resets \
                     the order)",
                )
                .changed()
            {
                changed = true;
                if self.draft.grouped && self.draft.keys.is_empty() {
                    self.next_key += 1;
                    self.draft.keys.push(KeyDraft {
                        key: self.next_key,
                        bucket: false,
                        field: first_field(entity),
                        width: "1h".to_string(),
                    });
                    self.next_key += 1;
                    self.draft.metrics.push(MetricDraft {
                        key: self.next_key,
                        kind: MetricKind::Count,
                        field: first_numeric(entity),
                    });
                }
                self.draft.order.clear();
            }
        });
        if !self.draft.grouped {
            return changed;
        }
        ui.indent("query_group", |ui| {
            let mut remove_key = None;
            for k in &mut self.draft.keys {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("by").weak());
                    let label = if k.bucket {
                        "time bucket"
                    } else {
                        k.field.label()
                    };
                    ComboBox::from_id_salt(("qkey", k.key))
                        .selected_text(label)
                        .show_ui(ui, |ui| {
                            if entity.is_timed()
                                && ui.selectable_label(k.bucket, "time bucket").clicked()
                            {
                                k.bucket = true;
                                changed = true;
                            }
                            for spec in fields::for_entity(entity).filter(|f| f.cost != Cost::Node)
                            {
                                if field_option(
                                    ui,
                                    !k.bucket && k.field == spec.id,
                                    spec,
                                    &features,
                                ) {
                                    k.bucket = false;
                                    k.field = spec.id;
                                    changed = true;
                                }
                            }
                        });
                    if k.bucket {
                        changed |= ui
                            .add(
                                TextEdit::singleline(&mut k.width)
                                    .id_salt(("qwidth", k.key))
                                    .hint_text("1h")
                                    .desired_width(60.0),
                            )
                            .changed();
                    }
                    if ui.small_button("×").on_hover_text("Remove").clicked() {
                        remove_key = Some(k.key);
                    }
                });
            }
            if let Some(key) = remove_key {
                self.draft.keys.retain(|k| k.key != key);
                changed = true;
            }
            let mut remove_metric = None;
            for m in &mut self.draft.metrics {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("metric").weak());
                    ComboBox::from_id_salt(("qmetric", m.key))
                        .selected_text(m.kind.label())
                        .show_ui(ui, |ui| {
                            for kind in MetricKind::ALL {
                                changed |= ui
                                    .selectable_value(&mut m.kind, kind, kind.label())
                                    .changed();
                            }
                        });
                    if m.kind.takes_field() {
                        let numeric_only = matches!(m.kind, MetricKind::Sum | MetricKind::Avg);
                        ComboBox::from_id_salt(("qmetric_field", m.key))
                            .selected_text(m.field.label())
                            .show_ui(ui, |ui| {
                                for spec in fields::for_entity(entity).filter(|f| {
                                    f.cost != Cost::Node
                                        && (!numeric_only
                                            || matches!(
                                                f.kind,
                                                FieldKind::Int
                                                    | FieldKind::Amount
                                                    | FieldKind::Float
                                            ))
                                }) {
                                    if field_option(ui, m.field == spec.id, spec, &features)
                                        && m.field != spec.id
                                    {
                                        m.field = spec.id;
                                        changed = true;
                                    }
                                }
                            });
                    }
                    if ui.small_button("×").on_hover_text("Remove").clicked() {
                        remove_metric = Some(m.key);
                    }
                });
            }
            if let Some(key) = remove_metric
                && let Some(i) = self.draft.metrics.iter().position(|m| m.key == key)
            {
                self.draft.metrics.remove(i);
                // The order keys point at metrics by position: drop the removed one's
                // and keep the rest pointing where they did.
                self.draft.order.retain(|o| o.target != OrderKey::Metric(i));
                for o in &mut self.draft.order {
                    if let OrderKey::Metric(j) = o.target
                        && j > i
                    {
                        o.target = OrderKey::Metric(j - 1);
                    }
                }
                changed = true;
            }
            ui.horizontal(|ui| {
                if ui.small_button("+ key").clicked() {
                    self.next_key += 1;
                    self.draft.keys.push(KeyDraft {
                        key: self.next_key,
                        bucket: false,
                        field: first_field(entity),
                        width: "1h".to_string(),
                    });
                    changed = true;
                }
                if ui.small_button("+ metric").clicked() {
                    self.next_key += 1;
                    self.draft.metrics.push(MetricDraft {
                        key: self.next_key,
                        kind: MetricKind::Count,
                        field: first_numeric(entity),
                    });
                    changed = true;
                }
            });
        });
        changed
    }

    fn order_ui(&mut self, ui: &mut Ui) -> bool {
        let mut changed = false;
        let entity = self.draft.entity;
        // What can be ordered by: fields, or with a grouping its keys and metrics.
        let mut options: Vec<(OrderKey, String)> = Vec::new();
        // Why an option can't be picked (a field whose index feature is off).
        let mut off: Vec<(OrderKey, String)> = Vec::new();
        if self.draft.grouped {
            for k in &self.draft.keys {
                if k.bucket {
                    options.push((OrderKey::Bucket, "time bucket".to_string()));
                } else {
                    options.push((OrderKey::Field(k.field), k.field.label().to_string()));
                }
            }
            for (i, m) in self.draft.metrics.iter().enumerate() {
                let name = if m.kind.takes_field() {
                    format!("{}({})", m.kind.label(), m.field.name())
                } else {
                    m.kind.label().to_string()
                };
                options.push((OrderKey::Metric(i), name));
            }
        } else {
            for spec in fields::for_entity(entity).filter(|f| f.cost != Cost::Node) {
                options.push((OrderKey::Field(spec.id), spec.label.to_string()));
                if let Some(why) = spec.id.unavailable(&self.features) {
                    off.push((OrderKey::Field(spec.id), why));
                }
            }
        }
        ui.horizontal(|ui| {
            field_label(ui, "Sort");
            let mut remove = None;
            for o in &mut self.draft.order {
                let current = options
                    .iter()
                    .find(|(k, _)| *k == o.target)
                    .map(|(_, l)| l.clone())
                    .unwrap_or_else(|| "?".to_string());
                ComboBox::from_id_salt(("qorder", o.key))
                    .selected_text(current)
                    .show_ui(ui, |ui| {
                        for (key, label) in &options {
                            let why = off.iter().find(|(k, _)| k == key).map(|(_, w)| w);
                            let response = ui
                                .add_enabled(
                                    why.is_none(),
                                    Button::selectable(o.target == *key, label.as_str()),
                                )
                                .on_disabled_hover_text(why.cloned().unwrap_or_default());
                            if response.clicked() && o.target != *key {
                                o.target = *key;
                                changed = true;
                            }
                        }
                    });
                if ui
                    .selectable_label(o.desc, if o.desc { "▼" } else { "▲" })
                    .on_hover_text(if o.desc { "Descending" } else { "Ascending" })
                    .clicked()
                {
                    o.desc = !o.desc;
                    changed = true;
                }
                if ui.small_button("×").on_hover_text("Remove").clicked() {
                    remove = Some(o.key);
                }
            }
            if let Some(key) = remove {
                self.draft.order.retain(|o| o.key != key);
                changed = true;
            }
            if self.draft.order.len() < MAX_ORDER
                && let Some((target, _)) = options.first()
                && ui.small_button("+ sort").clicked()
            {
                self.next_key += 1;
                self.draft.order.push(OrderDraft {
                    key: self.next_key,
                    target: *target,
                    desc: true,
                });
                changed = true;
            }
            if self.draft.order.is_empty() {
                ui.label(
                    RichText::new(if self.draft.grouped {
                        "(the first metric, largest first)"
                    } else if entity.is_timed() {
                        "(newest first)"
                    } else {
                        "(as found)"
                    })
                    .weak(),
                );
            }
        });
        changed
    }

    fn columns_menu(&mut self, ui: &mut Ui) -> bool {
        let features = self.features;
        let mut changed = false;
        let entity = self.draft.entity;
        let shown: Vec<FieldId> = self
            .draft
            .columns
            .clone()
            .unwrap_or_else(|| fields::default_columns(entity));
        let (response, _) =
            MenuButton::from_button(Button::new(format!("Columns ({}) ▾", shown.len())))
                .config(MenuConfig::new().close_behavior(PopupCloseBehavior::CloseOnClickOutside))
                .ui(ui, |ui| {
                    let mut columns = shown.clone();
                    // Taller than the window otherwise: the list scrolls, Defaults stays.
                    let max_height = ui.ctx().content_rect().height() * 0.6;
                    egui::ScrollArea::vertical()
                        .max_height(max_height)
                        .show(ui, |ui| {
                            for category in fields::categories(entity) {
                                section_title(ui, category);
                                for spec in
                                    fields::for_entity(entity).filter(|f| f.category == category)
                                {
                                    let mut on = columns.contains(&spec.id);
                                    let last = on && columns.len() == 1;
                                    // A column already shown can always be taken away.
                                    let off = spec.id.unavailable(&features).filter(|_| !on);
                                    if ui
                                        .add_enabled(
                                            !last && off.is_none(),
                                            egui::Checkbox::new(&mut on, spec.label),
                                        )
                                        .on_hover_text(spec.doc)
                                        .on_disabled_hover_text(
                                            off.unwrap_or_else(|| "At least one column".into()),
                                        )
                                        .changed()
                                    {
                                        if on {
                                            columns.push(spec.id);
                                        } else {
                                            columns.retain(|c| *c != spec.id);
                                        }
                                        changed = true;
                                    }
                                }
                            }
                        });
                    ui.separator();
                    if ui.button("Defaults").clicked() {
                        self.draft.columns = None;
                        changed = true;
                        ui.close();
                        return;
                    }
                    if changed && !columns.is_empty() {
                        self.draft.columns = Some(columns);
                    }
                });
        response.on_hover_text("Which fields the result shows");
        changed
    }

    fn text_line(&mut self, ui: &mut Ui) {
        let response = ui.add(
            TextEdit::singleline(&mut self.text)
                .id_salt("qtext")
                .hint_text("tx last 1d where fee > 1 KAS order by fee desc limit 100")
                .desired_width(f32::INFINITY),
        );
        if response.changed() {
            // Applied as typed when it parses, so the builder follows along; the text
            // itself is left as typed until Enter.
            self.text_dirty = true;
            self.text_error = None;
            self.apply_text(false);
        }
        if response.lost_focus() {
            if ui.input(|i| i.key_pressed(Key::Escape)) {
                self.revert_text();
            } else if self.text_dirty && self.text_error.is_none() {
                self.apply_text(true);
            }
        }
        if let Some(e) = &self.text_error {
            if let Some(pos) = e.pos {
                let column = self.text[..pos.min(self.text.len())].chars().count();
                let caret = format!("{}^", " ".repeat(column));
                ui.horizontal(|ui| {
                    // Under the field's text, which starts past its inner margin.
                    ui.add_space(ui.spacing().button_padding.x);
                    ui.label(RichText::new(caret).color(theme::ERROR));
                });
            }
            ui.label(RichText::new(&e.msg).color(theme::ERROR))
                .on_hover_text("Fix the text, or Esc to go back to the builder's query");
        } else if self.text_dirty {
            ui.label(RichText::new("Enter tidies the text; Esc goes back").weak());
        }
    }

    fn buttons(&mut self, ui: &mut Ui, app: &App, cmd_tx: &CommandSender) {
        ui.horizontal(|ui| {
            let ready = Self::ready(app).and_then(|()| match self.feature_block() {
                Some(why) => Err(why),
                None => Ok(()),
            });
            let running = app.query.is_running();
            if running {
                if ui
                    .button("Cancel")
                    .on_hover_text("Stop the run; the rows found so far are shown")
                    .clicked()
                {
                    let _ = cmd_tx.send(UiCommand::QueryCancel);
                }
            } else if ui
                .add_enabled(ready.is_ok(), primary_button("Run"))
                .on_hover_text(format!(
                    "Run the query ({}+Enter)",
                    ui.ctx().format_modifiers(Modifiers::COMMAND)
                ))
                .on_disabled_hover_text(ready.clone().err().unwrap_or_default())
                .clicked()
            {
                self.run(app, cmd_tx);
            }
            let loaded = self.loaded_query(app);
            // Only a valid query is worth a dialog: a broken one says so here instead.
            let mut open: Option<(SaveMode, String, String, Option<QueryWatch>)> = None;
            match &loaded {
                Some(saved) if !saved.is_preset() => {
                    if ui
                        .add_enabled(self.modified, Button::new("Save"))
                        .on_hover_text(format!("Replace \"{}\" with this query", saved.name))
                        .on_disabled_hover_text(format!("\"{}\" is already this query", saved.name))
                        .clicked()
                    {
                        open = Some((
                            SaveMode::Update(saved.id.clone()),
                            saved.name.clone(),
                            saved.description.clone(),
                            saved.watch.clone(),
                        ));
                    }
                    if ui
                        .button("Save as…")
                        .on_hover_text("Keep this query as a new saved one")
                        .clicked()
                    {
                        open = Some((
                            SaveMode::New,
                            format!("{} (copy)", saved.name),
                            saved.description.clone(),
                            None,
                        ));
                    }
                }
                Some(template) => {
                    if ui
                        .button("Save…")
                        .on_hover_text("Keep this template's query as a saved one")
                        .clicked()
                    {
                        open = Some((
                            SaveMode::New,
                            template.name.clone(),
                            template.description.clone(),
                            None,
                        ));
                    }
                }
                None => {
                    if ui
                        .button("Save…")
                        .on_hover_text("Keep this query in the sidebar")
                        .clicked()
                    {
                        open = Some((SaveMode::New, String::new(), String::new(), None));
                    }
                }
            }
            if let Some((mode, name, description, watch)) = open {
                match self.current_query() {
                    Ok(query) => {
                        self.save_dialog = Some(SaveDialog::new(
                            mode,
                            name,
                            description,
                            watch.as_ref(),
                            Some(&query),
                        ));
                    }
                    Err(e) => self.draft_error = Some(e),
                }
            }
            let blank = self
                .draft
                .build(false)
                .map(|q| q == Query::default_for(q.entity))
                .unwrap_or(false)
                && !self.text_dirty;
            if ui
                .add_enabled(!blank, Button::new("Reset"))
                .on_hover_text("Start over with an empty query (the saved one stays as it is)")
                .on_disabled_hover_text("Already empty")
                .clicked()
            {
                let entity = self.draft.entity;
                self.load(&Query::default_for(entity), None);
            }
            if let Some(e) = &self.draft_error {
                ui.label(RichText::new(e).color(theme::ERROR));
            }
        });
    }

    // --- Results ---

    fn results(&mut self, ui: &mut Ui, app: &App, cmd_tx: &CommandSender, height: f32) {
        // A new result is shown in its own order.
        if app.query.result_seq != self.sorted_result {
            self.sorted_result = app.query.result_seq;
            self.sort = None;
        }
        let name = app
            .query
            .result_name
            .clone()
            .unwrap_or_else(|| "query".to_string());
        card_with_header(
            ui,
            "Results",
            self,
            |ui, tab| {
                if let Some(result) = &app.query.result
                    && !result.rows.is_empty()
                {
                    let text = app
                        .query
                        .result_query
                        .as_ref()
                        .map(Query::to_text)
                        .unwrap_or_default();
                    let sorted = tab.sort.is_some();
                    let (response, _) = MenuButton::from_button(Button::new("Export ▾").small())
                        .ui(ui, |ui| {
                            for format in [ExportFormat::Csv, ExportFormat::Json] {
                                let ext = format.extension().to_ascii_uppercase();
                                if ui.button(format!("Rows as {ext}")).clicked() {
                                    let mut rows = result.clone();
                                    if let Some((col, desc)) = tab.sort {
                                        rows.rows = sort_rows(&rows.rows, col, desc)
                                            .into_iter()
                                            .map(|i| result.rows[i].clone())
                                            .collect();
                                    }
                                    let _ = cmd_tx.send(UiCommand::Export(ExportRequest::Query {
                                        name: name.clone(),
                                        text: text.clone(),
                                        result: Box::new(rows),
                                        format,
                                    }));
                                    ui.close();
                                }
                            }
                        });
                    response.on_hover_text(if sorted {
                        "The rows as shown, in the table's sort order, to ~/.x4kas/exports"
                    } else {
                        "The rows as shown, to ~/.x4kas/exports"
                    });
                }
                status_line(ui, app, &tab.text);
            },
            |ui, tab| {
                if !app.connection.is_direct() {
                    placeholder(
                        ui,
                        direct_node_placeholder(app, "Connect to a node to run queries"),
                    );
                    return;
                }
                if let Some(e) = &app.query.error {
                    ui.label(RichText::new(e).color(theme::ERROR));
                }
                let Some(result) = &app.query.result else {
                    if app.query.is_running() {
                        placeholder(ui, "Running…");
                    } else if app.query.error.is_none() {
                        placeholder(ui, "Run a query to see its rows here.");
                    }
                    return;
                };
                if let Some(status) = app.address.export.of(&ExportOrigin::Query(name.clone())) {
                    export_status(ui, status);
                }
                // The rows of an earlier run stay while a new one runs or after one
                // failed, dimmed so they aren't taken for its answer.
                let stale = app.query.is_running() || app.query.error.is_some();
                if stale {
                    placeholder(ui, "The previous result:");
                    ui.set_opacity(0.55);
                }
                if result.rows.is_empty() {
                    placeholder(
                        ui,
                        if app.chain.coverage.is_none() {
                            "No rows match: the index has nothing yet, the analyzer is still \
                             working through the DAG."
                        } else if matches!(
                            app.chain.phase,
                            ChainPhase::Seeking | ChainPhase::CatchingUp
                        ) {
                            "No rows match so far; the index is still catching up."
                        } else {
                            "No rows match."
                        },
                    );
                    return;
                }
                let bucketed = result
                    .time_column
                    .is_some_and(|c| result.columns[c].source == ColumnSource::Bucket);
                let mut table_height = height;
                if result.time_column.is_some() && !bucketed {
                    histogram(ui, result);
                    table_height -= HISTOGRAM_HEIGHT + 4.0;
                }
                tab.table(ui, result, table_height);
            },
        );
    }

    fn table(&mut self, ui: &mut Ui, result: &ResultSet, height: f32) {
        let n = result.rows.len();
        // The row order: the result's, or by the clicked column.
        let order: Vec<usize> = match self.sort {
            None => (0..n).collect(),
            Some((col, desc)) => sort_rows(&result.rows, col, desc),
        };
        let entity = result.entity;
        let primary = result.primary;
        let mut clicked_sort: Option<usize> = None;
        // Dragged column widths are remembered per shape of result, not across them.
        let shape: Vec<&str> = result.columns.iter().map(|c| c.name.as_str()).collect();
        ui.push_id(("query_results", shape), |ui| {
            let row_height = table_row_height(ui);
            let mut table = page_table(ui, height)
                .resizable(true)
                .column(Column::auto().at_least(28.0));
            let last = result.columns.len().saturating_sub(1);
            for (i, c) in result.columns.iter().enumerate() {
                let (min, max) = match c.kind {
                    FieldKind::Address => (230.0, f32::INFINITY),
                    FieldKind::AddressList => (230.0, MAX_TEXT_COLUMN),
                    FieldKind::Hash => (170.0, f32::INFINITY),
                    FieldKind::HashList => (170.0, MAX_TEXT_COLUMN),
                    FieldKind::Time => (150.0, f32::INFINITY),
                    FieldKind::Amount => (110.0, f32::INFINITY),
                    FieldKind::Text => (120.0, MAX_TEXT_COLUMN),
                    _ => (70.0, f32::INFINITY),
                };
                table = table.column(if i == last {
                    Column::remainder().at_least(min).clip(true)
                } else {
                    Column::auto().at_least(min).at_most(max).clip(true)
                });
            }
            let sort = self.sort;
            let truncated = result.truncated;
            table
                .header(theme::ROW_HEIGHT + 4.0, |mut header| {
                    header.col(|ui| {
                        ui.label(RichText::new("#").color(theme::LABEL));
                        header_rule(ui);
                    });
                    for (i, c) in result.columns.iter().enumerate() {
                        header.col(|ui| {
                            let title = match sort {
                                Some((col, desc)) if col == i => {
                                    format!("{} {}", c.label, if desc { "▼" } else { "▲" })
                                }
                                _ => c.label.clone(),
                            };
                            let hover = match (c.source, truncated) {
                                (ColumnSource::Field(FieldId::AddrBalance), _) => {
                                    "From the node, for the first 200 rows. Sorts the rows shown."
                                        .to_string()
                                }
                                (_, true) => "Sorts the rows shown; add \"order by\" to the \
                                              query to sort everything that matched"
                                    .to_string(),
                                _ => "Sort by this column".to_string(),
                            };
                            if ui
                                .selectable_label(false, RichText::new(title).color(theme::ACCENT))
                                .on_hover_text(hover)
                                .clicked()
                            {
                                clicked_sort = Some(i);
                            }
                            header_rule(ui);
                        });
                    }
                })
                .body(|body| {
                    body.rows(row_height, n, |mut row| {
                        let shown = row.index();
                        let cells = &result.rows[order[shown]];
                        row.col(|ui| {
                            ui.label(RichText::new((shown + 1).to_string()).weak());
                        });
                        for cell in cells.iter() {
                            row.col(|ui| cell_ui(ui, cell));
                        }
                        let response = row.response();
                        let page = primary.and_then(|p| page_of(entity, &cells[p]));
                        egui::Popup::context_menu(&response).show(|ui| {
                            if let Some(page) = &page {
                                if ui.button("Show in info pane").clicked() {
                                    request_pane(ui.ctx(), page.clone());
                                    ui.close();
                                }
                                if ui.button("Open in Explorer").clicked() {
                                    request_explorer(ui.ctx(), page.clone(), true);
                                    ui.close();
                                }
                                ui.separator();
                            }
                            if ui
                                .button("Copy row")
                                .on_hover_text("The row's values, tab-separated")
                                .clicked()
                            {
                                let line: Vec<String> = cells.iter().map(Cell::text).collect();
                                ui.ctx().copy_text(line.join("\t"));
                                ui.close();
                            }
                        });
                    });
                });
        });
        if let Some(col) = clicked_sort {
            self.sort = match self.sort {
                Some((c, true)) if c == col => Some((c, false)),
                Some((c, false)) if c == col => None,
                _ => Some((col, true)),
            };
        }
    }

    // --- Save dialog ---

    /// `close`: this frame's Esc is for the dialog (the frame loop's decision, so a
    /// focused field or an open menu takes it first).
    fn save_dialog_ui(
        &mut self,
        ctx: &egui::Context,
        app: &App,
        cmd_tx: &CommandSender,
        close: bool,
    ) {
        let Some(dialog) = &mut self.save_dialog else {
            return;
        };
        let title = match dialog.mode {
            SaveMode::New => "Save query",
            SaveMode::Update(_) => "Save query",
            SaveMode::Rename(_) => "Rename query",
        };
        // Another saved query of this name: the CLI runs saved queries by name.
        let own_id = match &dialog.mode {
            SaveMode::Update(id) | SaveMode::Rename(id) => Some(id.as_str()),
            SaveMode::New => None,
        };
        let taken = app
            .query
            .saved
            .by_name(&dialog.name)
            .is_some_and(|other| Some(other.id.as_str()) != own_id);
        let window = egui::Window::new(title)
            .id(egui::Id::new("query_save_dialog"))
            .resizable(false)
            .default_width(460.0);
        let mut done = false;
        let mut save = false;
        let open = modal_window(ctx, window, close, |ui| {
            let mut enter = false;
            kv_grid(ui, "query_save", |ui| {
                field_label(ui, "Name");
                let output = TextEdit::singleline(&mut dialog.name)
                    .hint_text("a short name")
                    .desired_width(320.0)
                    .show(ui);
                let field = output.response;
                if std::mem::take(&mut dialog.focus) {
                    // Focused with the prefilled name selected, so typing replaces it.
                    field.request_focus();
                    let mut state = output.state;
                    state
                        .cursor
                        .set_char_range(Some(egui::text::CCursorRange::two(
                            egui::text::CCursor::new(0),
                            egui::text::CCursor::new(dialog.name.chars().count()),
                        )));
                    state.store(ui.ctx(), field.id);
                }
                enter |= field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                ui.end_row();
                field_label(ui, "Description");
                let field = ui.add(
                    TextEdit::singleline(&mut dialog.description)
                        .hint_text("what it finds (optional)")
                        .desired_width(320.0),
                );
                enter |= field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                ui.end_row();
                if taken {
                    ui.label("");
                    ui.label(
                        RichText::new("Another saved query has this name").color(theme::ERROR),
                    );
                    ui.end_row();
                }
                field_label(ui, "Watch");
                ui.vertical(|ui| {
                    let hover = match dialog.unwatchable {
                        Some(why) => why.to_string(),
                        None if dialog.addresses => {
                            "Re-run every so often; the first run only learns which \
                             addresses match, later ones raise the new ones"
                                .to_string()
                        }
                        None => "Re-run every so often over the time since the last run; \
                                 every new row raises an alert"
                            .to_string(),
                    };
                    ui.add_enabled(
                        dialog.unwatchable.is_none(),
                        egui::Checkbox::new(&mut dialog.watch, "Re-run as the index moves"),
                    )
                    .on_hover_text(hover.clone())
                    .on_disabled_hover_text(hover);
                    ui.label(
                        RichText::new(
                            "Runs while connected to a direct node; new rows show as toasts \
                             and in the sidebar's Alerts",
                        )
                        .weak()
                        .small(),
                    );
                });
                ui.end_row();
                if dialog.watch && dialog.unwatchable.is_none() {
                    field_label(ui, "Every");
                    ui.horizontal(|ui| {
                        let valid = dialog.every().is_some();
                        let mut field = TextEdit::singleline(&mut dialog.every_secs)
                            .id_salt("qevery")
                            .desired_width(60.0);
                        if !valid {
                            field = field.text_color(theme::ERROR);
                        }
                        let field = ui.add(field).on_hover_text(format!(
                            "A number of seconds, {} at least",
                            QueryWatch::MIN_EVERY_SECS
                        ));
                        enter |= field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                        ui.label(
                            RichText::new(format!(
                                "seconds ({} at least)",
                                QueryWatch::MIN_EVERY_SECS
                            ))
                            .weak(),
                        );
                        ui.checkbox(&mut dialog.notify, "Toast for each new row")
                            .on_hover_text("Off, the rows only land in the sidebar's Alerts");
                    });
                    ui.end_row();
                }
            });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let valid = dialog.valid() && !taken;
                if ui
                    .add_enabled(valid, primary_button("Save"))
                    .on_disabled_hover_text(if taken {
                        "Pick another name"
                    } else if dialog.name.trim().is_empty() {
                        "A name is needed"
                    } else {
                        "The interval isn't a number of seconds"
                    })
                    .clicked()
                    || (enter && valid)
                {
                    save = true;
                }
                if ui.button("Cancel").clicked() {
                    done = true;
                }
            });
        });
        if save {
            let dialog = self.save_dialog.take().expect("open");
            let mut list = app.query.saved.clone();
            match dialog.mode {
                SaveMode::New | SaveMode::Update(_) => match self.current_query() {
                    Ok(query) => {
                        let id = match &dialog.mode {
                            SaveMode::Update(id) => Some(id.clone()),
                            _ => None,
                        };
                        let mut entry = match id.as_ref().and_then(|id| list.get(id)) {
                            Some(existing) => existing.clone(),
                            None => SavedQuery::new(&dialog.name, &dialog.description, &query),
                        };
                        entry.name = dialog.name.trim().to_string();
                        entry.description = dialog.description.trim().to_string();
                        entry.watch = dialog.watch_setting();
                        entry.set_query(&query);
                        self.loaded = Some(entry.id.clone());
                        list.upsert(entry);
                        let _ = cmd_tx.send(UiCommand::QueriesSet(list));
                    }
                    Err(e) => self.draft_error = Some(e),
                },
                SaveMode::Rename(ref id) => {
                    if let Some(entry) = list.get_mut(id) {
                        entry.name = dialog.name.trim().to_string();
                        entry.description = dialog.description.trim().to_string();
                        entry.watch = dialog.watch_setting();
                        entry.updated_ms = now_ms();
                    }
                    let _ = cmd_tx.send(UiCommand::QueriesSet(list));
                }
            }
            return;
        }
        if done || !open {
            self.save_dialog = None;
        }
    }
}

/// The rows the watched queries found, newest first, with their read marks.
fn alerts_section(ui: &mut Ui, app: &mut App, folded: &mut bool) {
    if app.query.events.is_empty() {
        return;
    }
    ui.add_space(4.0);
    let unread = app.query.unread();
    let mut clear = false;
    ui.horizontal(|ui| {
        let arrow = if *folded { "▸" } else { "▾" };
        if ui
            .selectable_label(
                false,
                RichText::new(format!("{arrow} Alerts")).color(theme::LABEL),
            )
            .on_hover_text(if *folded {
                "Show the alerts"
            } else {
                "Fold the alerts away"
            })
            .clicked()
        {
            *folded = !*folded;
        }
        if unread > 0 {
            ui.label(RichText::new(format!("{unread} unread")).color(theme::WARN));
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .small_button("Clear")
                .on_hover_text("Forget these alerts")
                .clicked()
            {
                clear = true;
            }
            if unread > 0 && ui.small_button("Mark all read").clicked() {
                app.query.mark_all_read();
            }
        });
    });
    if clear {
        app.query.clear_events();
        return;
    }
    if *folded {
        return;
    }
    let mut toggled: Option<(usize, bool)> = None;
    for (i, event) in app.query.events.iter().take(ALERTS_SHOWN).enumerate() {
        alert_row(ui, i, event, app, &mut toggled);
    }
    if app.query.events.len() > ALERTS_SHOWN {
        ui.label(
            RichText::new(format!(
                "and {} more (Mark all read or Clear to see fewer)",
                app.query.events.len() - ALERTS_SHOWN
            ))
            .weak(),
        );
    }
    if let Some((i, read)) = toggled {
        app.query.set_read(i, read);
    }
}

/// One alert: its read dot (a click toggles it), the query (a click runs it) and the
/// row's id as a link.
fn alert_row(
    ui: &mut Ui,
    i: usize,
    event: &QueryEvent,
    app: &App,
    toggled: &mut Option<(usize, bool)>,
) {
    ui.horizontal(|ui| {
        let hover = if event.read {
            "Read; click to mark unread".to_string()
        } else {
            "Unread; click to mark read".to_string()
        };
        if alert_dot(ui, Some(&(event.read, hover))) {
            *toggled = Some((i, !event.read));
        }
        let saved = app.query.saved.get(&event.query_id);
        let query = saved.and_then(|q| q.query().ok());
        match query {
            Some(query) => {
                if ui
                    .link(RichText::new(&event.query_name).color(theme::TEXT_BRIGHT))
                    .on_hover_text(format!("Run \"{}\"\n{}", event.query_name, event.summary()))
                    .clicked()
                {
                    super::widgets::request_saved_query(ui.ctx(), query, &event.query_id);
                }
            }
            None => {
                ui.label(RichText::new(&event.query_name).color(theme::TEXT_BRIGHT))
                    .on_hover_text(event.summary());
            }
        }
        ui.label(RichText::new(format!("{} ago", ago(event.time_ms))).weak());
    });
    ui.indent(("alert", i), |ui| primary_link(ui, event));
}

/// How long ago `time_ms` was, as text.
fn ago(time_ms: u64) -> String {
    format_duration(std::time::Duration::from_millis(
        now_ms().saturating_sub(time_ms),
    ))
}

/// The rule under a table header cell, as `table_header` draws it.
fn header_rule(ui: &mut Ui) {
    let rect = ui.max_rect();
    let gap = ui.spacing().item_spacing.x / 2.0;
    ui.painter().hline(
        (rect.left() - gap)..=(rect.right() + gap),
        rect.bottom() - 0.5,
        egui::Stroke::new(1.0_f32, theme::BORDER_HI),
    );
}

/// The row indices of `rows` ordered by column `col`.
fn sort_rows(rows: &[Vec<Cell>], col: usize, desc: bool) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..rows.len()).collect();
    idx.sort_by(|a, b| {
        let o = rows[*a][col].compare(&rows[*b][col]);
        if desc { o.reverse() } else { o }
    });
    idx
}

/// The run's progress or the result's summary, for the Results header. `text` is
/// the builder's current query, to say when the result answers another.
fn status_line(ui: &mut Ui, app: &App, text: &str) {
    if let Some(run) = &app.query.run {
        ui.spinner();
        let what = if run.fetching_balances {
            "fetching balances from the node".to_string()
        } else {
            format!(
                "scanned {} · matched {}",
                format_number(run.scanned),
                format_number(run.matched)
            )
        };
        ui.label(
            RichText::new(format!(
                "{what} · {:.1}s",
                run.started.elapsed().as_secs_f64()
            ))
            .weak(),
        );
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(250));
        return;
    }
    if let Some(c) = &app.query.cancelled {
        ui.label(
            RichText::new(format!(
                "Cancelled after {} scanned ({} matched, {:.1}s); the rows found are on \
                 their way",
                format_number(c.scanned),
                format_number(c.matched),
                c.elapsed.as_secs_f64()
            ))
            .color(theme::WARN),
        );
        return;
    }
    let Some(result) = &app.query.result else {
        return;
    };
    let mut summary = format!(
        "{} row{} ({} matched, {} scanned in {:.1}s)",
        format_number(result.rows.len() as u64),
        if result.rows.len() == 1 { "" } else { "s" },
        format_number(result.matched),
        format_number(result.scanned),
        result.elapsed.as_secs_f64()
    );
    if result.truncated {
        summary.push_str(", more matched than shown");
    }
    ui.label(RichText::new(summary).weak())
        .on_hover_text(&result.plan);
    if result.truncated {
        let limit = app
            .query
            .result_query
            .as_ref()
            .and_then(|q| q.limit)
            .unwrap_or(MAX_RESULT_ROWS)
            .min(MAX_RESULT_ROWS);
        ui.label(RichText::new(format!("· limit {limit}")).weak())
            .on_hover_text(format!(
                "Only {} rows were kept: raise the limit (up to {}), or add \"order by\" so \
                 the ones kept are the ones that matter",
                format_number(limit as u64),
                format_number(MAX_RESULT_ROWS as u64)
            ));
    }
    if let Some(partial) = result.partial {
        ui.label(RichText::new(format!("⚠ {}", partial.label())).color(theme::WARN))
            .on_hover_text(partial.advice());
    }
    // An opt-in index feature turned on after the window starts.
    if !result.notes.is_empty() {
        ui.label(RichText::new("⚠ older rows incomplete").color(theme::WARN))
            .on_hover_text(result.notes.join("\n\n"));
    }
    // The index may not reach as far back as the query asked.
    if let Some((from, _)) = app.chain.coverage
        && result.window.0 < from
        && result.entity.is_timed()
    {
        ui.label(RichText::new(format!("index from {}", format_utc(from))).color(theme::WARN))
            .on_hover_text(format!(
                "The index only covers the time since {} ({} ago), so earlier rows can't \
                 be found{}",
                format_utc(from),
                ago(from),
                if app.chain.phase == ChainPhase::CatchingUp {
                    "; it is still catching up"
                } else {
                    ""
                }
            ));
    }
    if let Some(q) = &app.query.result_query
        && q.to_text() != text
    {
        ui.label(RichText::new("(an earlier query; Run for this one)").weak())
            .on_hover_text(q.to_text());
    }
}

/// Matches over the window, in `HISTOGRAM_BUCKETS` bars.
fn histogram(ui: &mut Ui, result: &ResultSet) {
    let Some(col) = result.time_column else {
        return;
    };
    let times: Vec<u64> = result
        .rows
        .iter()
        .filter_map(|r| match r[col] {
            Cell::Time(t) => Some(t),
            _ => None,
        })
        .collect();
    let (Some(&min), Some(&max)) = (times.iter().min(), times.iter().max()) else {
        return;
    };
    let from = if result.window.0 == 0 {
        min
    } else {
        result.window.0.min(min)
    };
    let to = if result.window.1 == u64::MAX {
        now_ms().max(max + 1)
    } else {
        result.window.1.max(max + 1)
    };
    let span = (to - from).max(1);
    let mut counts = vec![0u64; HISTOGRAM_BUCKETS];
    for t in &times {
        let i = ((t - from) as u128 * HISTOGRAM_BUCKETS as u128 / span as u128) as usize;
        counts[i.min(HISTOGRAM_BUCKETS - 1)] += 1;
    }
    let peak = counts.iter().copied().max().unwrap_or(1).max(1);
    let size = egui::vec2(ui.available_width(), HISTOGRAM_HEIGHT);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::hover());
    let painter = ui.painter_at(rect);
    let baseline = rect.bottom() - 1.0;
    painter.hline(
        rect.x_range(),
        baseline + 0.5,
        egui::Stroke::new(1.0_f32, theme::BORDER_HI),
    );
    let slot = rect.width() / HISTOGRAM_BUCKETS as f32;
    let gap = if slot >= 3.0 { 1.0 } else { 0.0 };
    let hovered = response
        .hover_pos()
        .map(|p| (((p.x - rect.left()) / slot) as usize).min(HISTOGRAM_BUCKETS - 1));
    for (i, count) in counts.iter().enumerate() {
        let left = rect.left() + i as f32 * slot;
        let column = egui::Rect::from_x_y_ranges(left..=left + slot - gap, rect.top()..=baseline);
        if hovered == Some(i) {
            painter.rect_filled(column, 0.0, theme::ROW_HOVER);
        }
        if *count == 0 {
            continue;
        }
        let height = (*count as f32 / peak as f32 * column.height()).max(1.0);
        let color = if hovered == Some(i) {
            theme::ACCENT_BRIGHT
        } else {
            theme::ACCENT
        };
        let bar = egui::Rect::from_x_y_ranges(column.x_range(), baseline - height..=baseline);
        painter.rect_filled(bar, 0.0, color);
    }
    if let Some(i) = hovered {
        let start = from + span * i as u64 / HISTOGRAM_BUCKETS as u64;
        let end = from + span * (i as u64 + 1) / HISTOGRAM_BUCKETS as u64;
        response.on_hover_ui_at_pointer(|ui| {
            ui.label(format!(
                "{} row{}, {} to {}",
                format_number(counts[i]),
                if counts[i] == 1 { "" } else { "s" },
                format_utc(start),
                format_utc(end)
            ));
        });
    }
    ui.add_space(2.0);
}

/// The page a row's primary cell opens.
fn page_of(entity: Entity, cell: &Cell) -> Option<ExplorerPage> {
    Some(match (entity, cell) {
        (Entity::Transactions, Cell::Txid(h)) => ExplorerPage::transaction(&hex(h)),
        (Entity::Blocks | Entity::Payouts, Cell::Hash(h)) => ExplorerPage::Block(hex(h)),
        (Entity::Addresses, Cell::Address(a)) => ExplorerPage::Address(a.clone()),
        _ => return None,
    })
}

/// One cell: links for ids and addresses, amounts in KAS, times as UTC.
fn cell_ui(ui: &mut Ui, cell: &Cell) {
    match cell {
        Cell::Null => {
            ui.label(RichText::new("—").weak());
        }
        Cell::Bool(b) => {
            ui.label(if *b { "yes" } else { "no" });
        }
        Cell::Int(n) => {
            ui.label(if *n >= 0 {
                format_number(*n as u64)
            } else {
                format!("-{}", format_number(n.unsigned_abs()))
            });
        }
        Cell::Amount(sompi) => {
            ui.label(format_kas(*sompi as f64, 2))
                .on_hover_text(format!(
                    "{} KAS ({sompi} sompi)",
                    format_sompi_exact(*sompi)
                ));
        }
        Cell::Float(f) => {
            let text = if f.fract() == 0.0 && f.abs() < 1e15 {
                format!("{f:.0}")
            } else {
                let s = format!("{f:.4}");
                s.trim_end_matches('0').trim_end_matches('.').to_string()
            };
            ui.label(text).on_hover_text(f.to_string());
        }
        Cell::Time(ms) => {
            ui.label(format_utc(*ms)).on_hover_text(format!(
                "{} ago",
                format_duration(std::time::Duration::from_millis(
                    now_ms().saturating_sub(*ms)
                ))
            ));
        }
        Cell::Hash(h) => {
            block_hash(ui, &hex(h), false);
        }
        Cell::Txid(h) => transaction_id(ui, &hex(h)),
        Cell::Address(a) => address(ui, a),
        Cell::Text(t) => copy_value(ui, t, "Copy"),
        Cell::Enum(e) => {
            ui.label(*e);
        }
        Cell::List(items) => {
            if items.is_empty() {
                ui.label(RichText::new("—").weak());
                return;
            }
            for item in items.iter().take(LIST_PREVIEW) {
                cell_ui(ui, item);
            }
            if items.len() > LIST_PREVIEW {
                let rest: Vec<String> = items.iter().skip(LIST_PREVIEW).map(Cell::text).collect();
                ui.label(RichText::new(format!("+{}", items.len() - LIST_PREVIEW)).weak())
                    .on_hover_text(rest.join("\n"));
            }
        }
    }
}

/// How a group combines its conditions, with its negation folded in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum GroupMode {
    All,
    Any,
    None,
    NotAll,
}

impl GroupMode {
    const ALL: [Self; 4] = [Self::All, Self::Any, Self::None, Self::NotAll];

    fn of(any: bool, not: bool) -> Self {
        match (any, not) {
            (false, false) => Self::All,
            (true, false) => Self::Any,
            (true, true) => Self::None,
            (false, true) => Self::NotAll,
        }
    }

    /// `(any, not)`.
    fn flags(self) -> (bool, bool) {
        match self {
            Self::All => (false, false),
            Self::Any => (true, false),
            Self::None => (true, true),
            Self::NotAll => (false, true),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::All => "all of",
            Self::Any => "any of",
            Self::None => "none of",
            Self::NotAll => "not all of",
        }
    }

    fn doc(self) -> &'static str {
        match self {
            Self::All => "Every condition must hold (and)",
            Self::Any => "At least one condition must hold (or)",
            Self::None => "No condition may hold (not … or …)",
            Self::NotAll => "At least one condition must fail (not … and …)",
        }
    }
}

/// The operators the row's combo offers for `kind`, as `(op, negated)`: every
/// operator of the kind, plus the negation of those that have no opposite of their
/// own (`!=` is `=`'s, `not in` is `in`'s, `is not null` is `is null`'s).
fn op_entries(kind: FieldKind) -> Vec<(Op, bool)> {
    let mut entries: Vec<(Op, bool)> = kind.operators().iter().map(|op| (*op, false)).collect();
    for op in kind.operators() {
        if matches!(op, Op::Contains | Op::StartsWith | Op::Between) {
            entries.push((*op, true));
        }
    }
    entries
}

/// The label of `op`, negated or not, as the row's combo shows it.
fn op_label(op: Op, not: bool) -> String {
    if !not {
        return op.label().to_string();
    }
    match op {
        Op::Eq => Op::Ne.label().to_string(),
        Op::Ne => Op::Eq.label().to_string(),
        Op::In => Op::NotIn.label().to_string(),
        Op::NotIn => Op::In.label().to_string(),
        Op::IsNull => Op::IsNotNull.label().to_string(),
        Op::IsNotNull => Op::IsNull.label().to_string(),
        Op::Contains => "doesn't contain".to_string(),
        Op::StartsWith => "doesn't start with".to_string(),
        Op::Between => "not between".to_string(),
        other => format!("not {}", other.label()),
    }
}

/// The builder's filter tree. Returns whether anything changed. `remove` is set to
/// the group's key when its × is clicked (a nested group only).
fn group_ui(
    ui: &mut Ui,
    node: &mut Node,
    entity: Entity,
    features: &IndexSettings,
    next_key: &mut u64,
    root: bool,
    remove: &mut Option<u64>,
) -> bool {
    let Node::Group {
        key,
        any,
        not,
        children,
        ..
    } = node
    else {
        return false;
    };
    let key = *key;
    let mut changed = false;
    let mut remove_child = None;
    let mut add_cond = false;
    let mut add_group = false;
    ui.horizontal(|ui| {
        if root {
            field_label(ui, "Where");
        }
        // How the group's conditions combine, negation included: "none of" is
        // `not (a or b)`, "not all of" is `not (a and b)`.
        let mut mode = GroupMode::of(*any, *not);
        ComboBox::from_id_salt(("qgroup", key))
            .selected_text(mode.label())
            .show_ui(ui, |ui| {
                for candidate in GroupMode::ALL {
                    changed |= ui
                        .selectable_value(&mut mode, candidate, candidate.label())
                        .on_hover_text(candidate.doc())
                        .changed();
                }
            });
        (*any, *not) = mode.flags();
        if children.is_empty() {
            ui.label(RichText::new("(every row)").weak());
        }
        if !root
            && ui
                .small_button("×")
                .on_hover_text("Remove the group")
                .clicked()
        {
            *remove = Some(key);
        }
    });
    ui.indent(("qgroup_body", key), |ui| {
        for child in children.iter_mut() {
            match child {
                Node::Group { .. } => {
                    changed |= group_ui(
                        ui,
                        child,
                        entity,
                        features,
                        next_key,
                        false,
                        &mut remove_child,
                    );
                }
                Node::Cond { .. } => {
                    changed |= condition_ui(ui, child, entity, features, &mut remove_child);
                }
            }
        }
        ui.horizontal(|ui| {
            if ui
                .small_button("+ condition")
                .on_hover_text("Add a condition to this group")
                .clicked()
            {
                add_cond = true;
            }
            if ui
                .small_button("+ group")
                .on_hover_text("Add a nested group (any of / all of / none of)")
                .clicked()
            {
                add_group = true;
            }
        });
    });
    if let Some(k) = remove_child {
        children.retain(|c| c.key() != k);
        changed = true;
    }
    if add_cond {
        *next_key += 1;
        let field = first_field(entity);
        children.push(Node::Cond {
            key: *next_key,
            not: false,
            field,
            op: field.kind().operators()[0],
            value: String::new(),
            value2: String::new(),
            kas: true,
            error: None,
            search_open: false,
        });
        changed = true;
    }
    if add_group {
        *next_key += 1;
        children.push(Node::Group {
            key: *next_key,
            any: true,
            not: false,
            children: Vec::new(),
        });
        changed = true;
    }
    changed
}

/// One condition row: `[not] field operator value ×`.
fn condition_ui(
    ui: &mut Ui,
    node: &mut Node,
    entity: Entity,
    features: &IndexSettings,
    remove: &mut Option<u64>,
) -> bool {
    let Node::Cond {
        key,
        not,
        field,
        op,
        value,
        value2,
        kas,
        error,
        search_open,
    } = node
    else {
        return false;
    };
    let key = *key;
    let mut changed = false;
    ui.horizontal(|ui| {
        let before = *field;
        ComboBox::from_id_salt(("qfield", key))
            .selected_text(field.label())
            .width(150.0)
            .show_ui(ui, |ui| {
                for category in fields::categories(entity) {
                    section_title(ui, category);
                    for spec in fields::for_entity(entity)
                        .filter(|f| f.category == category && f.cost != Cost::Node)
                    {
                        if field_option(ui, *field == spec.id, spec, features) {
                            *field = spec.id;
                        }
                    }
                }
            });
        if *field != before {
            changed = true;
            if before.kind() != field.kind() {
                value.clear();
                value2.clear();
            }
            if !field.kind().allows(*op) {
                *op = field.kind().operators()[0];
            }
        }
        let kind = field.kind();
        // The operator, negation included: "doesn't contain" is `not (x contains v)`.
        let mut choice = (*op, *not);
        ComboBox::from_id_salt(("qop", key))
            .selected_text(op_label(*op, *not))
            .show_ui(ui, |ui| {
                for candidate in op_entries(kind) {
                    changed |= ui
                        .selectable_value(
                            &mut choice,
                            candidate,
                            op_label(candidate.0, candidate.1),
                        )
                        .changed();
                }
            });
        (*op, *not) = choice;
        let arity = op.arity();
        if arity != Arity::None {
            changed |= value_ui(ui, key, kind, arity, value, kas, search_open, 0);
            if arity == Arity::Two {
                ui.label(RichText::new("and").weak());
                changed |= value_ui(ui, key, kind, Arity::One, value2, kas, search_open, 1);
            }
            if kind == FieldKind::Amount {
                let unit = if *kas { "KAS" } else { "sompi" };
                if ui
                    .small_button(unit)
                    .on_hover_text("Switch the unit")
                    .clicked()
                {
                    *kas = !*kas;
                    changed = true;
                }
            }
        }
        if let Some(e) = error {
            ui.label(RichText::new("!").color(theme::ERROR))
                .on_hover_text(e.as_str());
        }
        if ui
            .small_button("×")
            .on_hover_text("Remove the condition")
            .clicked()
        {
            *remove = Some(key);
        }
    });
    changed
}

/// A value field typed for `kind`.
#[allow(clippy::too_many_arguments)]
fn value_ui(
    ui: &mut Ui,
    key: u64,
    kind: FieldKind,
    arity: Arity,
    value: &mut String,
    kas: &bool,
    search_open: &mut bool,
    slot: u8,
) -> bool {
    let mut changed = false;
    match (kind, arity) {
        (FieldKind::Bool, _) => {
            if value.is_empty() {
                *value = "true".to_string();
                changed = true;
            }
            ComboBox::from_id_salt(("qval", key, slot))
                .selected_text(value.as_str())
                .show_ui(ui, |ui| {
                    for option in ["true", "false"] {
                        changed |= ui
                            .selectable_value(value, option.to_string(), option)
                            .changed();
                    }
                });
        }
        (FieldKind::Enum(options), Arity::One) => {
            if value.is_empty()
                && let Some(first) = options.first()
            {
                *value = (*first).to_string();
                changed = true;
            }
            ComboBox::from_id_salt(("qval", key, slot))
                .selected_text(value.as_str())
                .show_ui(ui, |ui| {
                    for option in options {
                        changed |= ui
                            .selectable_value(value, (*option).to_string(), *option)
                            .changed();
                    }
                });
        }
        _ => {
            let hint = match (kind, arity) {
                (_, Arity::List) => "one, two, three (comma-separated)".to_string(),
                (FieldKind::Amount, _) => if *kas { "1.5" } else { "150000000" }.to_string(),
                (FieldKind::Time, _) => "2026-10-01T12:00Z or 24h ago".to_string(),
                (FieldKind::Hash | FieldKind::HashList, _) => "64 hex characters".to_string(),
                (FieldKind::Address | FieldKind::AddressList, _) => {
                    "kaspa:… or a label".to_string()
                }
                (FieldKind::Enum(options), _) => options.join(", "),
                (FieldKind::Float, _) => "number".to_string(),
                (FieldKind::Int, _) => "whole number".to_string(),
                (FieldKind::Text, _) => "text".to_string(),
                (FieldKind::Bool, _) => String::new(),
            };
            let width = match kind {
                FieldKind::Address | FieldKind::AddressList => 320.0,
                FieldKind::Hash | FieldKind::HashList => 300.0,
                FieldKind::Time | FieldKind::Text => 200.0,
                _ => 120.0,
            };
            let response = ui.add(
                TextEdit::singleline(value)
                    .id_salt(("qval_edit", key, slot))
                    .hint_text(hint)
                    .desired_width(width),
            );
            changed |= response.changed();
            if matches!(kind, FieldKind::Address | FieldKind::AddressList) && arity != Arity::List {
                if response.changed() || response.gained_focus() {
                    *search_open = true;
                }
                let book = super::widgets::labels(ui.ctx());
                if let Some(book) = book
                    && !value.trim().starts_with("kaspa")
                    && let Some(picked) =
                        label_search_popup(ui, &response, search_open, value, &book)
                {
                    *value = picked;
                    changed = true;
                }
            }
        }
    }
    changed
}

/// Where a condition's text came from, for tests of the draft round trip.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draft_round_trips_a_query() {
        let mut keys = 0;
        for text in [
            "tx last 6h where fee_rate > 10.0 and protocol in (krc, kns) and not is_coinbase order by fee desc limit 200 select time, txid, fee, sender",
            "payouts last 1d count, sum(amount) by miner order by count desc limit 50",
            "tx last 1d count by time(1h), protocol order by bucket",
            "addresses where label contains \"binance\" and received between 1 KAS and 2.5 KAS order by received desc",
            "blocks between 2026-10-01T00:00:00Z and 2026-10-02T00:00:00Z where is_chain or not (miner_tag starts_with \"2\" and blue_score > 5)",
            "tx all time where time > 1d and address = kaspa:qq1 and fee is null",
        ] {
            let q = text::parse_valid(text).unwrap();
            let mut draft = Draft::from_query(&q, &mut keys);
            let back = draft.build(true).unwrap_or_else(|e| panic!("{text}: {e}"));
            assert_eq!(back, q, "{text}");
        }
    }

    #[test]
    fn draft_reports_bad_values_on_the_row() {
        let mut keys = 0;
        let mut draft = Draft::new(Entity::Transactions, &mut keys);
        let Node::Group { children, .. } = &mut draft.root else {
            panic!()
        };
        children.push(Node::Cond {
            key: 99,
            not: false,
            field: FieldId::TxFee,
            op: Op::Gt,
            value: "lots".to_string(),
            value2: String::new(),
            kas: true,
            error: None,
            search_open: false,
        });
        let err = draft.build(true).unwrap_err();
        assert!(err.starts_with("Fee:"), "{err}");
        let Node::Group { children, .. } = &draft.root else {
            panic!()
        };
        assert!(matches!(&children[0], Node::Cond { error: Some(_), .. }));
    }

    #[test]
    fn an_empty_condition_is_left_out_until_it_matters() {
        let mut keys = 0;
        let mut draft = Draft::new(Entity::Transactions, &mut keys);
        let Node::Group { children, .. } = &mut draft.root else {
            panic!()
        };
        children.push(Node::Cond {
            key: 7,
            not: false,
            field: FieldId::TxTxid,
            op: Op::Eq,
            value: String::new(),
            value2: String::new(),
            kas: true,
            error: None,
            search_open: false,
        });
        let lenient = draft.build(false).unwrap();
        assert!(
            lenient.filter.is_none(),
            "nothing typed yet is no condition"
        );
        let err = draft.build(true).unwrap_err();
        assert!(err.contains("value"), "{err}");
    }

    #[test]
    fn times_may_say_ago() {
        assert_eq!(
            parse_typed(FieldKind::Time, "24h ago", true).unwrap(),
            Value::Duration(24 * 3_600_000)
        );
        assert_eq!(strip_ago("36h"), "36h");
        let mut keys = 0;
        let mut draft = Draft::new(Entity::Transactions, &mut keys);
        draft.range = None;
        draft.custom_from = "36h ago".to_string();
        assert_eq!(
            draft.build(true).unwrap().range,
            TimeRange::Last(36 * 3_600_000)
        );
    }

    #[test]
    fn amounts_take_the_chosen_unit() {
        assert_eq!(
            parse_typed(FieldKind::Amount, "1.5", true).unwrap(),
            Value::Amount(150_000_000)
        );
        assert_eq!(
            parse_typed(FieldKind::Amount, "150", false).unwrap(),
            Value::Amount(150)
        );
        assert_eq!(
            parse_typed(FieldKind::Amount, "2 sompi", true).unwrap(),
            Value::Amount(2)
        );
        assert_eq!(
            parse_typed(FieldKind::Int, "7", true).unwrap(),
            Value::Int(7)
        );
    }
}
