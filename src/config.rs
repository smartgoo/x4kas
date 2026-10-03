use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonConfig {
    // General
    pub network: String,
    pub utxo_index: bool,
    #[serde(default)]
    pub archival: bool,
    pub ram_scale: f64,
    pub log_level: String,
    pub async_threads: usize,
    #[serde(default)]
    pub auto_start_daemon: bool,

    // Networking
    #[serde(default)]
    pub listen: Option<String>,
    #[serde(default)]
    pub externalip: Option<String>,
    #[serde(default = "default_outpeers")]
    pub outbound_target: usize,
    #[serde(default = "default_inbound_limit")]
    pub inbound_limit: usize,
    #[serde(default)]
    pub connect_peers: String,
    #[serde(default)]
    pub add_peers: String,
    #[serde(default)]
    pub disable_upnp: bool,
    #[serde(default)]
    pub disable_dns_seed: bool,

    // Storage
    pub app_dir: String,
    #[serde(default = "default_rocksdb_preset")]
    pub rocksdb_preset: String,
    #[serde(default)]
    pub rocksdb_wal_dir: Option<String>,
    #[serde(default)]
    pub rocksdb_cache_size: Option<usize>,
    #[serde(default)]
    pub retention_period_days: Option<f64>,
    #[serde(default)]
    pub reset_db: bool,
    #[serde(default = "default_rpc_max_clients")]
    pub rpc_max_clients: usize,

    // Performance
    #[serde(default)]
    pub perf_metrics: bool,
}

fn default_rocksdb_preset() -> String {
    "default".to_string()
}
fn default_outpeers() -> usize {
    8
}
fn default_inbound_limit() -> usize {
    128
}
fn default_rpc_max_clients() -> usize {
    128
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            network: "mainnet".to_string(),
            utxo_index: false,
            archival: false,
            ram_scale: 1.0,
            app_dir: default_app_dir(),
            log_level: "WARN".to_string(),
            async_threads: num_cpus::get(),
            auto_start_daemon: false,

            listen: None,
            externalip: None,
            outbound_target: 8,
            inbound_limit: 128,
            connect_peers: String::new(),
            add_peers: String::new(),
            disable_upnp: false,
            disable_dns_seed: false,

            rocksdb_preset: "default".to_string(),
            rocksdb_wal_dir: None,
            rocksdb_cache_size: None,
            retention_period_days: None,
            reset_db: false,
            rpc_max_clients: 128,

            perf_metrics: false,
        }
    }
}

#[allow(dead_code)]
impl DaemonConfig {
    pub fn config_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".tui4kas")
            .join("config.toml")
    }

    pub fn load() -> Result<Self> {
        let path = Self::config_path();
        if !path.exists() {
            return Ok(Self::default());
        }
        let contents = std::fs::read_to_string(&path)?;
        let config: Self = toml::from_str(&contents)?;
        Ok(config)
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let contents = toml::to_string_pretty(self)?;
        std::fs::write(&path, contents)?;
        Ok(())
    }

    pub fn valid_networks() -> &'static [&'static str] {
        &["mainnet", "testnet-10", "testnet-11"]
    }

    pub fn valid_log_levels() -> &'static [&'static str] {
        &["TRACE", "DEBUG", "INFO", "WARN", "ERROR"]
    }

    pub fn valid_rocksdb_presets() -> &'static [&'static str] {
        &["default", "hdd"]
    }

    /// Parse comma-separated peer strings into Vec of trimmed non-empty strings
    pub fn parse_peers(s: &str) -> Vec<String> {
        s.split(',')
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect()
    }
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
    /// The embedded node (configured in the Node tab).
    Embedded,
}

/// The last-used connection choice, persisted at `~/.tui4kas/connection.toml`.
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

impl ConnectionSettings {
    pub fn path() -> PathBuf {
        DaemonConfig::config_path().with_file_name("connection.toml")
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

fn default_app_dir() -> String {
    dirs::home_dir()
        .map(|h| h.join(".tui4kas").to_string_lossy().into_owned())
        .unwrap_or_else(|| ".tui4kas".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_values() {
        let config = DaemonConfig::default();
        assert_eq!(config.network, "mainnet");
        assert!(!config.utxo_index);
        assert!(!config.archival);
        assert!((config.ram_scale - 1.0).abs() < f64::EPSILON);
        assert_eq!(config.log_level, "WARN");
        assert!(config.async_threads > 0);
        assert_eq!(config.outbound_target, 8);
        assert_eq!(config.inbound_limit, 128);
        assert_eq!(config.rpc_max_clients, 128);
        assert_eq!(config.rocksdb_preset, "default");
        assert!(!config.perf_metrics);
        assert!(!config.disable_upnp);
        assert!(!config.disable_dns_seed);
        assert!(!config.reset_db);
    }

    #[test]
    fn serialize_deserialize_roundtrip() {
        let config = DaemonConfig {
            network: "testnet-10".to_string(),
            utxo_index: true,
            archival: true,
            ram_scale: 2.5,
            app_dir: "/tmp/test".to_string(),
            log_level: "DEBUG".to_string(),
            async_threads: 4,
            auto_start_daemon: true,
            listen: Some("0.0.0.0:16111".to_string()),
            externalip: None,
            outbound_target: 16,
            inbound_limit: 64,
            connect_peers: "1.2.3.4:16111".to_string(),
            add_peers: String::new(),
            disable_upnp: true,
            disable_dns_seed: false,
            rocksdb_preset: "hdd".to_string(),
            rocksdb_wal_dir: Some("/fast/wal".to_string()),
            rocksdb_cache_size: Some(512),
            retention_period_days: Some(30.0),
            reset_db: false,
            rpc_max_clients: 256,
            perf_metrics: true,
        };
        let serialized = toml::to_string_pretty(&config).unwrap();
        let deserialized: DaemonConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(deserialized.network, "testnet-10");
        assert!(deserialized.utxo_index);
        assert!(deserialized.archival);
        assert!((deserialized.ram_scale - 2.5).abs() < f64::EPSILON);
        assert_eq!(deserialized.log_level, "DEBUG");
        assert_eq!(deserialized.async_threads, 4);
        assert_eq!(deserialized.outbound_target, 16);
        assert_eq!(deserialized.inbound_limit, 64);
        assert_eq!(deserialized.connect_peers, "1.2.3.4:16111");
        assert!(deserialized.disable_upnp);
        assert_eq!(deserialized.rocksdb_preset, "hdd");
        assert_eq!(deserialized.rocksdb_wal_dir, Some("/fast/wal".to_string()));
        assert_eq!(deserialized.rocksdb_cache_size, Some(512));
        assert_eq!(deserialized.rpc_max_clients, 256);
        assert!(deserialized.perf_metrics);
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
        let loaded: ConnectionSettings = toml::from_str("kind = \"embedded\"").unwrap();
        assert_eq!(loaded.kind, ConnectionKind::Embedded);
        assert_eq!(loaded.url, ConnectionSettings::default().url);
        assert_eq!(loaded.network, "mainnet");
    }

    #[test]
    fn parse_peers() {
        assert_eq!(
            DaemonConfig::parse_peers("1.2.3.4:16111, 5.6.7.8:16111"),
            vec!["1.2.3.4:16111", "5.6.7.8:16111"]
        );
        assert!(DaemonConfig::parse_peers("").is_empty());
        assert!(DaemonConfig::parse_peers("  ,  ").is_empty());
    }

    #[test]
    fn deserialize_minimal_config() {
        // Existing configs without new fields should still load with defaults
        let toml_str = r#"
network = "mainnet"
utxo_index = false
ram_scale = 1.0
app_dir = "/tmp/test"
log_level = "WARN"
async_threads = 4
"#;
        let config: DaemonConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.outbound_target, 8);
        assert_eq!(config.inbound_limit, 128);
        assert_eq!(config.rpc_max_clients, 128);
        assert!(!config.archival);
        assert!(!config.perf_metrics);
        assert_eq!(config.rocksdb_preset, "default");
    }
}
