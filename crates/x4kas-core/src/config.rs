use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// The app's data directory, `~/.x4kas`.
pub fn data_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".x4kas")
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

#[cfg(test)]
mod tests {
    use super::*;

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
