//! `x4kas-cli watchlist …`: the named watchlists the GUI's Monitoring tab shows and
//! `x4kas-cli watch` follows. Edits the file only (no node); a running GUI picks a
//! change up with its next watchlist change or launch, and keeps it.

use anyhow::{Result, anyhow};
use clap::Subcommand;
use serde::Serialize;

use x4kas_core::watch::{self, AlertRules, ListId, WatchEdit, WatchEntry, Watchlists};

#[derive(Subcommand, Debug, Clone, PartialEq)]
pub enum WatchlistCommand {
    /// The watchlists with their address counts, or one list's entries
    Ls {
        /// A list's name
        name: Option<String>,
    },
    /// Create an empty watchlist
    Create { name: String },
    /// Rename a watchlist
    Rename { name: String, new_name: String },
    /// Delete a watchlist and its entries (the last one stays)
    Rm { name: String },
    /// Add addresses to a watchlist with the default rules (alerts on any activity)
    Add {
        list: String,
        #[arg(required = true)]
        addresses: Vec<String>,
    },
    /// Remove addresses from a watchlist
    Remove {
        list: String,
        #[arg(required = true)]
        addresses: Vec<String>,
    },
    /// Move an address, its rules included, from one watchlist to another
    Move {
        address: String,
        from: String,
        to: String,
    },
}

#[derive(Serialize)]
struct ListRow<'a> {
    id: ListId,
    name: &'a str,
    /// Entries on the `--network`.
    addresses: usize,
}

#[derive(Serialize)]
struct EntryRow<'a> {
    address: &'a str,
    alerts: bool,
    rules: &'a AlertRules,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_activity_ms: Option<u64>,
}

pub fn run(network: &str, cmd: WatchlistCommand) -> Result<()> {
    let out = match cmd {
        WatchlistCommand::Ls { name } => {
            let mut lists = Watchlists::load()?;
            lists.ensure_default();
            match name {
                None => serde_json::to_string_pretty(&list_rows(&lists, network))?,
                Some(name) => {
                    let list = find(&lists, &name).map_err(|e| anyhow!(e))?;
                    let rows: Vec<EntryRow> = list
                        .on_network(network)
                        .map(|e| EntryRow {
                            address: &e.address,
                            alerts: e.enabled,
                            rules: &e.rules,
                            last_activity_ms: e.last_activity_ms,
                        })
                        .collect();
                    serde_json::to_string_pretty(&rows)?
                }
            }
        }
        cmd => {
            for address in addresses_of(&cmd) {
                watch::validate_address(address, network).map_err(|e| anyhow!("{address}: {e}"))?;
            }
            let lists = Watchlists::update_file(|lists| {
                edits(lists, network, &cmd)?
                    .into_iter()
                    .try_for_each(|edit| lists.apply(edit))
            })?;
            eprintln!("note: a running GUI picks this up with its next watchlist change or launch");
            serde_json::to_string_pretty(&list_rows(&lists, network))?
        }
    };
    println!("{out}");
    Ok(())
}

fn list_rows<'a>(lists: &'a Watchlists, network: &str) -> Vec<ListRow<'a>> {
    lists
        .lists
        .iter()
        .map(|l| ListRow {
            id: l.id,
            name: &l.name,
            addresses: l.on_network(network).count(),
        })
        .collect()
}

/// The addresses an edit names, to validate before touching the file.
fn addresses_of(cmd: &WatchlistCommand) -> Vec<&str> {
    match cmd {
        WatchlistCommand::Add { addresses, .. } => addresses.iter().map(|a| a.trim()).collect(),
        _ => Vec::new(),
    }
}

fn find<'a>(lists: &'a Watchlists, name: &str) -> Result<&'a watch::Watchlist, String> {
    lists.by_name(name).ok_or_else(|| {
        let names: Vec<&str> = lists.lists.iter().map(|l| l.name.as_str()).collect();
        format!(
            "no watchlist named {name:?} (there are: {})",
            names.join(", ")
        )
    })
}

/// The edits a command makes, its list names resolved against `lists` as on disk.
fn edits(
    lists: &Watchlists,
    network: &str,
    cmd: &WatchlistCommand,
) -> Result<Vec<WatchEdit>, String> {
    let id = |name: &str| find(lists, name).map(|l| l.id);
    Ok(match cmd {
        WatchlistCommand::Ls { .. } => Vec::new(),
        WatchlistCommand::Create { name } => vec![WatchEdit::CreateList { name: name.clone() }],
        WatchlistCommand::Rename { name, new_name } => vec![WatchEdit::RenameList {
            id: id(name)?,
            name: new_name.clone(),
        }],
        WatchlistCommand::Rm { name } => vec![WatchEdit::DeleteList { id: id(name)? }],
        WatchlistCommand::Add { list, addresses } => {
            let list = id(list)?;
            addresses
                .iter()
                .map(|a| {
                    // Keep the rules of an address already on the list.
                    let entry = lists
                        .get(list)
                        .and_then(|l| l.entry(a.trim(), network))
                        .cloned()
                        .unwrap_or_else(|| WatchEntry::new(a, network));
                    WatchEdit::Put { list, entry }
                })
                .collect()
        }
        WatchlistCommand::Remove { list, addresses } => {
            let list = id(list)?;
            addresses
                .iter()
                .map(|a| WatchEdit::Remove {
                    list,
                    address: a.trim().to_string(),
                    network: network.to_string(),
                })
                .collect()
        }
        WatchlistCommand::Move { address, from, to } => vec![WatchEdit::Move {
            from: id(from)?,
            to: id(to)?,
            address: address.trim().to_string(),
            network: network.to_string(),
        }],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lists() -> Watchlists {
        let mut lists = Watchlists::default();
        lists.ensure_default();
        lists
            .apply(WatchEdit::CreateList {
                name: "Exchanges".into(),
            })
            .unwrap();
        lists
    }

    #[test]
    fn names_resolve_to_ids() {
        let l = lists();
        let cmd = WatchlistCommand::Move {
            address: " kaspa:a ".into(),
            from: "default".into(),
            to: "EXCHANGES".into(),
        };
        assert_eq!(
            edits(&l, "mainnet", &cmd).unwrap(),
            vec![WatchEdit::Move {
                from: 1,
                to: 2,
                address: "kaspa:a".into(),
                network: "mainnet".into(),
            }]
        );
        let missing = WatchlistCommand::Rm {
            name: "Nope".into(),
        };
        let err = edits(&l, "mainnet", &missing).unwrap_err();
        assert!(err.contains("Default, Exchanges"), "{err}");
    }

    #[test]
    fn add_keeps_existing_rules() {
        let mut l = lists();
        let mut entry = WatchEntry::new("kaspa:a", "mainnet");
        entry.rules.sent_min = Some(5);
        l.apply(WatchEdit::Put {
            list: 2,
            entry: entry.clone(),
        })
        .unwrap();
        let cmd = WatchlistCommand::Add {
            list: "Exchanges".into(),
            addresses: vec!["kaspa:a".into(), "kaspa:b".into()],
        };
        let out = edits(&l, "mainnet", &cmd).unwrap();
        assert_eq!(out[0], WatchEdit::Put { list: 2, entry });
        assert_eq!(
            out[1],
            WatchEdit::Put {
                list: 2,
                entry: WatchEntry::new("kaspa:b", "mainnet"),
            }
        );
    }
}
