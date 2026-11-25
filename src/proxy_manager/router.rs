use super::{error, model::Snapshot};
use crate::error::GatewayError;
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
use tokio::sync::{Notify, OwnedRwLockReadGuard, OwnedRwLockWriteGuard};

type Pair = (reqwest::Client, reqwest::Client);
pub struct AccountProxyRouter {
    snapshot: RwLock<Snapshot>,
    gate: Arc<tokio::sync::RwLock<()>>,
    account_gates: std::sync::Mutex<HashMap<i64, Arc<tokio::sync::RwLock<()>>>>,
    affected: RwLock<HashSet<i64>>,
    pub(crate) applying: AtomicBool,
    ready: AtomicBool,
    halted: AtomicBool,
    enabled: bool,
    epoch: AtomicU64,
    timeout: std::time::Duration,
    active: std::sync::Mutex<Vec<std::sync::Weak<LeaseInner>>>,
    clients: std::sync::Mutex<HashMap<(i64, u64, u64), Pair>>,
}
struct LeaseInner {
    _guard: OwnedRwLockReadGuard<()>,
    cancelled: AtomicBool,
    notify: Notify,
    critical: bool,
    account_id: i64,
    bound: bool,
}
pub(crate) struct RouteBarrier {
    _bound: OwnedRwLockWriteGuard<()>,
    _accounts: Vec<OwnedRwLockWriteGuard<()>>,
}
#[derive(Clone)]
pub struct AccountTransportLease {
    inner: Arc<LeaseInner>,
    pub account_id: i64,
    pub generation: u64,
    pub core_epoch: u64,
    pub binding_revision: Option<u64>,
    pub got: Option<reqwest::Client>,
    pub chat: Option<reqwest::Client>,
}
impl AccountProxyRouter {
    pub fn new(snapshot: Snapshot, enabled: bool, timeout: std::time::Duration) -> Self {
        Self {
            snapshot: RwLock::new(snapshot),
            gate: Arc::new(tokio::sync::RwLock::new(())),
            account_gates: Default::default(),
            affected: Default::default(),
            applying: AtomicBool::new(false),
            ready: AtomicBool::new(false),
            halted: AtomicBool::new(false),
            enabled,
            epoch: AtomicU64::new(0),
            timeout,
            active: Default::default(),
            clients: Default::default(),
        }
    }
    pub(crate) fn subscriptions(&self) -> Vec<super::model::Subscription> {
        self.snapshot
            .read()
            .expect("proxy snapshot")
            .subscriptions
            .values()
            .cloned()
            .collect()
    }
    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.read().expect("proxy snapshot").clone()
    }
    pub(crate) fn publish_metadata(&self, state: Snapshot) {
        *self.snapshot.write().expect("proxy snapshot") = state;
    }
    pub(crate) fn publish(&self, state: Snapshot, ready: bool) {
        *self.snapshot.write().expect("proxy snapshot") = state;
        self.clients.lock().expect("proxy clients").clear();
        self.epoch.fetch_add(1, Ordering::SeqCst);
        self.ready.store(ready, Ordering::SeqCst);
    }
    pub(crate) fn unavailable(&self) {
        self.ready.store(false, Ordering::SeqCst);
        self.cancel_normal();
    }
    pub(crate) fn core_epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }
    pub fn ready(&self) -> bool {
        self.enabled && self.ready.load(Ordering::SeqCst)
    }
    fn account_gate(&self, id: i64) -> Arc<tokio::sync::RwLock<()>> {
        self.account_gates
            .lock()
            .expect("account gates")
            .entry(id)
            .or_insert_with(|| Arc::new(tokio::sync::RwLock::new(())))
            .clone()
    }
    pub(crate) fn begin_shutdown(&self) {
        self.halted.store(true, Ordering::SeqCst);
        let accounts = self
            .account_gates
            .lock()
            .expect("account gates")
            .keys()
            .copied()
            .collect();
        self.begin_apply(accounts);
        self.cancel_normal();
    }
    pub(crate) fn begin_apply(&self, accounts: Vec<i64>) {
        *self.affected.write().expect("affected accounts") = accounts.into_iter().collect();
        self.applying.store(true, Ordering::SeqCst);
    }
    pub(crate) fn end_apply(&self) {
        self.applying.store(false, Ordering::SeqCst);
        self.affected.write().expect("affected accounts").clear();
    }
    pub(crate) async fn drain(&self) -> RouteBarrier {
        let accounts = self
            .affected
            .read()
            .expect("affected accounts")
            .iter()
            .map(|id| self.account_gate(*id))
            .collect::<Vec<_>>();
        let bound = self.gate.clone().write_owned().await;
        let mut guards = Vec::new();
        for gate in accounts {
            guards.push(gate.write_owned().await);
        }
        self.clients.lock().expect("proxy clients").clear();
        RouteBarrier {
            _bound: bound,
            _accounts: guards,
        }
    }
    pub fn lease(
        &self,
        account_id: i64,
        critical: bool,
    ) -> Result<AccountTransportLease, GatewayError> {
        if self.halted.load(Ordering::SeqCst) {
            return Err(error("proxy_unavailable", "gateway is shutting down"));
        }
        let state = self.snapshot.read().expect("proxy snapshot");
        let binding = state.bindings.get(&account_id);
        if self.applying.load(Ordering::SeqCst)
            && (binding.is_some()
                || self
                    .affected
                    .read()
                    .expect("affected accounts")
                    .contains(&account_id))
        {
            return Err(error(
                "proxy_applying",
                "proxy configuration is applying; retry after completion",
            ));
        }
        let gate = if binding.is_some() {
            self.gate.clone()
        } else {
            self.account_gate(account_id)
        };
        let guard = gate
            .try_read_owned()
            .map_err(|_| error("proxy_applying", "proxy configuration is applying"))?;
        if self.halted.load(Ordering::SeqCst) {
            return Err(error("proxy_unavailable", "gateway is shutting down"));
        }
        if let Some(b) = binding {
            if !self.enabled {
                return Err(error(
                    "proxy_manager_disabled",
                    "account has a binding but proxy manager is disabled",
                ));
            }
            if !self.ready.load(Ordering::SeqCst) {
                return Err(error(
                    "proxy_unavailable",
                    "account proxy core is unavailable",
                ));
            }
            if !state.route_active(b) {
                return Err(error(
                    "proxy_route_blocked",
                    "account proxy node is missing or its subscription is disabled",
                ));
            }
        }
        let epoch = self.epoch.load(Ordering::SeqCst);
        let (got, chat) = if let Some(b) = binding {
            let mut clients = self.clients.lock().expect("proxy clients");
            let key = (account_id, b.revision, epoch);
            if let std::collections::hash_map::Entry::Vacant(e) = clients.entry(key) {
                let build = |transport| {
                    crate::proxies::bound_client(b.listener_port, self.timeout, transport).map_err(
                        |_| {
                            error(
                                "proxy_unavailable",
                                "account transport initialization failed",
                            )
                        },
                    )
                };
                e.insert((
                    build(crate::proxies::Transport::Got)?,
                    build(crate::proxies::Transport::ChatFetch)?,
                ));
            }
            let (got, chat) = clients[&key].clone();
            (Some(got), Some(chat))
        } else {
            (None, None)
        };
        let inner = Arc::new(LeaseInner {
            _guard: guard,
            cancelled: AtomicBool::new(false),
            notify: Notify::new(),
            critical,
            account_id,
            bound: binding.is_some(),
        });
        let mut active = self.active.lock().expect("proxy leases");
        active.retain(|x| x.strong_count() > 0);
        active.push(Arc::downgrade(&inner));
        Ok(AccountTransportLease {
            inner,
            account_id,
            generation: state.generation,
            core_epoch: epoch,
            binding_revision: binding.map(|b| b.revision),
            got,
            chat,
        })
    }
    pub(crate) fn cancel_normal(&self) {
        let affected = self.affected.read().expect("affected accounts");
        for lease in self
            .active
            .lock()
            .expect("proxy leases")
            .iter()
            .filter_map(std::sync::Weak::upgrade)
        {
            if !lease.critical && (lease.bound || affected.contains(&lease.account_id)) {
                lease.cancelled.store(true, Ordering::SeqCst);
                lease.notify.notify_waiters();
            }
        }
    }
}
impl AccountTransportLease {
    pub fn bound(&self) -> bool {
        self.binding_revision.is_some()
    }
    pub async fn cancelled(&self) {
        loop {
            let notified = self.inner.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.inner.cancelled.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }
    pub fn check(&self) -> Result<(), GatewayError> {
        if self.inner.cancelled.load(Ordering::SeqCst) {
            Err(error(
                "proxy_applying",
                "request interrupted for proxy configuration update",
            ))
        } else {
            Ok(())
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn apply_blocks_affected_accounts_but_preserves_unbound_and_critical_leases() {
        let router =
            AccountProxyRouter::new(Snapshot::default(), true, std::time::Duration::from_secs(1));
        let normal = router.lease(1, false).unwrap();
        let oauth = router.lease(1, true).unwrap();
        let other = router.lease(2, false).unwrap();
        router.begin_apply(vec![1]);
        assert!(router.lease(1, false).is_err());
        assert!(router.lease(2, false).is_ok());
        router.cancel_normal();
        assert!(normal.check().is_err());
        assert!(oauth.check().is_ok());
        assert!(other.check().is_ok());
        drop(normal);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), router.drain())
                .await
                .is_err()
        );
        drop(oauth);
        let guard = router.drain().await;
        router.end_apply();
        drop(guard);
        assert!(router.lease(1, false).is_ok());
    }
}
