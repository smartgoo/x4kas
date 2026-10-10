//! `x4kas-cli query …`: run queries over the address index (`x4kas_core::query`) in
//! their text form, keep them in the saved queries the GUI shows, list what can be
//! asked, and `watch`: keep the index current from a node (as `index run` does) and
//! print every new row the watched queries find. `run` and `explain` open the index on
//! disk as it is (`IndexStore::open_existing`: never rebuilt, and not while another
//! x4kas process holds it); `list`, `save`, `rm` and `fields` need neither a node nor
//! the index.
//!
//! Exit codes of `run`: 0 when the result is complete, 1 when the run failed (no index,
//! a locked store, a read error), 2 when the text doesn't parse or validate, 3 when the
//! run stopped early (the scan cap, the time budget) and the rows are only part of what
//! matches.

use std::fmt;
use std::io::{IsTerminal, Write};
use std::str::FromStr;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use clap::Subcommand;

use x4kas_core::format::{format_number, format_utc, now_ms, shorten_middle};
use x4kas_core::index::IndexStore;
use x4kas_core::index::export;
use x4kas_core::labels::LabelBook;
use x4kas_core::query::exec::{self, Cell, Inputs, ResultSet, RunControl, Source};
use x4kas_core::query::fields::{self, Cost, FieldKind};
use x4kas_core::query::saved::{QueryWatch, SavedQueries, SavedQuery, presets};
use x4kas_core::query::watch::POLL;
use x4kas_core::query::{Entity, Query, QueryError, TimeRange, text};
use x4kas_core::watch::Watchlist;

use crate::index::{progress_line, start_pipeline, stop_pipeline};

/// The examples under `x4kas-cli query --help`.
pub const AFTER_HELP: &str = "\
Examples:
  x4kas-cli query run \"tx last 1d where fee > 1 KAS order by fee desc limit 20\"
  x4kas-cli query run \"payouts last 1d count, sum(amount) by miner order by count desc\"
  x4kas-cli query run --saved \"Large transfers\" --format csv > large.csv

`query fields` lists what each entity can be asked; `query list` the saved queries
and templates. The index is built by the GUI (with a direct node) or `index run`.";

#[derive(Subcommand, Debug, Clone, PartialEq)]
pub enum QueryCommand {
    /// Run a query, given as text or by its saved name, and print the rows
    ///
    /// Exit codes: 0 when the result is complete, 1 when the run failed, 2 when the
    /// text doesn't parse or validate (the error points at the place), 3 when the run
    /// stopped early (the scan cap or time budget: the rows are part of what matches;
    /// narrow the time range, add an address, txid or block condition, or set a limit).
    /// Rows are capped at 10,000; the table shortens long ids, --format csv or json
    /// writes them in full.
    Run {
        /// The query, quoted: "tx last 1d where fee > 1 KAS order by fee desc limit 50"
        #[arg(num_args = 1.., conflicts_with = "saved", value_name = "TEXT")]
        text: Vec<String>,
        /// Run a saved query or template by name (or a template's slug, e.g.
        /// large-transfers; see `query list`)
        #[arg(long, value_name = "NAME")]
        saved: Option<String>,
        /// Output format: table (ids shortened), json or csv (full values)
        #[arg(long, default_value = "table")]
        format: OutputFormat,
        /// Override the query's row limit (at most 10,000)
        #[arg(long, value_name = "N")]
        limit: Option<usize>,
        /// No progress on stderr
        #[arg(long)]
        quiet: bool,
    },
    /// The saved queries and the built-in templates
    List,
    /// Save a query under a name (replacing one with the same name)
    ///
    /// Writes ~/.x4kas/queries.toml, the file the GUI's Query tab shows; takes effect
    /// in the GUI on its next launch.
    Save {
        /// The name to save it under (case-insensitive; replaces a query of that name)
        name: String,
        /// The query text, quoted: "tx last 1d where fee > 1 KAS"
        #[arg(num_args = 1.., required = true, value_name = "TEXT")]
        text: Vec<String>,
        /// A line about what the query is for, shown in the GUI's sidebar
        #[arg(long, default_value = "", value_name = "TEXT")]
        description: String,
        /// Show it first in the GUI's sidebar
        #[arg(long)]
        pin: bool,
        /// Watch it: re-run it as the index moves (in the GUI and `query watch`),
        /// every SECS seconds at most (30 without a value, never less)
        #[arg(long, num_args = 0..=1, default_missing_value = "30", value_name = "SECS")]
        watch: Option<u64>,
    },
    /// Delete a saved query by name
    Rm {
        /// The saved query's name (case-insensitive)
        name: String,
    },
    /// The fields of an entity (tx, blocks, payouts, addresses), with their kinds and
    /// operators; all of them without one
    Fields {
        /// tx, blocks, payouts or addresses
        entity: Option<String>,
    },
    /// Check a query and show its canonical text and how it would read the index
    ///
    /// The text is printed on stdout with the plan (what the index reads, over how
    /// many slabs). Needs the index on disk for the plan; without it only the text.
    Explain {
        /// The query text, quoted
        #[arg(num_args = 1.., required = true, value_name = "TEXT")]
        text: Vec<String>,
    },
    /// Keep the index current from a node (needs --url, like `index run`) and print one
    /// JSON line per new row the watched queries find, until Ctrl+C
    ///
    /// Each line is {time_ms, query_id, query_name, row: {column: value, …}, primary}.
    /// Rows indexed from now on raise events; the backfill doesn't, and an address
    /// query's first run only learns what is there.
    Watch {
        /// Saved queries or templates to watch (by name or template slug); none means
        /// every query saved as watched
        names: Vec<String>,
        /// How many hours back to fill in on start; 0 for everything the node retains
        #[arg(long, default_value = "24")]
        backfill_hours: f64,
        /// Print the indexer's progress on stderr this often, in seconds; 0 for none
        #[arg(long, default_value = "5")]
        progress: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Table,
    Json,
    Csv,
}

impl FromStr for OutputFormat {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "table" => Ok(Self::Table),
            "json" => Ok(Self::Json),
            "csv" => Ok(Self::Csv),
            other => Err(anyhow!("unknown format {other:?}: use table, json or csv")),
        }
    }
}

impl fmt::Display for OutputFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Table => "table",
            Self::Json => "json",
            Self::Csv => "csv",
        })
    }
}

/// Exit code for a query that doesn't parse or validate.
const USAGE_EXIT: i32 = 2;
/// Exit code for a run that stopped early, so the rows are part of what matches.
const PARTIAL_EXIT: i32 = 3;
/// What to do when a run stopped early.
const PARTIAL_ADVICE: &str =
    "narrow the time range, add an address/txid/block condition, or set a limit";

/// The exit code a finished run ends with: `PARTIAL_EXIT` when it stopped early.
fn exit_code(result: &ResultSet) -> i32 {
    if result.partial.is_some() {
        PARTIAL_EXIT
    } else {
        0
    }
}

/// Whether running `q` reads every row of the index: a time scan over all time with no
/// limit to stop at (the one shape worth a warning before it starts).
fn scans_everything(q: &Query, source: &Source) -> bool {
    q.range == TimeRange::All
        && (q.limit.is_none() || q.group.is_some())
        && matches!(
            source,
            Source::TxTimeScan | Source::BlockTimeScan | Source::PayoutScan
        )
}

pub async fn run(url: Option<&str>, network: &str, cmd: QueryCommand) -> Result<()> {
    match cmd {
        QueryCommand::Watch {
            names,
            backfill_hours,
            progress,
        } => {
            let url = url.ok_or_else(|| {
                anyhow!("query watch needs a direct node: pass --url (the resolver won't do)")
            })?;
            let backfill =
                (backfill_hours > 0.0).then(|| Duration::from_secs_f64(backfill_hours * 3600.0));
            watch(url, network, names, backfill, progress).await
        }
        other => run_offline(network, other),
    }
}

fn run_offline(network: &str, cmd: QueryCommand) -> Result<()> {
    match cmd {
        QueryCommand::Watch { .. } => unreachable!("handled by run"),
        QueryCommand::Run {
            text,
            saved,
            format,
            limit,
            quiet,
        } => {
            let text = match saved {
                Some(name) => find_saved(&SavedQueries::load()?, &name)?.text,
                None => text.join(" "),
            };
            if text.trim().is_empty() {
                return Err(anyhow!("give a query, or --saved <name>"));
            }
            let mut query = parse_or_exit(&text);
            let mut clamped = false;
            if let Some(limit) = limit {
                query.limit = Some(limit);
                // `--limit 0` fails as `limit 0` in the text would.
                if let Err(e) = query.validate() {
                    report_error(&text, &e);
                    std::process::exit(USAGE_EXIT);
                }
                clamped = limit > exec::MAX_RESULT_ROWS;
            }
            let store = IndexStore::open_existing(network)?;
            if store.slabs().is_empty() {
                eprintln!(
                    "the index is empty: build it in the GUI with a direct node or \
                     `x4kas-cli index run --url …`"
                );
            }
            let labels = LabelBook::load();
            let watchlist = Watchlist::load().unwrap_or_default();
            let inputs = Inputs {
                store: &store,
                labels: &labels,
                watchlist: &watchlist,
                now_ms: now_ms(),
                prune_floor_ms: None,
                features: x4kas_core::config::IndexSettings::load()?,
            };
            if let Ok(plan) = exec::plan(&query, &store, inputs.now_ms, None)
                && scans_everything(&query, &plan.source)
            {
                eprintln!(
                    "warning: no time range and no limit: this reads every row of the \
                     index (add `last 1d`, or a limit)"
                );
            }
            let mut ctl = RunControl::with_budget(exec::QUERY_BUDGET);
            // Progress rewrites one stderr line, which only a terminal shows well.
            let shown = !quiet && std::io::stderr().is_terminal();
            if shown {
                let mut last = Instant::now();
                ctl.progress = Some(Box::new(move |p| {
                    if last.elapsed() >= Duration::from_secs(1) {
                        last = Instant::now();
                        eprint!(
                            "\rscanned {} · matched {} · {:.0}s",
                            format_number(p.scanned),
                            format_number(p.matched),
                            p.elapsed.as_secs_f64()
                        );
                        let _ = std::io::stderr().flush();
                    }
                }));
            }
            let result = exec::run(&inputs, &query, &mut ctl)?;
            if shown && result.elapsed >= Duration::from_secs(1) {
                eprintln!();
            }
            for note in &result.notes {
                eprintln!("note: {note}");
            }
            let canonical = query.to_text();
            let mut notes: Vec<&str> = Vec::new();
            if clamped {
                notes.push("limit clamped to 10,000");
            }
            match format {
                OutputFormat::Table => {
                    let (text, shortened) = render_table(&result);
                    print!("{text}");
                    if shortened {
                        notes.push("ids shortened; --format csv or json for full values");
                    }
                    eprintln!("{}", summary(&result, &notes));
                }
                OutputFormat::Json => println!("{}", export::result_json(&result, &canonical)?),
                OutputFormat::Csv => {
                    print!("{}", export::result_csv(&result));
                    eprintln!("{}", summary(&result, &notes));
                }
            }
            if let Some(partial) = result.partial {
                eprintln!("{}: {PARTIAL_ADVICE}", partial.label());
            }
            let code = exit_code(&result);
            if code != 0 {
                let _ = std::io::stdout().flush();
                std::process::exit(code);
            }
            Ok(())
        }
        QueryCommand::List => {
            let list = SavedQueries::load()?;
            let mut out = String::new();
            if list.queries.is_empty() {
                out.push_str("No saved queries (save one with `query save <name> <text>`).\n");
            } else {
                for q in list.sorted() {
                    let marks = format!(
                        "{}{}",
                        if q.pinned { "pinned " } else { "" },
                        if q.is_watched() { "watched " } else { "" }
                    );
                    out.push_str(&format!("{}  {}{}\n", q.name, marks, q.description));
                    out.push_str(&format!("    {}\n", q.text));
                    if let Err(e) = q.query() {
                        out.push_str(&format!("    (doesn't parse: {e})\n"));
                    }
                }
            }
            out.push_str("\nTemplates:\n");
            for p in presets() {
                out.push_str(&format!("{}  {}\n    {}\n", p.name, p.description, p.text));
            }
            print!("{out}");
            Ok(())
        }
        QueryCommand::Save {
            name,
            text,
            description,
            pin,
            watch,
        } => {
            let text = text.join(" ");
            let query = parse_or_exit(&text);
            let mut list = SavedQueries::load()?;
            let mut entry = match list.by_name(&name).cloned() {
                Some(existing) => existing,
                None => SavedQuery::new(&name, &description, &query),
            };
            entry.name = name.trim().to_string();
            if !description.is_empty() {
                entry.description = description;
            }
            entry.pinned |= pin;
            if let Some(secs) = watch {
                let every_secs = secs.max(QueryWatch::MIN_EVERY_SECS);
                entry.watch = Some(QueryWatch {
                    enabled: true,
                    every_secs,
                    ..entry.watch.clone().unwrap_or_default()
                });
            }
            entry.set_query(&query);
            let watched = match &entry.watch {
                Some(w) if w.enabled => format!(" (watched every {}s)", w.every_secs),
                _ => String::new(),
            };
            println!("saved {:?}: {}{watched}", entry.name, entry.text);
            list.upsert(entry);
            list.save()?;
            Ok(())
        }
        QueryCommand::Rm { name } => {
            let mut list = SavedQueries::load()?;
            let id = list
                .by_name(&name)
                .map(|q| q.id.clone())
                .ok_or_else(|| anyhow!("no saved query named {name:?}"))?;
            list.remove(&id);
            list.save()?;
            println!("removed {name:?}");
            Ok(())
        }
        QueryCommand::Fields { entity } => {
            let entities: Vec<Entity> = match entity {
                Some(name) => vec![Entity::from_name(&name).ok_or_else(|| {
                    anyhow!("unknown entity {name:?}: tx, blocks, payouts or addresses")
                })?],
                None => Entity::ALL.to_vec(),
            };
            print!("{}", fields_text(&entities));
            Ok(())
        }
        QueryCommand::Explain { text } => {
            let query = parse_or_exit(&text.join(" "));
            println!("{}", query.to_text());
            // As it is on disk: never rebuilt, and not waited for while in use.
            match IndexStore::open_existing(network) {
                Ok(store) => {
                    let plan = exec::plan(&query, &store, now_ms(), None)?;
                    println!("{}", plan.explain());
                }
                Err(e) => eprintln!("(the index couldn't be opened to count its slabs: {e:#})"),
            }
            Ok(())
        }
    }
}

/// The saved query or template `key` names (`SavedQueries::find`), or a usage error.
fn find_saved(list: &SavedQueries, key: &str) -> Result<SavedQuery> {
    list.find(key)
        .ok_or_else(|| anyhow!("no saved query or template named {key:?} (see `query list`)"))
}

/// Run the pipeline and the watched queries, printing their events as JSON lines.
async fn watch(
    url: &str,
    network: &str,
    names: Vec<String>,
    backfill: Option<Duration>,
    progress_secs: u64,
) -> Result<()> {
    let mut list = SavedQueries::load()?;
    if names.is_empty() {
        if list.watched().next().is_none() {
            return Err(anyhow!(
                "no saved query is watched: name some, or watch one in the GUI"
            ));
        }
    } else {
        let mut ids = Vec::new();
        for name in &names {
            let mut entry = find_saved(&list, name)?;
            entry.watch = Some(QueryWatch {
                enabled: true,
                ..entry.watch.clone().unwrap_or_default()
            });
            ids.push(entry.id.clone());
            list.upsert(entry);
        }
        // Only the named ones are watched this session; the file is left alone.
        for q in &mut list.queries {
            if !ids.contains(&q.id) {
                q.watch = None;
            }
        }
    }
    for q in list.watched() {
        q.query().map_err(|e| anyhow!("{}: {e}", q.name))?;
        eprintln!("watching {:?}: {}", q.name, q.text);
    }
    eprintln!(
        "events appear on stdout as JSON lines, one per row the watched queries find \
         among rows indexed from now on; the first check is in {}s. The backfill raises \
         none, and an address query's first run only learns which addresses match.",
        POLL.as_secs()
    );
    let mut pipeline = start_pipeline(url, network, backfill)?;
    pipeline.app.write().await.query.saved = list;
    x4kas_core::query::watch::start_query_watch(
        pipeline.store.clone(),
        pipeline.app.clone(),
        &mut pipeline.handles,
    );

    let mut seen = 0u64;
    let mut ticker = tokio::time::interval(Duration::from_millis(250));
    let mut progress = tokio::time::interval(Duration::from_secs(progress_secs.max(1)));
    progress.tick().await;
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = progress.tick() => {
                if progress_secs > 0 {
                    eprintln!("{}", progress_line(&*pipeline.app.read().await));
                }
            }
            _ = ticker.tick() => {
                let app = pipeline.app.read().await;
                let new = app.query.events_raised.saturating_sub(seen) as usize;
                for event in app.query.events.iter().take(new).collect::<Vec<_>>().into_iter().rev() {
                    println!("{}", serde_json::to_string(event)?);
                }
                seen = app.query.events_raised;
            }
        }
    }
    stop_pipeline(pipeline).await;
    Ok(())
}

/// Parse and validate `text`, or print the error with a caret and exit with
/// `USAGE_EXIT`.
fn parse_or_exit(text: &str) -> Query {
    match text::parse_valid(text) {
        Ok(q) => q,
        Err(e) => {
            report_error(text, &e);
            std::process::exit(USAGE_EXIT);
        }
    }
}

/// The text, a caret under the error's place, and the message, on stderr.
fn report_error(text: &str, e: &QueryError) {
    eprintln!("{text}");
    if let Some(column) = caret_column(text, e) {
        eprintln!("{}^", " ".repeat(column));
    }
    eprintln!("error: {}", e.msg);
}

/// The column the caret goes at: the error's byte offset counted in characters.
fn caret_column(text: &str, e: &QueryError) -> Option<usize> {
    let pos = e.pos?.min(text.len());
    let pos = (0..=pos).rev().find(|p| text.is_char_boundary(*p))?;
    Some(text[..pos].chars().count())
}

/// `N rows (M matched, S scanned in T) · plan`, then the `notes`.
fn summary(result: &ResultSet, notes: &[&str]) -> String {
    let mut s = format!(
        "{} row{} ({} matched, {} scanned in {:.2}s) · {}",
        format_number(result.rows.len() as u64),
        if result.rows.len() == 1 { "" } else { "s" },
        format_number(result.matched),
        format_number(result.scanned),
        result.elapsed.as_secs_f64(),
        result.plan
    );
    if result.truncated {
        s.push_str(" · more matched than shown");
    }
    if let Some(partial) = result.partial {
        s.push_str(&format!(" · {}", partial.label()));
    }
    for note in notes {
        s.push_str(" · ");
        s.push_str(note);
    }
    s
}

/// The widest a column of this kind gets in the table.
fn max_width(kind: FieldKind) -> usize {
    match kind {
        FieldKind::Hash | FieldKind::HashList => 20,
        FieldKind::Address | FieldKind::AddressList => 28,
        FieldKind::Text => 40,
        FieldKind::Time => 20,
        _ => 24,
    }
}

fn right_aligned(kind: FieldKind) -> bool {
    matches!(kind, FieldKind::Int | FieldKind::Amount | FieldKind::Float)
}

/// A cell as the table shows it: amounts in KAS, times as UTC, long ids shortened
/// (`true` when it was).
fn cell_text(cell: &Cell, kind: FieldKind) -> (String, bool) {
    let text = match cell {
        Cell::Null => "—".to_string(),
        Cell::Time(ms) => format_utc(*ms),
        Cell::List(items) => items
            .iter()
            .map(|c| cell_text(c, kind).0)
            .collect::<Vec<_>>()
            .join("; "),
        other => other.text(),
    };
    let shown = shorten_middle(&text, max_width(kind));
    let shortened = shown != text;
    (shown, shortened)
}

/// The result as a fixed-width table, a header row then one row per result row, and
/// whether any cell was shortened to fit its column.
pub fn render_table(result: &ResultSet) -> (String, bool) {
    let mut shortened = false;
    let cells: Vec<Vec<String>> = result
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .zip(&result.columns)
                .map(|(c, col)| {
                    let (text, short) = cell_text(c, col.kind);
                    shortened |= short;
                    text
                })
                .collect()
        })
        .collect();
    let widths: Vec<usize> = result
        .columns
        .iter()
        .enumerate()
        .map(|(i, col)| {
            cells
                .iter()
                .map(|row| row[i].chars().count())
                .max()
                .unwrap_or(0)
                .max(col.name.chars().count())
        })
        .collect();
    let mut out = String::new();
    let line = |out: &mut String, parts: Vec<String>| {
        out.push_str(parts.join("  ").trim_end());
        out.push('\n');
    };
    line(
        &mut out,
        result
            .columns
            .iter()
            .zip(&widths)
            .map(|(c, w)| format!("{:<w$}", c.name, w = *w))
            .collect(),
    );
    line(&mut out, widths.iter().map(|w| "-".repeat(*w)).collect());
    for row in &cells {
        line(
            &mut out,
            row.iter()
                .zip(&result.columns)
                .zip(&widths)
                .map(|((text, col), w)| {
                    if right_aligned(col.kind) {
                        format!("{text:>w$}", w = *w)
                    } else {
                        format!("{text:<w$}", w = *w)
                    }
                })
                .collect(),
        );
    }
    (out, shortened)
}

/// The field catalog of `entities` as text.
pub fn fields_text(entities: &[Entity]) -> String {
    let mut out = String::new();
    for entity in entities {
        out.push_str(&format!("{} ({})\n", entity.label(), entity.name()));
        for category in fields::categories(*entity) {
            out.push_str(&format!("  {category}\n"));
            for spec in fields::for_entity(*entity).filter(|f| f.category == category) {
                let kind = match spec.kind {
                    FieldKind::Enum(options) => format!("one of {}", options.join(", ")),
                    other => other.describe().to_string(),
                };
                let ops = if spec.cost == Cost::Node {
                    "column only".to_string()
                } else {
                    spec.kind
                        .operators()
                        .iter()
                        .map(|op| op.name())
                        .collect::<Vec<_>>()
                        .join(" ")
                };
                out.push_str(&format!("    {:<24} {kind}; {ops}\n", spec.name));
                out.push_str(&format!("    {:<24} {}\n", "", spec.doc));
            }
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Args, Command};
    use clap::Parser;
    use x4kas_core::query::exec::{Column, ColumnSource};
    use x4kas_core::query::fields::FieldId;

    fn query_command(args: &[&str]) -> QueryCommand {
        let mut full = vec!["x4kas-cli", "query"];
        full.extend(args);
        match Args::try_parse_from(full).unwrap().command {
            Command::Query { cmd } => cmd,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn args_parse_run_with_text_and_format() {
        let cmd = query_command(&["run", "tx", "last", "1d", "limit", "5", "--format", "csv"]);
        assert_eq!(
            cmd,
            QueryCommand::Run {
                text: vec![
                    "tx".into(),
                    "last".into(),
                    "1d".into(),
                    "limit".into(),
                    "5".into()
                ],
                saved: None,
                format: OutputFormat::Csv,
                limit: None,
                quiet: false,
            }
        );
        let cmd = query_command(&["run", "--saved", "Whales", "--limit", "3", "--quiet"]);
        assert!(matches!(
            cmd,
            QueryCommand::Run {
                saved: Some(ref name),
                limit: Some(3),
                quiet: true,
                format: OutputFormat::Table,
                ..
            } if name == "Whales"
        ));
        assert!(
            Args::try_parse_from(["x4kas-cli", "query", "run", "tx", "--format", "xml"]).is_err()
        );
    }

    #[test]
    fn saved_conflicts_with_text() {
        assert!(Args::try_parse_from(["x4kas-cli", "query", "run", "tx", "--saved", "x"]).is_err());
        let cmd = query_command(&[
            "save", "Big", "tx", "where", "fee", ">", "1", "KAS", "--pin",
        ]);
        assert!(matches!(
            cmd,
            QueryCommand::Save { ref name, pin: true, watch: None, .. } if name == "Big"
        ));
        // `--watch` alone means every 30 s; `--watch 120` every two minutes.
        let cmd = query_command(&["save", "Big", "tx", "--watch"]);
        assert!(matches!(
            cmd,
            QueryCommand::Save {
                watch: Some(30),
                ..
            }
        ));
        let cmd = query_command(&["save", "Big", "--watch", "120", "--", "tx"]);
        assert!(matches!(
            cmd,
            QueryCommand::Save {
                watch: Some(120),
                ..
            }
        ));
        assert!(Args::try_parse_from(["x4kas-cli", "query", "save", "Big"]).is_err());
        assert_eq!(
            query_command(&["fields"]),
            QueryCommand::Fields { entity: None }
        );
        assert!(matches!(
            query_command(&["explain", "tx"]),
            QueryCommand::Explain { .. }
        ));
        assert!(matches!(
            query_command(&["watch", "Whales", "--backfill-hours", "2"]),
            QueryCommand::Watch { ref names, progress: 5, .. } if names == &["Whales"]
        ));
        // The `query` group has examples under its help.
        let mut cmd = <Args as clap::CommandFactory>::command();
        let help = cmd
            .find_subcommand_mut("query")
            .unwrap()
            .render_long_help()
            .to_string();
        assert!(help.contains("--format csv"), "{help}");
        assert!(help.contains("payouts last 1d count"), "{help}");
        assert!(help.contains("last 1d where fee > 1 KAS"), "{help}");
    }

    #[test]
    fn exit_codes_and_scan_warning() {
        let mut result = ResultSet {
            entity: Entity::Transactions,
            columns: vec![],
            rows: vec![],
            matched: 0,
            scanned: 0,
            truncated: false,
            partial: None,
            elapsed: Duration::ZERO,
            window: (0, u64::MAX),
            plan: "p".into(),
            primary: None,
            time_column: None,
            balances_pending: false,
            notes: Vec::new(),
        };
        assert_eq!(exit_code(&result), 0);
        result.partial = Some(x4kas_core::query::exec::Partial::ScanCap);
        assert_eq!(exit_code(&result), PARTIAL_EXIT);
        assert!(
            summary(&result, &["limit clamped to 10,000"])
                .ends_with("stopped at the scan cap · limit clamped to 10,000")
        );
        let all = text::parse("tx").unwrap();
        assert!(scans_everything(&all, &Source::TxTimeScan));
        assert!(!scans_everything(&all, &Source::TxByAddress(vec![])));
        assert!(!scans_everything(
            &text::parse("tx limit 10").unwrap(),
            &Source::TxTimeScan
        ));
        assert!(!scans_everything(
            &text::parse("tx last 1d").unwrap(),
            &Source::TxTimeScan
        ));
        assert!(scans_everything(
            &text::parse("blocks count by miner limit 10").unwrap(),
            &Source::BlockTimeScan
        ));
    }

    #[test]
    fn caret_counts_characters_not_bytes() {
        let text = "tx where label = \"é\" and fees > 1";
        let e = text::parse_valid(text).unwrap_err();
        assert_eq!(e.pos, Some(text.find("fees").unwrap()));
        assert_eq!(caret_column(text, &e), Some(25));
        let e = QueryError::at(text.len() + 5, "past the end");
        assert_eq!(caret_column(text, &e), Some(text.chars().count()));
        assert_eq!(caret_column(text, &QueryError::new("nowhere")), None);
    }

    #[test]
    fn saved_lookup_takes_template_slugs() {
        let list = SavedQueries::default();
        assert_eq!(
            find_saved(&list, "large-transfers").unwrap().name,
            "Large transfers"
        );
        assert_eq!(
            find_saved(&list, "preset:mining-share").unwrap().id,
            "preset:mining-share"
        );
        assert!(find_saved(&list, "nothing").is_err());
    }

    #[test]
    fn table_renders_cells_fixed_width() {
        let result = ResultSet {
            entity: Entity::Transactions,
            columns: vec![
                Column {
                    name: "txid".into(),
                    label: "Transaction".into(),
                    kind: FieldKind::Hash,
                    source: ColumnSource::Field(FieldId::TxTxid),
                },
                Column {
                    name: "fee".into(),
                    label: "Fee".into(),
                    kind: FieldKind::Amount,
                    source: ColumnSource::Field(FieldId::TxFee),
                },
            ],
            rows: vec![
                vec![Cell::Txid([0xab; 32]), Cell::Amount(150_000_000)],
                vec![Cell::Txid([0x01; 32]), Cell::Null],
            ],
            matched: 2,
            scanned: 9,
            truncated: true,
            partial: None,
            elapsed: Duration::from_millis(10),
            window: (0, u64::MAX),
            plan: "p".into(),
            primary: Some(0),
            time_column: None,
            balances_pending: false,
            notes: Vec::new(),
        };
        let (t, shortened) = render_table(&result);
        assert!(shortened, "the ids were shortened");
        let lines: Vec<&str> = t.lines().collect();
        assert_eq!(lines[0], "txid                  fee");
        assert_eq!(lines[1], "--------------------  ---");
        assert_eq!(
            lines[2],
            "abababababab...ababab  1.5".replace(
                "abababababab...ababab",
                &shorten_middle(&"ab".repeat(32), 20)
            )
        );
        assert_eq!(
            lines[3].trim_end(),
            format!("{:<20}    —", shorten_middle(&"01".repeat(32), 20))
        );
        let s = summary(&result, &[]);
        assert!(s.starts_with("2 rows (2 matched, 9 scanned"));
        assert!(s.ends_with("more matched than shown"));
        let s = summary(
            &result,
            &["ids shortened; --format csv or json for full values"],
        );
        assert!(s.ends_with(
            "more matched than shown · ids shortened; --format csv or json for full values"
        ));
        // Short cells aren't reported as shortened.
        let mut short = result.clone();
        short.rows = vec![vec![Cell::Null, Cell::Amount(1)]];
        assert!(!render_table(&short).1);
    }

    #[test]
    fn fields_lists_every_entity() {
        let text = fields_text(&Entity::ALL);
        for entity in Entity::ALL {
            assert!(text.contains(&format!("{} ({})", entity.label(), entity.name())));
            for spec in fields::for_entity(entity) {
                assert!(text.contains(spec.name), "{}", spec.name);
            }
        }
        assert!(text.contains("column only"));
        assert!(text.contains("one of krc"));
        // A time field says both ways of writing one.
        assert!(
            text.contains("a time (2026-10-01T12:00Z) or how long ago (24h"),
            "{text}"
        );
    }

    #[test]
    fn output_format_parses() {
        assert_eq!("JSON".parse::<OutputFormat>().unwrap(), OutputFormat::Json);
        assert!("yaml".parse::<OutputFormat>().is_err());
        assert_eq!(OutputFormat::Table.to_string(), "table");
    }
}
