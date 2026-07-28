use std::{
    fmt, fs,
    path::{Path, PathBuf},
};

use recebi_core::{GenesisHash, PublicKey};
use serde::Deserialize;
use thiserror::Error;
use url::Url;

const MAX_CONFIG_BYTES: u64 = 64 * 1024;

#[derive(Clone, Deserialize)]
pub struct AppConfig {
    pub recebi: TrustedConfig,
}

#[derive(Clone, Deserialize)]
pub struct TrustedConfig {
    pub cluster: Cluster,
    pub genesis_hash: GenesisHash,
    pub merchant_wallet: PublicKey,
    pub accepted_mint: PublicKey,
    pub token_decimals: u8,
    pub rpc_url: Url,
    pub data_dir: PathBuf,
    pub ptax_policy: PtaxPolicy,
    pub max_open_reconcile: u16,
}

impl fmt::Debug for AppConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AppConfig")
            .field("recebi", &self.recebi)
            .finish()
    }
}

impl fmt::Debug for TrustedConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TrustedConfig")
            .field("cluster", &self.cluster)
            .field("genesis_hash", &"[redacted]")
            .field("merchant_wallet", &"[redacted]")
            .field("accepted_mint", &"[redacted]")
            .field("token_decimals", &self.token_decimals)
            .field("rpc_url", &"[redacted]")
            .field("data_dir", &"[redacted]")
            .field("ptax_policy", &self.ptax_policy)
            .field("max_open_reconcile", &self.max_open_reconcile)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Cluster {
    MainnetBeta,
    Devnet,
}

impl Cluster {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MainnetBeta => "mainnet_beta",
            Self::Devnet => "devnet",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum PtaxPolicy {
    StrictSameDay,
}

impl PtaxPolicy {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::StrictSameDay => "strict_same_day",
        }
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("configuration file is unavailable")]
    Unavailable,
    #[error("configuration file exceeds the size limit")]
    TooLarge,
    #[error("configuration is invalid")]
    Invalid,
    #[error("RPC URL must use HTTPS and contain no credentials")]
    UnsafeRpcUrl,
    #[error("data directory is unavailable")]
    DataDirectoryUnavailable,
}

impl AppConfig {
    /// # Errors
    ///
    /// Returns redacted, typed errors; no endpoint, path, or file contents are
    /// included in user-facing messages.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let metadata = fs::metadata(path).map_err(|_| ConfigError::Unavailable)?;
        if metadata.len() > MAX_CONFIG_BYTES {
            return Err(ConfigError::TooLarge);
        }
        let contents = fs::read_to_string(path).map_err(|_| ConfigError::Unavailable)?;
        let config: Self = toml::from_str(&contents).map_err(|_| ConfigError::Invalid)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        let rpc_url = &self.recebi.rpc_url;
        if rpc_url.scheme() != "https"
            || !rpc_url.username().is_empty()
            || rpc_url.password().is_some()
        {
            return Err(ConfigError::UnsafeRpcUrl);
        }
        if self.recebi.data_dir.as_os_str().is_empty()
            || self.recebi.max_open_reconcile == 0
            || self.recebi.token_decimals > 18
            || self.recebi.merchant_wallet.as_str().is_empty()
            || self.recebi.accepted_mint.as_str().is_empty()
        {
            return Err(ConfigError::Invalid);
        }
        Ok(())
    }

    /// # Errors
    ///
    /// Returns a redacted error when the trusted local data directory cannot be
    /// created or inspected.
    pub fn ensure_data_directory(&self) -> Result<(), ConfigError> {
        fs::create_dir_all(&self.recebi.data_dir)
            .map_err(|_| ConfigError::DataDirectoryUnavailable)?;
        if self.recebi.data_dir.is_dir() {
            Ok(())
        } else {
            Err(ConfigError::DataDirectoryUnavailable)
        }
    }

    #[must_use]
    pub fn database_path(&self) -> PathBuf {
        self.recebi.data_dir.join("recebi.sqlite3")
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{AppConfig, ConfigError};
    use tempfile::tempdir;

    const VALID_CONFIG: &str = r#"
[recebi]
cluster = "devnet"
genesis_hash = "EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG"
merchant_wallet = "11111111111111111111111111111111"
accepted_mint = "11111111111111111111111111111111"
token_decimals = 6
rpc_url = "https://api.devnet.solana.com"
data_dir = "data"
ptax_policy = "strict_same_day"
max_open_reconcile = 10
"#;

    #[test]
    fn accepts_trusted_valid_configuration() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("recebi.toml");
        fs::write(&path, VALID_CONFIG).expect("write config");
        let config = AppConfig::load(&path).expect("valid configuration");
        assert_eq!(config.recebi.token_decimals, 6);
    }

    #[test]
    fn rejects_unsafe_rpc_scheme_without_echoing_it() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("recebi.toml");
        fs::write(
            &path,
            VALID_CONFIG.replace("https://api.devnet.solana.com", "http://secret.invalid"),
        )
        .expect("write config");
        assert!(matches!(
            AppConfig::load(&path),
            Err(ConfigError::UnsafeRpcUrl)
        ));
        assert!(!ConfigError::UnsafeRpcUrl.to_string().contains("secret"));
    }

    #[test]
    fn debug_output_redacts_trusted_identities_endpoints_and_paths() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("recebi.toml");
        let sensitive_config = VALID_CONFIG
            .replace(
                "https://api.devnet.solana.com",
                "https://private-rpc.example.invalid",
            )
            .replace("data_dir = \"data\"", "data_dir = \"private-ledger\"");
        fs::write(&path, sensitive_config).expect("write config");
        let debug = format!("{:?}", AppConfig::load(&path).expect("valid configuration"));
        assert!(!debug.contains("private-rpc"));
        assert!(!debug.contains("private-ledger"));
        assert!(!debug.contains("11111111111111111111111111111111"));
        assert!(debug.contains("[redacted]"));
    }

    #[test]
    fn rejects_invalid_mint() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("recebi.toml");
        fs::write(
            &path,
            VALID_CONFIG.replace(
                "accepted_mint = \"11111111111111111111111111111111\"",
                "accepted_mint = \"not-a-public-key\"",
            ),
        )
        .expect("write config");
        assert!(matches!(AppConfig::load(&path), Err(ConfigError::Invalid)));
    }

    #[test]
    fn rejects_empty_configuration() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("recebi.toml");
        fs::write(&path, "").expect("write empty config");
        assert!(matches!(AppConfig::load(&path), Err(ConfigError::Invalid)));
    }

    #[test]
    fn rejects_oversized_configuration() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("recebi.toml");
        fs::write(&path, "x".repeat(65 * 1024)).expect("write oversized config");
        assert!(matches!(AppConfig::load(&path), Err(ConfigError::TooLarge)));
    }
}
