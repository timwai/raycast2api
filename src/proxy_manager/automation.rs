//! Automatic assignment uses the same application barrier and persisted operations as manual changes.
use super::*;

impl ProxyManager {
    pub(super) fn available_auto_node(&self, snapshot: &Snapshot) -> Option<String> {
        let cfg = self.config();
        snapshot
            .nodes
            .values()
            .filter(|node| {
                node.presence == "present"
                    && node.test_passed()
                    && node.delay_checked_at.is_some_and(|checked| {
                        checked <= now()
                            && now().saturating_sub(checked) <= cfg.auto_selection_max_test_age_secs
                    })
                    && snapshot
                        .subscriptions
                        .get(&node.subscription_id)
                        .is_some_and(|sub| sub.enabled)
                    && !snapshot
                        .bindings
                        .values()
                        .any(|binding| binding.node_id == node.id)
            })
            .min_by_key(|node| (node.delay_ms.unwrap_or(u64::MAX), node.id.clone()))
            .map(|node| node.id.clone())
    }

    pub(super) fn assign_node(
        &self,
        snapshot: &mut Snapshot,
        account_id: i64,
        node_id: String,
        operation_id: &str,
        reason: Option<&str>,
    ) -> Result<u64, GatewayError> {
        let previous = snapshot.bindings.get(&account_id);
        let port = if let Some(binding) = previous {
            binding.listener_port
        } else {
            let cfg = self.config();
            let port = (cfg.port_start..=cfg.port_end)
                .find(|port| {
                    !snapshot.bindings.values().any(|b| b.listener_port == *port)
                        && std::net::TcpListener::bind(("127.0.0.1", *port)).is_ok()
                })
                .ok_or_else(|| {
                    error(
                        "proxy_port_exhausted",
                        "no account listener port is available",
                    )
                })?;
            self.store.reserve(port, account_id, operation_id)?;
            port
        };
        let revision = previous.map_or(1, |binding| binding.revision + 1);
        let binding = Binding {
            account_id,
            node_id,
            listener_port: port,
            revision,
            created_at: previous.map_or(now(), |binding| binding.created_at),
            updated_at: now(),
            automatic_reason: reason.map(str::to_string),
            previous_node_id: previous.map(|binding| binding.node_id.clone()),
        };
        snapshot.bindings.insert(account_id, binding);
        Ok(revision)
    }

    pub(super) fn reconcile_bindings(
        &self,
        snapshot: &mut Snapshot,
        op: &mut Operation,
    ) -> Result<(), GatewayError> {
        let cfg = self.config();
        if !self.router.ready() {
            return Err(error("proxy_unavailable", "core is unavailable"));
        }
        let accounts = self.store.account_labels()?;
        let mut targets = Vec::new();
        if cfg.auto_failover_enabled {
            for binding in snapshot.bindings.values() {
                if !snapshot.route_active(binding) {
                    targets.push((binding.account_id, "failover"));
                }
            }
        }
        if cfg.auto_bind_new_accounts {
            targets.extend(
                self.store
                    .pending_auto_accounts()?
                    .into_iter()
                    .filter(|id| !snapshot.bindings.contains_key(id))
                    .map(|id| (id, "new_account")),
            );
        }
        let mut items = Vec::new();
        for (account_id, reason) in targets {
            if !accounts.contains_key(&account_id) {
                continue;
            }
            let Some(node_id) = self.available_auto_node(snapshot) else {
                items.push(json!({"account_id":account_id,"success":false,"code":"proxy_no_available_node"}));
                continue;
            };
            let revision =
                self.assign_node(snapshot, account_id, node_id.clone(), &op.id, Some(reason))?;
            items.push(json!({"account_id":account_id,"success":true,"node_id":node_id,"revision":revision,"reason":reason}));
        }
        op.result = Some(
            json!({"items":items,"total":items.len(),"succeeded":items.iter().filter(|i|i["success"]==true).count()}),
        );
        Ok(())
    }

    pub(super) async fn update_settings_locked(
        &self,
        op: &mut Operation,
        revision: u64,
        settings: &ProxySettings,
    ) -> Result<(), GatewayError> {
        let previous = self.settings_view();
        check_revision(previous.revision, revision)?;
        settings
            .apply(&self.settings.proxy_manager)
            .map_err(GatewayError::BadRequest)?;
        let _control = self.control_gate.write().await;
        let reset_tests = previous.settings.manual_test_url != settings.manual_test_url
            || previous.settings.manual_test_timeout_ms != settings.manual_test_timeout_ms;
        let mut next = self.router.snapshot();
        if reset_tests {
            for node in next.nodes.values_mut() {
                node.delay_ms = None;
                node.delay_checked_at = None;
                node.delay_error_code = Some("proxy_test_required".into());
            }
        }
        let mut schedule = self
            .store
            .preference::<TestSchedule>("test_schedule")?
            .unwrap_or_default();
        if reset_tests || previous.settings.auto_test_enabled != settings.auto_test_enabled {
            schedule.next_test_at = now();
            schedule.operation_id = None;
        } else if previous.settings.auto_test_interval_secs != settings.auto_test_interval_secs {
            schedule.next_test_at = scheduler::refresh_due(now(), settings.auto_test_interval_secs);
        }
        schedule.last_reconcile_at = 0;
        schedule.last_reconcile_fingerprint = None;
        let record = SettingsRecord {
            revision: revision + 1,
            settings: settings.clone(),
        };
        op.phase = "committed".into();
        op.action_ciphertext = None;
        op.revision = Some(record.revision);
        op.updated_at = now();
        self.store.commit_settings(&next, op, &record, &schedule)?;
        *self.preferences.write().expect("proxy preferences") = record;
        self.router.publish_metadata(next);
        self.wake.notify_one();
        Ok(())
    }

    pub(super) async fn automation_tick(self: &Arc<Self>) -> Result<(), GatewayError> {
        let _apply = self.apply.lock().await;
        let cfg = self.config();
        if !cfg.enabled || !self.router.ready() || self.stopping.load(Ordering::SeqCst) {
            return Ok(());
        }
        for account_id in self.router.snapshot().bindings.keys() {
            self.store.cancel_auto_account(*account_id)?;
        }
        let mut schedule = self
            .store
            .preference::<TestSchedule>("test_schedule")?
            .unwrap_or_default();
        let pending = self.store.pending()?;
        let tests_running = pending.iter().any(|op| {
            self.decode_action(op).is_ok_and(|action| {
                matches!(
                    action,
                    Action::TestNode { .. } | Action::TestNodes { .. } | Action::TestAllNodes
                )
            })
        });
        let snapshot = self.router.snapshot();
        let pending_accounts = self.store.pending_auto_accounts()?;
        use sha2::Digest;
        let fingerprint = hex::encode(sha2::Sha256::digest(serde_json::to_vec(&json!([
            self.settings_view().revision,
            snapshot
                .nodes
                .values()
                .map(|n| (
                    &n.id,
                    &n.presence,
                    n.delay_ms,
                    n.delay_checked_at,
                    &n.delay_error_code
                ))
                .collect::<Vec<_>>(),
            snapshot
                .subscriptions
                .values()
                .map(|s| (&s.id, s.enabled, s.revision))
                .collect::<Vec<_>>(),
            snapshot
                .bindings
                .values()
                .map(|b| (b.account_id, &b.node_id, b.revision))
                .collect::<Vec<_>>(),
            pending_accounts
        ]))?));
        // Reconcile first, after the current complete batch, so candidates use the latest results.
        let needs_binding = (cfg.auto_bind_new_accounts
            && !self.store.pending_auto_accounts()?.is_empty())
            || (cfg.auto_failover_enabled
                && snapshot
                    .bindings
                    .values()
                    .any(|binding| !snapshot.route_active(binding)));
        let mut reconciling = pending
            .iter()
            .any(|op| op.resource.as_deref() == Some("reconcile-bindings"));
        if needs_binding
            && self.available_auto_node(&snapshot).is_some()
            && !tests_running
            && (schedule.last_reconcile_fingerprint.as_deref() != Some(&fingerprint)
                || now().saturating_sub(schedule.last_reconcile_at) >= 30)
        {
            self.enqueue(
                Action::ReconcileBindings,
                Some(format!("auto-reconcile:{fingerprint}:{}", now() / 30)),
            )
            .await?;
            schedule.last_reconcile_at = now();
            schedule.last_reconcile_fingerprint = Some(fingerprint);
            reconciling = true;
        }
        if let Some(id) = schedule.operation_id.as_deref() {
            let op = self.store.operation(id)?;
            if terminal(&op.phase) {
                schedule.operation_id = None;
                schedule.next_test_at = scheduler::refresh_due(now(), cfg.auto_test_interval_secs);
            }
        }
        if cfg.auto_test_enabled
            && !tests_running
            && !reconciling
            && schedule.operation_id.is_none()
            && now() >= schedule.next_test_at
        {
            let op = self
                .enqueue(
                    Action::TestAllNodes,
                    Some(format!(
                        "auto-test:{}:{}",
                        self.settings_view().revision,
                        schedule.next_test_at
                    )),
                )
                .await?;
            schedule.operation_id = Some(op.id);
        }
        self.store.save_preference("test_schedule", &schedule)?;
        Ok(())
    }

    pub fn automation_status(&self) -> Result<Value, GatewayError> {
        let schedule = self
            .store
            .preference::<TestSchedule>("test_schedule")?
            .unwrap_or_default();
        Ok(
            json!({"next_test_at":schedule.next_test_at,"operation_id":schedule.operation_id,
            "pending_accounts":self.store.pending_auto_accounts()?.len()}),
        )
    }
}
