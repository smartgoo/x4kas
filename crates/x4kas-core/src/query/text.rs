//! The text form of a query, and its parser and printer.
//!
//! ```text
//! tx last 6h where fee_rate > 10 and protocol in (krc, kns) and not is_coinbase
//!    order by fee desc limit 200 select time, txid, fee, sender
//! payouts last 1d count, sum(amount) by miner order by count desc
//! addresses where label contains "binance" order by received desc limit 50
//! ```
//!
//! The canonical form (what [`print`] writes) puts the time range before `where`, a
//! bare bool field for `= true`, and durations in their largest units (`1d`, not `24h`).
//!
//! Grammar (keywords are case-insensitive; clauses come in any order, each at most once):
//!
//! ```text
//! query   = entity { "where" expr | range | group | order | limit | select } ;
//! entity  = "tx" | "transactions" | "blocks" | "payouts" | "addresses" ;
//! expr    = and { "or" and } ;  and = not { "and" not } ;
//! not     = "not" not | "(" expr ")" | cond ;
//! cond    = field ( ("=" | "!=" | "<" | "<=" | ">" | ">=") value
//!                 | "between" value "and" value
//!                 | ["not"] "in" "(" value { "," value } ")"
//!                 | "contains" value | "starts_with" value
//!                 | "is" ["not"] "null" | "is" ("true" | "false") ) ;
//! value   = amount | number | duration | datetime | hex64 | address | "…" | name ;
//! range   = "last" duration | "since" datetime | "between" datetime "and" datetime | "all" "time" ;
//! group   = metric { "," metric } "by" key { "," key } ;
//! metric  = "count" | ("count_distinct" | "sum" | "avg" | "min" | "max") "(" field ")" ;
//! key     = field | "time" "(" duration ")" ;
//! order   = "order" "by" (field | metric | "bucket") ["asc" | "desc"] { "," … } ;
//! limit   = "limit" digits ;   select = "select" field { "," field } ;
//! ```
//!
//! Literals: `1.5 KAS` or `100 sompi` (amounts), `24h`/`1d12h`/`90m` (durations),
//! `2026-10-01` or `2026-10-01T12:00Z` (UTC times), 64 hex characters (hashes),
//! `kaspa:…` (addresses), `"…"` (text; a bare word works too), bare names for enums
//! and `true`/`false`. The parser is lenient about spellings the printer never writes:
//! `_` between digits (`1_000_000`), `'single quotes'`, `==` for `=` and `<>` for `!=`;
//! `&&`, `||`, `#` and `1,000` get an error saying what to write. [`print`] writes the
//! canonical form and [`parse`] reads it back: `parse(&print(q)) == q`.

use super::fields::{self, FieldKind};
use super::{
    Condition, Dir, Entity, FieldId, Filter, GroupBy, GroupKey, Metric, Op, OrderKey, Query,
    QueryError, TimeRange, Value,
};
use crate::format::{
    format_duration_ms, format_sompi_exact, format_utc, parse_duration_ms, parse_kas_to_sompi,
    parse_utc,
};
use crate::index::{hex, parse_hex};

// --- Tokens ---

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    /// A run of word characters: a keyword, a field, a number, a time, an address…
    Word(String),
    /// A quoted string, unescaped.
    Str(String),
    /// `= != < <= > >= ( ) ,`
    Sym(&'static str),
}

#[derive(Debug, Clone)]
struct Token {
    tok: Tok,
    pos: usize,
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '.' | ':' | '-' | '+')
}

fn tokenize(text: &str) -> Result<Vec<Token>, QueryError> {
    let mut tokens = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = text[i..].chars().next().unwrap();
        if c.is_whitespace() {
            i += c.len_utf8();
            continue;
        }
        let pos = i;
        match c {
            // A string in double or single quotes (the printer writes double ones).
            quote @ ('"' | '\'') => {
                let mut s = String::new();
                let mut j = i + 1;
                loop {
                    let Some(ch) = text[j..].chars().next() else {
                        return Err(QueryError::at(pos, "unterminated string"));
                    };
                    j += ch.len_utf8();
                    match ch {
                        ch if ch == quote => break,
                        '\\' => {
                            let Some(esc) = text[j..].chars().next() else {
                                return Err(QueryError::at(pos, "unterminated string"));
                            };
                            j += esc.len_utf8();
                            s.push(match esc {
                                'n' => '\n',
                                't' => '\t',
                                other => other,
                            });
                        }
                        other => s.push(other),
                    }
                }
                tokens.push(Token {
                    tok: Tok::Str(s),
                    pos,
                });
                i = j;
            }
            '(' | ')' | ',' => {
                tokens.push(Token {
                    tok: Tok::Sym(match c {
                        '(' => "(",
                        ')' => ")",
                        _ => ",",
                    }),
                    pos,
                });
                i += 1;
            }
            // `==` reads as `=` and `<>` as `!=`; the printer writes `=` and `!=`.
            '=' => {
                let two = bytes.get(i + 1) == Some(&b'=');
                tokens.push(Token {
                    tok: Tok::Sym("="),
                    pos,
                });
                i += if two { 2 } else { 1 };
            }
            '!' | '<' | '>' => {
                let next = bytes.get(i + 1).copied();
                let sym = match (c, next) {
                    ('!', Some(b'=')) => "!=",
                    ('<', Some(b'>')) => "!=",
                    ('<', Some(b'=')) => "<=",
                    ('>', Some(b'=')) => ">=",
                    ('<', _) => "<",
                    ('>', _) => ">",
                    _ => return Err(QueryError::at(pos, "expected != after !")),
                };
                tokens.push(Token {
                    tok: Tok::Sym(sym),
                    pos,
                });
                i += if sym.len() == 2 { 2 } else { 1 };
            }
            '&' => return Err(QueryError::at(pos, "use and, not &&")),
            '|' => return Err(QueryError::at(pos, "use or, not ||")),
            '#' => return Err(QueryError::at(pos, "comments aren't supported")),
            c if is_word_char(c) => {
                let mut j = i;
                while let Some(ch) = text[j..].chars().next() {
                    if !is_word_char(ch) {
                        break;
                    }
                    j += ch.len_utf8();
                }
                tokens.push(Token {
                    tok: Tok::Word(text[i..j].to_string()),
                    pos,
                });
                i = j;
            }
            other => {
                return Err(QueryError::at(
                    pos,
                    format!("unexpected character {other:?}"),
                ));
            }
        }
    }
    Ok(tokens)
}

// --- Literals ---

const ADDRESS_PREFIXES: [&str; 4] = ["kaspa:", "kaspatest:", "kaspadev:", "kaspasim:"];

/// `word` without the `_` separators a number may carry between digits (`1_000_000`);
/// unchanged when it isn't a number written that way (`fee_rate` keeps its underscore).
fn strip_digit_separators(word: &str) -> std::borrow::Cow<'_, str> {
    let bytes = word.as_bytes();
    let separated = word.contains('_')
        && bytes.iter().enumerate().all(|(i, b)| {
            *b != b'_'
                || (i > 0
                    && bytes[i - 1].is_ascii_digit()
                    && bytes.get(i + 1).is_some_and(u8::is_ascii_digit))
        });
    if separated {
        let stripped = word.replace('_', "");
        if is_number(&stripped)
            || ["kas", "sompi"].iter().any(|unit| {
                stripped
                    .to_ascii_lowercase()
                    .strip_suffix(unit)
                    .is_some_and(is_number)
            })
        {
            return std::borrow::Cow::Owned(stripped);
        }
    }
    std::borrow::Cow::Borrowed(word)
}

/// Whether `word` is plain digits (what a thousands group after a comma looks like).
fn is_digits(word: &str) -> bool {
    !word.is_empty() && word.bytes().all(|b| b.is_ascii_digit())
}

fn is_number(s: &str) -> bool {
    let s = s.strip_prefix(['-', '+']).unwrap_or(s);
    let (mantissa, exponent) = match s.split_once(['e', 'E']) {
        Some((m, e)) => (m, Some(e)),
        None => (s, None),
    };
    let (int, frac) = match mantissa.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (mantissa, None),
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let mantissa_ok = match frac {
        Some(f) => {
            (digits(int) || int.is_empty())
                && (digits(f) || f.is_empty())
                && !(int.is_empty() && f.is_empty())
        }
        None => digits(int),
    };
    mantissa_ok && exponent.is_none_or(|e| digits(e.strip_prefix(['-', '+']).unwrap_or(e)))
}

/// What a bare word is, without knowing the field: an amount with its unit attached
/// (`1.5kas`), a number, a hash, an address, a time, a duration, a bool, `null`, or a
/// name. The caller turns a name into text or an enum option from the field's kind.
pub fn classify_word(word: &str) -> Result<Value, QueryError> {
    let word = &*strip_digit_separators(word);
    let lower = word.to_ascii_lowercase();
    for (unit, scale) in [("sompi", 1i64), ("kas", 100_000_000)] {
        if let Some(number) = lower.strip_suffix(unit)
            && is_number(number)
        {
            return amount(number, scale);
        }
    }
    if let Some(hash) = parse_hex(word) {
        return Ok(Value::Hash(hash));
    }
    if is_number(word) {
        return if word.contains(['.', 'e', 'E']) {
            word.parse::<f64>()
                .map(Value::Float)
                .map_err(|_| QueryError::new(format!("{word} isn't a number")))
        } else {
            word.parse::<i64>()
                .map(Value::Int)
                .map_err(|_| QueryError::new(format!("{word} is too large")))
        };
    }
    if ADDRESS_PREFIXES.iter().any(|p| lower.starts_with(p)) {
        return Ok(Value::Address(word.to_string()));
    }
    if let Some(ms) = parse_utc(word) {
        return Ok(Value::Time(ms));
    }
    if let Some(ms) = parse_duration_ms(word) {
        return Ok(Value::Duration(ms));
    }
    Ok(match lower.as_str() {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        "null" => Value::Null,
        _ => Value::Enum(word.to_string()),
    })
}

/// `number` in units of `scale` sompi, exactly.
fn amount(number: &str, scale: i64) -> Result<Value, QueryError> {
    if scale == 1 {
        return number
            .parse::<i64>()
            .map(Value::Amount)
            .map_err(|_| QueryError::new(format!("{number} sompi isn't a whole number")));
    }
    if number.contains(['e', 'E']) {
        // `1e6 KAS`: say what to write instead.
        let full = number
            .parse::<f64>()
            .ok()
            .filter(|f| f.is_finite() && f.abs() < 1e13)
            .map(|f| format_sompi_exact((f * 1e8).round() as i64))
            .unwrap_or_else(|| "1000000".to_string());
        return Err(QueryError::new(format!(
            "write the amount in full: {full} KAS"
        )));
    }
    parse_kas_to_sompi(number)
        .map(Value::Amount)
        .ok_or_else(|| QueryError::new(format!("{number} KAS: at most 8 decimals")))
}

/// Make `value` fit a field of `kind`: a name becomes text or an enum option, a whole
/// number a float. Anything else is left for validation to judge.
pub fn coerce(value: Value, kind: FieldKind) -> Value {
    match (kind, value) {
        (FieldKind::Text, Value::Enum(word)) => Value::Text(word),
        (FieldKind::Enum(_), Value::Enum(word)) => Value::Enum(word.to_ascii_lowercase()),
        (FieldKind::Enum(_), Value::Text(word)) => Value::Enum(word.to_ascii_lowercase()),
        (FieldKind::Float, Value::Int(n)) => Value::Float(n as f64),
        (FieldKind::Text, Value::Bool(b)) => Value::Text(b.to_string()),
        (_, Value::List(items)) => {
            Value::List(items.into_iter().map(|v| coerce(v, kind)).collect())
        }
        (_, value) => value,
    }
}

/// A value typed for a field of `kind`, as the builder's value field takes it: the
/// text form of one literal (quotes optional for text).
pub fn parse_value(kind: FieldKind, text: &str) -> Result<Value, QueryError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(QueryError::new("a value is needed"));
    }
    if let Some(inner) = text.strip_prefix('"').and_then(|t| t.strip_suffix('"')) {
        return Ok(coerce(Value::Text(inner.to_string()), kind));
    }
    if kind == FieldKind::Text {
        return Ok(Value::Text(text.to_string()));
    }
    let tokens = tokenize(text)?;
    let mut p = Parser::new(tokens, text);
    let value = p.value()?;
    if p.idx != p.tokens.len() {
        return Err(QueryError::at(p.pos(), "one value only"));
    }
    Ok(coerce(value, kind))
}

// --- Parser ---

struct Parser<'a> {
    tokens: Vec<Token>,
    idx: usize,
    text: &'a str,
    entity: Entity,
    /// `order by <metric>` keys, resolved against the grouping once it is known.
    deferred: Vec<(usize, Metric)>,
    /// How many `in (…)` lists are open: a comma after a number inside one separates
    /// items; outside, it's a thousands separator the grammar doesn't have.
    list_depth: usize,
    /// Where each condition starts, in the order `Query::conditions` lists them, so a
    /// validation error can point at its condition.
    cond_positions: Vec<usize>,
}

const METRICS: [&str; 6] = ["count", "count_distinct", "sum", "avg", "min", "max"];

impl<'a> Parser<'a> {
    fn new(tokens: Vec<Token>, text: &'a str) -> Self {
        Self {
            tokens,
            idx: 0,
            text,
            entity: Entity::Transactions,
            deferred: Vec::new(),
            list_depth: 0,
            cond_positions: Vec::new(),
        }
    }
}

impl Parser<'_> {
    fn pos(&self) -> usize {
        self.tokens
            .get(self.idx)
            .map(|t| t.pos)
            .unwrap_or(self.text.len())
    }

    fn peek(&self) -> Option<&Tok> {
        self.tokens.get(self.idx).map(|t| &t.tok)
    }

    /// The next token if it is this word (case-insensitive).
    fn peek_word(&self, word: &str) -> bool {
        matches!(self.peek(), Some(Tok::Word(w)) if w.eq_ignore_ascii_case(word))
    }

    fn peek_sym(&self, sym: &str) -> bool {
        matches!(self.peek(), Some(Tok::Sym(s)) if *s == sym)
    }

    fn eat_word(&mut self, word: &str) -> bool {
        if self.peek_word(word) {
            self.idx += 1;
            true
        } else {
            false
        }
    }

    fn eat_sym(&mut self, sym: &str) -> bool {
        if self.peek_sym(sym) {
            self.idx += 1;
            true
        } else {
            false
        }
    }

    fn expect_word(&mut self, word: &str) -> Result<(), QueryError> {
        if self.eat_word(word) {
            Ok(())
        } else {
            Err(self.error(format!("expected {word}")))
        }
    }

    fn expect_sym(&mut self, sym: &str) -> Result<(), QueryError> {
        if self.eat_sym(sym) {
            Ok(())
        } else {
            Err(self.error(format!("expected {sym}")))
        }
    }

    fn error(&self, msg: impl Into<String>) -> QueryError {
        let msg = msg.into();
        match self.peek() {
            Some(Tok::Word(w)) => QueryError::at(self.pos(), format!("{msg}, found {w}")),
            Some(Tok::Str(_)) => QueryError::at(self.pos(), format!("{msg}, found a string")),
            Some(Tok::Sym(s)) => QueryError::at(self.pos(), format!("{msg}, found {s}")),
            None => QueryError::at(self.pos(), format!("{msg} at the end")),
        }
    }

    fn word(&mut self, what: &str) -> Result<(String, usize), QueryError> {
        match self.tokens.get(self.idx) {
            Some(Token {
                tok: Tok::Word(w),
                pos,
            }) => {
                let out = (w.clone(), *pos);
                self.idx += 1;
                Ok(out)
            }
            _ => Err(self.error(format!("expected {what}"))),
        }
    }

    fn query(&mut self) -> Result<Query, QueryError> {
        let (name, pos) = self.word("tx, blocks, payouts or addresses")?;
        let entity = Entity::from_name(&name).ok_or_else(|| {
            QueryError::at(
                pos,
                format!("expected tx, blocks, payouts or addresses, found {name}"),
            )
        })?;
        self.entity = entity;
        let mut q = Query {
            entity,
            filter: None,
            range: TimeRange::All,
            group: None,
            order: Vec::new(),
            limit: None,
            columns: None,
        };
        let mut seen: Vec<&'static str> = Vec::new();
        let mut once = |clause: &'static str, pos: usize| -> Result<(), QueryError> {
            if seen.contains(&clause) {
                return Err(QueryError::at(pos, format!("{clause} is given twice")));
            }
            seen.push(clause);
            Ok(())
        };
        while let Some(tok) = self.peek() {
            let pos = self.pos();
            let Tok::Word(w) = tok else {
                return Err(self.error("expected a clause (where, last, order by, limit, …)"));
            };
            match w.to_ascii_lowercase().as_str() {
                "where" => {
                    once("where", pos)?;
                    self.idx += 1;
                    q.filter = Some(self.expr()?);
                }
                "last" => {
                    once("the time range", pos)?;
                    self.idx += 1;
                    let (d, dpos) = self.word("a duration such as 24h")?;
                    let ms = parse_duration_ms(&d).ok_or_else(|| {
                        QueryError::at(dpos, format!("{d} isn't a duration (24h, 7d, 90m)"))
                    })?;
                    q.range = TimeRange::Last(ms);
                }
                "since" => {
                    once("the time range", pos)?;
                    self.idx += 1;
                    q.range = TimeRange::Since(self.datetime()?);
                }
                "between" => {
                    once("the time range", pos)?;
                    self.idx += 1;
                    let from = self.datetime()?;
                    self.expect_word("and")?;
                    let to = self.datetime()?;
                    q.range = TimeRange::Between(from, to);
                }
                "all" => {
                    once("the time range", pos)?;
                    self.idx += 1;
                    self.expect_word("time")?;
                    q.range = TimeRange::All;
                }
                "order" => {
                    once("order by", pos)?;
                    self.idx += 1;
                    self.expect_word("by")?;
                    q.order = self.order()?;
                }
                "limit" => {
                    once("limit", pos)?;
                    self.idx += 1;
                    let (n, npos) = self.word("a number")?;
                    q.limit = Some(
                        n.parse()
                            .map_err(|_| QueryError::at(npos, format!("{n} isn't a count")))?,
                    );
                }
                "select" => {
                    once("select", pos)?;
                    self.idx += 1;
                    let mut columns = vec![self.field()?];
                    while self.eat_sym(",") {
                        columns.push(self.field()?);
                    }
                    q.columns = Some(columns);
                }
                m if METRICS.contains(&m) => {
                    once("the grouping", pos)?;
                    q.group = Some(self.group()?);
                }
                _ => {
                    return Err(self.error("expected a clause (where, last, order by, limit, …)"));
                }
            }
        }
        // `order by count` is parsed against the metrics, which may come later in the text.
        if !self.deferred.is_empty() {
            if q.group.is_none() {
                return Err(QueryError::at(
                    self.deferred[0].0,
                    "ordering by a metric needs grouping (count … by …)",
                ));
            }
            q.order = self.resolve_order(&q)?;
        }
        Ok(q)
    }

    fn datetime(&mut self) -> Result<u64, QueryError> {
        let (w, pos) = self.word("a time such as 2026-10-01T12:00Z")?;
        parse_utc(&w).ok_or_else(|| {
            QueryError::at(
                pos,
                format!("{w} isn't a UTC time (2026-10-01 or 2026-10-01T12:00Z)"),
            )
        })
    }

    fn field(&mut self) -> Result<FieldId, QueryError> {
        let (name, pos) = self.word("a field")?;
        fields::by_name(self.entity, &name).ok_or_else(|| {
            let hint = match fields::suggest(self.entity, &name) {
                Some(s) => format!(": did you mean {s}?"),
                None => String::new(),
            };
            QueryError::at(
                pos,
                format!("{} have no field {name}{hint}", self.entity.name()),
            )
        })
    }

    fn expr(&mut self) -> Result<Filter, QueryError> {
        let mut items = vec![self.and_expr()?];
        while self.eat_word("or") {
            items.push(self.and_expr()?);
        }
        Ok(if items.len() == 1 {
            items.pop().unwrap()
        } else {
            Filter::Or(items)
        })
    }

    fn and_expr(&mut self) -> Result<Filter, QueryError> {
        let mut items = vec![self.not_expr()?];
        while self.eat_word("and") {
            items.push(self.not_expr()?);
        }
        Ok(if items.len() == 1 {
            items.pop().unwrap()
        } else {
            Filter::And(items)
        })
    }

    fn not_expr(&mut self) -> Result<Filter, QueryError> {
        if self.eat_word("not") {
            return Ok(Filter::Not(Box::new(self.not_expr()?)));
        }
        if self.eat_sym("(") {
            let inner = self.expr()?;
            self.expect_sym(")")?;
            return Ok(inner);
        }
        self.condition().map(Filter::Cond)
    }

    fn condition(&mut self) -> Result<Condition, QueryError> {
        self.cond_positions.push(self.pos());
        let field = self.field()?;
        let kind = field.kind();
        let cond = |op, value| Condition {
            field,
            op,
            value: coerce(value, kind),
        };
        // A bare bool field stands for `field = true`.
        let next_is_op = matches!(self.peek(), Some(Tok::Sym(s)) if ["=", "!=", "<", "<=", ">", ">="].contains(s));
        let continues = matches!(self.peek(), Some(Tok::Word(w))
            if ["between", "in", "not", "contains", "starts_with", "is"].contains(&w.to_ascii_lowercase().as_str()));
        if kind == FieldKind::Bool && !next_is_op && !continues {
            return Ok(cond(Op::Eq, Value::Bool(true)));
        }
        if let Some(Tok::Sym(sym)) = self.peek() {
            let op = match *sym {
                "=" => Op::Eq,
                "!=" => Op::Ne,
                "<" => Op::Lt,
                "<=" => Op::Le,
                ">" => Op::Gt,
                ">=" => Op::Ge,
                _ => return Err(self.error("expected an operator")),
            };
            self.idx += 1;
            let value = self.value()?;
            return Ok(cond(op, value));
        }
        let (word, pos) = self.word("an operator")?;
        match word.to_ascii_lowercase().as_str() {
            "between" => {
                let low = self.value()?;
                self.expect_word("and")?;
                let high = self.value()?;
                Ok(cond(Op::Between, Value::List(vec![low, high])))
            }
            "in" => Ok(cond(Op::In, self.list()?)),
            "not" => {
                self.expect_word("in")?;
                Ok(cond(Op::NotIn, self.list()?))
            }
            "contains" => Ok(cond(Op::Contains, self.value()?)),
            "starts_with" => Ok(cond(Op::StartsWith, self.value()?)),
            "is" => {
                let negated = self.eat_word("not");
                if self.eat_word("null") {
                    return Ok(cond(
                        if negated { Op::IsNotNull } else { Op::IsNull },
                        Value::Null,
                    ));
                }
                if self.eat_word("true") {
                    return Ok(cond(
                        if negated { Op::Ne } else { Op::Eq },
                        Value::Bool(true),
                    ));
                }
                if self.eat_word("false") {
                    return Ok(cond(
                        if negated { Op::Ne } else { Op::Eq },
                        Value::Bool(false),
                    ));
                }
                Err(self.error("expected null, true or false after is"))
            }
            other => Err(QueryError::at(
                pos,
                format!(
                    "expected an operator (=, !=, <, >, between, in, contains, is), found {other}"
                ),
            )),
        }
    }

    fn list(&mut self) -> Result<Value, QueryError> {
        self.expect_sym("(")?;
        self.list_depth += 1;
        let mut items = vec![self.value()?];
        while self.eat_sym(",") {
            items.push(self.value()?);
        }
        self.list_depth -= 1;
        self.expect_sym(")")?;
        Ok(Value::List(items))
    }

    /// One literal; a number followed by `kas`/`sompi` is an amount.
    fn value(&mut self) -> Result<Value, QueryError> {
        match self.tokens.get(self.idx).cloned() {
            Some(Token {
                tok: Tok::Str(s), ..
            }) => {
                self.idx += 1;
                Ok(Value::Text(s))
            }
            Some(Token {
                tok: Tok::Word(w),
                pos,
            }) => {
                self.idx += 1;
                let w = strip_digit_separators(&w).into_owned();
                if is_number(&w) {
                    // `1,000` outside a list: the comma isn't a separator here.
                    if self.list_depth == 0
                        && self.peek_sym(",")
                        && let Some(Token {
                            tok: Tok::Word(next),
                            ..
                        }) = self.tokens.get(self.idx + 1)
                        && is_digits(next)
                    {
                        return Err(QueryError::at(
                            self.pos(),
                            format!("thousands separators aren't used: write {w}{next}"),
                        ));
                    }
                    for (unit, scale) in [("kas", 100_000_000i64), ("sompi", 1)] {
                        if self.eat_word(unit) {
                            return amount(&w, scale).map_err(|e| QueryError::at(pos, e.msg));
                        }
                    }
                }
                classify_word(&w).map_err(|e| QueryError::at(pos, e.msg))
            }
            _ => Err(self.error("expected a value")),
        }
    }

    fn group(&mut self) -> Result<GroupBy, QueryError> {
        let mut metrics = vec![self.metric()?];
        while self.eat_sym(",") {
            metrics.push(self.metric()?);
        }
        self.expect_word("by")?;
        let mut keys = vec![self.group_key()?];
        while self.eat_sym(",") {
            keys.push(self.group_key()?);
        }
        Ok(GroupBy { keys, metrics })
    }

    fn metric(&mut self) -> Result<Metric, QueryError> {
        let (name, pos) = self.word("a metric (count, sum(field), …)")?;
        let name = name.to_ascii_lowercase();
        if name == "count" {
            return Ok(Metric::Count);
        }
        if !METRICS.contains(&name.as_str()) {
            return Err(QueryError::at(
                pos,
                format!(
                    "expected a metric (count, count_distinct, sum, avg, min, max), found {name}"
                ),
            ));
        }
        self.expect_sym("(")?;
        let field = self.field()?;
        self.expect_sym(")")?;
        Ok(match name.as_str() {
            "count_distinct" => Metric::CountDistinct(field),
            "sum" => Metric::Sum(field),
            "avg" => Metric::Avg(field),
            "min" => Metric::Min(field),
            _ => Metric::Max(field),
        })
    }

    fn group_key(&mut self) -> Result<GroupKey, QueryError> {
        if self.peek_word("time")
            && self
                .tokens
                .get(self.idx + 1)
                .is_some_and(|t| t.tok == Tok::Sym("("))
        {
            self.idx += 2;
            let (d, pos) = self.word("a bucket width such as 1h")?;
            let ms = parse_duration_ms(&d).ok_or_else(|| {
                QueryError::at(pos, format!("{d} isn't a duration (1h, 10m, 1d)"))
            })?;
            self.expect_sym(")")?;
            return Ok(GroupKey::TimeBucket(ms));
        }
        Ok(GroupKey::Field(self.field()?))
    }

    /// `order by` keys. A metric is kept as a placeholder field and resolved once the
    /// grouping is known (`resolve_order`), since the clauses come in any order.
    fn order(&mut self) -> Result<Vec<(OrderKey, Dir)>, QueryError> {
        let mut out = Vec::new();
        loop {
            let key = if self.eat_word("bucket") {
                OrderKey::Bucket
            } else if self.peek_metric() {
                let pos = self.pos();
                let metric = self.metric()?;
                // Remembered by its text position for `resolve_order`.
                self.deferred.push((pos, metric));
                OrderKey::Metric(usize::MAX - (self.deferred.len() - 1))
            } else {
                OrderKey::Field(self.field()?)
            };
            let dir = if self.eat_word("desc") {
                Dir::Desc
            } else {
                self.eat_word("asc");
                Dir::Asc
            };
            out.push((key, dir));
            if !self.eat_sym(",") {
                break;
            }
        }
        Ok(out)
    }

    fn peek_metric(&self) -> bool {
        match self.peek() {
            Some(Tok::Word(w)) => {
                let w = w.to_ascii_lowercase();
                w == "count"
                    || (METRICS.contains(&w.as_str())
                        && self
                            .tokens
                            .get(self.idx + 1)
                            .is_some_and(|t| t.tok == Tok::Sym("(")))
            }
            _ => false,
        }
    }

    fn resolve_order(&self, q: &Query) -> Result<Vec<(OrderKey, Dir)>, QueryError> {
        let group = q.group.as_ref().expect("called with a grouping");
        q.order
            .iter()
            .map(|(key, dir)| {
                let key =
                    match key {
                        OrderKey::Metric(slot) => {
                            let (pos, metric) = &self.deferred[usize::MAX - slot];
                            let i = group.metrics.iter().position(|m| m == metric).ok_or_else(
                                || {
                                    QueryError::at(
                                        *pos,
                                        format!(
                                            "order by {} isn't one of the metrics",
                                            metric.name()
                                        ),
                                    )
                                },
                            )?;
                            OrderKey::Metric(i)
                        }
                        other => *other,
                    };
                Ok((key, *dir))
            })
            .collect()
    }
}

/// Parse the text form of a query. The result is not validated (`Query::validate`).
pub fn parse(text: &str) -> Result<Query, QueryError> {
    parse_positioned(text).map(|(q, _)| q)
}

/// [`parse`], with the byte offset each condition starts at, in the order
/// [`Query::conditions`] lists them.
fn parse_positioned(text: &str) -> Result<(Query, Vec<usize>), QueryError> {
    let tokens = tokenize(text)?;
    if tokens.is_empty() {
        return Err(QueryError::at(
            0,
            "expected tx, blocks, payouts or addresses",
        ));
    }
    let mut p = Parser::new(tokens, text);
    let q = p.query()?;
    Ok((q, p.cond_positions))
}

/// Parse and validate. A validation error about a condition carries the position of
/// that condition in the text (`tx where fee > 5` points at `fee`).
pub fn parse_valid(text: &str) -> Result<Query, QueryError> {
    let (q, positions) = parse_positioned(text)?;
    match q.validate() {
        Ok(()) => Ok(q),
        Err(e) if e.pos.is_none() => {
            for (cond, pos) in q.conditions().into_iter().zip(positions) {
                if let Err(ce) = cond.validate(q.entity) {
                    return Err(QueryError::at(pos, ce.msg));
                }
            }
            Err(e)
        }
        Err(e) => Err(e),
    }
}

// --- Printer ---

/// The canonical text form of a query.
pub fn print(q: &Query) -> String {
    let mut out = q.entity.name().to_string();
    match q.range {
        TimeRange::All => {
            if q.entity.is_timed() {
                out.push_str(" all time");
            }
        }
        TimeRange::Last(ms) => {
            out.push_str(" last ");
            out.push_str(&format_duration_ms(ms));
        }
        TimeRange::Since(t) => {
            out.push_str(" since ");
            out.push_str(&format_time(t));
        }
        TimeRange::Between(from, to) => {
            out.push_str(&format!(
                " between {} and {}",
                format_time(from),
                format_time(to)
            ));
        }
    }
    if let Some(filter) = &q.filter
        && !is_empty(filter)
    {
        out.push_str(" where ");
        print_filter(filter, &mut out, Level::Top);
    }
    if let Some(group) = &q.group {
        out.push(' ');
        out.push_str(
            &group
                .metrics
                .iter()
                .map(|m| m.name())
                .collect::<Vec<_>>()
                .join(", "),
        );
        out.push_str(" by ");
        out.push_str(
            &group
                .keys
                .iter()
                .map(|k| match k {
                    GroupKey::Field(f) => f.name().to_string(),
                    GroupKey::TimeBucket(ms) => format!("time({})", format_duration_ms(*ms)),
                })
                .collect::<Vec<_>>()
                .join(", "),
        );
    }
    if !q.order.is_empty() {
        out.push_str(" order by ");
        out.push_str(
            &q.order
                .iter()
                .map(|(key, dir)| {
                    let key = match key {
                        OrderKey::Field(f) => f.name().to_string(),
                        OrderKey::Metric(i) => q
                            .group
                            .as_ref()
                            .and_then(|g| g.metrics.get(*i))
                            .map(|m| m.name())
                            .unwrap_or_else(|| "count".to_string()),
                        OrderKey::Bucket => "bucket".to_string(),
                    };
                    match dir {
                        Dir::Asc => key,
                        Dir::Desc => format!("{key} desc"),
                    }
                })
                .collect::<Vec<_>>()
                .join(", "),
        );
    }
    if let Some(limit) = q.limit {
        out.push_str(&format!(" limit {limit}"));
    }
    if let Some(columns) = &q.columns {
        out.push_str(" select ");
        out.push_str(
            &columns
                .iter()
                .map(|f| f.name())
                .collect::<Vec<_>>()
                .join(", "),
        );
    }
    out
}

/// A filter with no condition in it (an empty group the builder left behind).
pub fn is_empty(filter: &Filter) -> bool {
    match filter {
        Filter::And(items) | Filter::Or(items) => items.iter().all(is_empty),
        Filter::Not(inner) => is_empty(inner),
        Filter::Cond(_) => false,
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Level {
    Top,
    And,
    Not,
}

fn print_filter(filter: &Filter, out: &mut String, level: Level) {
    match filter {
        Filter::And(items) => {
            let items: Vec<&Filter> = items.iter().filter(|f| !is_empty(f)).collect();
            if items.len() == 1 {
                return print_filter(items[0], out, level);
            }
            let parens = level == Level::Not;
            if parens {
                out.push('(');
            }
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(" and ");
                }
                print_filter(item, out, Level::And);
            }
            if parens {
                out.push(')');
            }
        }
        Filter::Or(items) => {
            let items: Vec<&Filter> = items.iter().filter(|f| !is_empty(f)).collect();
            if items.len() == 1 {
                return print_filter(items[0], out, level);
            }
            let parens = level != Level::Top;
            if parens {
                out.push('(');
            }
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(" or ");
                }
                print_filter(item, out, Level::Top);
            }
            if parens {
                out.push(')');
            }
        }
        Filter::Not(inner) => {
            out.push_str("not ");
            print_filter(inner, out, Level::Not);
        }
        Filter::Cond(c) => out.push_str(&print_condition(c)),
    }
}

/// One condition in the text form.
pub fn print_condition(c: &Condition) -> String {
    let name = c.field.name();
    match (c.op, &c.value) {
        (Op::Eq, Value::Bool(true)) => name.to_string(),
        (Op::IsNull, _) => format!("{name} is null"),
        (Op::IsNotNull, _) => format!("{name} is not null"),
        (Op::Between, Value::List(items)) if items.len() == 2 => {
            format!(
                "{name} between {} and {}",
                format_value(&items[0]),
                format_value(&items[1])
            )
        }
        (Op::In | Op::NotIn, Value::List(items)) => {
            let list = items
                .iter()
                .map(format_value)
                .collect::<Vec<_>>()
                .join(", ");
            format!("{name} {} ({list})", c.op.name())
        }
        (op, value) => format!("{name} {} {}", op.name(), format_value(value)),
    }
}

/// One literal in the text form.
pub fn format_value(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Int(n) => n.to_string(),
        Value::Amount(sompi) => format!("{} KAS", format_sompi_exact(*sompi)),
        Value::Float(f) => {
            let s = format!("{f:?}");
            if s.contains(['.', 'e', 'E']) || s.contains("inf") || s.contains("NaN") {
                s
            } else {
                format!("{s}.0")
            }
        }
        Value::Time(ms) => format_time(*ms),
        Value::Duration(ms) => format_duration_ms(*ms),
        Value::Hash(h) => hex(h),
        Value::Address(a) => a.clone(),
        Value::Text(t) => format!("\"{}\"", t.replace('\\', "\\\\").replace('"', "\\\"")),
        Value::Enum(e) => e.clone(),
        Value::List(items) => format!(
            "({})",
            items
                .iter()
                .map(format_value)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// A UTC time literal, with milliseconds only when there are some.
pub fn format_time(ms: u64) -> String {
    let text = format_utc(ms);
    if ms.is_multiple_of(1000) {
        text
    } else {
        format!("{}.{:03}Z", text.trim_end_matches('Z'), ms % 1000)
    }
}

/// A short description of a condition for the builder: `fee > 1.5 KAS`.
pub fn describe(c: &Condition) -> String {
    print_condition(c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::fields::Cost;
    use crate::query::saved;

    fn roundtrip(text: &str) -> Query {
        let q = parse(text).unwrap_or_else(|e| panic!("{text}: {e}"));
        q.validate().unwrap_or_else(|e| panic!("{text}: {e}"));
        let printed = print(&q);
        let again = parse(&printed).unwrap_or_else(|e| panic!("{printed}: {e}"));
        assert_eq!(again, q, "{text} → {printed}");
        assert_eq!(print(&again), printed);
        q
    }

    #[test]
    fn parses_a_full_transaction_query() {
        let q = roundtrip(
            "tx where fee_rate > 10 and protocol in (krc, KNS) and not is_coinbase \
             last 6h order by fee desc limit 200 select time, txid, fee, sender",
        );
        assert_eq!(q.entity, Entity::Transactions);
        assert_eq!(q.range, TimeRange::Last(6 * 3_600_000));
        assert_eq!(q.limit, Some(200));
        assert_eq!(q.order, vec![(OrderKey::Field(FieldId::TxFee), Dir::Desc)]);
        assert_eq!(
            q.columns,
            Some(vec![
                FieldId::TxTime,
                FieldId::TxTxid,
                FieldId::TxFee,
                FieldId::TxSender
            ])
        );
        let Filter::And(items) = q.filter.clone().unwrap() else {
            panic!()
        };
        assert_eq!(items.len(), 3);
        assert_eq!(
            items[0],
            Filter::Cond(Condition::new(
                FieldId::TxFeeRate,
                Op::Gt,
                Value::Float(10.0)
            ))
        );
        assert_eq!(
            items[1],
            Filter::Cond(Condition::new(
                FieldId::TxProtocol,
                Op::In,
                Value::List(vec![Value::Enum("krc".into()), Value::Enum("kns".into())])
            ))
        );
        // A bare bool field is `= true`.
        assert_eq!(
            items[2],
            Filter::Not(Box::new(Filter::Cond(Condition::new(
                FieldId::TxIsCoinbase,
                Op::Eq,
                Value::Bool(true)
            ))))
        );
        assert_eq!(
            print(&q),
            "tx last 6h where fee_rate > 10.0 and protocol in (krc, kns) and not is_coinbase \
             order by fee desc limit 200 select time, txid, fee, sender"
        );
    }

    #[test]
    fn parses_every_preset() {
        for preset in saved::presets() {
            let q = roundtrip(&preset.text);
            assert_eq!(q.entity, preset.query().unwrap().entity);
        }
    }

    #[test]
    fn clauses_in_any_order_print_canonically() {
        let a = roundtrip("tx limit 5 last 1h where fee > 1 KAS order by time");
        let b = roundtrip("TX WHERE fee > 1kas LAST 60m ORDER BY time ASC LIMIT 5");
        assert_eq!(a, b);
        assert_eq!(
            print(&a),
            "tx last 1h where fee > 1 KAS order by time limit 5"
        );
    }

    #[test]
    fn amounts_times_and_durations_are_exact() {
        let q = roundtrip("tx where fee between 0.00000001 KAS and 100 sompi");
        let printed = print(&q);
        assert!(
            printed.contains("between 0.00000001 KAS and 0.000001 KAS"),
            "{printed}"
        );
        let q = roundtrip("tx where time > 2026-10-01T12:00:00.250Z and block_time > 2026-10-01");
        assert!(
            print(&q).contains("2026-10-01T12:00:00.250Z and block_time > 2026-10-01T00:00:00Z")
        );
        let q = roundtrip("tx where time > 90m");
        assert!(print(&q).contains("time > 1h30m"));
        let q = roundtrip("blocks since 2026-10-01");
        assert_eq!(q.range, TimeRange::Since(1_790_812_800_000));
        let q = roundtrip("blocks between 2026-10-01 and 2026-10-02T06:00Z");
        assert_eq!(
            print(&q),
            "blocks between 2026-10-01T00:00:00Z and 2026-10-02T06:00:00Z"
        );
    }

    #[test]
    fn grouping_and_ordering_by_metric() {
        let q = roundtrip(
            "payouts last 24h count, sum(amount) by miner order by count desc, sum(amount) limit 20",
        );
        let group = q.group.as_ref().unwrap();
        assert_eq!(group.keys, vec![GroupKey::Field(FieldId::PayoutMiner)]);
        assert_eq!(
            group.metrics,
            vec![Metric::Count, Metric::Sum(FieldId::PayoutAmount)]
        );
        assert_eq!(
            q.order,
            vec![
                (OrderKey::Metric(0), Dir::Desc),
                (OrderKey::Metric(1), Dir::Asc)
            ]
        );
        // The order clause may come before the grouping.
        let q2 = roundtrip(
            "payouts order by count desc, sum(amount) limit 20 last 24h count, sum(amount) by miner",
        );
        assert_eq!(q2, q);
        let q = roundtrip("tx last 24h count by time(1h), protocol order by bucket");
        assert_eq!(
            q.group.as_ref().unwrap().keys,
            vec![
                GroupKey::TimeBucket(3_600_000),
                GroupKey::Field(FieldId::TxProtocol)
            ]
        );
        assert_eq!(q.order, vec![(OrderKey::Bucket, Dir::Asc)]);
        assert_eq!(
            parse("payouts count by miner order by avg(amount)")
                .unwrap_err()
                .msg,
            "order by avg(amount) isn't one of the metrics"
        );
        assert!(
            parse("tx order by count")
                .unwrap_err()
                .msg
                .contains("needs grouping")
        );
    }

    #[test]
    fn precedence_prints_with_parens_only_where_needed() {
        let q = roundtrip(
            "tx where (fee > 1 KAS or mass > 100) and not (is_coinbase or self_transfer) or zk",
        );
        assert_eq!(
            print(&q),
            "tx all time where (fee > 1 KAS or mass > 100) and not (is_coinbase or self_transfer) or zk"
        );
        let q = roundtrip("tx where not not fee is null and fee is not null");
        assert_eq!(
            print(&q),
            "tx all time where not not fee is null and fee is not null"
        );
        let q = roundtrip("tx where not (fee > 1 KAS and mass > 1)");
        assert_eq!(
            print(&q),
            "tx all time where not (fee > 1 KAS and mass > 1)"
        );
        // A bool field at the end of a filter takes `= false` and `!=` too.
        let q = roundtrip("tx where zk = false and not is_coinbase != true last 1h");
        assert_eq!(
            print(&q),
            "tx last 1h where zk = false and not is_coinbase != true"
        );
    }

    #[test]
    fn text_values_quote_and_escape() {
        let q = roundtrip(r#"addresses where label contains "Bin\"ance" and category = exchange"#);
        assert_eq!(
            print(&q),
            r#"addresses where label contains "Bin\"ance" and category = "exchange""#
        );
        let q = roundtrip("blocks between 2026-10-01 and 2026-10-02 where is_chain");
        assert_eq!(
            print(&q),
            "blocks between 2026-10-01T00:00:00Z and 2026-10-02T00:00:00Z where is_chain"
        );
        let q = roundtrip("blocks where miner_tag starts_with 2miners");
        assert!(print(&q).contains(r#"starts_with "2miners""#));
        assert_eq!(
            parse_value(FieldKind::Text, "hello world").unwrap(),
            Value::Text("hello world".into())
        );
        assert_eq!(
            parse_value(FieldKind::Amount, "1.5 KAS").unwrap(),
            Value::Amount(150_000_000)
        );
        assert_eq!(
            parse_value(FieldKind::Amount, "1.5kas").unwrap(),
            Value::Amount(150_000_000)
        );
        assert_eq!(
            parse_value(FieldKind::Enum(fields::PROTOCOLS), "KRC").unwrap(),
            Value::Enum("krc".into())
        );
        assert_eq!(
            parse_value(FieldKind::Float, "3").unwrap(),
            Value::Float(3.0)
        );
        assert_eq!(
            parse_value(FieldKind::Time, "24h").unwrap(),
            Value::Duration(86_400_000)
        );
        assert!(parse_value(FieldKind::Amount, "").is_err());
        assert!(parse_value(FieldKind::Amount, "1 2").is_err());
    }

    #[test]
    fn errors_carry_positions() {
        let e = parse("tx where fee >").unwrap_err();
        assert_eq!(e.pos, Some(14));
        assert!(e.msg.contains("expected a value"), "{e}");
        let e = parse("tx where fees > 1 KAS").unwrap_err();
        assert_eq!(e.pos, Some(9));
        assert!(e.msg.contains("did you mean fee?"), "{e}");
        let e = parse("blocks where miner = kaspa:q limit").unwrap_err();
        assert!(e.msg.contains("expected a number"), "{e}");
        let e = parse("utxos where x").unwrap_err();
        assert_eq!(e.pos, Some(0));
        let e = parse("tx where fee > 1 KAS where mass > 1").unwrap_err();
        assert!(e.msg.contains("given twice"), "{e}");
        let e = parse("tx where \"unterminated").unwrap_err();
        assert!(e.msg.contains("unterminated"), "{e}");
        let e = parse("tx where fee ! 1").unwrap_err();
        assert!(e.msg.contains("!="), "{e}");
        let e = parse("tx last soon").unwrap_err();
        assert!(e.msg.contains("isn't a duration"), "{e}");
        let e = parse("tx where fee > 1 KAS and").unwrap_err();
        assert!(e.msg.contains("expected a field"), "{e}");
        assert!(parse("").unwrap_err().msg.contains("expected tx"));
        let e = parse_valid("tx where fee > 5").unwrap_err();
        assert!(e.msg.contains("1.5 KAS"), "{e}");
    }

    #[test]
    fn validation_errors_point_at_their_condition() {
        // The caret goes under `fee`, the condition that fails.
        let e = parse_valid("tx where fee > 5").unwrap_err();
        assert_eq!(e.pos, Some(9), "{e}");
        let e = parse_valid("tx last 1d where mass > 1 and (fee > 5 or zk)").unwrap_err();
        assert_eq!(e.pos, Some(31), "{e}");
        assert!(e.msg.contains("1.5 KAS"), "{e}");
        let e = parse_valid("tx where not txid = abc").unwrap_err();
        assert_eq!(e.pos, Some(13), "{e}");
        assert!(e.msg.contains("64 hex characters"), "{e}");
        let e = parse_valid("tx where sender = binance").unwrap_err();
        assert_eq!(e.pos, Some(9), "{e}");
        assert!(e.msg.contains("starts with kaspa:"), "{e}");
        let e = parse_valid("tx where time > yesterday").unwrap_err();
        assert!(e.msg.contains("2026-10-01T12:00Z"), "{e}");
        // Errors about other clauses keep no position.
        let e = parse_valid("tx limit 0").unwrap_err();
        assert_eq!(e.pos, None);
        assert!(e.msg.contains("at least 1"), "{e}");
    }

    #[test]
    fn lenient_spellings_parse_or_explain() {
        let q = roundtrip("tx where mass > 1_000_000 and fee > 1_000 sompi and fee < 1_0.5 KAS");
        assert_eq!(
            print(&q),
            "tx all time where mass > 1000000 and fee > 0.00001 KAS and fee < 10.5 KAS"
        );
        assert_eq!(
            parse_value(FieldKind::Amount, "1_000kas").unwrap(),
            Value::Amount(100_000_000_000)
        );
        assert_eq!(
            roundtrip("tx where fee_rate >= 1"),
            roundtrip("tx where fee_rate>=1")
        );
        assert_eq!(
            roundtrip("tx where mass == 1"),
            roundtrip("tx where mass = 1")
        );
        assert_eq!(
            roundtrip("tx where mass <> 1"),
            roundtrip("tx where mass != 1")
        );
        assert_eq!(
            roundtrip("addresses where label contains 'Bin\\'ance'"),
            roundtrip(r#"addresses where label contains "Bin'ance""#)
        );
        assert_eq!(
            roundtrip("tx where mass in (1_000, 2_000)"),
            roundtrip("tx where mass in (1000, 2000)")
        );
        let e = parse("tx where mass > 1,000").unwrap_err();
        assert_eq!(e.pos, Some(17), "{e}");
        assert!(e.msg.contains("write 1000"), "{e}");
        let e = parse("tx where fee > 1e6 KAS").unwrap_err();
        assert!(
            e.msg.contains("write the amount in full: 1000000 KAS"),
            "{e}"
        );
        let e = parse("tx where fee > 2.5e3kas").unwrap_err();
        assert!(e.msg.contains("2500 KAS"), "{e}");
        let e = parse("tx where zk && mass > 1").unwrap_err();
        assert_eq!(e.pos, Some(12));
        assert!(e.msg.contains("use and"), "{e}");
        let e = parse("tx where zk || mass > 1").unwrap_err();
        assert!(e.msg.contains("use or"), "{e}");
        let e = parse("tx # the lot").unwrap_err();
        assert!(e.msg.contains("comments"), "{e}");
        let e = parse("tx where label = 'open").unwrap_err();
        assert!(e.msg.contains("unterminated"), "{e}");
        // A float field still takes an exponent, and `_` elsewhere is a word character.
        assert_eq!(classify_word("1e3").unwrap(), Value::Float(1000.0));
        assert_eq!(
            classify_word("fee_rate").unwrap(),
            Value::Enum("fee_rate".into())
        );
        assert_eq!(classify_word("1_").unwrap(), Value::Enum("1_".into()));
    }

    #[test]
    fn unknown_field_names_an_entity() {
        let e = parse("blocks where fee > 1 KAS").unwrap_err();
        assert!(e.msg.starts_with("blocks have no field fee"), "{e}");
    }

    #[test]
    fn classify_words() {
        assert_eq!(classify_word("12").unwrap(), Value::Int(12));
        assert_eq!(classify_word("-3.5").unwrap(), Value::Float(-3.5));
        assert_eq!(classify_word("1e3").unwrap(), Value::Float(1000.0));
        assert_eq!(classify_word("100sompi").unwrap(), Value::Amount(100));
        assert_eq!(classify_word("2KAS").unwrap(), Value::Amount(200_000_000));
        assert_eq!(
            classify_word(&"ab".repeat(32)).unwrap(),
            Value::Hash([0xab; 32])
        );
        assert_eq!(
            classify_word("kaspa:qq").unwrap(),
            Value::Address("kaspa:qq".into())
        );
        assert_eq!(
            classify_word("2026-01-01").unwrap(),
            Value::Time(1_767_225_600_000)
        );
        assert_eq!(classify_word("7d").unwrap(), Value::Duration(604_800_000));
        assert_eq!(classify_word("TRUE").unwrap(), Value::Bool(true));
        assert_eq!(classify_word("null").unwrap(), Value::Null);
        assert_eq!(classify_word("krc").unwrap(), Value::Enum("krc".into()));
        assert!(classify_word("0.123456789kas").is_err());
    }

    // --- Round-trip property ---

    struct Rng(u64);

    impl Rng {
        fn next(&mut self, m: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % m
        }

        fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
            &items[self.next(items.len() as u64) as usize]
        }
    }

    fn random_value(rng: &mut Rng, kind: FieldKind, list: bool) -> Value {
        let one = |rng: &mut Rng| match kind {
            FieldKind::Hash | FieldKind::HashList => Value::Hash([rng.next(256) as u8; 32]),
            FieldKind::Address | FieldKind::AddressList => {
                Value::Address(format!("kaspa:qq{}", rng.next(1000)))
            }
            FieldKind::Text => Value::Text(
                ["Binance", "a \"quoted\" one", "back\\slash", "x"][rng.next(4) as usize]
                    .to_string(),
            ),
            FieldKind::Int => Value::Int(rng.next(1_000_000) as i64 - 500),
            FieldKind::Amount => Value::Amount(rng.next(10_000_000_000) as i64 - 5),
            FieldKind::Float => Value::Float([0.5, 10.0, -2.25, 1e21, 3.0][rng.next(5) as usize]),
            FieldKind::Time => {
                if rng.next(2) == 0 {
                    Value::Time(1_700_000_000_000 + rng.next(100_000_000_000))
                } else {
                    Value::Duration(1 + rng.next(10_000_000_000))
                }
            }
            FieldKind::Bool => Value::Bool(rng.next(2) == 0),
            FieldKind::Enum(options) => Value::Enum(rng.pick(options).to_string()),
        };
        if list {
            let n = 1 + rng.next(3) as usize;
            Value::List((0..n).map(|_| one(rng)).collect())
        } else {
            one(rng)
        }
    }

    fn random_condition(rng: &mut Rng, entity: Entity) -> Condition {
        loop {
            let spec = *rng.pick(&fields::for_entity(entity).collect::<Vec<_>>());
            if spec.cost == Cost::Node {
                continue;
            }
            let op = *rng.pick(spec.kind.operators());
            let value = match op {
                Op::IsNull | Op::IsNotNull => Value::Null,
                Op::Between => Value::List(vec![
                    random_value(rng, spec.kind, false),
                    random_value(rng, spec.kind, false),
                ]),
                Op::In | Op::NotIn => random_value(rng, spec.kind, true),
                _ => random_value(rng, spec.kind, false),
            };
            return Condition::new(spec.id, op, value);
        }
    }

    fn random_filter(rng: &mut Rng, entity: Entity, depth: u8) -> Filter {
        match if depth == 0 { 0 } else { rng.next(4) } {
            0 => Filter::Cond(random_condition(rng, entity)),
            1 => Filter::Not(Box::new(random_filter(rng, entity, depth - 1))),
            2 => Filter::And(
                (0..2 + rng.next(2))
                    .map(|_| {
                        loop {
                            let f = random_filter(rng, entity, depth - 1);
                            if !matches!(f, Filter::And(_)) {
                                break f;
                            }
                        }
                    })
                    .collect(),
            ),
            _ => Filter::Or(
                (0..2 + rng.next(2))
                    .map(|_| {
                        loop {
                            let f = random_filter(rng, entity, depth - 1);
                            if !matches!(f, Filter::Or(_)) {
                                break f;
                            }
                        }
                    })
                    .collect(),
            ),
        }
    }

    fn random_query(rng: &mut Rng) -> Query {
        let entity = *rng.pick(&Entity::ALL);
        let fields: Vec<FieldId> = fields::for_entity(entity).map(|f| f.id).collect();
        let numeric: Vec<FieldId> = fields
            .iter()
            .copied()
            .filter(|f| {
                matches!(
                    f.kind(),
                    FieldKind::Int | FieldKind::Amount | FieldKind::Float
                ) && f.cost() != Cost::Node
            })
            .collect();
        let group = if rng.next(3) == 0 {
            let mut keys = vec![GroupKey::Field(*rng.pick(&fields))];
            if entity.is_timed() && rng.next(2) == 0 {
                keys.push(GroupKey::TimeBucket(60_000 * (1 + rng.next(120))));
            }
            let mut metrics: Vec<Metric> = Vec::new();
            for _ in 0..1 + rng.next(3) {
                let m = match rng.next(6) {
                    0 => Metric::Count,
                    1 => Metric::CountDistinct(*rng.pick(&fields)),
                    2 => Metric::Sum(*rng.pick(&numeric)),
                    3 => Metric::Avg(*rng.pick(&numeric)),
                    4 => Metric::Min(*rng.pick(&numeric)),
                    _ => Metric::Max(*rng.pick(&numeric)),
                };
                // `order by sum(x)` names the first such metric.
                if !metrics.contains(&m) {
                    metrics.push(m);
                }
            }
            Some(GroupBy { keys, metrics })
        } else {
            None
        };
        let order = (0..rng.next(3))
            .map(|_| {
                let dir = if rng.next(2) == 0 {
                    Dir::Asc
                } else {
                    Dir::Desc
                };
                let key = match &group {
                    Some(g) => match rng.next(3) {
                        0 => OrderKey::Metric(rng.next(g.metrics.len() as u64) as usize),
                        1 if g.keys.iter().any(|k| matches!(k, GroupKey::TimeBucket(_))) => {
                            OrderKey::Bucket
                        }
                        _ => match g.keys[0] {
                            GroupKey::Field(f) => OrderKey::Field(f),
                            GroupKey::TimeBucket(_) => OrderKey::Bucket,
                        },
                    },
                    None => loop {
                        let f = *rng.pick(&fields);
                        if f.cost() != Cost::Node {
                            break OrderKey::Field(f);
                        }
                    },
                };
                (key, dir)
            })
            .collect();
        let columns = if group.is_none() && rng.next(3) == 0 {
            Some((0..1 + rng.next(4)).map(|_| *rng.pick(&fields)).collect())
        } else {
            None
        };
        Query {
            entity,
            filter: (rng.next(4) != 0).then(|| random_filter(rng, entity, 3)),
            range: match if entity.is_timed() { rng.next(4) } else { 0 } {
                0 => TimeRange::All,
                1 => TimeRange::Last(1 + rng.next(1_000_000_000)),
                2 => TimeRange::Since(1_700_000_000_000 + rng.next(1_000_000)),
                _ => TimeRange::Between(
                    1_700_000_000_000,
                    1_700_000_000_000 + 1 + rng.next(1_000_000),
                ),
            },
            group,
            order,
            limit: (rng.next(2) == 0).then(|| 1 + rng.next(1000) as usize),
            columns,
        }
    }

    #[test]
    fn roundtrip_property() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let mut valid = 0;
        for _ in 0..500 {
            let q = random_query(&mut rng);
            if q.validate().is_err() {
                continue;
            }
            valid += 1;
            let printed = print(&q);
            let parsed = parse(&printed).unwrap_or_else(|e| panic!("{printed}\n{e}"));
            assert_eq!(parsed, q, "\n{printed}");
            parsed
                .validate()
                .unwrap_or_else(|e| panic!("{printed}\n{e}"));
        }
        assert!(valid > 300, "{valid} valid queries");
    }
}
