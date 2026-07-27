use recebi_core::Provenance;
use serde::Serialize;

use crate::config::AppConfig;

#[derive(Clone)]
pub struct HealthService {
    config: AppConfig,
}

#[derive(Debug, Serialize)]
pub struct HealthResult {
    status: &'static str,
    configuration: &'static str,
    data_directory: &'static str,
    network_checks: &'static str,
    custody: &'static str,
    cluster: &'static str,
    ptax_policy: &'static str,
    provenance: Provenance,
}

impl HealthService {
    #[must_use]
    pub const fn new(config: AppConfig) -> Self {
        Self { config }
    }

    /// # Errors
    ///
    /// Returns only a typed redacted configuration error.
    pub fn check(&self) -> Result<HealthResult, crate::config::ConfigError> {
        self.config.ensure_data_directory()?;
        Ok(HealthResult {
            status: "ok",
            configuration: "valid",
            data_directory: "available",
            network_checks: "not_run",
            custody: "none",
            cluster: self.config.recebi.cluster.as_str(),
            ptax_policy: self.config.recebi.ptax_policy.as_str(),
            provenance: Provenance::Derived,
        })
    }
}
