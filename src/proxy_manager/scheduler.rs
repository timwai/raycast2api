//! Independent limits keep slow downloads/tests from occupying mutation capacity.
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobClass {
    Download,
    Test,
    Control,
    Batch,
}
impl JobClass {
    pub fn limit(self, config: &super::config::ProxyManagerConfig) -> usize {
        match self {
            Self::Download => config.subscription_concurrency,
            Self::Test => config.manual_test_concurrency,
            Self::Control => 1,
            Self::Batch => 4,
        }
    }
}
/// Add up to 10%, capped at five minutes, after the normal refresh interval.
pub fn refresh_due(base: u64, interval: u64) -> u64 {
    let window = (interval / 10).clamp(1, 300);
    let mut bytes = [0u8; 8];
    getrandom::getrandom(&mut bytes).expect("OS random source");
    base.saturating_add(interval)
        .saturating_add(1 + u64::from_le_bytes(bytes) % window)
}
pub const POLL_INTERVAL: Duration = Duration::from_millis(100);

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn randomized_refresh_is_persistent_ready_and_never_early() {
        let times = (0..64)
            .map(|_| refresh_due(1000, 21600))
            .collect::<std::collections::HashSet<_>>();
        assert!(times.len() > 1);
        assert!(times.iter().all(|t| *t > 22600 && *t <= 22900));
        for _ in 0..32 {
            let t = refresh_due(1000, 300);
            assert!((1301..=1330).contains(&t));
        }
    }
}
