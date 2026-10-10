//! CSV and JSON exports of what the index knows: an address's transactions and a flow
//! graph; and of a block as the Explorer shows it (JSON). Files go under `~/.x4kas/exports/` (`export_path`); the CLI prints the same
//! text to stdout. CSV is plain RFC 4180 (quoted only where needed), one row per
//! transaction or per flow edge, with amounts both in sompi and in KAS.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use super::IndexStore;
use super::query::{self, FlowGraph, TxRow};
use super::records::AddrId;
use crate::config;
use crate::explorer::BlockView;
use crate::format::{format_sompi_exact, format_utc, now_ms};
use crate::labels::LabelBook;
use crate::query::exec::{Cell, ResultSet};
use crate::query::fields::FieldKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExportFormat {
    Csv,
    Json,
}

impl ExportFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Csv => "csv",
            Self::Json => "json",
        }
    }
}

impl fmt::Display for ExportFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.extension())
    }
}

impl FromStr for ExportFormat {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "csv" => Ok(Self::Csv),
            "json" => Ok(Self::Json),
            other => Err(anyhow!("unknown format {other:?}: use csv or json")),
        }
    }
}

/// Transactions an export stops at, so a busy exchange address can't fill the disk.
pub const EXPORT_MAX_ROWS: usize = 250_000;
/// Rows fetched per page while collecting.
const EXPORT_PAGE: usize = 2_000;

/// Every indexed transaction of `id`, newest first, up to `max` rows.
pub fn all_transactions(store: &IndexStore, id: AddrId, max: usize) -> Result<Vec<TxRow>> {
    let mut rows = Vec::new();
    let mut before = None;
    loop {
        let page = query::transactions(store, id, before, EXPORT_PAGE.min(max - rows.len()))?;
        rows.extend(page.items);
        before = page.next;
        if before.is_none() || rows.len() >= max {
            return Ok(rows);
        }
    }
}

/// An address's transactions as JSON: `{"address", "transactions": [TxRow…]}`.
pub fn transactions_json(address: &str, rows: &[TxRow]) -> Result<String> {
    #[derive(Serialize)]
    struct Out<'a> {
        address: &'a str,
        transactions: &'a [TxRow],
    }
    Ok(serde_json::to_string_pretty(&Out {
        address,
        transactions: rows,
    })?)
}

pub const TRANSACTIONS_CSV_HEADER: &str = "address,txid,time_ms,time_utc,daa_score,accepting_block,delta_sompi,delta_kas,fee_sompi,is_coinbase,protocol,inputs,outputs";

/// An address's transactions as CSV, one row each, headed by `TRANSACTIONS_CSV_HEADER`.
pub fn transactions_csv(address: &str, rows: &[TxRow]) -> String {
    let mut out = String::with_capacity(64 + rows.len() * 200);
    out.push_str(TRANSACTIONS_CSV_HEADER);
    out.push('\n');
    for row in rows {
        let fields = [
            csv_field(address),
            row.txid.clone(),
            row.time_ms.to_string(),
            format_utc(row.time_ms),
            row.daa_score.to_string(),
            row.accepting_block.clone(),
            row.delta.to_string(),
            kas(row.delta),
            row.fee.map(|f| f.to_string()).unwrap_or_default(),
            row.is_coinbase.to_string(),
            row.protocol
                .map(|p| csv_field(&format!("{p:?}")))
                .unwrap_or_default(),
            row.input_count.to_string(),
            row.output_count.to_string(),
        ];
        out.push_str(&fields.join(","));
        out.push('\n');
    }
    out
}

/// A flow graph as JSON (`FlowGraph`: nodes and edges, `via` on collapsed edges).
pub fn flows_json(graph: &FlowGraph) -> Result<String> {
    Ok(serde_json::to_string_pretty(graph)?)
}

/// A block as the Explorer shows it (`BlockView`: header, DAG standing, miner, its
/// transactions with the index's acceptance and fees) as JSON.
pub fn block_json(view: &BlockView) -> Result<String> {
    Ok(serde_json::to_string_pretty(view)?)
}

/// A query result as CSV: one column per result column, amounts as a `_sompi` and a
/// `_kas` column, times as `_ms` and `_utc`, lists joined by `;`.
pub fn result_csv(result: &ResultSet) -> String {
    let mut header: Vec<String> = Vec::new();
    for c in &result.columns {
        match c.kind {
            FieldKind::Amount => {
                header.push(format!("{}_sompi", c.name));
                header.push(format!("{}_kas", c.name));
            }
            FieldKind::Time => {
                header.push(format!("{}_ms", c.name));
                header.push(format!("{}_utc", c.name));
            }
            _ => header.push(c.name.clone()),
        }
    }
    let mut out = String::with_capacity(64 + result.rows.len() * 160);
    out.push_str(
        &header
            .iter()
            .map(|h| csv_field(h))
            .collect::<Vec<_>>()
            .join(","),
    );
    out.push('\n');
    for row in &result.rows {
        let mut fields: Vec<String> = Vec::with_capacity(header.len());
        for (cell, column) in row.iter().zip(&result.columns) {
            match (column.kind, cell) {
                (FieldKind::Amount, Cell::Amount(sompi)) => {
                    fields.push(sompi.to_string());
                    fields.push(kas(*sompi));
                }
                (FieldKind::Amount, _) => {
                    fields.push(String::new());
                    fields.push(String::new());
                }
                (FieldKind::Time, Cell::Time(ms)) => {
                    fields.push(ms.to_string());
                    fields.push(format_utc(*ms));
                }
                (FieldKind::Time, _) => {
                    fields.push(String::new());
                    fields.push(String::new());
                }
                (_, cell) => fields.push(csv_field(&cell.text())),
            }
        }
        out.push_str(&fields.join(","));
        out.push('\n');
    }
    out
}

/// A query result as JSON: the query's text, the columns, and one object per row
/// (amounts in sompi, times in milliseconds, hashes in hex).
pub fn result_json(result: &ResultSet, text: &str) -> Result<String> {
    let columns: Vec<serde_json::Value> = result
        .columns
        .iter()
        .map(|c| {
            serde_json::json!({
                "name": c.name,
                "label": c.label,
                "kind": format!("{:?}", c.kind).to_lowercase().split('(').next().unwrap_or("").to_string(),
            })
        })
        .collect();
    let rows: Vec<serde_json::Value> = result
        .rows
        .iter()
        .map(|row| {
            serde_json::Value::Object(
                row.iter()
                    .zip(&result.columns)
                    .map(|(cell, c)| (c.name.clone(), cell.to_json()))
                    .collect(),
            )
        })
        .collect();
    let out = serde_json::json!({
        "query": text,
        "entity": result.entity.name(),
        "matched": result.matched,
        "scanned": result.scanned,
        "truncated": result.truncated,
        "partial": result.partial.map(|p| p.label()),
        "window_ms": [result.window.0, result.window.1],
        "plan": result.plan,
        "columns": columns,
        "rows": rows,
    });
    Ok(serde_json::to_string_pretty(&out)?)
}

/// A short file stem for a query: `query_<name>`.
pub fn query_stem(name: &str) -> String {
    let name = name.trim();
    if name.is_empty() {
        "query".to_string()
    } else {
        format!("query_{}", name.chars().take(32).collect::<String>())
    }
}

/// A short file stem for a block: `block_<first 12 hex digits>`.
pub fn block_stem(hash: &str) -> String {
    let head: String = hash.chars().take(12).collect();
    format!("block_{head}")
}

pub const FLOWS_CSV_HEADER: &str =
    "from,from_label,to,to_label,amount_sompi,amount_kas,tx_count,hops,via";

/// A flow graph's edges as CSV, headed by `FLOWS_CSV_HEADER`; `via` lists a collapsed
/// edge's pass-through addresses separated by spaces.
pub fn flows_csv(graph: &FlowGraph, labels: &LabelBook) -> String {
    let address = |id: AddrId| -> &str {
        graph
            .nodes
            .iter()
            .find(|n| n.id == id)
            .map(|n| n.address.as_str())
            .unwrap_or_default()
    };
    let mut out = String::with_capacity(64 + graph.edges.len() * 200);
    out.push_str(FLOWS_CSV_HEADER);
    out.push('\n');
    for edge in &graph.edges {
        let (from, to) = (address(edge.from), address(edge.to));
        let fields = [
            csv_field(from),
            csv_field(labels.name(from).unwrap_or_default()),
            csv_field(to),
            csv_field(labels.name(to).unwrap_or_default()),
            edge.amount.to_string(),
            kas(i64::try_from(edge.amount).unwrap_or(i64::MAX)),
            edge.tx_count.to_string(),
            edge.hops().to_string(),
            csv_field(&edge.via.join(" ")),
        ];
        out.push_str(&fields.join(","));
        out.push('\n');
    }
    out
}

/// Sompi as an exact KAS decimal with eight places and no separators, for
/// spreadsheets: `150000000` → `1.50000000`.
fn kas(sompi: i64) -> String {
    let exact = format_sompi_exact(sompi);
    match exact.split_once('.') {
        Some((int, frac)) => format!("{int}.{frac:0<8}"),
        None => format!("{exact}.00000000"),
    }
}

/// `s` quoted when it holds a comma, quote or line break.
fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// Where exports are written: `~/.x4kas/exports/`.
pub fn export_dir() -> PathBuf {
    config::data_dir().join("exports")
}

/// A fresh file name in `export_dir()`: `<stem>-<utc time>.<ext>`.
pub fn export_path(stem: &str, format: ExportFormat) -> PathBuf {
    let stamp = format_utc(now_ms()).replace([':', '-'], "");
    let stamp = stamp.trim_end_matches('Z');
    let stem: String = stem
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    export_dir().join(format!("{stem}-{stamp}.{}", format.extension()))
}

/// Write `contents` to `path`, creating the directory.
pub fn write(path: &Path, contents: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    std::fs::write(path, contents).with_context(|| format!("write {}", path.display()))
}

/// A short file stem for an address: its prefix and last characters, e.g. `kaspa_qq12_3gujgy`.
pub fn address_stem(address: &str) -> String {
    let (prefix, body) = address.split_once(':').unwrap_or(("", address));
    let head: String = body.chars().take(4).collect();
    let tail: String = body
        .chars()
        .rev()
        .take(6)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{prefix}_{head}_{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::query::{FlowEdge, FlowNode};
    use crate::tx_inspect::TransactionProtocol;

    fn row(delta: i64) -> TxRow {
        TxRow {
            txid: "ab".repeat(32),
            time_ms: 1_759_926_896_123,
            daa_score: 42,
            accepting_block: "cd".repeat(32),
            delta,
            fee: Some(1_000),
            is_coinbase: false,
            protocol: Some(TransactionProtocol::Kasplex),
            input_count: 1,
            output_count: 2,
        }
    }

    #[test]
    fn result_csv_and_json_render_cells() {
        use crate::query::Entity;
        use crate::query::exec::{Column, ColumnSource};
        use crate::query::fields::FieldId;
        let result = ResultSet {
            entity: Entity::Transactions,
            columns: vec![
                Column {
                    name: "time".into(),
                    label: "Time".into(),
                    kind: FieldKind::Time,
                    source: ColumnSource::Field(FieldId::TxTime),
                },
                Column {
                    name: "fee".into(),
                    label: "Fee".into(),
                    kind: FieldKind::Amount,
                    source: ColumnSource::Field(FieldId::TxFee),
                },
                Column {
                    name: "label".into(),
                    label: "Label".into(),
                    kind: FieldKind::Text,
                    source: ColumnSource::Field(FieldId::TxLabel),
                },
            ],
            rows: vec![
                vec![
                    Cell::Time(1_759_926_896_000),
                    Cell::Amount(150_000_000),
                    Cell::List(vec![Cell::Text("a, b".into()), Cell::Text("c".into())]),
                ],
                vec![Cell::Null, Cell::Null, Cell::Null],
            ],
            matched: 2,
            scanned: 10,
            truncated: false,
            partial: None,
            elapsed: std::time::Duration::from_millis(5),
            window: (0, u64::MAX),
            plan: "test".into(),
            primary: None,
            time_column: Some(0),
            balances_pending: false,
            notes: Vec::new(),
        };
        let csv = result_csv(&result);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines[0], "time_ms,time_utc,fee_sompi,fee_kas,label");
        assert_eq!(
            lines[1],
            "1759926896000,2025-10-08T12:34:56Z,150000000,1.50000000,\"a, b; c\""
        );
        assert_eq!(lines[2], ",,,,");
        let json: serde_json::Value =
            serde_json::from_str(&result_json(&result, "tx").unwrap()).unwrap();
        assert_eq!(json["query"], "tx");
        assert_eq!(json["rows"][0]["fee"], 150_000_000);
        assert_eq!(json["rows"][0]["label"][0], "a, b");
        assert_eq!(json["rows"][1]["fee"], serde_json::Value::Null);
        assert_eq!(json["columns"][1]["kind"], "amount");
        assert_eq!(query_stem(" Big fees "), "query_Big fees");
        assert_eq!(query_stem(""), "query");
    }

    #[test]
    fn transactions_csv_has_one_row_per_transaction() {
        let csv = transactions_csv("kaspa:qq1", &[row(-150_000_000), row(5)]);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], TRANSACTIONS_CSV_HEADER);
        assert_eq!(
            lines[1],
            format!(
                "kaspa:qq1,{},1759926896123,2025-10-08T12:34:56Z,42,{},-150000000,-1.50000000,1000,false,Kasplex,1,2",
                "ab".repeat(32),
                "cd".repeat(32)
            )
        );
        assert!(lines[2].contains(",5,0.00000005,"));
        let json = transactions_json("kaspa:qq1", &[row(5)]).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["address"], "kaspa:qq1");
        assert_eq!(v["transactions"][0]["delta"], 5);
    }

    #[test]
    fn block_json_names_the_file_after_the_hash() {
        assert_eq!(block_stem(&"ab".repeat(32)), "block_abababababab");
        let view = BlockView {
            hash: "ab".repeat(32),
            version: 1,
            timestamp_ms: 1_759_926_896_123,
            bits: 0,
            nonce: 7,
            daa_score: 42,
            blue_score: 41,
            blue_work: "ff".into(),
            difficulty: None,
            parents: vec!["cd".repeat(32)],
            parent_levels: 1,
            hash_merkle_root: String::new(),
            accepted_id_merkle_root: String::new(),
            utxo_commitment: String::new(),
            pruning_point: String::new(),
            selected_parent: None,
            children: Vec::new(),
            merge_set_blues: Vec::new(),
            merge_set_reds: Vec::new(),
            is_chain_block: Some(true),
            is_header_only: false,
            miner: None,
            transactions: Vec::new(),
            reward: None,
        };
        let v: serde_json::Value = serde_json::from_str(&block_json(&view).unwrap()).unwrap();
        assert_eq!(v["hash"], "ab".repeat(32));
        assert_eq!(v["daa_score"], 42);
        assert_eq!(v["parents"][0], "cd".repeat(32));
    }

    #[test]
    fn flows_csv_quotes_labels_and_lists_via() {
        let graph = FlowGraph {
            nodes: vec![
                FlowNode {
                    id: 1,
                    address: "kaspa:a".into(),
                    volume: 0,
                    hop: 0,
                },
                FlowNode {
                    id: 2,
                    address: "kaspa:b".into(),
                    volume: 0,
                    hop: 2,
                },
            ],
            edges: vec![FlowEdge {
                from: 1,
                to: 2,
                amount: 250_000_000,
                tx_count: 3,
                via: vec!["kaspa:x".into(), "kaspa:y".into()],
            }],
        };
        let mut labels = LabelBook::default();
        labels.set_heuristic("kaspa:b", "Pool, \"big\"");
        let csv = flows_csv(&graph, &labels);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines[0], FLOWS_CSV_HEADER);
        assert_eq!(
            lines[1],
            "kaspa:a,,kaspa:b,\"Pool, \"\"big\"\"\",250000000,2.50000000,3,3,kaspa:x kaspa:y"
        );
    }

    #[test]
    fn export_paths_are_safe_file_names() {
        let path = export_path(&address_stem("kaspa:qq12abcdef3gujgy"), ExportFormat::Csv);
        let name = path.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("kaspa_qq12_3gujgy-"));
        assert!(name.ends_with(".csv"));
        assert!(
            name.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.T".contains(c))
        );
        assert_eq!(path.parent().unwrap(), export_dir());
        assert_eq!("CSV".parse::<ExportFormat>().unwrap(), ExportFormat::Csv);
        assert!("xml".parse::<ExportFormat>().is_err());
    }
}
