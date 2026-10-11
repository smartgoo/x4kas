use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// The app's data directory, `~/.x4kas`.
pub fn data_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".x4kas")
}

/// The folder of the data directory the user's exports go into (`index::export`): the
/// one thing a reset keeps.
pub const EXPORTS_DIR: &str = "exports";

/// Reset the app: delete everything in the data directory (settings, connection,
/// watchlists, labels, saved queries, the index, the GUI's window state) but the
/// exports. Run it with nothing open on the directory: the GUI does after its window
/// closed and its runtime stopped. Refuses, deleting nothing, while another x4kas
/// process has an index open; otherwise deletes all it can and says what it couldn't.
pub fn reset_data_dir() -> std::result::Result<(), String> {
    if let Some(path) = crate::index::locked_index() {
        return Err(format!(
            "Nothing was reset: the index at {} is in use by another x4kas process (a \
             second window or x4kas-cli). Quit it and reset again.",
            path.display()
        ));
    }
    let failed = clear_dir(&data_dir(), &[EXPORTS_DIR]);
    if failed.is_empty() {
        return Ok(());
    }
    let list: Vec<String> = failed
        .iter()
        .map(|(path, e)| format!("{}: {e}", path.display()))
        .collect();
    Err(format!(
        "The reset couldn't delete everything; delete these by hand:\n{}",
        list.join("\n")
    ))
}

/// Remove every entry of `dir` whose name isn't in `keep`, going on past the ones that
/// fail, which it returns. A missing `dir` is fine.
fn clear_dir(dir: &std::path::Path, keep: &[&str]) -> Vec<(PathBuf, std::io::Error)> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => return vec![(dir.to_path_buf(), e)],
    };
    let mut failed = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                failed.push((dir.to_path_buf(), e));
                continue;
            }
        };
        if keep.iter().any(|k| entry.file_name() == *k) {
            continue;
        }
        let path = entry.path();
        let removed = match entry.file_type() {
            Ok(t) if t.is_dir() => std::fs::remove_dir_all(&path),
            Ok(_) => std::fs::remove_file(&path),
            Err(e) => Err(e),
        };
        if let Err(e) = removed {
            failed.push((path, e));
        }
    }
    failed
}

pub fn valid_networks() -> &'static [&'static str] {
    &["mainnet", "testnet-10", "testnet-11"]
}

/// How the app connects to a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionKind {
    /// A node at a user-supplied wRPC URL.
    Url,
    /// A public node picked by the Kaspa resolver.
    #[default]
    Resolver,
}

/// The last-used connection choice, persisted at `~/.x4kas/connection.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConnectionSettings {
    pub kind: ConnectionKind,
    pub url: String,
    pub network: String,
}

impl Default for ConnectionSettings {
    fn default() -> Self {
        Self {
            kind: ConnectionKind::default(),
            url: "ws://127.0.0.1:17110".to_string(),
            network: "mainnet".to_string(),
        }
    }
}

/// Why `url` can't be connected to, if it can't: a wRPC URL is `ws://host[:port]` or
/// `wss://host[:port]`.
pub fn validate_url(url: &str) -> Result<(), String> {
    let url = url.trim();
    if url.is_empty() {
        return Err("Enter a URL".to_string());
    }
    let rest = url
        .strip_prefix("ws://")
        .or_else(|| url.strip_prefix("wss://"))
        .ok_or_else(|| "The URL must start with ws:// or wss://".to_string())?;
    let host_port = rest.split('/').next().unwrap_or("");
    let (host, port) = match host_port.rsplit_once(':') {
        Some((host, port)) if !host.ends_with(']') || host.starts_with('[') => (host, Some(port)),
        _ => (host_port, None),
    };
    if host.is_empty() || host.chars().any(char::is_whitespace) {
        return Err("The URL needs a host, e.g. ws://127.0.0.1:17110".to_string());
    }
    if let Some(port) = port
        && port.parse::<u16>().is_err()
    {
        return Err(format!("\"{port}\" is not a valid port"));
    }
    Ok(())
}

impl ConnectionSettings {
    pub fn path() -> PathBuf {
        data_dir().join("connection.toml")
    }

    pub fn load() -> Result<Self> {
        let path = Self::path();
        if !path.exists() {
            return Ok(Self::default());
        }
        Ok(toml::from_str(&std::fs::read_to_string(&path)?)?)
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, toml::to_string_pretty(self)?)?;
        Ok(())
    }
}

/// What the address index keeps beyond what every page and query needs, each costing
/// disk space in proportion to how much of it the chain carries. Off by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IndexFeature {
    /// Payloads longer than a record's head (`index::records::PAYLOAD_HEAD`), up to
    /// ~250 KB each.
    FullPayloads,
    /// The redeem script every P2SH spend reveals.
    RedeemScripts,
}

impl IndexFeature {
    pub const ALL: [Self; 2] = [Self::FullPayloads, Self::RedeemScripts];

    /// The setting's name, as Settings shows it.
    pub fn label(self) -> &'static str {
        match self {
            Self::FullPayloads => "Index full payloads",
            Self::RedeemScripts => "Index redeem scripts",
        }
    }

    /// What the setting does and costs.
    pub fn doc(self) -> &'static str {
        match self {
            Self::FullPayloads => {
                "Keep every transaction's whole payload, so queries can search all of it and \
                 transaction pages show it. If disabled, only the first 128 bytes are kept. \
                 This can consume a significant amount of disk space."
            }
            Self::RedeemScripts => {
                "Keep the redeem script every P2SH spend reveals, so queries can search its \
                 bytes (multisig keys, time locks, covenant code). This can consume a \
                 significant amount of disk space."
            }
        }
    }

    /// The CLI flag that sets it (`x4kas-cli index settings`).
    pub fn flag(self) -> &'static str {
        match self {
            Self::FullPayloads => "--full-payloads",
            Self::RedeemScripts => "--redeem-scripts",
        }
    }

    /// Why a query field that needs this can't be used while it's off.
    pub fn off_reason(self) -> String {
        format!(
            "{} is off: turn it on in Settings › Index (or `x4kas-cli index settings {} on`). \
             It applies to transactions indexed from then on; Resync to cover the whole window.",
            self.label(),
            self.flag()
        )
    }
}

/// The width of an index slab (`index::Slab`) unless the settings say otherwise.
pub const DEFAULT_SLAB_HOURS: u32 = 6;

/// The slab widths one can pick: whole hours that divide a day, so slabs line up with
/// days. One hour is the least: narrower slabs would multiply the keyspaces for little.
pub const SLAB_HOURS_CHOICES: [u32; 8] = [1, 2, 3, 4, 6, 8, 12, 24];

/// Whether `hours` is one of [`SLAB_HOURS_CHOICES`].
pub fn validate_slab_hours(hours: u32) -> std::result::Result<(), String> {
    if SLAB_HOURS_CHOICES.contains(&hours) {
        Ok(())
    } else {
        Err(format!(
            "slab size must be one of {} hours, not {hours}",
            SLAB_HOURS_CHOICES.map(|h| h.to_string()).join(", ")
        ))
    }
}

/// Which [`IndexFeature`]s are on and how wide new slabs are (`~/.x4kas/index.toml`).
/// The index writer reads the features before every batch, so a change applies from the
/// next one; the slab width is fixed when an index is created (it is stored in the
/// index), so a change applies from the next Resync.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexSettings {
    #[serde(default)]
    pub full_payloads: bool,
    #[serde(default)]
    pub redeem_scripts: bool,
    /// The slab width of the next index built, one of [`SLAB_HOURS_CHOICES`].
    #[serde(default = "default_slab_hours")]
    pub slab_hours: u32,
}

fn default_slab_hours() -> u32 {
    DEFAULT_SLAB_HOURS
}

impl Default for IndexSettings {
    fn default() -> Self {
        Self {
            full_payloads: false,
            redeem_scripts: false,
            slab_hours: DEFAULT_SLAB_HOURS,
        }
    }
}

impl IndexSettings {
    pub fn path() -> PathBuf {
        data_dir().join("index.toml")
    }

    /// The settings on disk ([`IndexSettings::load_with_warning`] without the warning).
    pub fn load() -> Result<Self> {
        Ok(Self::load_with_warning()?.0)
    }

    /// The settings on disk, and why a slab width that isn't one of
    /// [`SLAB_HOURS_CHOICES`] (a hand edit) was replaced by the default: the rest of the
    /// file still counts.
    pub fn load_with_warning() -> Result<(Self, Option<String>)> {
        let path = Self::path();
        if !path.exists() {
            return Ok((Self::default(), None));
        }
        Self::parse(&std::fs::read_to_string(&path)?)
            .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
    }

    fn parse(text: &str) -> Result<(Self, Option<String>)> {
        let mut settings: Self = toml::from_str(text)?;
        let warning = validate_slab_hours(settings.slab_hours).err().map(|e| {
            settings.slab_hours = DEFAULT_SLAB_HOURS;
            format!("{e}: using {DEFAULT_SLAB_HOURS}")
        });
        Ok((settings, warning))
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, toml::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn enabled(&self, feature: IndexFeature) -> bool {
        match feature {
            IndexFeature::FullPayloads => self.full_payloads,
            IndexFeature::RedeemScripts => self.redeem_scripts,
        }
    }

    pub fn set(&mut self, feature: IndexFeature, on: bool) {
        match feature {
            IndexFeature::FullPayloads => self.full_payloads = on,
            IndexFeature::RedeemScripts => self.redeem_scripts = on,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clear_dir_keeps_only_the_exports() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("watchlist.toml"), "x").unwrap();
        std::fs::create_dir_all(dir.join("index/mainnet")).unwrap();
        std::fs::write(dir.join("index/mainnet/data"), "x").unwrap();
        std::fs::create_dir(dir.join(EXPORTS_DIR)).unwrap();
        std::fs::write(dir.join(EXPORTS_DIR).join("a.csv"), "x").unwrap();
        assert!(clear_dir(dir, &[EXPORTS_DIR]).is_empty());
        let left: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, [EXPORTS_DIR]);
        assert!(dir.join(EXPORTS_DIR).join("a.csv").exists());
        // Nothing to clear is not an error.
        assert!(clear_dir(&dir.join("missing"), &[EXPORTS_DIR]).is_empty());
    }

    #[test]
    fn index_settings_default_off_and_round_trip() {
        let s = IndexSettings::default();
        assert!(IndexFeature::ALL.iter().all(|f| !s.enabled(*f)));
        let mut s = s;
        s.set(IndexFeature::RedeemScripts, true);
        let text = toml::to_string_pretty(&s).unwrap();
        assert_eq!(toml::from_str::<IndexSettings>(&text).unwrap(), s);
        // An empty file is all off.
        assert_eq!(
            toml::from_str::<IndexSettings>("").unwrap(),
            IndexSettings::default()
        );
        assert!(IndexFeature::FullPayloads.off_reason().contains("Settings"));
        // A file of an earlier version, without the slab width, gets the default.
        let old: IndexSettings = toml::from_str("full_payloads = true").unwrap();
        assert_eq!(old.slab_hours, DEFAULT_SLAB_HOURS);
        // A width that isn't a choice falls back to the default, keeping the rest.
        let (s, warning) = IndexSettings::parse("full_payloads = true\nslab_hours = 5").unwrap();
        assert!(s.full_payloads);
        assert_eq!(s.slab_hours, DEFAULT_SLAB_HOURS);
        assert!(warning.unwrap().contains("not 5"));
        assert_eq!(IndexSettings::parse("slab_hours = 12").unwrap().1, None);
    }

    #[test]
    fn slab_hours_divide_a_day() {
        for h in SLAB_HOURS_CHOICES {
            assert_eq!(24 % h, 0);
            assert!(validate_slab_hours(h).is_ok());
        }
        assert!(validate_slab_hours(0).is_err());
        assert!(validate_slab_hours(5).is_err());
        assert!(validate_slab_hours(48).is_err());
    }

    #[test]
    fn url_validation() {
        assert!(validate_url("ws://127.0.0.1:17110").is_ok());
        assert!(validate_url("wss://node.example.com").is_ok());
        assert!(validate_url(" ws://[::1]:17110 ").is_ok());
        assert!(validate_url("").unwrap_err().contains("Enter"));
        assert!(
            validate_url("127.0.0.1:17110")
                .unwrap_err()
                .contains("ws://")
        );
        assert!(validate_url("http://x:1").unwrap_err().contains("ws://"));
        assert!(validate_url("ws://").unwrap_err().contains("host"));
        assert!(validate_url("ws://host:abc").unwrap_err().contains("port"));
        assert!(
            validate_url("ws://host:70000")
                .unwrap_err()
                .contains("port")
        );
    }

    #[test]
    fn connection_settings_roundtrip() {
        let settings = ConnectionSettings {
            kind: ConnectionKind::Url,
            url: "ws://10.0.0.5:17210".to_string(),
            network: "testnet-10".to_string(),
        };
        let toml_str = toml::to_string_pretty(&settings).unwrap();
        assert!(toml_str.contains("kind = \"url\""));
        let loaded: ConnectionSettings = toml::from_str(&toml_str).unwrap();
        assert_eq!(loaded, settings);
    }

    #[test]
    fn connection_settings_fill_missing_fields() {
        let loaded: ConnectionSettings = toml::from_str("kind = \"url\"").unwrap();
        assert_eq!(loaded.kind, ConnectionKind::Url);
        assert_eq!(loaded.url, ConnectionSettings::default().url);
        assert_eq!(loaded.network, "mainnet");
    }
}
