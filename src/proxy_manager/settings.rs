//! Settings that can be changed from the authenticated console without a restart.
use super::config::ProxyManagerConfig;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxySettings {
    pub refresh_interval_secs: u64,
    pub subscription_download_route: String,
    pub subscription_timeout_secs: u64,
    pub subscription_max_bytes: usize,
    pub subscription_max_nodes: usize,
    pub subscription_concurrency: usize,
    pub subscription_allowed_private_hosts: Vec<String>,
    pub manual_test_url: String,
    pub manual_test_timeout_ms: u64,
    pub manual_test_concurrency: usize,
    pub auto_test_enabled: bool,
    pub auto_test_interval_secs: u64,
    pub auto_failover_enabled: bool,
    pub auto_bind_new_accounts: bool,
    pub auto_selection_max_test_age_secs: u64,
    pub apply_drain_timeout_secs: u64,
    pub apply_timeout_secs: u64,
}

impl ProxySettings {
    pub fn from_config(config: &ProxyManagerConfig) -> Self {
        Self {
            refresh_interval_secs: config.refresh_interval_secs,
            subscription_download_route: config.subscription_download_route.clone(),
            subscription_timeout_secs: config.subscription_timeout_secs,
            subscription_max_bytes: config.subscription_max_bytes,
            subscription_max_nodes: config.subscription_max_nodes,
            subscription_concurrency: config.subscription_concurrency,
            subscription_allowed_private_hosts: config.subscription_allowed_private_hosts.clone(),
            manual_test_url: config.manual_test_url.clone(),
            manual_test_timeout_ms: config.manual_test_timeout_ms,
            manual_test_concurrency: config.manual_test_concurrency,
            auto_test_enabled: config.auto_test_enabled,
            auto_test_interval_secs: config.auto_test_interval_secs,
            auto_failover_enabled: config.auto_failover_enabled,
            auto_bind_new_accounts: config.auto_bind_new_accounts,
            auto_selection_max_test_age_secs: config.auto_selection_max_test_age_secs,
            apply_drain_timeout_secs: config.apply_drain_timeout_secs,
            apply_timeout_secs: config.apply_timeout_secs,
        }
    }
    pub fn apply(&self, deployment: &ProxyManagerConfig) -> Result<ProxyManagerConfig, String> {
        let mut config = deployment.clone();
        config.refresh_interval_secs = self.refresh_interval_secs;
        config.subscription_download_route = self.subscription_download_route.clone();
        config.subscription_timeout_secs = self.subscription_timeout_secs;
        config.subscription_max_bytes = self.subscription_max_bytes;
        config.subscription_max_nodes = self.subscription_max_nodes;
        config.subscription_concurrency = self.subscription_concurrency;
        config.subscription_allowed_private_hosts = self.subscription_allowed_private_hosts.clone();
        config.manual_test_url = self.manual_test_url.clone();
        config.manual_test_timeout_ms = self.manual_test_timeout_ms;
        config.manual_test_concurrency = self.manual_test_concurrency;
        config.auto_test_enabled = self.auto_test_enabled;
        config.auto_test_interval_secs = self.auto_test_interval_secs;
        config.auto_failover_enabled = self.auto_failover_enabled;
        config.auto_bind_new_accounts = self.auto_bind_new_accounts;
        config.auto_selection_max_test_age_secs = self.auto_selection_max_test_age_secs;
        config.apply_drain_timeout_secs = self.apply_drain_timeout_secs;
        config.apply_timeout_secs = self.apply_timeout_secs;
        config.validate()?;
        if config.subscription_timeout_secs > 300
            || config.manual_test_timeout_ms > 30000
            || config.subscription_max_bytes > 50 * 1024 * 1024
            || config.subscription_max_nodes > 10000
            || config.apply_timeout_secs > 300
        {
            return Err("配置超出允许范围：下载超时不超过 300 秒、测速超时不超过 30000 毫秒、订阅不超过 50 MiB / 10000 节点、应用超时不超过 300 秒。".into());
        }
        Ok(config)
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct SettingsRecord {
    pub revision: u64,
    pub settings: ProxySettings,
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct TestSchedule {
    pub next_test_at: u64,
    pub operation_id: Option<String>,
    #[serde(default)]
    pub last_reconcile_at: u64,
    #[serde(default)]
    pub last_reconcile_fingerprint: Option<String>,
}
