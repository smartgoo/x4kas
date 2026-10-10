//! `x4kas-cli labels …`: the address labels the GUI shows (user labels, the public
//! api.kaspa.org list as last fetched). No node needed.

use anyhow::Result;
use clap::Subcommand;
use serde::Serialize;

use x4kas_core::labels::{self, Label, LabelBook, LabelSettings, LabelSource};

#[derive(Subcommand, Debug, Clone, PartialEq)]
pub enum LabelsCommand {
    /// The label of one address, or every label matching a query
    Get {
        /// A kaspa:… address, or text to find in label names
        query: String,
    },
    /// All known labels
    List,
    /// Set your own label for an address
    Set { address: String, name: String },
    /// Remove your own label for an address
    Rm { address: String },
    /// Fetch the public api.kaspa.org list now (the GUI otherwise refreshes it hourly)
    Refresh,
    /// Ask the enabled online sources (kas.fyi with a key, KNS) about an address
    Online { address: String },
    /// Set the kas.fyi API key (empty to remove); enables kas.fyi lookups
    Key { key: String },
    /// Turn `.kas` name resolution through KNS on or off
    Kns {
        #[arg(value_parser = ["on", "off"])]
        state: String,
    },
}

#[derive(Serialize)]
struct Row<'a> {
    address: &'a str,
    name: &'a str,
    source: LabelSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    link: Option<&'a str>,
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    categories: &'a [String],
}

impl<'a> Row<'a> {
    fn new(address: &'a str, label: &'a Label) -> Self {
        Self {
            address,
            name: &label.name,
            source: label.source,
            link: label.link.as_deref(),
            categories: &label.categories,
        }
    }
}

pub async fn run(cmd: LabelsCommand) -> Result<()> {
    let mut book = LabelBook::load();
    let out = match cmd {
        LabelsCommand::Get { query } => {
            let rows: Vec<Row> = match book.get(&query) {
                Some(label) => vec![Row::new(&query, label)],
                None => book
                    .search(&query)
                    .into_iter()
                    .map(|(a, l)| Row::new(a, l))
                    .collect(),
            };
            serde_json::to_string_pretty(&rows)?
        }
        LabelsCommand::List => {
            let rows: Vec<Row> = book
                .all()
                .into_iter()
                .map(|(a, l)| Row::new(a, l))
                .collect();
            serde_json::to_string_pretty(&rows)?
        }
        LabelsCommand::Set { address, name } => {
            book.set_user(&address, Some(&name))?;
            serde_json::to_string_pretty(&serde_json::json!({ "address": address, "name": name }))?
        }
        LabelsCommand::Rm { address } => {
            book.set_user(&address, None)?;
            serde_json::to_string_pretty(
                &serde_json::json!({ "address": address, "removed": true }),
            )?
        }
        LabelsCommand::Refresh => {
            let list = labels::fetch_kaspa_org_names().await?;
            serde_json::to_string_pretty(&serde_json::json!({ "fetched": list.len() }))?
        }
        LabelsCommand::Online { address } => {
            let settings = LabelSettings::load();
            if !settings.any_enabled() {
                anyhow::bail!(
                    "no online source enabled: set a kas.fyi key (`labels key`) or `labels kns on`"
                );
            }
            let entries = labels::lookup_online(&settings, &address).await?;
            serde_json::to_string_pretty(&entries)?
        }
        LabelsCommand::Key { key } => {
            let mut settings = LabelSettings::load();
            settings.kas_fyi_api_key = Some(key.trim().to_string()).filter(|k| !k.is_empty());
            settings.save()?;
            serde_json::to_string_pretty(
                &serde_json::json!({ "kas_fyi": settings.kas_fyi_api_key.is_some() }),
            )?
        }
        LabelsCommand::Kns { state } => {
            let mut settings = LabelSettings::load();
            settings.kns = state == "on";
            settings.save()?;
            serde_json::to_string_pretty(&serde_json::json!({ "kns": settings.kns }))?
        }
    };
    println!("{out}");
    Ok(())
}
