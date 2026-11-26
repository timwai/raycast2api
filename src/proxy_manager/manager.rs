use super::{
    config::ProxyManagerConfig,
    crypto::Crypto,
    error, id,
    mihomo::Core,
    model::*,
    router::AccountProxyRouter,
    scheduler::{self, JobClass},
    settings::{ProxySettings, SettingsRecord, TestSchedule},
    store::Store,
    subscription,
};
use crate::{config::Settings, error::GatewayError, storage::db::Db};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Mutex, Notify, RwLock};
#[path = "automation.rs"]
mod automation;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubscriptionInput {
    pub name: Option<String>,
    pub url: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    pub refresh_interval_secs: Option<u64>,
}
fn default_enabled() -> bool {
    true
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    CreateSubscriptions {
        items: Vec<SubscriptionInput>,
    },
    RefreshAll,
    ReconcileBindings,
    UpdateSettings {
        expected_revision: u64,
        settings: ProxySettings,
    },
    TestAllNodes,
    TestNode {
        id: String,
    },
    TestNodes {
        ids: Vec<String>,
    },
    CreateSubscription {
        name: String,
        url: String,
        enabled: bool,
        refresh_interval_secs: u64,
    },
    UpdateSubscription {
        id: String,
        expected_revision: u64,
        name: Option<String>,
        url: Option<String>,
        enabled: Option<bool>,
        refresh_interval_secs: Option<u64>,
    },
    DeleteSubscription {
        id: String,
        expected_revision: u64,
    },
    Refresh {
        id: String,
    },
    SetBinding {
        email: String,
        node_id: String,
        expected_revision: u64,
    },
    DeleteBinding {
        email: String,
        expected_revision: u64,
    },
    DeleteAccounts {
        ids: Vec<i64>,
    },
}
impl Action {
    fn resource(&self) -> Option<String> {
        match self {
            Self::Refresh { id } => Some(format!("refresh:{id}")),
            Self::TestNode { id } => Some(format!("test:{id}")),
            Self::RefreshAll => Some("refresh-all".into()),
            Self::ReconcileBindings => Some("reconcile-bindings".into()),
            Self::TestAllNodes => Some("test-all".into()),
            Self::TestNodes { ids } => {
                use sha2::Digest;
                let mut ids = ids.clone();
                ids.sort();
                ids.dedup();
                Some(format!(
                    "test-batch:{}",
                    hex::encode(sha2::Sha256::digest(
                        serde_json::to_vec(&ids).expect("string array")
                    ))
                ))
            }
            _ => None,
        }
    }
    fn class(&self) -> JobClass {
        match self {
            Self::Refresh { .. } => JobClass::Download,
            Self::TestNode { .. } => JobClass::Test,
            Self::RefreshAll | Self::TestNodes { .. } | Self::TestAllNodes => JobClass::Batch,
            _ => JobClass::Control,
        }
    }
}
pub struct ProxyManager {
    settings: Arc<Settings>,
    preferences: std::sync::RwLock<SettingsRecord>,
    store: Store,
    crypto: Option<Crypto>,
    pub router: Arc<AccountProxyRouter>,
    core: Mutex<Option<Core>>,
    apply: Mutex<()>,
    queue: Mutex<()>,
    control_gate: RwLock<()>,
    wake: Notify,
    stopping: AtomicBool,
    started: AtomicBool,
    _database_lock: Option<std::fs::File>,
}
impl ProxyManager {
    pub fn new(settings: Arc<Settings>, db: Db) -> Result<Arc<Self>, String> {
        let crypto = if settings.proxy_manager.enabled {
            Some(Crypto::from_env()?)
        } else {
            None
        };
        Self::with_crypto(settings, db, crypto)
    }
    fn with_crypto(
        settings: Arc<Settings>,
        db: Db,
        crypto: Option<Crypto>,
    ) -> Result<Arc<Self>, String> {
        let database_lock = if settings.accounts_db == ":memory:" {
            None
        } else {
            use fs2::FileExt;
            let database = std::fs::canonicalize(&settings.accounts_db)
                .map_err(|_| "cannot locate proxy database lock")?;
            let path = std::path::PathBuf::from(format!(
                "{}.proxy-manager.lock",
                database.to_string_lossy()
            ));
            let mut options = std::fs::OpenOptions::new();
            options.read(true).write(true).create(true).truncate(false);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let file = options
                .open(path)
                .map_err(|_| "cannot create proxy database ownership lock")?;
            file.try_lock_exclusive()
                .map_err(|_| "another gateway instance already owns this proxy database")?;
            Some(file)
        };
        let store = Store::new(db)?;
        let preferences = store
            .preference::<SettingsRecord>("settings")
            .map_err(|_| "cannot read proxy settings")?
            .unwrap_or_else(|| SettingsRecord {
                revision: 0,
                settings: ProxySettings::from_config(&settings.proxy_manager),
            });
        preferences.settings.apply(&settings.proxy_manager)?;
        let mut snapshot = store
            .load()
            .map_err(|_| "proxy snapshot could not be loaded")?;
        if let Some(c) = &crypto {
            for s in snapshot.subscriptions.values() {
                c.open(&format!("subscription:{}", s.id), &s.url_ciphertext)?;
            }
            for n in snapshot.nodes.values() {
                c.open(&format!("node:{}", n.id), &n.config_ciphertext)?;
            }
        }
        let mut schedules_changed = false;
        for sub in snapshot.subscriptions.values_mut() {
            if sub.next_refresh_at.is_none() {
                sub.next_refresh_at = Some(scheduler::refresh_due(
                    sub.last_attempt_at
                        .or(sub.last_success_at)
                        .unwrap_or(sub.created_at),
                    sub.refresh_interval_secs,
                ));
                schedules_changed = true;
            }
        }
        if schedules_changed {
            store
                .persist_schedules(&snapshot)
                .map_err(|_| "subscription schedules could not be initialized")?;
        }
        let router = Arc::new(AccountProxyRouter::new(
            snapshot,
            settings.proxy_manager.enabled,
            settings.upstream_timeout(),
        ));
        Ok(Arc::new(Self {
            settings,
            preferences: std::sync::RwLock::new(preferences),
            store,
            crypto,
            router,
            core: Mutex::new(None),
            apply: Mutex::new(()),
            queue: Mutex::new(()),
            control_gate: RwLock::new(()),
            wake: Notify::new(),
            stopping: AtomicBool::new(false),
            started: AtomicBool::new(false),
            _database_lock: database_lock,
        }))
    }
    pub async fn start(self: &Arc<Self>) -> Result<(), GatewayError> {
        if self.started.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        if self.config().enabled {
            let mut core = Core::new(&self.config())?;
            let snapshot = self.router.snapshot();
            let path = core.generate(&snapshot, self.crypto()?)?;
            core.validate(&path).await?;
            core.start(&path, &snapshot).await?;
            *self.core.lock().await = Some(core);
            self.router.publish(snapshot, true);
        }
        self.store.release_reservations()?;
        for mut op in self.store.pending()? {
            if !matches!(op.phase.as_str(), "queued" | "preparing") {
                op.phase = "rolled_back".into();
                op.code = Some("proxy_recovered_previous_generation".into());
                op.candidate = None;
                op.updated_at = now();
                self.store.save_operation(&op)?;
            } else if op.phase == "preparing" {
                op.phase = "queued".into();
                self.store.save_operation(&op)?;
            }
        }
        self.spawn_tasks();
        Ok(())
    }
    fn spawn_tasks(self: &Arc<Self>) {
        let manager = self.clone();
        tokio::spawn(async move {
            manager.worker().await;
        });
        let manager = self.clone();
        tokio::spawn(async move {
            manager.maintenance().await;
        });
    }
    pub fn config(&self) -> ProxyManagerConfig {
        self.preferences
            .read()
            .expect("proxy preferences")
            .settings
            .apply(&self.settings.proxy_manager)
            .expect("validated proxy settings")
    }
    pub fn settings_view(&self) -> SettingsRecord {
        self.preferences.read().expect("proxy preferences").clone()
    }
    pub async fn import_accounts(
        self: &Arc<Self>,
        accounts: &crate::account::store::AccountStore,
        records: &[crate::account::wire::AccountRecord],
    ) -> Result<Vec<crate::account::store::UpsertOutcome>, GatewayError> {
        let _apply = self.apply.lock().await;
        let cfg = self.config();
        let result = accounts
            .upsert_batch_with_auto_binding(records, cfg.enabled && cfg.auto_bind_new_accounts)?;
        self.wake.notify_one();
        Ok(result)
    }
    pub(crate) fn ensure_account_assignment(&self, account_id: i64) -> Result<(), GatewayError> {
        if !self.router.snapshot().bindings.contains_key(&account_id)
            && self.store.pending_auto_accounts()?.contains(&account_id)
        {
            return Err(error(
                "proxy_assignment_pending",
                "账号正在等待可用的空闲代理节点，请先测速、添加节点或手动选择出口。",
            ));
        }
        Ok(())
    }
    pub fn node_binding_labels(
        &self,
    ) -> Result<std::collections::BTreeMap<String, Vec<String>>, GatewayError> {
        let accounts = self.store.account_labels()?;
        let mut labels = std::collections::BTreeMap::<String, Vec<String>>::new();
        for binding in self.router.snapshot().bindings.values() {
            if let Some(email) = accounts.get(&binding.account_id) {
                labels
                    .entry(binding.node_id.clone())
                    .or_default()
                    .push(email.clone());
            }
        }
        Ok(labels)
    }
    fn crypto(&self) -> Result<&Crypto, GatewayError> {
        self.crypto.as_ref().ok_or_else(|| {
            error(
                "proxy_manager_disabled",
                "enable proxy manager and provide its encryption key",
            )
        })
    }
    pub fn account_id(&self, email: &str) -> Result<i64, GatewayError> {
        self.store.account_id(email)
    }
    pub fn status(&self) -> Value {
        let s = self.router.snapshot();
        json!({"enabled":self.config().enabled,"ready":self.router.ready(),"applying":self.router.applying.load(Ordering::SeqCst),"generation":s.generation,
            "core_version":self.config().expected_version,"bindings":s.bindings.len(),"active_bindings":s.bindings.values().filter(|b|self.router.ready()&&s.route_active(b)).count()})
    }
    pub fn snapshot(&self) -> Snapshot {
        self.router.snapshot().public()
    }
    pub fn binding_view(&self, email: &str) -> Result<Option<Value>, GatewayError> {
        let id = self.account_id(email)?;
        let s = self.router.snapshot();
        let Some(b) = s.bindings.get(&id) else {
            return Ok(if self.store.pending_auto_accounts()?.contains(&id) {
                Some(
                    json!({"node_id":null,"node_name":"等待分配","status":"pending","reason":"proxy_assignment_pending","revision":0}),
                )
            } else {
                None
            });
        };
        let n = s.nodes.get(&b.node_id);
        let sub = n.and_then(|n| s.subscriptions.get(&n.subscription_id));
        let reason = if !self.config().enabled {
            Some("proxy_manager_disabled")
        } else if !self.router.ready() {
            Some("proxy_unavailable")
        } else if !s.route_active(b) {
            Some("proxy_route_blocked")
        } else {
            None
        };
        Ok(Some(
            json!({"node_id":b.node_id,"node_name":n.map(|n|&n.original_name),"subscription_name":sub.map(|s|&s.name),"listener_port":b.listener_port,"revision":b.revision,"generation":s.generation,
            "status":if self.router.applying.load(Ordering::SeqCst){"applying"}else if reason.is_some(){"blocked"}else{"active"},"reason":reason,
            "automatic_reason":b.automatic_reason,"previous_node_id":b.previous_node_id,"updated_at":b.updated_at,
            "auto_failover_pending":self.config().auto_failover_enabled && !s.route_active(b),
            "test_status":n.map(|n|n.test_status()),"delay_ms":n.and_then(|n|n.delay_ms),"delay_checked_at":n.and_then(|n|n.delay_checked_at),"delay_error_code":n.and_then(|n|n.delay_error_code.as_deref())}),
        ))
    }
    pub fn bindings(&self) -> Result<Vec<Value>, GatewayError> {
        let s = self.router.snapshot();
        s.bindings
            .values()
            .map(|b| {
                let email = self.store.account_email(b.account_id)?.unwrap_or_default();
                let mut view = self.binding_view(&email)?.unwrap_or(json!({}));
                view["email"] = json!(email);
                Ok(view)
            })
            .collect()
    }
    pub async fn enqueue(
        self: &Arc<Self>,
        action: Action,
        key: Option<String>,
    ) -> Result<Operation, GatewayError> {
        use sha2::Digest;
        let key = key.map(|key| {
            format!(
                "internal:{}",
                hex::encode(sha2::Sha256::digest(key.as_bytes()))
            )
        });
        self.enqueue_request(action, key).await
    }
    pub async fn enqueue_request(
        self: &Arc<Self>,
        action: Action,
        key: Option<String>,
    ) -> Result<Operation, GatewayError> {
        if self.stopping.load(Ordering::SeqCst) {
            return Err(error("proxy_unavailable", "proxy manager is shutting down"));
        }
        if !self.config().enabled
            && !matches!(
                action,
                Action::DeleteBinding { .. } | Action::DeleteAccounts { .. }
            )
        {
            return Err(error("proxy_manager_disabled", "proxy manager is disabled"));
        }
        if !self.started.load(Ordering::SeqCst) {
            if self.config().enabled {
                return Err(error(
                    "proxy_unavailable",
                    "proxy manager startup has not completed",
                ));
            }
            if !self.started.swap(true, Ordering::SeqCst) {
                self.spawn_tasks();
            }
        }
        let bytes = serde_json::to_vec(&action)
            .map_err(|_| error("proxy_invalid", "invalid proxy action"))?;
        use sha2::Digest;
        let hash = hex::encode(sha2::Sha256::digest(&bytes));
        let _lock = self.queue.lock().await;
        if let Some(key) = key.as_deref() {
            if key.len() > 200 {
                return Err(GatewayError::BadRequest("idempotency key too long".into()));
            }
            if let Some(existing) = self.store.idempotent(key, &hash)? {
                return Ok(public_operation(existing));
            }
        }
        let resource = action.resource();
        if let Some(resource) = &resource
            && let Some(existing) = self.store.pending()?.into_iter().find(|op| {
                op.resource.as_ref() == Some(resource) && op.phase != "recovery_required"
            })
        {
            if let Some(key) = key.as_deref() {
                self.store.remember_key(key, &hash, &existing.id)?;
            }
            return Ok(public_operation(existing));
        }
        let op_id = id();
        let sealed = if let Some(c) = &self.crypto {
            c.seal(&format!("operation:{op_id}"), &bytes)
                .map_err(|_| error("proxy_encryption_failed", "operation encryption failed"))?
        } else {
            // Disabled-mode deletion contains identifiers only. Never persist a plaintext subscription action.
            format!(
                "local:{}",
                String::from_utf8(bytes)
                    .map_err(|_| error("proxy_invalid", "invalid operation"))?
            )
        };
        let op = Operation {
            id: op_id,
            phase: "queued".into(),
            code: None,
            message: None,
            generation: None,
            revision: None,
            created_at: now(),
            updated_at: now(),
            candidate: None,
            action_ciphertext: Some(sealed),
            idempotency_key: None,
            request_hash: Some(hash.clone()),
            resource,
            result: None,
        };
        self.store.insert_operation(&op, key.as_deref(), &hash)?;
        self.wake.notify_one();
        Ok(public_operation(op))
    }
    pub fn operation(&self, id: &str) -> Result<Operation, GatewayError> {
        Ok(public_operation(self.store.operation(id)?))
    }
    fn decode_action(&self, op: &Operation) -> Result<Action, GatewayError> {
        let stored = op.action_ciphertext.as_deref().unwrap_or("");
        let local = stored.strip_prefix("local:");
        let bytes = if let Some(local) = local {
            local.as_bytes().to_vec()
        } else {
            self.crypto()?
                .open(&format!("operation:{}", op.id), stored)
                .map_err(|_| error("proxy_decryption_failed", "operation cannot be decrypted"))?
        };
        let action: Action = serde_json::from_slice(&bytes)
            .map_err(|_| error("proxy_invalid", "invalid stored operation"))?;
        if local.is_some()
            && !matches!(
                action,
                Action::DeleteBinding { .. } | Action::DeleteAccounts { .. }
            )
        {
            return Err(error(
                "proxy_invalid",
                "unencrypted operation is not an identifier-only deletion",
            ));
        }
        Ok(action)
    }
    async fn worker(self: Arc<Self>) {
        use std::collections::{HashMap, HashSet};
        let mut tasks = tokio::task::JoinSet::new();
        let mut active = HashMap::new();
        let mut resources = HashSet::new();
        loop {
            while let Some(completed) = tasks.try_join_next_with_id() {
                let task_id = match completed {
                    Ok((id, ())) => id,
                    Err(error) => {
                        tracing::error!("proxy task terminated unexpectedly");
                        error.id()
                    }
                };
                if let Some((_, Some(resource), _)) = active.remove(&task_id) {
                    resources.remove(&resource);
                }
            }
            if self.stopping.load(Ordering::SeqCst) && tasks.is_empty() {
                break;
            }
            if !self.stopping.load(Ordering::SeqCst) {
                for mut op in self
                    .store
                    .pending()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|op| op.phase == "queued")
                {
                    if active.values().any(|(id, _, _)| id == &op.id) {
                        continue;
                    }
                    let action = match self.decode_action(&op) {
                        Ok(action) => action,
                        Err(e) => {
                            self.fail_operation(&mut op, e).await;
                            continue;
                        }
                    };
                    let resource = action.resource();
                    if resource.as_ref().is_some_and(|key| resources.contains(key)) {
                        continue;
                    }
                    let class = action.class();
                    if active.values().filter(|(_, _, job)| *job == class).count()
                        >= class.limit(&self.config())
                    {
                        continue;
                    }
                    if let Some(resource) = &resource {
                        resources.insert(resource.clone());
                    }
                    let operation_id = op.id.clone();
                    let manager = self.clone();
                    let handle = tasks.spawn(async move {
                        manager.run_operation(op).await;
                    });
                    active.insert(handle.id(), (operation_id, resource, class));
                }
            }
            tokio::select! {_=self.wake.notified()=>{},_=tokio::time::sleep(scheduler::POLL_INTERVAL)=>{}}
        }
    }
    async fn run_operation(self: &Arc<Self>, mut op: Operation) {
        use futures_util::FutureExt;
        let sealed = op.action_ciphertext.clone();
        let outcome = std::panic::AssertUnwindSafe(self.execute(&mut op))
            .catch_unwind()
            .await;
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(_) => {
                if matches!(op.phase.as_str(), "prepared" | "runtime_applied")
                    && self.router.applying.load(Ordering::SeqCst)
                {
                    op.phase = "recovery_required".into();
                    self.router.unavailable();
                }
                Err(error(
                    "proxy_operation_failed",
                    "proxy task terminated unexpectedly",
                ))
            }
        };
        if let Err(e) = outcome {
            if op.action_ciphertext.is_none() {
                op.action_ciphertext = sealed;
            }
            self.fail_operation(&mut op, e).await;
        } else {
            for account_id in self.router.snapshot().bindings.keys() {
                let _ = self.store.cancel_auto_account(*account_id);
            }
            let created = op
                .result
                .as_ref()
                .map(|result| {
                    let mut ids = Vec::new();
                    if let Some(id) = result["subscription_id"].as_str() {
                        ids.push(id.to_string());
                    }
                    if let Some(items) = result["items"].as_array() {
                        ids.extend(
                            items
                                .iter()
                                .filter_map(|i| i["subscription_id"].as_str().map(str::to_string)),
                        );
                    }
                    ids
                })
                .unwrap_or_default();
            for sub_id in created {
                if self
                    .router
                    .snapshot()
                    .subscriptions
                    .get(&sub_id)
                    .is_some_and(|s| s.enabled)
                {
                    let _ = self
                        .enqueue(
                            Action::Refresh { id: sub_id.clone() },
                            Some(format!("initial-refresh:{sub_id}")),
                        )
                        .await;
                }
            }
        }
    }
    async fn fail_operation(&self, op: &mut Operation, e: GatewayError) {
        // Only the operation whose runtime application failed can require recovery.
        op.phase = if op.phase == "recovery_required" {
            "recovery_required"
        } else {
            "failed"
        }
        .into();
        op.code = Some(match &e {
            GatewayError::Admission { code, .. } => (*code).into(),
            GatewayError::Conflict(_) => "revision_conflict".into(),
            GatewayError::BadRequest(_) => "proxy_invalid".into(),
            GatewayError::NotFound(_) => "proxy_object_missing".into(),
            _ => "proxy_operation_failed".into(),
        });
        op.message = Some(e.to_string());
        op.candidate = None;
        op.updated_at = now();
        let _ = self.store.save_operation(op);
        if op.phase != "recovery_required"
            && let Ok(Action::Refresh { id }) = self.decode_action(op)
        {
            let _apply = self.apply.lock().await;
            let mut snapshot = self.router.snapshot();
            if let Some(sub) = snapshot.subscriptions.get_mut(&id) {
                if op.code.as_deref() != Some("revision_conflict") {
                    sub.last_attempt_at = Some(now());
                    sub.last_error_code = op.code.clone();
                }
                if op.code.as_deref() != Some("revision_conflict")
                    || sub.next_refresh_at.is_none_or(|due| due <= now())
                {
                    sub.next_refresh_at =
                        Some(scheduler::refresh_due(now(), sub.refresh_interval_secs));
                }
                if self.store.commit(&snapshot, op, &[]).is_ok() {
                    self.router.publish_metadata(snapshot);
                }
            }
        }
        let _ = self.store.release_operation_reservations(&op.id);
        self.wake.notify_one();
    }
    async fn execute(self: &Arc<Self>, op: &mut Operation) -> Result<(), GatewayError> {
        op.phase = "preparing".into();
        op.updated_at = now();
        self.store.save_operation(op)?;
        let action = self.decode_action(op)?;
        if matches!(
            action,
            Action::RefreshAll | Action::TestNodes { .. } | Action::TestAllNodes
        ) {
            return self.execute_batch(op, &action).await;
        }
        if let Action::TestNode { id } = &action {
            return self.execute_test(op, id).await;
        }
        // Subscription downloads do not hold the application lock.
        let refresh_data = if let Action::Refresh { id } = &action {
            let state = self.router.snapshot();
            let sub = state
                .subscriptions
                .get(id)
                .ok_or_else(|| GatewayError::NotFound("subscription does not exist".into()))?;
            if !sub.enabled {
                return Err(GatewayError::Conflict("subscription is disabled".into()));
            }
            let url = self
                .crypto()?
                .open(&format!("subscription:{id}"), &sub.url_ciphertext)
                .map_err(|_| {
                    error(
                        "proxy_decryption_failed",
                        "subscription cannot be decrypted",
                    )
                })?;
            let data = subscription::download(
                std::str::from_utf8(&url)
                    .map_err(|_| error("subscription_invalid", "invalid subscription URL"))?,
                &{
                    let mut settings = (*self.settings).clone();
                    settings.proxy_manager = self.config();
                    settings
                },
            )
            .await?;
            Some((sub.revision, data))
        } else {
            None
        };
        let _apply = self.apply.lock().await;
        if self.stopping.load(Ordering::SeqCst) {
            return Err(error("proxy_unavailable", "proxy manager is shutting down"));
        }
        if self.router.applying.load(Ordering::SeqCst) {
            return Err(error(
                "proxy_recovery_required",
                "previous operation requires recovery",
            ));
        }
        if let Action::UpdateSettings {
            expected_revision,
            settings,
        } = &action
        {
            return self
                .update_settings_locked(op, *expected_revision, settings)
                .await;
        }
        let old = self.router.snapshot();
        let mut next = old.clone();
        let mut delete_ids = Vec::new();
        match &action {
            Action::UpdateSettings { .. } => unreachable!("handled above"),
            Action::ReconcileBindings => self.reconcile_bindings(&mut next, op)?,
            Action::CreateSubscriptions { items } => {
                if items.is_empty() || items.len() > 50 {
                    return Err(GatewayError::BadRequest(
                        "batch must contain 1 to 50 subscriptions".into(),
                    ));
                }
                let mut seen = std::collections::HashSet::new();
                let mut results = Vec::new();
                for (index, item) in items.iter().enumerate() {
                    let interval = item
                        .refresh_interval_secs
                        .unwrap_or(self.config().refresh_interval_secs);
                    let name = item
                        .name
                        .as_deref()
                        .filter(|name| !name.trim().is_empty())
                        .map(str::to_string)
                        .unwrap_or_else(|| {
                            let host = reqwest::Url::parse(&item.url)
                                .ok()
                                .and_then(|u| u.host_str().map(str::to_string))
                                .unwrap_or_else(|| "订阅".into());
                            format!("{host} {}", index + 1)
                        });
                    let validation = if !seen.insert(item.url.trim().to_string()) {
                        Err(GatewayError::BadRequest(
                            "duplicate subscription URL in this batch".into(),
                        ))
                    } else {
                        validate_subscription(&name, &item.url, interval)
                    };
                    if let Err(e) = validation {
                        results.push(json!({"index":index,"success":false,"code":"subscription_invalid","message":e.to_string()}));
                        continue;
                    }
                    let sub_id = id();
                    let t = now();
                    let ciphertext = self
                        .crypto()?
                        .seal(&format!("subscription:{sub_id}"), item.url.as_bytes())
                        .map_err(|_| error("proxy_encryption_failed", "URL encryption failed"))?;
                    next.subscriptions.insert(
                        sub_id.clone(),
                        Subscription {
                            id: sub_id.clone(),
                            name: name.clone(),
                            url_ciphertext: ciphertext,
                            enabled: item.enabled,
                            refresh_interval_secs: interval,
                            revision: 1,
                            next_refresh_at: Some(scheduler::refresh_due(t, interval)),
                            last_attempt_at: None,
                            last_success_at: None,
                            last_error_code: None,
                            created_at: t,
                            updated_at: t,
                        },
                    );
                    results.push(
                        json!({"index":index,"name":name,"subscription_id":sub_id,"success":true}),
                    );
                }
                op.result = Some(
                    json!({"items":results,"succeeded":results.iter().filter(|r|r["success"]==true).count(),"total":results.len()}),
                );
            }
            Action::RefreshAll
            | Action::TestNodes { .. }
            | Action::TestNode { .. }
            | Action::TestAllNodes => {
                unreachable!("handled before configuration application")
            }
            Action::CreateSubscription {
                name,
                url,
                enabled,
                refresh_interval_secs,
            } => {
                validate_subscription(name, url, *refresh_interval_secs)?;
                let sub_id = id();
                let t = now();
                next.subscriptions.insert(
                    sub_id.clone(),
                    Subscription {
                        id: sub_id.clone(),
                        name: name.clone(),
                        url_ciphertext: self
                            .crypto()?
                            .seal(&format!("subscription:{sub_id}"), url.as_bytes())
                            .map_err(|_| {
                                error("proxy_encryption_failed", "URL encryption failed")
                            })?,
                        enabled: *enabled,
                        refresh_interval_secs: *refresh_interval_secs,
                        revision: 1,
                        next_refresh_at: Some(scheduler::refresh_due(t, *refresh_interval_secs)),
                        last_attempt_at: None,
                        last_success_at: None,
                        last_error_code: None,
                        created_at: t,
                        updated_at: t,
                    },
                );
                op.result = Some(json!({"subscription_id":sub_id}));
                op.revision = Some(1);
            }
            Action::UpdateSubscription {
                id,
                expected_revision,
                name,
                url,
                enabled,
                refresh_interval_secs,
            } => {
                let s = next
                    .subscriptions
                    .get_mut(id)
                    .ok_or_else(|| GatewayError::NotFound("subscription does not exist".into()))?;
                check_revision(s.revision, *expected_revision)?;
                if let Some(name) = name {
                    if name.trim().is_empty() || name.len() > 256 {
                        return Err(GatewayError::BadRequest("invalid subscription name".into()));
                    }
                    s.name = name.clone();
                }
                if let Some(url) = url.as_ref().filter(|x| !x.is_empty()) {
                    validate_subscription(
                        &s.name,
                        url,
                        refresh_interval_secs.unwrap_or(s.refresh_interval_secs),
                    )?;
                    s.url_ciphertext = self
                        .crypto()?
                        .seal(&format!("subscription:{id}"), url.as_bytes())
                        .map_err(|_| error("proxy_encryption_failed", "URL encryption failed"))?;
                }
                if let Some(enabled) = enabled {
                    s.enabled = *enabled;
                }
                if let Some(interval) = refresh_interval_secs {
                    if *interval < 300 {
                        return Err(GatewayError::BadRequest(
                            "refresh interval must be at least 300 seconds".into(),
                        ));
                    }
                    s.refresh_interval_secs = *interval;
                }
                if enabled.is_some() || refresh_interval_secs.is_some() {
                    s.next_refresh_at =
                        Some(scheduler::refresh_due(now(), s.refresh_interval_secs));
                }
                s.revision += 1;
                s.updated_at = now();
                op.revision = Some(s.revision);
            }
            Action::DeleteSubscription {
                id,
                expected_revision,
            } => {
                let sub = next
                    .subscriptions
                    .get(id)
                    .ok_or_else(|| GatewayError::NotFound("subscription does not exist".into()))?;
                check_revision(sub.revision, *expected_revision)?;
                if next.bindings.values().any(|b| {
                    next.nodes
                        .get(&b.node_id)
                        .is_some_and(|n| n.subscription_id == *id)
                }) {
                    return Err(GatewayError::Conflict(
                        "subscription still has account bindings".into(),
                    ));
                }
                next.nodes.retain(|_, n| n.subscription_id != *id);
                next.subscriptions.remove(id);
            }
            Action::Refresh { id } => {
                let sub = next
                    .subscriptions
                    .get(id)
                    .ok_or_else(|| GatewayError::NotFound("subscription does not exist".into()))?;
                let (revision, data) = refresh_data.as_ref().expect("refresh data");
                check_revision(sub.revision, *revision)?;
                let nodes = subscription::parse(data, sub, &next, self.crypto()?, &self.config())?;
                next.replace_subscription_nodes(id, nodes);
                if next.nodes.len() > 100_000 {
                    return Err(error(
                        "subscription_invalid",
                        "total proxy node limit exceeded",
                    ));
                }
                let sub = next.subscriptions.get_mut(id).unwrap();
                sub.last_attempt_at = Some(now());
                sub.last_success_at = Some(now());
                sub.last_error_code = None;
                sub.next_refresh_at =
                    Some(scheduler::refresh_due(now(), sub.refresh_interval_secs));
                sub.revision += 1;
                sub.updated_at = now();
                op.revision = Some(sub.revision);
            }
            Action::SetBinding {
                email,
                node_id,
                expected_revision,
            } => {
                let account = self.account_id(email)?;
                let previous = next.bindings.get(&account);
                check_revision(previous.map_or(0, |b| b.revision), *expected_revision)?;
                let node = next
                    .nodes
                    .get(node_id)
                    .ok_or_else(|| GatewayError::NotFound("proxy node does not exist".into()))?;
                if node.presence != "present"
                    || !next
                        .subscriptions
                        .get(&node.subscription_id)
                        .is_some_and(|s| s.enabled)
                {
                    return Err(GatewayError::Conflict(
                        "proxy node is missing or subscription is disabled".into(),
                    ));
                }
                if !node.test_passed() {
                    return Err(GatewayError::Admission {
                        status: axum::http::StatusCode::CONFLICT,
                        code: if node.test_status() == "failed" {
                            "proxy_test_failed"
                        } else {
                            "proxy_node_untested"
                        },
                        message: "该节点尚未通过测试，请先测试通过后再选为代理出口。".into(),
                    });
                }
                let revision =
                    self.assign_node(&mut next, account, node_id.clone(), &op.id, None)?;
                op.revision = Some(revision);
            }
            Action::DeleteBinding {
                email,
                expected_revision,
            } => {
                let account = self.account_id(email)?;
                if let Some(binding) = next.bindings.get(&account) {
                    check_revision(binding.revision, *expected_revision)?;
                } else if self.store.pending_auto_accounts()?.contains(&account) {
                    check_revision(0, *expected_revision)?;
                } else {
                    return Err(GatewayError::NotFound("account has no binding".into()));
                }
                op.result = Some(json!({"auto_assignment_cancelled":account}));
                next.bindings.remove(&account);
            }
            Action::DeleteAccounts { ids } => {
                for id in ids {
                    next.bindings.remove(id);
                }
                delete_ids = ids.clone();
            }
        }
        next.prune_unbound_missing_nodes();
        if runtime_signature(&old) == runtime_signature(&next) && delete_ids.is_empty() {
            op.phase = "committed".into();
            op.generation = Some(old.generation);
            op.candidate = None;
            op.action_ciphertext = None;
            op.updated_at = now();
            self.store.commit(&next, op, &[])?;
            self.router.publish_metadata(next);
            return Ok(());
        }
        next.generation = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as u64)
            .max(old.generation + 1);
        op.candidate = Some(next.clone());
        op.generation = Some(next.generation);
        op.phase = "prepared".into();
        self.store.save_operation(op)?;
        let path = if self.config().enabled {
            let core = self.core.lock().await;
            let core = core
                .as_ref()
                .ok_or_else(|| error("proxy_unavailable", "managed core is unavailable"))?;
            let path = core.generate(&next, self.crypto()?)?;
            core.validate(&path).await?;
            Some(path)
        } else {
            None
        };
        let _control = self.control_gate.write().await;
        self.router.begin_apply(
            next.bindings
                .keys()
                .filter(|id| !old.bindings.contains_key(id))
                .copied()
                .chain(delete_ids.iter().copied())
                .collect(),
        );
        let deadline =
            tokio::time::Instant::now() + Duration::from_secs(self.config().apply_timeout_secs);
        let guard = match tokio::time::timeout(
            Duration::from_secs(self.config().apply_drain_timeout_secs),
            self.router.drain(),
        )
        .await
        {
            Ok(guard) => guard,
            Err(_) => {
                self.router.cancel_normal();
                match tokio::time::timeout_at(deadline, self.router.drain()).await {
                    Ok(guard) => guard,
                    Err(_) => {
                        self.router.end_apply();
                        return Err(error(
                            "proxy_apply_failed",
                            "active requests, including necessary OAuth, did not drain before timeout",
                        ));
                    }
                }
            }
        };
        let apply_result = tokio::time::timeout_at(deadline, async {
            if let Some(path) = &path {
                let core = self.core.lock().await;
                let core = core
                    .as_ref()
                    .ok_or_else(|| error("proxy_unavailable", "core unavailable"))?;
                core.reload(path, &next).await?;
            }
            op.phase = "runtime_applied".into();
            self.store.save_operation(op)?;
            let mut committed = op.clone();
            committed.phase = "committed".into();
            committed.candidate = None;
            committed.action_ciphertext = None;
            committed.updated_at = now();
            self.store.commit(&next, &committed, &delete_ids)?;
            *op = committed;
            Ok::<_, GatewayError>(())
        })
        .await
        .unwrap_or_else(|_| {
            Err(error(
                "proxy_apply_failed",
                "core application exceeded its deadline",
            ))
        });
        if let Err(e) = apply_result {
            let restored = if self.config().enabled {
                let core = self.core.lock().await;
                if let Some(core) = core.as_ref() {
                    match core.generate(&old, self.crypto()?) {
                        Ok(path) => core.reload(&path, &old).await.is_ok(),
                        Err(_) => false,
                    }
                } else {
                    false
                }
            } else {
                true
            };
            if restored {
                self.router.publish(old, self.config().enabled);
                self.router.end_apply();
            } else {
                op.phase = "recovery_required".into();
                self.router.unavailable();
            }
            drop(guard);
            return Err(e);
        }
        let current_generation = next.generation;
        self.router.publish(next, self.config().enabled);
        if let Some(core) = self.core.lock().await.as_ref() {
            core.cleanup_generations(&[current_generation, old.generation]);
        }
        self.router.end_apply();
        drop(guard);
        Ok(())
    }
    async fn maintenance(self: Arc<Self>) {
        let mut backoff = 1u64;
        loop {
            tokio::time::sleep(Duration::from_secs(backoff)).await;
            if self.stopping.load(Ordering::SeqCst) {
                break;
            }
            if !self.config().enabled {
                continue;
            }
            let _apply = self.apply.lock().await;
            let _control = self.control_gate.write().await;
            let mut core = self.core.lock().await;
            if core.as_mut().is_some_and(Core::exited)
                || self.router.applying.load(Ordering::SeqCst)
            {
                self.router.unavailable();
                self.router.begin_apply(Vec::new());
                let guard = tokio::time::timeout(
                    Duration::from_secs(self.config().apply_timeout_secs),
                    self.router.drain(),
                )
                .await;
                if let Ok(_guard) = guard {
                    let snapshot = self.store.load().unwrap_or_else(|_| self.router.snapshot());
                    let result = if let Some(c) = core.as_mut() {
                        c.stop().await;
                        match c.generate(&snapshot, self.crypto.as_ref().unwrap()) {
                            Ok(path) => c.start(&path, &snapshot).await,
                            Err(e) => Err(e),
                        }
                    } else {
                        Err(error("proxy_unavailable", "core missing"))
                    };
                    if result.is_ok() {
                        self.router.publish(snapshot, true);
                        self.router.end_apply();
                        backoff = 1;
                    } else {
                        backoff = (backoff * 2).min(60);
                    }
                } else {
                    backoff = (backoff * 2).min(60);
                }
            }
            drop(core);
            drop(_control);
            drop(_apply);
            if self.router.ready() {
                for s in self.router.subscriptions().iter().filter(|s| s.enabled) {
                    let due = s.next_refresh_at.unwrap_or_else(|| {
                        scheduler::refresh_due(
                            s.last_attempt_at
                                .or(s.last_success_at)
                                .unwrap_or(s.created_at),
                            s.refresh_interval_secs,
                        )
                    });
                    if now() >= due {
                        let key = format!("auto-refresh:{}:{due}", s.id);
                        let _ = self
                            .enqueue(Action::Refresh { id: s.id.clone() }, Some(key))
                            .await;
                    }
                }
                if let Err(error) = self.automation_tick().await {
                    tracing::warn!(%error,"proxy automation deferred");
                }
            }
        }
    }
    pub async fn wait(&self, id: &str) -> Result<Operation, GatewayError> {
        for _ in 0..900 {
            let op = self.operation(id)?;
            match op.phase.as_str() {
                "committed" => return Ok(op),
                "failed" | "rolled_back" | "recovery_required" => {
                    return Err(error(
                        "proxy_operation_failed",
                        op.message
                            .unwrap_or_else(|| "proxy operation failed".into()),
                    ));
                }
                _ => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
        Err(error(
            "proxy_operation_pending",
            "proxy operation is still queued; inspect its status",
        ))
    }
    async fn execute_test(&self, op: &mut Operation, node_id: &str) -> Result<(), GatewayError> {
        let control = self.control_gate.read().await;
        let settings_revision = self.settings_view().revision;
        let cfg = self.config();
        if self.stopping.load(Ordering::SeqCst) || !self.router.ready() {
            return Err(error("proxy_unavailable", "core is unavailable"));
        }
        let snapshot = self.router.snapshot();
        let node = snapshot
            .nodes
            .get(node_id)
            .ok_or_else(|| GatewayError::NotFound("node not found".into()))?;
        if node.presence != "present"
            || !snapshot
                .subscriptions
                .get(&node.subscription_id)
                .is_some_and(|s| s.enabled)
        {
            return Err(GatewayError::Conflict(
                "node is missing or subscription disabled".into(),
            ));
        }
        let fingerprint = node.config_fingerprint.clone();
        let generation = snapshot.generation;
        let core_epoch = self.router.core_epoch();
        let controller = self
            .core
            .lock()
            .await
            .as_ref()
            .ok_or_else(|| error("proxy_unavailable", "core missing"))?
            .controller();
        let mut url = reqwest::Url::parse(&format!(
            "http://{}/providers/proxies/rp_sub_{}/{}/healthcheck",
            self.config().controller_addr,
            node.subscription_id,
            node.mihomo_name
        ))
        .unwrap();
        url.query_pairs_mut()
            .append_pair("url", &cfg.manual_test_url)
            .append_pair("timeout", &cfg.manual_test_timeout_ms.to_string());
        let outcome = controller
            .api_with_timeout(
                reqwest::Method::GET,
                &format!("{}?{}", url.path(), url.query().unwrap()),
                None,
                Duration::from_millis(cfg.manual_test_timeout_ms.saturating_add(1000)),
            )
            .await;
        let checked_at = now();
        drop(control);
        // Never wait for the application mutex while holding the shared core lease.
        let _apply = self.apply.lock().await;
        let mut next = self.router.snapshot();
        if self.stopping.load(Ordering::SeqCst)
            || !self.router.ready()
            || settings_revision != self.settings_view().revision
            || generation != next.generation
            || core_epoch != self.router.core_epoch()
        {
            return Err(error(
                "proxy_test_stale",
                "configuration changed while the node was being tested",
            ));
        }
        let node = next
            .nodes
            .get_mut(node_id)
            .filter(|n| n.presence == "present" && n.config_fingerprint == fingerprint)
            .ok_or_else(|| error("proxy_test_stale", "node changed while it was being tested"))?;
        node.delay_checked_at = Some(checked_at);
        node.delay_ms = outcome
            .as_ref()
            .ok()
            .and_then(|v| v["delay"].as_u64())
            .filter(|d| *d > 0);
        node.delay_error_code = if node.delay_ms.is_some() {
            None
        } else {
            Some("proxy_test_failed".into())
        };
        op.result = Some(
            json!({"node_id":node_id,"success":node.delay_ms.is_some(),"delay":node.delay_ms,"checked_at":checked_at,"code":node.delay_error_code}),
        );
        op.generation = Some(next.generation);
        op.phase = "committed".into();
        op.action_ciphertext = None;
        op.updated_at = now();
        self.store.commit(&next, op, &[])?;
        self.router.publish_metadata(next);
        Ok(())
    }
    async fn execute_batch(
        self: &Arc<Self>,
        op: &mut Operation,
        action: &Action,
    ) -> Result<(), GatewayError> {
        let mut items = if let Some(items) = op
            .result
            .as_ref()
            .and_then(|r| r.get("items"))
            .and_then(Value::as_array)
        {
            items.clone()
        } else {
            let targets: Vec<String> = match action {
                Action::RefreshAll => self
                    .router
                    .subscriptions()
                    .into_iter()
                    .filter(|s| s.enabled)
                    .map(|s| s.id)
                    .collect(),
                Action::TestNodes { ids } => {
                    if ids.is_empty() || ids.len() > 20 {
                        return Err(GatewayError::BadRequest(
                            "select 1 to 20 nodes for batch testing".into(),
                        ));
                    }
                    let mut seen = std::collections::HashSet::new();
                    ids.iter()
                        .filter(|id| seen.insert((*id).clone()))
                        .cloned()
                        .collect()
                }
                Action::TestAllNodes => {
                    let snapshot = self.router.snapshot();
                    snapshot
                        .nodes
                        .values()
                        .filter(|node| {
                            node.presence == "present"
                                && snapshot
                                    .subscriptions
                                    .get(&node.subscription_id)
                                    .is_some_and(|sub| sub.enabled)
                        })
                        .map(|node| node.id.clone())
                        .collect()
                }
                _ => unreachable!(),
            };
            targets
                .into_iter()
                .enumerate()
                .map(|(index, target)| json!({"index":index,"target_id":target}))
                .collect()
        };
        op.result = Some(json!({"items":items}));
        self.store.save_operation(op)?;
        // An all-node test may contain thousands of nodes. Keep its child queue bounded.
        let window = if matches!(action, Action::TestAllNodes) {
            self.config().manual_test_concurrency
        } else {
            usize::MAX
        };
        loop {
            if self.stopping.load(Ordering::SeqCst) {
                return Err(error(
                    "proxy_unavailable",
                    "batch interrupted by shutdown; retry after restart",
                ));
            }
            let mut completed = 0;
            let mut inflight = 0;
            let mut changed = false;
            for item in &mut items {
                if item.get("success").is_some() {
                    completed += 1;
                    continue;
                }
                if let Some(child_id) = item.get("operation_id").and_then(Value::as_str) {
                    let child = self.store.operation(child_id)?;
                    if terminal(&child.phase) {
                        completed += 1;
                        changed = true;
                        item["phase"] = json!(child.phase);
                        item["success"] = json!(
                            child.phase == "committed"
                                && child.result.as_ref().is_none_or(|v| v["success"] != false)
                        );
                        if let Some(result) = child.result {
                            item["result"] = result;
                        }
                        if let Some(code) = child.code {
                            item["code"] = json!(code);
                        }
                        if let Some(message) = child.message {
                            item["message"] = json!(message);
                        }
                    } else {
                        inflight += 1;
                    }
                }
            }
            for item in &mut items {
                if inflight >= window {
                    break;
                }
                if item.get("operation_id").is_some() || item.get("success").is_some() {
                    continue;
                }
                let target = item["target_id"].as_str().unwrap_or_default().to_string();
                let child = match action {
                    Action::RefreshAll => Action::Refresh { id: target.clone() },
                    Action::TestNodes { .. } | Action::TestAllNodes => {
                        Action::TestNode { id: target.clone() }
                    }
                    _ => unreachable!(),
                };
                match self
                    .enqueue(child, Some(format!("batch:{}:{target}", op.id)))
                    .await
                {
                    Ok(child) => {
                        item["operation_id"] = json!(child.id);
                        inflight += 1;
                    }
                    Err(e) => {
                        completed += 1;
                        item["success"] = json!(false);
                        item["code"] = json!("proxy_enqueue_failed");
                        item["message"] = json!(e.to_string());
                    }
                }
                changed = true;
            }
            if changed || completed == items.len() {
                op.result = Some(
                    json!({"items":items,"completed":completed,"total":items.len(),"succeeded":items.iter().filter(|i|i["success"]==true).count()}),
                );
                op.updated_at = now();
                if completed == items.len() {
                    op.phase = "committed".into();
                    op.action_ciphertext = None;
                }
                self.store.save_operation(op)?;
            }
            if completed == items.len() {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
    pub async fn shutdown(&self) {
        if self.stopping.swap(true, Ordering::SeqCst) {
            return;
        }
        self.wake.notify_waiters();
        let _apply = self.apply.lock().await;
        let _control = self.control_gate.write().await;
        self.router.begin_shutdown();
        let _guard = self.router.drain().await;
        if let Some(mut core) = self.core.lock().await.take() {
            core.stop().await;
        }
        self.router.unavailable();
    }
}
fn terminal(phase: &str) -> bool {
    matches!(
        phase,
        "committed" | "failed" | "rolled_back" | "recovery_required"
    )
}

fn runtime_signature(s: &Snapshot) -> Value {
    let nodes = s
        .nodes
        .values()
        .filter(|n| {
            n.presence == "present"
                && s.subscriptions
                    .get(&n.subscription_id)
                    .is_some_and(|sub| sub.enabled)
        })
        .map(|n| (&n.id, &n.config_fingerprint))
        .collect::<Vec<_>>();
    let bindings = s
        .bindings
        .values()
        .map(|b| {
            (
                b.account_id,
                &b.node_id,
                b.listener_port,
                s.route_configured(b),
            )
        })
        .collect::<Vec<_>>();
    json!([nodes, bindings])
}

fn public_operation(mut o: Operation) -> Operation {
    o.candidate = None;
    o.action_ciphertext = None;
    o.idempotency_key = None;
    o.request_hash = None;
    o
}
fn check_revision(actual: u64, expected: u64) -> Result<(), GatewayError> {
    if actual != expected {
        Err(GatewayError::Conflict(
            "proxy revision changed; refresh and retry".into(),
        ))
    } else {
        Ok(())
    }
}
fn validate_subscription(name: &str, url: &str, interval: u64) -> Result<(), GatewayError> {
    let url = reqwest::Url::parse(url)
        .map_err(|_| GatewayError::BadRequest("invalid subscription URL".into()))?;
    if name.trim().is_empty()
        || name.len() > 256
        || url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || interval < 300
    {
        return Err(GatewayError::BadRequest(
            "subscription requires a name, HTTPS URL without credentials, and interval >= 300"
                .into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    pub(super) fn queued_manager(downloads: usize, tests: usize) -> Arc<ProxyManager> {
        let state = crate::server::state::compact_test_state();
        let mut settings = (*state.settings).clone();
        settings.proxy_manager.enabled = true;
        settings.proxy_manager.subscription_concurrency = downloads;
        settings.proxy_manager.manual_test_concurrency = tests;
        settings.proxy_manager.work_dir = std::env::temp_dir().join(format!("r2a-queue-{}", id()));
        settings.proxy_manager.subscription_allowed_private_hosts = vec!["127.0.0.1".into()];
        settings.proxy_manager.subscription_timeout_secs = 2;
        let manager = ProxyManager::with_crypto(
            Arc::new(settings),
            state.db.clone(),
            Some(
                Crypto::from_base64(&base64::engine::general_purpose::STANDARD.encode([42u8; 32]))
                    .unwrap(),
            ),
        )
        .unwrap();
        manager.started.store(true, Ordering::SeqCst);
        manager
    }
    pub(super) async fn fixture_snapshot(manager: &Arc<ProxyManager>, url: &str, count: usize) {
        use super::super::model::test_data::{node, subscription};
        let mut snapshot = Snapshot::default();
        for i in 0..count {
            let sub_id = format!("sub{i}");
            let mut sub = subscription(&sub_id);
            sub.url_ciphertext = manager
                .crypto()
                .unwrap()
                .seal(&format!("subscription:{sub_id}"), url.as_bytes())
                .unwrap();
            sub.next_refresh_at = Some(scheduler::refresh_due(now(), 21600));
            snapshot.subscriptions.insert(sub_id.clone(), sub);
            let node_id = format!("node{i}");
            snapshot
                .nodes
                .insert(node_id.clone(), node(&node_id, &sub_id, "present"));
        }
        let op = manager.enqueue(Action::RefreshAll, None).await.unwrap();
        let mut stored = manager.store.operation(&op.id).unwrap();
        stored.phase = "committed".into();
        manager.store.commit(&snapshot, &stored, &[]).unwrap();
        manager.router.publish(snapshot, true);
    }
    #[tokio::test]
    async fn binding_requires_a_passing_test_and_existing_failures_remain_visible() {
        use super::super::model::test_data::binding;
        let manager = queued_manager(2, 3);
        fixture_snapshot(&manager, "https://example.invalid/sub", 2).await;
        let accounts =
            crate::account::store::AccountStore::new(manager.store.database_for_test()).unwrap();
        accounts
            .upsert(
                &serde_json::from_value(json!({"user":{"email":"health@test"},
            "device_id":"1".repeat(64),"access_token":"rca_fixture"}))
                .unwrap(),
            )
            .unwrap();
        let account_id = manager.account_id("health@test").unwrap();
        let mut snapshot = manager.router.snapshot();
        snapshot
            .bindings
            .insert(account_id, binding(account_id, "node1"));
        manager.router.publish_metadata(snapshot);
        for failed in [false, true] {
            let mut snapshot = manager.router.snapshot();
            let node = snapshot.nodes.get_mut("node0").unwrap();
            if failed {
                node.delay_checked_at = Some(now());
                node.delay_error_code = Some("proxy_test_failed".into());
            }
            manager.router.publish_metadata(snapshot);
            let op = manager
                .enqueue(
                    Action::SetBinding {
                        email: "health@test".into(),
                        node_id: "node0".into(),
                        expected_revision: 1,
                    },
                    None,
                )
                .await
                .unwrap();
            manager
                .run_operation(manager.store.operation(&op.id).unwrap())
                .await;
            assert_eq!(
                manager.operation(&op.id).unwrap().code.as_deref(),
                Some(if failed {
                    "proxy_test_failed"
                } else {
                    "proxy_node_untested"
                })
            );
            assert_eq!(
                manager.router.snapshot().bindings[&account_id].node_id,
                "node1"
            );
        }
        let mut snapshot = manager.router.snapshot();
        let node = snapshot.nodes.get_mut("node1").unwrap();
        node.delay_checked_at = Some(now());
        node.delay_ms = Some(42);
        manager.router.publish_metadata(snapshot);
        let op = manager
            .enqueue(
                Action::SetBinding {
                    email: "health@test".into(),
                    node_id: "node1".into(),
                    expected_revision: 1,
                },
                None,
            )
            .await
            .unwrap();
        manager
            .run_operation(manager.store.operation(&op.id).unwrap())
            .await;
        assert_eq!(manager.operation(&op.id).unwrap().phase, "committed");
        let view = manager.binding_view("health@test").unwrap().unwrap();
        assert_eq!(view["test_status"], "passed");
        assert_eq!(view["revision"], 2);
        let mut snapshot = manager.router.snapshot();
        let node = snapshot.nodes.get_mut("node1").unwrap();
        node.delay_ms = None;
        node.delay_error_code = Some("proxy_test_failed".into());
        manager.router.publish_metadata(snapshot);
        let view = manager.binding_view("health@test").unwrap().unwrap();
        assert_eq!(view["test_status"], "failed");
        assert_eq!(view["node_id"], "node1");
        assert_eq!(view["delay_error_code"], "proxy_test_failed");
        assert_eq!(manager.router.snapshot().bindings[&account_id].revision, 2);
    }
    #[tokio::test]
    async fn test_all_covers_more_than_twenty_nodes_and_skips_missing_or_disabled() {
        let manager = queued_manager(2, 3);
        fixture_snapshot(&manager, "https://example.invalid/sub", 25).await;
        let mut snapshot = manager.router.snapshot();
        snapshot.nodes.get_mut("node0").unwrap().presence = "missing".into();
        snapshot.subscriptions.get_mut("sub1").unwrap().enabled = false;
        manager.router.publish_metadata(snapshot);
        let all = manager
            .enqueue(Action::TestAllNodes, Some("all".into()))
            .await
            .unwrap();
        assert_eq!(
            manager
                .enqueue(Action::TestAllNodes, Some("all-again".into()))
                .await
                .unwrap()
                .id,
            all.id
        );
        let worker = tokio::spawn(manager.clone().worker());
        // No core: every selected child must report failure without dropping other results.
        let done = manager.wait(&all.id).await.unwrap();
        let result = done.result.unwrap();
        assert_eq!(result["total"], 23);
        assert_eq!(result["completed"], 23);
        assert_eq!(result["succeeded"], 0);
        assert!(
            result["items"]
                .as_array()
                .unwrap()
                .iter()
                .all(|item| item["success"] == false
                    && item["target_id"] != "node0"
                    && item["target_id"] != "node1")
        );
        manager.stopping.store(true, Ordering::SeqCst);
        worker.await.unwrap();
        let empty = queued_manager(2, 3);
        let op = empty.enqueue(Action::TestAllNodes, None).await.unwrap();
        empty
            .run_operation(empty.store.operation(&op.id).unwrap())
            .await;
        assert_eq!(empty.operation(&op.id).unwrap().result.unwrap()["total"], 0);
    }
    #[tokio::test]
    async fn duplicate_jobs_keep_each_retry_key_and_reject_changed_batch_bodies() {
        let manager = queued_manager(2, 3);
        let first = manager
            .enqueue_request(
                Action::Refresh { id: "sub".into() },
                Some("request:first".into()),
            )
            .await
            .unwrap();
        let alias = manager
            .enqueue_request(
                Action::Refresh { id: "sub".into() },
                Some("request:second".into()),
            )
            .await
            .unwrap();
        assert_eq!(first.id, alias.id);
        assert!(matches!(
            manager
                .enqueue_request(
                    Action::Refresh { id: "other".into() },
                    Some("request:second".into())
                )
                .await,
            Err(GatewayError::Conflict(_))
        ));
        let action = Action::TestNodes {
            ids: vec!["a".into(), "b".into()],
        };
        let batch = manager
            .enqueue_request(action.clone(), Some("request:batch".into()))
            .await
            .unwrap();
        assert_eq!(
            batch.id,
            manager
                .enqueue_request(action, Some("request:batch".into()))
                .await
                .unwrap()
                .id
        );
        assert!(matches!(
            manager
                .enqueue_request(
                    Action::TestNodes {
                        ids: vec!["a".into()]
                    },
                    Some("request:batch".into())
                )
                .await,
            Err(GatewayError::Conflict(_))
        ));
        let merged = manager
            .enqueue_request(
                Action::TestNodes {
                    ids: vec!["b".into(), "a".into(), "a".into()],
                },
                Some("request:batch-reordered".into()),
            )
            .await
            .unwrap();
        assert_eq!(merged.id, batch.id);
        let mut finished = manager.store.operation(&first.id).unwrap();
        finished.phase = "committed".into();
        manager.store.save_operation(&finished).unwrap();
        assert_eq!(
            manager
                .enqueue_request(
                    Action::Refresh { id: "sub".into() },
                    Some("request:second".into())
                )
                .await
                .unwrap()
                .id,
            first.id
        );
        assert_ne!(
            manager
                .enqueue(Action::Refresh { id: "sub".into() }, None)
                .await
                .unwrap()
                .id,
            first.id
        );
    }
    #[tokio::test]
    async fn batch_creation_reports_individual_validation_and_persists_refresh_schedule() {
        let manager = queued_manager(2, 3);
        let item = |url: &str| SubscriptionInput {
            name: None,
            url: url.into(),
            enabled: false,
            refresh_interval_secs: Some(300),
        };
        let op = manager
            .enqueue(
                Action::CreateSubscriptions {
                    items: vec![
                        item("https://example.invalid/sub"),
                        item("http://invalid.invalid/sub"),
                        item("https://example.invalid/sub"),
                    ],
                },
                None,
            )
            .await
            .unwrap();
        manager
            .run_operation(manager.store.operation(&op.id).unwrap())
            .await;
        let done = manager.operation(&op.id).unwrap();
        assert_eq!(done.phase, "committed");
        let result = done.result.unwrap();
        assert_eq!(result["total"], 3);
        assert_eq!(result["succeeded"], 1);
        assert_eq!(result["items"][1]["success"], false);
        assert_eq!(result["items"][2]["success"], false);
        let snapshot = manager.router.snapshot();
        let sub = snapshot.subscriptions.values().next().unwrap();
        assert!(sub.next_refresh_at.unwrap() > sub.created_at + 300);
        let restored = ProxyManager::with_crypto(
            manager.settings.clone(),
            manager.store.database_for_test(),
            Some(
                Crypto::from_base64(&base64::engine::general_purpose::STANDARD.encode([42u8; 32]))
                    .unwrap(),
            ),
        )
        .unwrap();
        assert_eq!(
            restored.router.snapshot().subscriptions[&sub.id].next_refresh_at,
            sub.next_refresh_at
        );
    }
    #[tokio::test]
    async fn downloads_run_in_parallel_with_the_configured_limit_and_refresh_all_reports_failures()
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("https://{}/sub", listener.local_addr().unwrap());
        let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let peak = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (a, p, c) = (active.clone(), peak.clone(), calls.clone());
        let server = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (a, p, c) = (a.clone(), p.clone(), c.clone());
                tokio::spawn(async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    let n = a.fetch_add(1, Ordering::SeqCst) + 1;
                    p.fetch_max(n, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    drop(stream);
                    a.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
        let manager = queued_manager(2, 3);
        fixture_snapshot(&manager, &url, 5).await;
        let mut snap = manager.router.snapshot();
        snap.subscriptions.get_mut("sub4").unwrap().enabled = false;
        manager.router.publish_metadata(snap);
        let op = manager.enqueue(Action::RefreshAll, None).await.unwrap();
        let worker = tokio::spawn(manager.clone().worker());
        let done = manager.wait(&op.id).await.unwrap();
        let result = done.result.unwrap();
        assert_eq!(result["total"], 4);
        assert_eq!(result["succeeded"], 0);
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        assert_eq!(peak.load(Ordering::SeqCst), 2);
        let snap = manager.router.snapshot();
        assert_eq!(snap.nodes.len(), 5);
        for s in snap.subscriptions.values().filter(|s| s.enabled) {
            assert!(s.next_refresh_at.unwrap() > now() + 21599);
        }
        manager.stopping.store(true, Ordering::SeqCst);
        worker.await.unwrap();
        server.abort();
    }
    #[tokio::test]
    async fn node_tests_run_concurrently_merge_duplicates_and_preserve_partial_results() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let peak = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (a, p, c) = (active.clone(), peak.clone(), calls.clone());
        let fail_node3 = Arc::new(AtomicBool::new(true));
        let server_failure = fail_node3.clone();
        let server = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let (a, p, c) = (a.clone(), p.clone(), c.clone());
                let server_failure = server_failure.clone();
                tokio::spawn(async move {
                    let mut data = vec![0; 4096];
                    let length = stream.read(&mut data).await.unwrap();
                    let request = String::from_utf8_lossy(&data[..length]);
                    assert!(request.contains("/healthcheck?"));
                    c.fetch_add(1, Ordering::SeqCst);
                    let n = a.fetch_add(1, Ordering::SeqCst) + 1;
                    p.fetch_max(n, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    let body =
                        if request.contains("node3/") && server_failure.load(Ordering::SeqCst) {
                            r#"{"delay":0}"#
                        } else {
                            r#"{"delay":42}"#
                        };
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    );
                    stream.write_all(response.as_bytes()).await.unwrap();
                    a.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
        let mut manager = queued_manager(2, 3);
        Arc::get_mut(&mut manager).unwrap().settings = Arc::new({
            let mut s = (*manager.settings).clone();
            s.proxy_manager.controller_addr = address;
            s
        });
        fixture_snapshot(&manager, "https://example.invalid/sub", 5).await;
        let accounts =
            crate::account::store::AccountStore::new(manager.store.database_for_test()).unwrap();
        accounts.upsert(&serde_json::from_value(json!({"user":{"email":"bound-test@fixture"},"device_id":"1".repeat(64),"access_token":"rca_fixture"})).unwrap()).unwrap();
        let account_id = manager.account_id("bound-test@fixture").unwrap();
        let mut snapshot = manager.router.snapshot();
        snapshot.bindings.insert(
            account_id,
            super::super::model::test_data::binding(account_id, "node3"),
        );
        manager.router.publish_metadata(snapshot);
        *manager.core.lock().await = Some(Core::new(&manager.config()).unwrap());
        let single = manager
            .enqueue(
                Action::TestNode { id: "node0".into() },
                Some("single".into()),
            )
            .await
            .unwrap();
        assert_eq!(
            single.id,
            manager
                .enqueue(
                    Action::TestNode { id: "node0".into() },
                    Some("duplicate".into())
                )
                .await
                .unwrap()
                .id
        );
        let batch = manager
            .enqueue(
                Action::TestNodes {
                    ids: vec![
                        "node0".into(),
                        "node1".into(),
                        "node2".into(),
                        "node3".into(),
                        "node3".into(),
                        "missing".into(),
                    ],
                },
                None,
            )
            .await
            .unwrap();
        let worker = tokio::spawn(manager.clone().worker());
        let done = manager.wait(&batch.id).await.unwrap();
        let r = done.result.unwrap();
        assert_eq!(r["total"], 5);
        assert_eq!(r["succeeded"], 3);
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        assert_eq!(peak.load(Ordering::SeqCst), 3);
        let snapshot = manager.router.snapshot();
        assert_eq!(snapshot.nodes["node1"].delay_ms, Some(42));
        assert_eq!(snapshot.nodes["node2"].delay_ms, Some(42));
        assert_eq!(snapshot.nodes["node3"].delay_ms, None);
        let mut snapshot = manager.router.snapshot();
        snapshot.subscriptions.get_mut("sub4").unwrap().enabled = false;
        manager.router.publish_metadata(snapshot);
        let all = manager.enqueue(Action::TestAllNodes, None).await.unwrap();
        let result = manager.wait(&all.id).await.unwrap().result.unwrap();
        assert_eq!(result["total"], 4);
        assert_eq!(result["succeeded"], 3);
        assert_eq!(calls.load(Ordering::SeqCst), 8);
        assert_eq!(peak.load(Ordering::SeqCst), 3);
        let view = manager.binding_view("bound-test@fixture").unwrap().unwrap();
        assert_eq!(view["test_status"], "failed");
        assert_eq!(view["node_id"], "node3");
        fail_node3.store(false, Ordering::SeqCst);
        let retest = manager
            .enqueue(Action::TestNode { id: "node3".into() }, None)
            .await
            .unwrap();
        manager.wait(&retest.id).await.unwrap();
        let view = manager.binding_view("bound-test@fixture").unwrap().unwrap();
        assert_eq!(view["test_status"], "passed");
        assert_eq!(view["revision"], 1);
        assert!(manager.store.load().unwrap().nodes["node3"].test_passed());
        manager.shutdown().await;
        worker.await.unwrap();
        server.abort();
        std::fs::remove_dir_all(&manager.config().work_dir).unwrap();
    }
    #[tokio::test]
    #[ignore = "requires MIHOMO_TEST_BINARY pointing to the verified v1.19.34 native core"]
    async fn real_core_binding_switch_block_restart_and_explicit_unbind() {
        let binary = std::env::var_os("MIHOMO_TEST_BINARY").expect("set MIHOMO_TEST_BINARY");
        let state = crate::server::state::compact_test_state();
        let root = std::env::temp_dir().join(format!("r2a-proxy-test-{}", id()));
        let control = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = control.local_addr().unwrap();
        drop(control);
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let node_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let node_port = node_listener.local_addr().unwrap().port();
        let counts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = counts.clone();
        let proxy = tokio::spawn(async move {
            while let Ok((mut stream, _)) = node_listener.accept().await {
                let observed = observed.clone();
                tokio::spawn(async move {
                    async fn headers(stream: &mut tokio::net::TcpStream) -> Vec<u8> {
                        let mut data = Vec::new();
                        let mut byte = [0];
                        while data.len() < 65536 && stream.read(&mut byte).await.unwrap_or(0) > 0 {
                            data.push(byte[0]);
                            if data.ends_with(b"\r\n\r\n") {
                                break;
                            }
                        }
                        data
                    }
                    let connect = headers(&mut stream).await;
                    if !connect.starts_with(b"CONNECT ") {
                        return;
                    }
                    stream
                        .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                        .await
                        .unwrap();
                    let request = headers(&mut stream).await;
                    if request.is_empty() {
                        return;
                    }
                    let header_text = String::from_utf8_lossy(&request);
                    let length = header_text
                        .lines()
                        .find_map(|line| {
                            let (key, value) = line.split_once(':')?;
                            key.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())
                                .flatten()
                        })
                        .unwrap_or(0);
                    if length > 0 {
                        let mut body = vec![0; length.min(1024 * 1024)];
                        if stream.read_exact(&mut body).await.is_err() {
                            return;
                        }
                    }
                    observed.fetch_add(1, Ordering::SeqCst);
                    let registration=serde_json::to_vec(&json!({"id":"file_fixture","direct_upload":{"url":"http://example.invalid/upload","headers":{"X-Fixture":"upload"}}})).unwrap();
                    let oauth=serde_json::to_vec(&json!({"access_token":"rca_fixture_rotated","refresh_token":"refresh_fixture_rotated","expires_in":3600})).unwrap();
                    let models = b"{\"models\":[]}".to_vec();
                    let body = if request.starts_with(b"POST /files ") {
                        &registration
                    } else if request.starts_with(b"POST /oauth/token ") {
                        &oauth
                    } else {
                        &models
                    };
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.write_all(body).await;
                });
            }
        });
        let record=serde_json::from_value(json!({"user":{"email":"proxy@test"},"device_id":"1111111111111111111111111111111111111111111111111111111111111111","access_token":"rca_fixture","refresh_token":"fixture"})).unwrap();
        state.accounts.upsert(&record).unwrap();
        let mut settings = (*state.settings).clone();
        settings.proxy_manager.enabled = true;
        settings.proxy_manager.binary_path = binary.into();
        settings.proxy_manager.work_dir = root.clone();
        settings.proxy_manager.controller_addr = address;
        settings.proxy_manager.port_start = port;
        settings.proxy_manager.port_end = port;
        settings.proxy_manager.apply_drain_timeout_secs = 1;
        settings.proxy_manager.auto_test_enabled = false;
        settings.proxy_manager.auto_failover_enabled = false;
        let crypto =
            Crypto::from_base64(&base64::engine::general_purpose::STANDARD.encode([42u8; 32]))
                .unwrap();
        let store = Store::new(state.db.clone()).unwrap();
        let t = now();
        let mut sub = Subscription {
            id: id(),
            name: "fixture".into(),
            url_ciphertext: String::new(),
            enabled: true,
            refresh_interval_secs: 21600,
            revision: 1,
            next_refresh_at: None,
            last_attempt_at: None,
            last_success_at: None,
            last_error_code: None,
            created_at: t,
            updated_at: t,
        };
        sub.url_ciphertext = crypto
            .seal(
                &format!("subscription:{}", sub.id),
                b"https://example.invalid/subscription",
            )
            .unwrap();
        let mut snapshot = Snapshot::default();
        snapshot.subscriptions.insert(sub.id.clone(), sub.clone());
        let data =
            format!("proxies:\n- {{name: node, type: http, server: 127.0.0.1, port: {node_port}}}");
        let nodes = subscription::parse(
            data.as_bytes(),
            &sub,
            &snapshot,
            &crypto,
            &settings.proxy_manager,
        )
        .unwrap();
        let node_id = nodes[0].id.clone();
        let mut tested_node = nodes[0].clone();
        tested_node.delay_checked_at = Some(now());
        tested_node.delay_ms = Some(1);
        snapshot.nodes.insert(node_id.clone(), tested_node);
        let init = Operation {
            id: id(),
            phase: "committed".into(),
            code: None,
            message: None,
            generation: Some(0),
            revision: None,
            created_at: t,
            updated_at: t,
            candidate: None,
            action_ciphertext: None,
            idempotency_key: None,
            request_hash: None,
            resource: None,
            result: None,
        };
        store.save_operation(&init).unwrap();
        store.commit(&snapshot, &init, &[]).unwrap();
        let settings = Arc::new(settings);
        let router = Arc::new(AccountProxyRouter::new(
            snapshot,
            true,
            Duration::from_secs(2),
        ));
        let preferences = SettingsRecord {
            revision: 0,
            settings: ProxySettings::from_config(&settings.proxy_manager),
        };
        let manager = Arc::new(ProxyManager {
            settings,
            preferences: std::sync::RwLock::new(preferences),
            store,
            crypto: Some(crypto),
            router,
            core: Mutex::new(None),
            apply: Mutex::new(()),
            queue: Mutex::new(()),
            control_gate: RwLock::new(()),
            wake: Notify::new(),
            stopping: AtomicBool::new(false),
            started: AtomicBool::new(false),
            _database_lock: None,
        });
        manager.start().await.unwrap();
        let bind = Action::SetBinding {
            email: "proxy@test".into(),
            node_id: node_id.clone(),
            expected_revision: 0,
        };
        let op = manager
            .enqueue(bind.clone(), Some("bind-once".into()))
            .await
            .unwrap();
        manager.wait(&op.id).await.unwrap();
        assert_eq!(
            manager
                .enqueue(bind, Some("bind-once".into()))
                .await
                .unwrap()
                .id,
            op.id
        );
        assert!(
            !serde_json::to_string(&manager.snapshot())
                .unwrap()
                .contains("config_ciphertext")
        );
        let mut client = state.dispatcher.client.clone();
        client.proxy_manager = Some(manager.clone());
        client.models_url = Some("http://example.invalid/models".into());
        client.files_url = Some("http://example.invalid/files".into());
        client.oauth_url = Some("http://example.invalid/oauth/token".into());
        let scoped = client.for_account("proxy@test", false).unwrap();
        assert!(scoped.fetch_models("rca_fixture").await.unwrap().is_empty());
        drop(scoped);
        assert_eq!(counts.load(Ordering::SeqCst), 1);
        assert_eq!(
            client
                .for_account("proxy@test", false)
                .unwrap()
                .upload_file(
                    "rca_fixture",
                    &"1".repeat(64),
                    "chat",
                    "fixture.png",
                    "image/png",
                    b"fixture"
                )
                .await
                .unwrap(),
            "file_fixture"
        );
        let account = state.accounts.get("proxy@test").unwrap().unwrap();
        crate::account::token_renewal::auto_renew(&state.accounts, &client, &account)
            .await
            .unwrap();
        assert_eq!(
            state
                .accounts
                .get("proxy@test")
                .unwrap()
                .unwrap()
                .refresh_token
                .as_deref(),
            Some("refresh_fixture_rotated")
        );
        assert_eq!(counts.load(Ordering::SeqCst), 4);
        // A failed database commit after runtime application must restore the old route.
        use diesel::connection::SimpleConnection;
        state.db.lock().batch_execute("CREATE TRIGGER proxy_commit_abort BEFORE INSERT ON account_proxy_bindings BEGIN SELECT RAISE(ABORT,'fixture failure'); END;").unwrap();
        let failed = manager
            .enqueue(
                Action::UpdateSubscription {
                    id: sub.id.clone(),
                    expected_revision: 1,
                    name: None,
                    url: None,
                    enabled: Some(false),
                    refresh_interval_secs: None,
                },
                None,
            )
            .await
            .unwrap();
        assert!(manager.wait(&failed.id).await.is_err());
        assert_eq!(
            manager.binding_view("proxy@test").unwrap().unwrap()["revision"],
            1
        );
        assert!(manager.router.ready());
        state
            .db
            .lock()
            .batch_execute("DROP TRIGGER proxy_commit_abort;")
            .unwrap();
        assert!(
            client
                .for_account("proxy@test", false)
                .unwrap()
                .fetch_models("rca_fixture")
                .await
                .unwrap()
                .is_empty()
        );
        let disabled = manager
            .enqueue(
                Action::UpdateSubscription {
                    id: sub.id.clone(),
                    expected_revision: 1,
                    name: None,
                    url: None,
                    enabled: Some(false),
                    refresh_interval_secs: None,
                },
                None,
            )
            .await
            .unwrap();
        manager.wait(&disabled.id).await.unwrap();
        assert!(client.for_account("proxy@test", false).is_err());
        assert_eq!(counts.load(Ordering::SeqCst), 5);
        let enabled = manager
            .enqueue(
                Action::UpdateSubscription {
                    id: sub.id.clone(),
                    expected_revision: 2,
                    name: None,
                    url: None,
                    enabled: Some(true),
                    refresh_interval_secs: None,
                },
                None,
            )
            .await
            .unwrap();
        manager.wait(&enabled.id).await.unwrap();
        assert!(
            client
                .for_account("proxy@test", true)
                .unwrap()
                .fetch_models("rca_fixture")
                .await
                .unwrap()
                .is_empty()
        );
        {
            let mut core = manager.core.lock().await;
            core.as_mut().unwrap().stop().await;
        }
        for _ in 0..100 {
            if manager.router.ready()
                && manager
                    .core
                    .lock()
                    .await
                    .as_mut()
                    .is_some_and(|c| !c.exited())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(manager.router.ready());
        assert!(
            client
                .for_account("proxy@test", false)
                .unwrap()
                .fetch_models("rca_fixture")
                .await
                .unwrap()
                .is_empty()
        );
        // Simulate an uncommitted runtime generation left by an interrupted process.
        let mut orphan = init.clone();
        orphan.id = id();
        orphan.phase = "runtime_applied".into();
        orphan.candidate = Some(manager.router.snapshot());
        manager.store.save_operation(&orphan).unwrap();
        manager.shutdown().await;
        let restored = ProxyManager::with_crypto(
            manager.settings.clone(),
            state.db.clone(),
            Some(
                Crypto::from_base64(&base64::engine::general_purpose::STANDARD.encode([42u8; 32]))
                    .unwrap(),
            ),
        )
        .unwrap();
        restored.start().await.unwrap();
        assert_eq!(restored.operation(&orphan.id).unwrap().phase, "rolled_back");
        client.proxy_manager = Some(restored.clone());
        assert!(
            client
                .for_account("proxy@test", false)
                .unwrap()
                .fetch_models("rca_fixture")
                .await
                .unwrap()
                .is_empty()
        );
        let manager = restored;
        let unbind = manager
            .enqueue(
                Action::DeleteBinding {
                    email: "proxy@test".into(),
                    expected_revision: 1,
                },
                None,
            )
            .await
            .unwrap();
        manager.wait(&unbind.id).await.unwrap();
        assert!(
            !client
                .for_account("proxy@test", false)
                .unwrap()
                .transport_lease
                .unwrap()
                .bound()
        );
        manager.shutdown().await;
        proxy.abort();
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
#[path = "automation_tests.rs"]
mod automation_tests;
