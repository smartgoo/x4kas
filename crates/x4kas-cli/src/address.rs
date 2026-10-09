//! `x4kas-cli address …`: read the address index built by the GUI or by
//! `x4kas-cli index run`. These commands don't connect to a node; they open the index
//! on disk, which must not be in use by another x4kas process.

use anyhow::{Result, anyhow};
use clap::{Args, Subcommand};
use serde::Serialize;

use x4kas_core::index::export::{self, ExportFormat};
use x4kas_core::index::query::{self, Cursor};
use x4kas_core::index::{IndexStore, parse_hex};
use x4kas_core::labels::LabelBook;

#[derive(Subcommand, Debug, Clone, PartialEq)]
pub enum AddressCommand {
    /// Totals for an address: transactions, received, sent, first and last seen
    Profile(AddressArg),
    /// An address's transactions, newest first
    Txs {
        #[command(flatten)]
        address: AddressArg,
        /// Rows per page
        #[arg(short, long, default_value = "50")]
        limit: usize,
        /// Continue from a previous page's `next` cursor
        #[arg(short, long)]
        before: Option<Cursor>,
        /// Every indexed transaction instead of one page (an export; at most 250,000 rows)
        #[arg(long, conflicts_with = "before")]
        all: bool,
        /// Output format: json, or csv (one row per transaction; with a page, the `next`
        /// cursor goes to stderr)
        #[arg(long, default_value = "json")]
        format: ExportFormat,
    },
    /// Who an address transacts with, by volume
    Peers {
        #[command(flatten)]
        address: AddressArg,
        /// How many counterparties to show
        #[arg(long, default_value = "20")]
        top: usize,
    },
    /// The likely-owner cluster of an address (common-input ownership and change)
    Cluster {
        #[command(flatten)]
        address: AddressArg,
        /// How many members to list
        #[arg(long, default_value = "100")]
        limit: usize,
    },
    /// Follow the money: counterparties of counterparties, as a graph
    Flows {
        #[command(flatten)]
        address: AddressArg,
        /// How many counterparty hops to follow
        #[arg(long, default_value = "2")]
        hops: u8,
        /// Counterparties followed per address
        #[arg(long, default_value = "12")]
        top: usize,
        /// Fold addresses that only pass money on (peel chains) into one edge each,
        /// listing them in `via`
        #[arg(long)]
        collapse: bool,
        /// Output format: json (nodes and edges), or csv (one row per edge, with labels)
        #[arg(long, default_value = "json")]
        format: ExportFormat,
    },
    /// A stored transaction with its addresses resolved
    Tx {
        /// Transaction id (hex)
        txid: String,
    },
    /// Balance changes over time, oldest first
    Balance {
        #[command(flatten)]
        address: AddressArg,
        /// Only changes after this unix time in ms (default: everything indexed)
        #[arg(long, default_value = "0")]
        from_ms: u64,
        /// The current balance in sompi, to turn changes into absolute balances
        #[arg(long)]
        now: Option<u64>,
    },
}

#[derive(Args, Debug, Clone, PartialEq)]
pub struct AddressArg {
    /// Kaspa address (kaspa:…)
    pub address: String,
}

#[derive(Serialize)]
struct Txs {
    address: String,
    page: query::Page<query::TxRow>,
}

#[derive(Serialize)]
struct Peers {
    address: String,
    peers: Vec<query::Peer>,
}

#[derive(Serialize)]
struct Balance {
    address: String,
    /// `(time_ms, balance)` when `now` was given, else `(time_ms, change since start)`.
    points: Vec<(u64, i64)>,
}

/// Open the index for `network` and run the command, printing JSON to stdout.
pub fn run(network: &str, cmd: AddressCommand) -> Result<()> {
    let store = IndexStore::open_existing(network)?;
    let out = match cmd {
        AddressCommand::Profile(a) => to_json(&query::profile(&store, &a.address)?)?,
        AddressCommand::Txs {
            address,
            limit,
            before,
            all,
            format,
        } => {
            let id = known(&store, &address.address)?;
            let page = if all {
                query::Page {
                    items: export::all_transactions(&store, id, export::EXPORT_MAX_ROWS)?,
                    next: None,
                }
            } else {
                query::transactions(&store, id, before, limit)?
            };
            match format {
                ExportFormat::Json => to_json(&Txs {
                    address: address.address,
                    page,
                })?,
                ExportFormat::Csv => {
                    if let Some(next) = page.next {
                        eprintln!("next: {next}");
                    }
                    let csv = export::transactions_csv(&address.address, &page.items);
                    csv.trim_end_matches('\n').to_string()
                }
            }
        }
        AddressCommand::Peers { address, top } => {
            let id = known(&store, &address.address)?;
            to_json(&Peers {
                address: address.address,
                peers: query::counterparties(&store, id, top)?,
            })?
        }
        AddressCommand::Cluster { address, limit } => {
            let id = known(&store, &address.address)?;
            let labels = LabelBook::load();
            to_json(&query::cluster(&store, &labels, id, limit)?)?
        }
        AddressCommand::Flows {
            address,
            hops,
            top,
            collapse,
            format,
        } => {
            let id = known(&store, &address.address)?;
            let mut graph = query::flows(&store, &[id], hops, top)?;
            if collapse {
                graph = graph.collapse_chains();
            }
            match format {
                ExportFormat::Json => to_json(&graph)?,
                ExportFormat::Csv => export::flows_csv(&graph, &LabelBook::load())
                    .trim_end_matches('\n')
                    .to_string(),
            }
        }
        AddressCommand::Tx { txid } => {
            let txid = parse_hex(&txid).ok_or_else(|| anyhow!("txid must be 64 hex chars"))?;
            match query::transaction(&store, &txid)? {
                Some(tx) => to_json(&tx)?,
                None => return Err(anyhow!("transaction not in the index")),
            }
        }
        AddressCommand::Balance {
            address,
            from_ms,
            now,
        } => {
            let id = known(&store, &address.address)?;
            let deltas = query::balance_deltas(&store, id, from_ms)?;
            to_json(&Balance {
                address: address.address,
                points: query::balance_curve(&deltas, now),
            })?
        }
    };
    println!("{out}");
    Ok(())
}

fn known(store: &IndexStore, address: &str) -> Result<u32> {
    store
        .lookup(address)?
        .ok_or_else(|| anyhow!("address not seen in the indexed window"))
}

fn to_json<T: Serialize>(value: &T) -> Result<String> {
    Ok(serde_json::to_string_pretty(value)?)
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;
    use crate::{Args, Command};

    fn address_command(args: &[&str]) -> AddressCommand {
        let args = Args::try_parse_from(["x4kas-cli", "address"].iter().chain(args)).unwrap();
        match args.command {
            Command::Address { cmd } => cmd,
            other => panic!("not an address command: {other:?}"),
        }
    }

    #[test]
    fn txs_takes_a_format_and_all() {
        let cmd = address_command(&["txs", "kaspa:qq1", "--all", "--format", "csv"]);
        let AddressCommand::Txs {
            all, format, limit, ..
        } = cmd
        else {
            panic!("{cmd:?}");
        };
        assert!(all);
        assert_eq!(format, ExportFormat::Csv);
        assert_eq!(limit, 50);
        // Paging and --all contradict each other; unknown formats are refused.
        assert!(
            Args::try_parse_from([
                "x4kas-cli",
                "address",
                "txs",
                "kaspa:qq1",
                "--all",
                "--before",
                "5:00"
            ])
            .is_err()
        );
        assert!(
            Args::try_parse_from([
                "x4kas-cli",
                "address",
                "txs",
                "kaspa:qq1",
                "--format",
                "xml"
            ])
            .is_err()
        );
    }

    #[test]
    fn flows_defaults_to_json_without_collapsing() {
        let cmd = address_command(&["flows", "kaspa:qq1"]);
        let AddressCommand::Flows {
            collapse,
            format,
            hops,
            ..
        } = cmd
        else {
            panic!("{cmd:?}");
        };
        assert!(!collapse);
        assert_eq!(format, ExportFormat::Json);
        assert_eq!(hops, 2);
        let cmd = address_command(&["flows", "kaspa:qq1", "--collapse", "--format", "CSV"]);
        assert!(matches!(
            cmd,
            AddressCommand::Flows {
                collapse: true,
                format: ExportFormat::Csv,
                ..
            }
        ));
    }
}
