//! Independent limits keep slow downloads/tests from occupying mutation capacity.
use std::{sync::Arc, time::Duration};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobClass {
    Download,
    Test,
    Control,
    Batch,
}
pub struct Limits {
    downloads: Arc<Semaphore>,
    tests: Arc<Semaphore>,
    control: Arc<Semaphore>,
    batches: Arc<Semaphore>,
}
impl Limits {
    pub fn new(downloads: usize, tests: usize) -> Self {
        Self {
            downloads: Arc::new(Semaphore::new(downloads)),
            tests: Arc::new(Semaphore::new(tests)),
            control: Arc::new(Semaphore::new(1)),
            batches: Arc::new(Semaphore::new(4)),
        }
    }
    pub fn try_acquire(&self, class: JobClass) -> Option<OwnedSemaphorePermit> {
        let semaphore = match class {
            JobClass::Download => &self.downloads,
            JobClass::Test => &self.tests,
            JobClass::Control => &self.control,
            JobClass::Batch => &self.batches,
        };
        semaphore.clone().try_acquire_owned().ok()
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
    fn limits_are_independent_and_return_capacity_on_completion() {
        let limits = Limits::new(2, 3);
        let a = limits.try_acquire(JobClass::Download).unwrap();
        let _b = limits.try_acquire(JobClass::Download).unwrap();
        assert!(limits.try_acquire(JobClass::Download).is_none());
        let _control = limits.try_acquire(JobClass::Control).unwrap();
        assert!(limits.try_acquire(JobClass::Control).is_none());
        let tests = (0..3)
            .map(|_| limits.try_acquire(JobClass::Test).unwrap())
            .collect::<Vec<_>>();
        assert!(limits.try_acquire(JobClass::Test).is_none());
        drop(a);
        assert!(limits.try_acquire(JobClass::Download).is_some());
        drop(tests);
        assert!(limits.try_acquire(JobClass::Test).is_some());
    }
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
