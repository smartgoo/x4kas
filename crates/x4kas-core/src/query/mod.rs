//! Queries over the indexed BlockDAG: a structured model (`Query`), the catalog of what
//! can be asked about transactions, blocks, payouts and addresses (`fields`), a text
//! form that round-trips with the model (`text`: `tx where fee > 1 KAS last 24h order by
//! fee desc limit 100`), the executor that answers one against the index (`exec`), and
//! the user's saved queries (`saved`). Shared by the GUI's Query tab and
//! `x4kas-cli query`; nothing here touches a node.
//!
//! The text form is the canonical one: saved queries store it, and the builder in the
//! GUI edits the model and prints it. `text::parse(&text::print(q)) == q` for every
//! valid query.

pub mod exec;
pub mod fields;
pub mod saved;
pub mod text;
pub mod watch;

use std::fmt;

pub use fields::{FieldId, FieldKind, FieldSpec};

use crate::index::records::Hash32;

/// What a query returns rows of.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Entity {
    /// Accepted transactions.
    Transactions,
    /// Chain blocks (with headers) and the merged blocks their transactions name.
    Blocks,
    /// Coinbase outputs of chain blocks: one per mergeset blue block rewarded, paid to
    /// that block's miner. Mining per pool over the whole DAG.
    Payouts,
    /// Addresses the index has seen, with their totals over the window.
    Addresses,
}

impl Entity {
    pub const ALL: [Self; 4] = [
        Self::Transactions,
        Self::Blocks,
        Self::Payouts,
        Self::Addresses,
    ];

    /// The keyword in the text form.
    pub fn name(self) -> &'static str {
        match self {
            Self::Transactions => "tx",
            Self::Blocks => "blocks",
            Self::Payouts => "payouts",
            Self::Addresses => "addresses",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Transactions => "Transactions",
            Self::Blocks => "Blocks",
            Self::Payouts => "Payouts",
            Self::Addresses => "Addresses",
        }
    }

    /// The entity named by a keyword (`tx`, `transactions`, `blocks`, `payouts`,
    /// `addresses`; case-insensitive).
    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "tx" | "txs" | "transactions" | "transaction" => Some(Self::Transactions),
            "blocks" | "block" => Some(Self::Blocks),
            "payouts" | "payout" => Some(Self::Payouts),
            "addresses" | "address" => Some(Self::Addresses),
            _ => None,
        }
    }

    /// Whether rows have a time, so a time range and "newest first" apply.
    pub fn is_timed(self) -> bool {
        !matches!(self, Self::Addresses)
    }

    /// The field that identifies a row (what a click opens).
    pub fn primary_field(self) -> FieldId {
        match self {
            Self::Transactions => FieldId::TxTxid,
            Self::Blocks => FieldId::BlockHash,
            Self::Payouts => FieldId::PayoutBlock,
            Self::Addresses => FieldId::AddrAddress,
        }
    }

    /// The time field, for timed entities.
    pub fn time_field(self) -> Option<FieldId> {
        match self {
            Self::Transactions => Some(FieldId::TxTime),
            Self::Blocks => Some(FieldId::BlockTime),
            Self::Payouts => Some(FieldId::PayoutTime),
            Self::Addresses => None,
        }
    }
}

/// A query: what to return, which rows, over which time, grouped how, in what order,
/// how many, and which columns.
#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    pub entity: Entity,
    pub filter: Option<Filter>,
    pub range: TimeRange,
    /// Aggregate instead of listing rows.
    pub group: Option<GroupBy>,
    pub order: Vec<(OrderKey, Dir)>,
    pub limit: Option<usize>,
    /// The columns to show; the entity's defaults when `None`. Not used with `group`.
    pub columns: Option<Vec<FieldId>>,
}

/// Rows a new query shows at most, until the user says otherwise.
pub const DEFAULT_LIMIT: usize = 200;

impl Query {
    /// A query listing the newest rows of `entity` over the last 24 hours.
    pub fn default_for(entity: Entity) -> Self {
        Self {
            entity,
            filter: None,
            range: if entity.is_timed() {
                TimeRange::Last(24 * 3_600_000)
            } else {
                TimeRange::All
            },
            group: None,
            order: Vec::new(),
            limit: Some(DEFAULT_LIMIT),
            columns: None,
        }
    }

    /// The transactions of `address`, newest first, over everything indexed (a quiet
    /// address has none in the last day).
    pub fn transactions_of(address: &str) -> Self {
        let mut q = Self::default_for(Entity::Transactions);
        q.range = TimeRange::All;
        q.filter = Some(Filter::Cond(Condition {
            field: FieldId::TxAddress,
            op: Op::Eq,
            value: Value::Address(address.to_string()),
        }));
        q
    }

    /// The transactions a chain block accepted.
    pub fn accepted_by(block: Hash32) -> Self {
        let mut q = Self::default_for(Entity::Transactions);
        q.range = TimeRange::All;
        q.filter = Some(Filter::Cond(Condition {
            field: FieldId::TxAcceptingBlock,
            op: Op::Eq,
            value: Value::Hash(block),
        }));
        q
    }

    /// The chain blocks `miner` mined, over everything indexed.
    pub fn mined_by(miner: &str) -> Self {
        let mut q = Self::default_for(Entity::Blocks);
        q.range = TimeRange::All;
        q.filter = Some(Filter::Cond(Condition {
            field: FieldId::BlockMiner,
            op: Op::Eq,
            value: Value::Address(miner.to_string()),
        }));
        q
    }

    /// The canonical text form (`text::print`).
    pub fn to_text(&self) -> String {
        text::print(self)
    }

    /// The columns the result has when not grouped.
    pub fn columns(&self) -> Vec<FieldId> {
        self.columns
            .clone()
            .unwrap_or_else(|| fields::default_columns(self.entity))
    }

    /// Every field the query reads: its conditions, columns (or grouping) and ordering.
    pub fn fields_used(&self) -> Vec<FieldId> {
        let mut out: Vec<FieldId> = self.conditions().iter().map(|c| c.field).collect();
        match &self.group {
            Some(group) => {
                out.extend(group.keys.iter().filter_map(|k| match k {
                    GroupKey::Field(f) => Some(*f),
                    GroupKey::TimeBucket(_) => None,
                }));
                out.extend(group.metrics.iter().filter_map(|m| m.field()));
            }
            None => out.extend(self.columns()),
        }
        out.extend(self.order.iter().filter_map(|(k, _)| match k {
            OrderKey::Field(f) => Some(*f),
            _ => None,
        }));
        out.sort_unstable_by_key(|f| *f as usize);
        out.dedup();
        out
    }

    /// Every condition, in order.
    pub fn conditions(&self) -> Vec<&Condition> {
        let mut out = Vec::new();
        if let Some(f) = &self.filter {
            f.conditions_into(&mut out);
        }
        out
    }

    /// Whether the query is well-formed: fields belong to the entity, operators and
    /// values fit the fields' kinds, the grouping and ordering refer to what exists.
    pub fn validate(&self) -> Result<(), QueryError> {
        let entity = self.entity;
        for cond in self.conditions() {
            cond.validate(entity)?;
        }
        if let Some(group) = &self.group {
            if self.columns.is_some() {
                return Err(QueryError::new(
                    "select isn't used with grouping: the group keys and metrics are the columns",
                ));
            }
            if group.keys.is_empty() {
                return Err(QueryError::new("group by needs at least one key"));
            }
            if group.metrics.is_empty() {
                return Err(QueryError::new(
                    "grouping needs a metric: count, sum(field), …",
                ));
            }
            for key in &group.keys {
                match key {
                    GroupKey::Field(f) => check_field(entity, *f)?,
                    GroupKey::TimeBucket(ms) => {
                        if !entity.is_timed() {
                            return Err(QueryError::new(format!(
                                "{} have no time to bucket by",
                                entity.name()
                            )));
                        }
                        if *ms == 0 {
                            return Err(QueryError::new("a time bucket needs a width"));
                        }
                    }
                }
            }
            for metric in &group.metrics {
                if let Some(f) = metric.field() {
                    check_field(entity, f)?;
                    let kind = f.kind();
                    let numeric =
                        matches!(kind, FieldKind::Int | FieldKind::Amount | FieldKind::Float);
                    match metric {
                        Metric::Sum(_) | Metric::Avg(_) if !numeric => {
                            return Err(QueryError::new(format!("{} isn't a number", f.name())));
                        }
                        Metric::Min(_) | Metric::Max(_)
                            if !(numeric || kind == FieldKind::Time) =>
                        {
                            return Err(QueryError::new(format!("{} can't be ordered", f.name())));
                        }
                        _ => {}
                    }
                }
            }
        }
        for (key, _) in &self.order {
            match (key, &self.group) {
                (OrderKey::Field(f), None) => {
                    check_field(entity, *f)?;
                    if f.spec().cost == fields::Cost::Node {
                        return Err(QueryError::new(format!(
                            "{} comes from the node for the rows shown; it can't order them",
                            f.name()
                        )));
                    }
                }
                (OrderKey::Field(f), Some(group)) => {
                    if !group.keys.contains(&GroupKey::Field(*f)) {
                        return Err(QueryError::new(format!(
                            "order by {} needs it as a group key (or order by a metric)",
                            f.name()
                        )));
                    }
                }
                (OrderKey::Metric(i), Some(group)) => {
                    if *i >= group.metrics.len() {
                        return Err(QueryError::new(
                            "order by names a metric the query doesn't have",
                        ));
                    }
                }
                (OrderKey::Bucket, Some(group)) => {
                    if !group
                        .keys
                        .iter()
                        .any(|k| matches!(k, GroupKey::TimeBucket(_)))
                    {
                        return Err(QueryError::new("order by bucket needs a time(…) group key"));
                    }
                }
                (OrderKey::Metric(_) | OrderKey::Bucket, None) => {
                    return Err(QueryError::new(
                        "ordering by a metric or bucket needs grouping",
                    ));
                }
            }
        }
        if let Some(columns) = &self.columns {
            if columns.is_empty() {
                return Err(QueryError::new("select needs at least one field"));
            }
            for f in columns {
                check_field(entity, *f)?;
            }
        }
        if self.limit == Some(0) {
            return Err(QueryError::new("limit must be at least 1"));
        }
        if let TimeRange::Between(from, to) = self.range
            && from >= to
        {
            return Err(QueryError::new("the time range ends before it starts"));
        }
        Ok(())
    }
}

fn check_field(entity: Entity, f: FieldId) -> Result<(), QueryError> {
    if f.entity() != entity {
        return Err(QueryError::new(format!(
            "{} is a field of {}, not {}",
            f.name(),
            f.entity().name(),
            entity.name()
        )));
    }
    Ok(())
}

/// Which rows: a tree of conditions.
#[derive(Debug, Clone, PartialEq)]
pub enum Filter {
    And(Vec<Filter>),
    Or(Vec<Filter>),
    Not(Box<Filter>),
    Cond(Condition),
}

impl Filter {
    fn conditions_into<'a>(&'a self, out: &mut Vec<&'a Condition>) {
        match self {
            Self::And(items) | Self::Or(items) => items.iter().for_each(|f| f.conditions_into(out)),
            Self::Not(inner) => inner.conditions_into(out),
            Self::Cond(c) => out.push(c),
        }
    }

    /// The conditions every matching row must satisfy: those reachable through `And`
    /// alone (the planner picks an access path among them).
    pub fn conjuncts(&self) -> Vec<&Condition> {
        let mut out = Vec::new();
        self.conjuncts_into(&mut out);
        out
    }

    fn conjuncts_into<'a>(&'a self, out: &mut Vec<&'a Condition>) {
        match self {
            Self::And(items) => items.iter().for_each(|f| f.conjuncts_into(out)),
            Self::Cond(c) => out.push(c),
            Self::Or(_) | Self::Not(_) => {}
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Condition {
    pub field: FieldId,
    pub op: Op,
    pub value: Value,
}

impl Condition {
    pub fn new(field: FieldId, op: Op, value: Value) -> Self {
        Self { field, op, value }
    }

    /// Whether the condition fits `entity`: its field, operator and value. The error has
    /// no position; `text::parse_valid` adds the condition's.
    pub(crate) fn validate(&self, entity: Entity) -> Result<(), QueryError> {
        check_field(entity, self.field)?;
        let spec = self.field.spec();
        if spec.cost == fields::Cost::Node {
            return Err(QueryError::new(format!(
                "{} comes from the node for the rows shown; it can't filter them",
                spec.name
            )));
        }
        if !spec.kind.allows(self.op) {
            return Err(QueryError::new(format!(
                "{} can't be used with {} ({})",
                self.op.name(),
                spec.name,
                spec.kind.label()
            )));
        }
        match self.op {
            Op::IsNull | Op::IsNotNull => {
                if self.value != Value::Null {
                    return Err(QueryError::new("is null takes no value"));
                }
                return Ok(());
            }
            Op::In | Op::NotIn => match &self.value {
                Value::List(items) if !items.is_empty() => {
                    for item in items {
                        check_value(spec, item)?;
                    }
                    return Ok(());
                }
                _ => {
                    return Err(QueryError::new(format!(
                        "{} in (…) needs a list",
                        spec.name
                    )));
                }
            },
            Op::Between => match &self.value {
                Value::List(items) if items.len() == 2 => {
                    for item in items {
                        check_value(spec, item)?;
                    }
                    return Ok(());
                }
                _ => {
                    return Err(QueryError::new(format!(
                        "{} between needs two values",
                        spec.name
                    )));
                }
            },
            _ => {}
        }
        check_value(spec, &self.value)
    }
}

/// Whether `value` fits a field of `spec`'s kind.
fn check_value(spec: &FieldSpec, value: &Value) -> Result<(), QueryError> {
    let ok = match (spec.kind, value) {
        (FieldKind::Hash | FieldKind::HashList, Value::Hash(_)) => true,
        (FieldKind::Address | FieldKind::AddressList, Value::Address(_)) => true,
        (FieldKind::Text, Value::Text(_) | Value::Enum(_)) => true,
        (FieldKind::Int, Value::Int(_)) => true,
        (FieldKind::Amount, Value::Amount(_)) => true,
        (FieldKind::Amount, Value::Int(_) | Value::Float(_)) => {
            return Err(QueryError::new(format!(
                "{} is an amount: say 1.5 KAS or 100 sompi",
                spec.name
            )));
        }
        (FieldKind::Float, Value::Float(_) | Value::Int(_)) => true,
        (FieldKind::Time, Value::Time(_) | Value::Duration(_)) => true,
        (FieldKind::Hash | FieldKind::HashList, value) => {
            return Err(QueryError::new(format!(
                "{} is a hash: 64 hex characters, not {}",
                spec.name,
                value.kind_label()
            )));
        }
        (FieldKind::Address | FieldKind::AddressList, value) => {
            return Err(QueryError::new(format!(
                "{} is an address: starts with kaspa: or kaspatest:, not {}",
                spec.name,
                value.kind_label()
            )));
        }
        (FieldKind::Time, value) => {
            return Err(QueryError::new(format!(
                "{} is a time: a UTC time like 2026-10-01T12:00Z or how long ago like 24h \
                 (time > 24h means within the last day), not {}",
                spec.name,
                value.kind_label()
            )));
        }
        (FieldKind::Bool, Value::Bool(_)) => true,
        (FieldKind::Enum(options), Value::Enum(name)) => {
            if !options.contains(&name.as_str()) {
                return Err(QueryError::new(format!(
                    "{} is one of {}, not {name}",
                    spec.name,
                    options.join(", ")
                )));
            }
            true
        }
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        Err(QueryError::new(format!(
            "{} is {}: {} doesn't fit",
            spec.name,
            spec.kind.label(),
            value.kind_label()
        )))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Op {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Between,
    In,
    NotIn,
    Contains,
    StartsWith,
    IsNull,
    IsNotNull,
}

impl Op {
    /// The text form.
    pub fn name(self) -> &'static str {
        match self {
            Self::Eq => "=",
            Self::Ne => "!=",
            Self::Lt => "<",
            Self::Le => "<=",
            Self::Gt => ">",
            Self::Ge => ">=",
            Self::Between => "between",
            Self::In => "in",
            Self::NotIn => "not in",
            Self::Contains => "contains",
            Self::StartsWith => "starts_with",
            Self::IsNull => "is null",
            Self::IsNotNull => "is not null",
        }
    }

    /// A readable name for the builder.
    pub fn label(self) -> &'static str {
        match self {
            Self::Eq => "is",
            Self::Ne => "is not",
            Self::Lt => "<",
            Self::Le => "≤",
            Self::Gt => ">",
            Self::Ge => "≥",
            Self::Between => "between",
            Self::In => "is one of",
            Self::NotIn => "is none of",
            Self::Contains => "contains",
            Self::StartsWith => "starts with",
            Self::IsNull => "is unknown",
            Self::IsNotNull => "is known",
        }
    }

    /// Whether the operator takes no value (`is null`), two (`between`) or a list.
    pub fn arity(self) -> Arity {
        match self {
            Self::IsNull | Self::IsNotNull => Arity::None,
            Self::Between => Arity::Two,
            Self::In | Self::NotIn => Arity::List,
            _ => Arity::One,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arity {
    None,
    One,
    Two,
    List,
}

/// A literal in a condition.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    /// Sompi.
    Amount(i64),
    Float(f64),
    /// Unix milliseconds.
    Time(u64),
    /// Milliseconds; on a time field, this long before now.
    Duration(u64),
    Hash(Hash32),
    Address(String),
    Text(String),
    /// A bare word: an option of an enum field (lowercase).
    Enum(String),
    List(Vec<Value>),
}

impl Value {
    fn kind_label(&self) -> &'static str {
        match self {
            Self::Null => "nothing",
            Self::Bool(_) => "true/false",
            Self::Int(_) => "a number",
            Self::Amount(_) => "an amount",
            Self::Float(_) => "a number",
            Self::Time(_) => "a time",
            Self::Duration(_) => "a duration",
            Self::Hash(_) => "a hash",
            Self::Address(_) => "an address",
            Self::Text(_) => "text",
            Self::Enum(_) => "a bare word",
            Self::List(_) => "a list",
        }
    }
}

/// Which rows by time, resolved against "now" when the query runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeRange {
    /// Everything the index holds.
    All,
    /// The last this many milliseconds.
    Last(u64),
    Since(u64),
    /// `from..to` in unix milliseconds.
    Between(u64, u64),
}

impl TimeRange {
    /// `(from_ms, to_ms)`, `to_ms` exclusive; `u64::MAX` for an open end.
    pub fn resolve(self, now_ms: u64) -> (u64, u64) {
        match self {
            Self::All => (0, u64::MAX),
            Self::Last(ms) => (now_ms.saturating_sub(ms), u64::MAX),
            Self::Since(from) => (from, u64::MAX),
            Self::Between(from, to) => (from, to),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupBy {
    pub keys: Vec<GroupKey>,
    pub metrics: Vec<Metric>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupKey {
    Field(FieldId),
    /// The row's time rounded down to a bucket this many milliseconds wide.
    TimeBucket(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    Count,
    CountDistinct(FieldId),
    Sum(FieldId),
    Avg(FieldId),
    Min(FieldId),
    Max(FieldId),
}

impl Metric {
    pub fn field(self) -> Option<FieldId> {
        match self {
            Self::Count => None,
            Self::CountDistinct(f) | Self::Sum(f) | Self::Avg(f) | Self::Min(f) | Self::Max(f) => {
                Some(f)
            }
        }
    }

    /// The text form, also the result column's name: `count`, `sum(fee)`.
    pub fn name(self) -> String {
        match self {
            Self::Count => "count".to_string(),
            Self::CountDistinct(f) => format!("count_distinct({})", f.name()),
            Self::Sum(f) => format!("sum({})", f.name()),
            Self::Avg(f) => format!("avg({})", f.name()),
            Self::Min(f) => format!("min({})", f.name()),
            Self::Max(f) => format!("max({})", f.name()),
        }
    }

    /// The kind of the metric's values.
    pub fn kind(self) -> FieldKind {
        match self {
            Self::Count | Self::CountDistinct(_) => FieldKind::Int,
            Self::Avg(_) => FieldKind::Float,
            Self::Sum(f) | Self::Min(f) | Self::Max(f) => f.kind(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderKey {
    Field(FieldId),
    /// The n-th metric of the grouping.
    Metric(usize),
    /// The time bucket of the grouping.
    Bucket,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Asc,
    Desc,
}

/// Why a text couldn't be parsed or a query can't run, with where in the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryError {
    /// Where in the text the error is, when it is about a place in it: a **byte**
    /// offset (always on a character boundary, so `text[..pos]` is valid). A caret
    /// under the place is at column `text[..pos].chars().count()`.
    pub pos: Option<usize>,
    pub msg: String,
}

impl QueryError {
    pub fn new(msg: impl Into<String>) -> Self {
        Self {
            pos: None,
            msg: msg.into(),
        }
    }

    pub fn at(pos: usize, msg: impl Into<String>) -> Self {
        Self {
            pos: Some(pos),
            msg: msg.into(),
        }
    }
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.pos {
            Some(pos) => write!(f, "{} (at {pos})", self.msg),
            None => f.write_str(&self.msg),
        }
    }
}

impl std::error::Error for QueryError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn cond(field: FieldId, op: Op, value: Value) -> Option<Filter> {
        Some(Filter::Cond(Condition::new(field, op, value)))
    }

    #[test]
    fn entity_names_round_trip() {
        for e in Entity::ALL {
            assert_eq!(Entity::from_name(e.name()), Some(e));
            assert_eq!(Entity::from_name(&e.name().to_uppercase()), Some(e));
            assert_eq!(e.primary_field().entity(), e);
            assert_eq!(e.time_field().is_some(), e.is_timed());
        }
        assert_eq!(
            Entity::from_name("transactions"),
            Some(Entity::Transactions)
        );
        assert_eq!(Entity::from_name("utxos"), None);
    }

    #[test]
    fn defaults_and_helpers_validate() {
        for e in Entity::ALL {
            Query::default_for(e).validate().unwrap();
        }
        Query::transactions_of("kaspa:qq").validate().unwrap();
        Query::accepted_by([1; 32]).validate().unwrap();
        Query::mined_by("kaspa:qq").validate().unwrap();
        // The helpers look over everything indexed, not the last day.
        assert_eq!(
            Query::transactions_of("kaspa:qq").to_text(),
            "tx all time where address = kaspa:qq limit 200"
        );
        assert_eq!(
            Query::mined_by("kaspa:qq").to_text(),
            "blocks all time where miner = kaspa:qq limit 200"
        );
        assert_eq!(Query::accepted_by([1; 32]).range, TimeRange::All);
        assert_eq!(
            Query::default_for(Entity::Transactions).columns(),
            fields::default_columns(Entity::Transactions)
        );
    }

    #[test]
    fn validation_catches_misfits() {
        let mut q = Query::default_for(Entity::Transactions);
        q.filter = cond(
            FieldId::BlockMiner,
            Op::Eq,
            Value::Address("kaspa:q".into()),
        );
        assert!(q.validate().unwrap_err().msg.contains("field of blocks"));

        q.filter = cond(FieldId::TxFee, Op::Gt, Value::Int(5));
        assert!(q.validate().unwrap_err().msg.contains("1.5 KAS"));
        q.filter = cond(FieldId::TxFee, Op::Gt, Value::Amount(5));
        q.validate().unwrap();

        q.filter = cond(FieldId::TxFee, Op::Contains, Value::Amount(5));
        assert!(q.validate().unwrap_err().msg.contains("can't be used"));

        q.filter = cond(FieldId::TxProtocol, Op::Eq, Value::Enum("bitcoin".into()));
        assert!(q.validate().unwrap_err().msg.contains("is one of"));
        q.filter = cond(FieldId::TxProtocol, Op::Eq, Value::Enum("krc".into()));
        q.validate().unwrap();

        q.filter = cond(FieldId::TxFee, Op::Between, Value::Amount(5));
        assert!(q.validate().unwrap_err().msg.contains("two values"));
        q.filter = cond(FieldId::TxFee, Op::In, Value::List(vec![]));
        assert!(q.validate().unwrap_err().msg.contains("needs a list"));
        q.filter = cond(FieldId::TxFee, Op::IsNull, Value::Null);
        q.validate().unwrap();
        q.filter = cond(FieldId::TxTime, Op::Gt, Value::Duration(1000));
        q.validate().unwrap();
        // A bare word on a hash, address or time field says what is expected.
        q.filter = cond(FieldId::TxTxid, Op::Eq, Value::Enum("abc".into()));
        let e = q.validate().unwrap_err();
        assert!(e.msg.contains("64 hex characters"), "{e}");
        q.filter = cond(FieldId::TxSender, Op::Eq, Value::Enum("binance".into()));
        let e = q.validate().unwrap_err();
        assert!(e.msg.contains("starts with kaspa:"), "{e}");
        q.filter = cond(FieldId::TxTime, Op::Gt, Value::Enum("yesterday".into()));
        let e = q.validate().unwrap_err();
        assert!(
            e.msg.contains("2026-10-01T12:00Z") && e.msg.contains("24h"),
            "{e}"
        );

        q.filter = None;
        q.limit = Some(0);
        assert!(q.validate().is_err());
        q.limit = None;
        q.range = TimeRange::Between(5, 5);
        assert!(q.validate().is_err());
        q.range = TimeRange::All;
        q.columns = Some(vec![]);
        assert!(q.validate().is_err());
        q.columns = Some(vec![FieldId::AddrAddress]);
        assert!(q.validate().is_err());
        q.columns = None;

        let mut a = Query::default_for(Entity::Addresses);
        a.filter = cond(FieldId::AddrBalance, Op::Gt, Value::Amount(1));
        assert!(a.validate().unwrap_err().msg.contains("from the node"));
        a.filter = None;
        a.order = vec![(OrderKey::Field(FieldId::AddrBalance), Dir::Desc)];
        assert!(a.validate().is_err());
        a.order = vec![(OrderKey::Field(FieldId::AddrReceived), Dir::Desc)];
        a.validate().unwrap();
    }

    #[test]
    fn grouping_rules() {
        let mut q = Query::default_for(Entity::Payouts);
        q.group = Some(GroupBy {
            keys: vec![GroupKey::Field(FieldId::PayoutMiner)],
            metrics: vec![Metric::Count, Metric::Sum(FieldId::PayoutAmount)],
        });
        q.order = vec![(OrderKey::Metric(0), Dir::Desc)];
        q.validate().unwrap();
        q.order = vec![(OrderKey::Metric(2), Dir::Desc)];
        assert!(q.validate().is_err());
        q.order = vec![(OrderKey::Bucket, Dir::Asc)];
        assert!(q.validate().is_err());
        q.order = vec![(OrderKey::Field(FieldId::PayoutAmount), Dir::Asc)];
        assert!(q.validate().is_err());
        q.order = vec![(OrderKey::Field(FieldId::PayoutMiner), Dir::Asc)];
        q.validate().unwrap();
        q.group
            .as_mut()
            .unwrap()
            .keys
            .push(GroupKey::TimeBucket(3_600_000));
        q.order = vec![(OrderKey::Bucket, Dir::Asc)];
        q.validate().unwrap();
        q.group.as_mut().unwrap().metrics = vec![Metric::Sum(FieldId::PayoutMiner)];
        assert!(q.validate().unwrap_err().msg.contains("isn't a number"));
        q.group.as_mut().unwrap().metrics = vec![Metric::Max(FieldId::PayoutTime)];
        q.validate().unwrap();
        q.columns = Some(vec![FieldId::PayoutTime]);
        assert!(q.validate().is_err());
        q.columns = None;
        let mut a = Query::default_for(Entity::Addresses);
        a.group = Some(GroupBy {
            keys: vec![GroupKey::TimeBucket(1)],
            metrics: vec![Metric::Count],
        });
        assert!(a.validate().unwrap_err().msg.contains("no time"));
        a.order = vec![(OrderKey::Metric(0), Dir::Desc)];
        a.group = None;
        assert!(a.validate().is_err());
    }

    #[test]
    fn conjuncts_are_the_and_level_conditions() {
        let c = |f| Condition::new(f, Op::IsNotNull, Value::Null);
        let filter = Filter::And(vec![
            Filter::Cond(c(FieldId::TxFee)),
            Filter::Or(vec![
                Filter::Cond(c(FieldId::TxMass)),
                Filter::Cond(c(FieldId::TxGas)),
            ]),
            Filter::Not(Box::new(Filter::Cond(c(FieldId::TxVersion)))),
            Filter::And(vec![Filter::Cond(c(FieldId::TxTime))]),
        ]);
        let names: Vec<&str> = filter.conjuncts().iter().map(|c| c.field.name()).collect();
        assert_eq!(names, vec!["fee", "time"]);
        let q = Query {
            filter: Some(filter),
            ..Query::default_for(Entity::Transactions)
        };
        assert_eq!(q.conditions().len(), 5);
    }

    #[test]
    fn time_ranges_resolve() {
        assert_eq!(TimeRange::All.resolve(100), (0, u64::MAX));
        assert_eq!(TimeRange::Last(30).resolve(100), (70, u64::MAX));
        assert_eq!(TimeRange::Last(300).resolve(100), (0, u64::MAX));
        assert_eq!(TimeRange::Since(50).resolve(100), (50, u64::MAX));
        assert_eq!(TimeRange::Between(1, 2).resolve(100), (1, 2));
    }
}
