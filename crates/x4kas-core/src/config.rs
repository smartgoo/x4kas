use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// The app's data directory, `~/.x4kas`.
pub fn data_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".x4kas")
}

/// The folder of the data directory the user's exports go into (`index::export`), which
/// a reset keeps, as it keeps the logs (`logging::LOGS_DIR`).
pub const EXPORTS_DIR: &str = "exports";

/// Show `path` in the system's file manager (Finder, Explorer, the desktop's).
pub fn open_folder(path: &Path) -> std::io::Result<()> {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "explorer"
    } else {
        "xdg-open"
    };
    std::process::Command::new(program).arg(path).spawn()?;
    Ok(())
}

/// Reset the app: delete everything in the data directory (settings, connection,
/// watchlists, labels, saved queries, the index, the GUI's window state) but the
/// exports and the logs. Run it with nothing open on the directory: the GUI does after its window
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
    let failed = clear_dir(&data_dir(), &[EXPORTS_DIR, crate::logging::LOGS_DIR]);
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

const DAY_MS: u64 = 24 * 60 * 60 * 1000;

/// The retention periods Settings offers, in days; the CLI takes any whole number of
/// days from 1 to [`MAX_RETENTION_DAYS`].
pub const RETENTION_DAYS_CHOICES: [u32; 6] = [1, 3, 7, 14, 30, 90];

/// The longest retention period one can set, in days: every slab is a set of fjall
/// keyspaces, and an index of more than a year's slabs hasn't been measured.
pub const MAX_RETENTION_DAYS: u32 = 365;

/// How long the address index keeps what it indexed. Pruning drops whole slabs, so up
/// to one slab more stays on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Retention {
    /// Keep what the node keeps: prune behind its pruning point. The node's own
    /// retention period (`--retention-period-days`) isn't visible over RPC, so a node
    /// that keeps more is still followed by its pruning point.
    #[default]
    Node,
    /// Keep this many days, whatever the node keeps: less saves disk space, more keeps
    /// data the node has already pruned.
    Days(u32),
}

impl Retention {
    /// The time before which indexed data is dropped, if any: the node's pruning point
    /// (unknown until the node has answered) or `days` before `now_ms`.
    pub fn floor_ms(self, now_ms: u64, pruning_point_ms: Option<u64>) -> Option<u64> {
        match self {
            Self::Node => pruning_point_ms,
            Self::Days(days) => Some(now_ms.saturating_sub(days as u64 * DAY_MS)),
        }
    }

    /// The fixed period, if it isn't the node's.
    pub fn duration_ms(self) -> Option<u64> {
        match self {
            Self::Node => None,
            Self::Days(days) => Some(days as u64 * DAY_MS),
        }
    }

    /// The setting's name, as Settings shows it.
    pub fn label(self) -> String {
        match self {
            Self::Node => "Mirror Kaspa node".to_string(),
            Self::Days(1) => "1 day".to_string(),
            Self::Days(days) => format!("{days} days"),
        }
    }
}

impl std::fmt::Display for Retention {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Node => f.write_str("node"),
            Self::Days(days) => write!(f, "{days}d"),
        }
    }
}

impl std::str::FromStr for Retention {
    type Err = String;

    /// `node`, or a number of days as `<N>d` (or a bare `<N>`).
    fn from_str(s: &str) -> std::result::Result<Self, String> {
        let s = s.trim().to_ascii_lowercase();
        if s == "node" {
            return Ok(Self::Node);
        }
        let digits = s.strip_suffix('d').unwrap_or(&s);
        match digits.parse::<u32>() {
            Ok(days) if (1..=MAX_RETENTION_DAYS).contains(&days) => Ok(Self::Days(days)),
            _ => Err(format!(
                "retention must be `node` or a number of days from 1d to {MAX_RETENTION_DAYS}d, not `{s}`"
            )),
        }
    }
}

impl TryFrom<String> for Retention {
    type Error = String;

    fn try_from(s: String) -> std::result::Result<Self, String> {
        s.parse()
    }
}

impl From<Retention> for String {
    fn from(r: Retention) -> Self {
        r.to_string()
    }
}

/// Which [`IndexFeature`]s are on, how long the index keeps data and how wide new slabs
/// are (`~/.x4kas/index.toml`). The index writer reads the features and the retention
/// before every batch, so a change applies from the next one; the slab width is fixed
/// when an index is created (it is stored in the index), so a change applies from the
/// next Resync.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexSettings {
    #[serde(default)]
    pub full_payloads: bool,
    #[serde(default)]
    pub redeem_scripts: bool,
    /// How long indexed data is kept.
    #[serde(default)]
    pub retention: Retention,
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
            retention: Retention::Node,
            slab_hours: DEFAULT_SLAB_HOURS,
        }
    }
}

/// One change to the index settings, applied to the file as it is
/// ([`IndexSettings::update_file`]) so it keeps what another process changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexSettingsEdit {
    Feature(IndexFeature, bool),
    Retention(Retention),
    SlabHours(u32),
}

impl IndexSettingsEdit {
    /// The key in `index.toml` the edit sets.
    pub fn key(self) -> &'static str {
        match self {
            Self::Feature(IndexFeature::FullPayloads, _) => "full_payloads",
            Self::Feature(IndexFeature::RedeemScripts, _) => "redeem_scripts",
            Self::Retention(_) => "retention",
            Self::SlabHours(_) => "slab_hours",
        }
    }
}

/// A key of `index.toml` that didn't load, and why.
pub type SettingsWarning = (&'static str, String);

/// Write `text` to a temporary file beside `path` and rename it over `path`, so a reader
/// never sees the file half written.
fn write_replacing(path: &Path, text: &str) -> Result<()> {
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Rewrite the settings file at `path`: `edit` gets its text (empty when there is none)
/// and returns a result and the text to save, which replaces the file by a rename. Under
/// an exclusive lock on a file beside it (`<name>.toml.lock`), so two processes editing
/// it at once each apply their change to the other's.
pub(crate) fn update_toml_file<T>(
    path: &Path,
    edit: impl FnOnce(&str) -> Result<(T, String)>,
) -> Result<T> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock = std::fs::File::create(path.with_extension("toml.lock"))?;
    lock.lock()?;
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    let (result, out) = edit(&text).map_err(|e| anyhow::anyhow!("{}: {e:#}", path.display()))?;
    write_replacing(path, &out)?;
    Ok(result)
}

pub fn join_warnings(warnings: &[SettingsWarning]) -> Option<String> {
    (!warnings.is_empty()).then(|| {
        warnings
            .iter()
            .map(|(_, w)| w.as_str())
            .collect::<Vec<_>>()
            .join("; ")
    })
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
    /// [`SLAB_HOURS_CHOICES`] or a retention that doesn't parse (a hand edit) was
    /// replaced by the default: the rest of the file still counts.
    pub fn load_with_warning() -> Result<(Self, Option<String>)> {
        let (settings, warnings) = Self::load_with_warnings()?;
        Ok((settings, join_warnings(&warnings)))
    }

    /// [`IndexSettings::load_with_warning`] with each warning under the key it is about
    /// (`retention`, `slab_hours`), so a command setting that key can leave its out.
    pub fn load_with_warnings() -> Result<(Self, Vec<SettingsWarning>)> {
        let path = Self::path();
        if !path.exists() {
            return Ok((Self::default(), Vec::new()));
        }
        Self::parse_keyed(&std::fs::read_to_string(&path)?)
            .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
    }

    /// Load the file, apply `edits`, save it and return the result with the load
    /// warnings the edits didn't settle, so a change made by another process (the CLI
    /// while the GUI runs) since the last load is kept. A key that didn't load keeps its
    /// text in the file unless an edit sets it, so a hand edit isn't lost to the
    /// fallback. Under an exclusive lock on a file beside it, as `Watchlists::update_file`.
    pub fn update_file(edits: &[IndexSettingsEdit]) -> Result<(Self, Vec<SettingsWarning>)> {
        update_toml_file(&Self::path(), |text| {
            let (settings, warnings, out) = Self::edit_text(text, edits)?;
            Ok(((settings, warnings), out))
        })
    }

    /// [`IndexSettings::update_file`] on the file's `text`: the settings, the warnings
    /// left and the text to save.
    fn edit_text(
        text: &str,
        edits: &[IndexSettingsEdit],
    ) -> Result<(Self, Vec<SettingsWarning>, String)> {
        let (mut settings, mut warnings) = Self::parse_keyed(text)?;
        for edit in edits {
            settings.apply(*edit);
        }
        warnings.retain(|(key, _)| !edits.iter().any(|e| e.key() == *key));
        let mut out = toml::Table::try_from(settings)?;
        if !warnings.is_empty() {
            let raw: toml::Table = toml::from_str(text)?;
            for (key, _) in &warnings {
                if let Some(value) = raw.get(*key) {
                    out.insert(key.to_string(), value.clone());
                }
            }
        }
        Ok((settings, warnings, toml::to_string_pretty(&out)?))
    }

    pub fn apply(&mut self, edit: IndexSettingsEdit) {
        match edit {
            IndexSettingsEdit::Feature(feature, on) => self.set(feature, on),
            IndexSettingsEdit::Retention(retention) => self.retention = retention,
            IndexSettingsEdit::SlabHours(hours) => self.slab_hours = hours,
        }
    }

    #[cfg(test)]
    fn parse(text: &str) -> Result<(Self, Option<String>)> {
        let (settings, warnings) = Self::parse_keyed(text)?;
        Ok((settings, join_warnings(&warnings)))
    }

    fn parse_keyed(text: &str) -> Result<(Self, Vec<SettingsWarning>)> {
        let mut table: toml::Table = toml::from_str(text)?;
        let mut warnings = Vec::new();
        if let Some(value) = table.get("retention") {
            let parsed = match value {
                toml::Value::String(s) => s.parse::<Retention>().map(|_| ()),
                other => Err(format!(
                    "retention must be `node` or a number of days, not {other}"
                )),
            };
            if let Err(e) = parsed {
                table.remove("retention");
                warnings.push(("retention", format!("{e}: using node")));
            }
        }
        let mut settings: Self = toml::Value::Table(table).try_into()?;
        if let Err(e) = validate_slab_hours(settings.slab_hours) {
            settings.slab_hours = DEFAULT_SLAB_HOURS;
            warnings.push(("slab_hours", format!("{e}: using {DEFAULT_SLAB_HOURS}")));
        }
        Ok((settings, warnings))
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        write_replacing(&path, &toml::to_string_pretty(self)?)
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
    fn warnings_name_their_key_and_edits_apply() {
        let (s, warnings) =
            IndexSettings::parse_keyed("retention = \"week\"\nslab_hours = 5").unwrap();
        let keys: Vec<&str> = warnings.iter().map(|(key, _)| *key).collect();
        assert_eq!(keys, ["retention", "slab_hours"]);
        let mut s = s;
        for edit in [
            IndexSettingsEdit::Feature(IndexFeature::RedeemScripts, true),
            IndexSettingsEdit::Retention(Retention::Days(30)),
            IndexSettingsEdit::SlabHours(12),
        ] {
            s.apply(edit);
        }
        assert_eq!(
            s,
            IndexSettings {
                redeem_scripts: true,
                retention: Retention::Days(30),
                slab_hours: 12,
                ..Default::default()
            }
        );
    }

    #[test]
    fn an_edit_keeps_the_text_of_a_key_that_did_not_load() {
        let text = "retention = \"2w\"\nslab_hours = 5";
        let edit = [IndexSettingsEdit::Feature(IndexFeature::FullPayloads, true)];
        let (s, warnings, out) = IndexSettings::edit_text(text, &edit).unwrap();
        assert!(s.full_payloads);
        assert_eq!(warnings.len(), 2);
        let saved: toml::Table = toml::from_str(&out).unwrap();
        assert_eq!(saved["retention"].as_str(), Some("2w"));
        assert_eq!(saved["slab_hours"].as_integer(), Some(5));
        assert_eq!(saved["full_payloads"].as_bool(), Some(true));

        // Setting the key replaces it and settles its warning.
        let edit = [IndexSettingsEdit::Retention(Retention::Days(14))];
        let (_, warnings, out) = IndexSettings::edit_text(text, &edit).unwrap();
        assert_eq!(
            warnings.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
            ["slab_hours"]
        );
        let saved: toml::Table = toml::from_str(&out).unwrap();
        assert_eq!(saved["retention"].as_str(), Some("14d"));
        assert_eq!(saved["slab_hours"].as_integer(), Some(5));
    }

    #[test]
    fn retention_parses_prints_and_floors() {
        assert_eq!(IndexSettings::default().retention, Retention::Node);
        assert_eq!("node".parse::<Retention>(), Ok(Retention::Node));
        assert_eq!("7d".parse::<Retention>(), Ok(Retention::Days(7)));
        assert_eq!(" 30D ".parse::<Retention>(), Ok(Retention::Days(30)));
        assert_eq!("14".parse::<Retention>(), Ok(Retention::Days(14)));
        for bad in ["0d", "366d", "1w", "", "-1d", "forever"] {
            assert!(bad.parse::<Retention>().is_err(), "{bad}");
        }
        for r in [Retention::Node, Retention::Days(1), Retention::Days(90)] {
            assert_eq!(r.to_string().parse::<Retention>(), Ok(r));
        }
        assert_eq!("365d".parse::<Retention>(), Ok(Retention::Days(365)));
        for d in RETENTION_DAYS_CHOICES {
            assert!(Retention::Days(d).to_string().parse::<Retention>().is_ok());
        }

        // The file keeps it as a string.
        let s = IndexSettings {
            retention: Retention::Days(7),
            ..Default::default()
        };
        let text = toml::to_string_pretty(&s).unwrap();
        assert!(text.contains("retention = \"7d\""), "{text}");
        assert_eq!(IndexSettings::parse(&text).unwrap(), (s, None));
        // One that doesn't parse falls back to the node's, keeping the rest.
        let (s, warning) =
            IndexSettings::parse("redeem_scripts = true\nretention = \"week\"").unwrap();
        assert!(s.redeem_scripts);
        assert_eq!(s.retention, Retention::Node);
        assert!(warning.unwrap().contains("week"));
        let (s, warning) = IndexSettings::parse("retention = 7\nslab_hours = 5").unwrap();
        assert_eq!(s.retention, Retention::Node);
        let warning = warning.unwrap();
        assert!(
            warning.contains("retention") && warning.contains("not 5"),
            "{warning}"
        );

        let now = 100 * DAY_MS;
        assert_eq!(Retention::Node.floor_ms(now, Some(42)), Some(42));
        assert_eq!(Retention::Node.floor_ms(now, None), None);
        assert_eq!(
            Retention::Days(7).floor_ms(now, Some(42)),
            Some(93 * DAY_MS)
        );
        assert_eq!(Retention::Days(7).floor_ms(now, None), Some(93 * DAY_MS));
        assert_eq!(Retention::Days(7).floor_ms(DAY_MS, None), Some(0));
        assert_eq!(Retention::Node.duration_ms(), None);
        assert_eq!(Retention::Days(2).duration_ms(), Some(2 * DAY_MS));
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
