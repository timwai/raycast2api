use serde::{Deserialize, Serialize};
use std::{net::SocketAddr, path::PathBuf};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProxyManagerConfig {
    pub enabled: bool,
    pub binary_path: PathBuf,
    pub expected_version: String,
    pub work_dir: PathBuf,
    pub controller_addr: SocketAddr,
    pub port_start: u16,
    pub port_end: u16,
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
impl Default for ProxyManagerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            binary_path: "/usr/local/bin/mihomo".into(),
            expected_version: "v1.19.34".into(),
            work_dir: "store/mihomo".into(),
            controller_addr: "127.0.0.1:19090".parse().unwrap(),
            port_start: 18000,
            port_end: 18999,
            refresh_interval_secs: 21600,
            subscription_download_route: "direct".into(),
            subscription_timeout_secs: 20,
            subscription_max_bytes: 10 * 1024 * 1024,
            subscription_max_nodes: 10000,
            subscription_concurrency: 3,
            subscription_allowed_private_hosts: vec![],
            manual_test_url: "https://www.gstatic.com/generate_204".into(),
            manual_test_timeout_ms: 5000,
            manual_test_concurrency: 3,
            auto_test_enabled: true,
            auto_test_interval_secs: 1800,
            auto_failover_enabled: true,
            auto_bind_new_accounts: true,
            auto_selection_max_test_age_secs: 3600,
            apply_drain_timeout_secs: 15,
            apply_timeout_secs: 60,
        }
    }
}
impl ProxyManagerConfig {
    pub fn validate(&self) -> Result<(), String> {
        if !self.controller_addr.ip().is_loopback() || self.controller_addr.port() == 0 {
            return Err("proxy controller must use a loopback address and nonzero port".into());
        }
        if self.port_start == 0
            || self.port_start > self.port_end
            || (self.port_start..=self.port_end).contains(&self.controller_addr.port())
        {
            return Err("invalid proxy port range or controller port overlap".into());
        }
        if self.expected_version != "v1.19.34" {
            return Err("proxy core version must match build/mihomo-lock.json (v1.19.34)".into());
        }
        if !matches!(
            self.subscription_download_route.as_str(),
            "direct" | "global"
        ) || self.refresh_interval_secs < 300
            || self.subscription_timeout_secs == 0
            || self.subscription_max_bytes == 0
            || self.subscription_max_nodes == 0
            || !(1..=32).contains(&self.subscription_concurrency)
            || !(1..=32).contains(&self.manual_test_concurrency)
            || self.manual_test_timeout_ms == 0
            || self.auto_test_interval_secs < 60
            || self.auto_selection_max_test_age_secs < 60
            || self.apply_timeout_secs <= self.apply_drain_timeout_secs
        {
            return Err("invalid proxy manager limits or download route".into());
        }
        let url =
            reqwest::Url::parse(&self.manual_test_url).map_err(|_| "invalid manual test URL")?;
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err("manual test URL must be HTTPS without credentials".into());
        }
        Ok(())
    }
}
