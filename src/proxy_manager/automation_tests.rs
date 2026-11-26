use super::tests::{fixture_snapshot, queued_manager};
use super::*;
use crate::account::{
    store::{AccountStore, UpsertOutcome},
    wire::AccountRecord,
};
use base64::Engine;

fn record(email: &str) -> AccountRecord {
    serde_json::from_value(json!({"user":{"email":email},"device_id":"1".repeat(64),"access_token":"rca_automation_fixture"})).unwrap()
}
fn policy(manager: &Arc<ProxyManager>, edit: impl FnOnce(&mut ProxySettings)) {
    edit(&mut manager.preferences.write().unwrap().settings);
}
async fn run(manager: &Arc<ProxyManager>, action: Action) -> Operation {
    let op = manager.enqueue(action, None).await.unwrap();
    manager
        .run_operation(manager.store.operation(&op.id).unwrap())
        .await;
    manager.operation(&op.id).unwrap()
}

#[tokio::test]
async fn automatic_selection_uses_fresh_lowest_latency_spares_without_double_binding() {
    let manager = queued_manager(2, 3);
    fixture_snapshot(&manager, "https://example.invalid/sub", 5).await;
    let accounts = AccountStore::new(manager.store.database_for_test()).unwrap();
    let records = vec![
        record("one@fixture"),
        record("two@fixture"),
        record("three@fixture"),
    ];
    manager.import_accounts(&accounts, &records).await.unwrap();
    let first = manager.account_id("one@fixture").unwrap();
    assert!(manager.ensure_account_assignment(first).is_err());
    assert_eq!(
        manager.binding_view("one@fixture").unwrap().unwrap()["status"],
        "pending"
    );
    let mut snapshot = manager.router.snapshot();
    for (index, delay) in [(0, 80), (1, 20), (2, 40), (3, 1), (4, 2)] {
        let node = snapshot.nodes.get_mut(&format!("node{index}")).unwrap();
        node.delay_ms = Some(delay);
        node.delay_checked_at = Some(now());
    }
    snapshot.nodes.get_mut("node3").unwrap().delay_checked_at = Some(now() - 7200);
    snapshot.subscriptions.get_mut("sub4").unwrap().enabled = false;
    manager.router.publish_metadata(snapshot.clone());
    let op = manager
        .enqueue(Action::ReconcileBindings, None)
        .await
        .unwrap();
    let mut op = manager.store.operation(&op.id).unwrap();
    manager.reconcile_bindings(&mut snapshot, &mut op).unwrap();
    assert_eq!(snapshot.bindings[&first].node_id, "node1");
    assert_eq!(
        snapshot.bindings[&manager.account_id("two@fixture").unwrap()].node_id,
        "node2"
    );
    assert_eq!(
        snapshot.bindings[&manager.account_id("three@fixture").unwrap()].node_id,
        "node0"
    );
    assert_eq!(
        snapshot
            .bindings
            .values()
            .map(|b| &b.node_id)
            .collect::<std::collections::HashSet<_>>()
            .len(),
        3
    );
    assert!(manager.available_auto_node(&snapshot).is_none());
    assert_eq!(op.result.unwrap()["succeeded"], 3);
}

#[tokio::test]
async fn failed_binding_moves_only_to_an_unused_node_and_keeps_port_or_waits() {
    let manager = queued_manager(2, 3);
    fixture_snapshot(&manager, "https://example.invalid/sub", 3).await;
    let accounts = AccountStore::new(manager.store.database_for_test()).unwrap();
    for email in ["a@fixture", "b@fixture"] {
        accounts.upsert(&record(email)).unwrap();
    }
    let a = manager.account_id("a@fixture").unwrap();
    let b = manager.account_id("b@fixture").unwrap();
    let mut snapshot = manager.router.snapshot();
    snapshot
        .bindings
        .insert(a, super::super::model::test_data::binding(a, "node0"));
    snapshot
        .bindings
        .insert(b, super::super::model::test_data::binding(b, "node1"));
    for id in ["node0", "node1"] {
        let node = snapshot.nodes.get_mut(id).unwrap();
        node.delay_checked_at = Some(now());
        node.delay_error_code = Some("proxy_test_failed".into());
    }
    let spare = snapshot.nodes.get_mut("node2").unwrap();
    spare.delay_ms = Some(10);
    spare.delay_checked_at = Some(now());
    manager.router.publish_metadata(snapshot.clone());
    let op = manager
        .enqueue(Action::ReconcileBindings, None)
        .await
        .unwrap();
    let mut op = manager.store.operation(&op.id).unwrap();
    manager.reconcile_bindings(&mut snapshot, &mut op).unwrap();
    assert_eq!(snapshot.bindings[&a].node_id, "node2");
    assert_eq!(snapshot.bindings[&a].listener_port, 18000);
    assert_eq!(snapshot.bindings[&a].revision, 2);
    assert_eq!(
        snapshot.bindings[&a].automatic_reason.as_deref(),
        Some("failover")
    );
    assert_eq!(snapshot.bindings[&b].node_id, "node1");
    assert!(!snapshot.route_active(&snapshot.bindings[&b]));
    assert_eq!(op.result.unwrap()["succeeded"], 1);
}

#[tokio::test]
async fn new_account_markers_are_atomic_and_do_not_rebind_updated_or_explicitly_unbound_accounts() {
    let manager = queued_manager(2, 3);
    let accounts = AccountStore::new(manager.store.database_for_test()).unwrap();
    let one = record("new@fixture");
    assert_eq!(
        manager
            .import_accounts(&accounts, std::slice::from_ref(&one))
            .await
            .unwrap(),
        vec![UpsertOutcome::Inserted]
    );
    let id = manager.account_id("new@fixture").unwrap();
    assert_eq!(manager.store.pending_auto_accounts().unwrap(), vec![id]);
    let cancelled = run(
        &manager,
        Action::DeleteBinding {
            email: "new@fixture".into(),
            expected_revision: 0,
        },
    )
    .await;
    assert_eq!(cancelled.phase, "committed");
    assert!(manager.ensure_account_assignment(id).is_ok());
    assert_eq!(
        manager.import_accounts(&accounts, &[one]).await.unwrap(),
        vec![UpsertOutcome::Unchanged]
    );
    assert!(manager.store.pending_auto_accounts().unwrap().is_empty());
    let mut invalid = record("invalid@fixture");
    invalid.device_id = Some("invalid".into());
    assert!(
        manager
            .import_accounts(&accounts, &[record("rolled-back@fixture"), invalid])
            .await
            .is_err()
    );
    assert!(accounts.get("rolled-back@fixture").unwrap().is_none());
    assert!(manager.store.pending_auto_accounts().unwrap().is_empty());
}

#[tokio::test]
async fn settings_are_versioned_persisted_and_reject_invalid_limits_without_mutation() {
    let manager = queued_manager(2, 3);
    let mut settings = manager.settings_view().settings;
    settings.manual_test_concurrency = 7;
    settings.auto_test_interval_secs = 600;
    let op = run(
        &manager,
        Action::UpdateSettings {
            expected_revision: 0,
            settings: settings.clone(),
        },
    )
    .await;
    assert_eq!(op.phase, "committed");
    assert_eq!(manager.config().manual_test_concurrency, 7);
    let stale = run(
        &manager,
        Action::UpdateSettings {
            expected_revision: 0,
            settings: settings.clone(),
        },
    )
    .await;
    assert_eq!(stale.phase, "failed");
    settings.manual_test_concurrency = 0;
    assert_eq!(
        run(
            &manager,
            Action::UpdateSettings {
                expected_revision: 1,
                settings
            }
        )
        .await
        .phase,
        "failed"
    );
    assert_eq!(manager.config().manual_test_concurrency, 7);
    let restored = ProxyManager::with_crypto(
        manager.settings.clone(),
        manager.store.database_for_test(),
        Some(
            Crypto::from_base64(&base64::engine::general_purpose::STANDARD.encode([42u8; 32]))
                .unwrap(),
        ),
    )
    .unwrap();
    assert_eq!(restored.settings_view().revision, 1);
    assert_eq!(restored.config().auto_test_interval_secs, 600);
    assert_eq!(restored.config().manual_test_concurrency, 7);
}

#[tokio::test]
async fn automatic_tests_persist_due_time_deduplicate_and_can_be_disabled() {
    let manager = queued_manager(2, 3);
    manager.router.publish(Snapshot::default(), true);
    manager.automation_tick().await.unwrap();
    let before: TestSchedule = manager.store.preference("test_schedule").unwrap().unwrap();
    let id = before.operation_id.unwrap();
    manager.automation_tick().await.unwrap();
    assert_eq!(manager.store.pending().unwrap().len(), 1);
    manager
        .run_operation(manager.store.operation(&id).unwrap())
        .await;
    manager.automation_tick().await.unwrap();
    let after: TestSchedule = manager.store.preference("test_schedule").unwrap().unwrap();
    assert!(after.operation_id.is_none());
    assert!(after.next_test_at > now() + 1799);
    assert_eq!(
        manager
            .store
            .preference::<TestSchedule>("test_schedule")
            .unwrap()
            .unwrap()
            .next_test_at,
        after.next_test_at
    );
    policy(&manager, |settings| settings.auto_test_enabled = false);
    manager
        .store
        .save_preference("test_schedule", &TestSchedule::default())
        .unwrap();
    manager.automation_tick().await.unwrap();
    assert!(manager.store.pending().unwrap().is_empty());
}

#[tokio::test]
async fn changed_pending_accounts_do_not_reuse_a_completed_reconciliation_in_the_same_time_window()
{
    let manager = queued_manager(2, 3);
    policy(&manager, |settings| settings.auto_test_enabled = false);
    fixture_snapshot(&manager, "https://example.invalid/sub", 2).await;
    let mut snapshot = manager.router.snapshot();
    for node in snapshot.nodes.values_mut() {
        node.delay_ms = Some(10);
        node.delay_checked_at = Some(now());
    }
    manager.router.publish_metadata(snapshot);
    let accounts = AccountStore::new(manager.store.database_for_test()).unwrap();
    manager
        .import_accounts(&accounts, &[record("first@fixture")])
        .await
        .unwrap();
    manager.automation_tick().await.unwrap();
    let first = manager
        .store
        .pending()
        .unwrap()
        .into_iter()
        .find(|op| op.resource.as_deref() == Some("reconcile-bindings"))
        .unwrap();
    let mut completed = first.clone();
    completed.phase = "committed".into();
    manager.store.save_operation(&completed).unwrap();
    manager
        .import_accounts(&accounts, &[record("second@fixture")])
        .await
        .unwrap();
    manager.automation_tick().await.unwrap();
    let next = manager
        .store
        .pending()
        .unwrap()
        .into_iter()
        .find(|op| op.resource.as_deref() == Some("reconcile-bindings"))
        .unwrap();
    assert_ne!(first.id, next.id);
}

#[tokio::test]
#[ignore = "requires MIHOMO_TEST_BINARY pointing to the pinned native core"]
async fn real_core_binding_automation_assigns_and_fails_over_without_reusing_a_node() {
    let mut manager = queued_manager(2, 3);
    let control = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = control.local_addr().unwrap();
    drop(control);
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    {
        let manager = Arc::get_mut(&mut manager).unwrap();
        let mut config = (*manager.settings).clone();
        config.proxy_manager.binary_path = std::env::var_os("MIHOMO_TEST_BINARY").unwrap().into();
        config.proxy_manager.controller_addr = address;
        config.proxy_manager.port_start = port;
        config.proxy_manager.port_end = port + 5;
        manager.settings = Arc::new(config);
        manager.started.store(false, Ordering::SeqCst);
    }
    policy(&manager, |settings| settings.auto_test_enabled = false);
    let accounts = AccountStore::new(manager.store.database_for_test()).unwrap();
    manager
        .import_accounts(
            &accounts,
            &[record("auto-one@fixture"), record("auto-two@fixture")],
        )
        .await
        .unwrap();
    let crypto = manager.crypto().unwrap();
    let sub_id = id();
    let mut sub = super::super::model::test_data::subscription(&sub_id);
    sub.url_ciphertext = crypto
        .seal(
            &format!("subscription:{sub_id}"),
            b"https://example.invalid/sub",
        )
        .unwrap();
    let mut snapshot = Snapshot::default();
    snapshot.subscriptions.insert(sub_id.clone(), sub.clone());
    let data=b"proxies:\n- {name: first, type: http, server: 127.0.0.1, port: 9}\n- {name: second, type: http, server: 127.0.0.1, port: 9}\n- {name: spare, type: http, server: 127.0.0.1, port: 9}";
    let mut nodes = subscription::parse(data, &sub, &snapshot, crypto, &manager.config()).unwrap();
    for (index, node) in nodes.iter_mut().enumerate() {
        node.delay_ms = Some(10 + index as u64);
        node.delay_checked_at = Some(now());
        snapshot.nodes.insert(node.id.clone(), node.clone());
    }
    let t = now();
    let init = Operation {
        id: id(),
        phase: "committed".into(),
        code: None,
        message: None,
        generation: None,
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
    manager.store.save_operation(&init).unwrap();
    manager.store.commit(&snapshot, &init, &[]).unwrap();
    manager.router.publish_metadata(snapshot);
    manager.start().await.unwrap();
    let op = manager
        .enqueue(Action::ReconcileBindings, None)
        .await
        .unwrap();
    manager.wait(&op.id).await.unwrap();
    let first = manager.account_id("auto-one@fixture").unwrap();
    let second = manager.account_id("auto-two@fixture").unwrap();
    let bound = manager.router.snapshot();
    assert_eq!(bound.bindings[&first].node_id, nodes[0].id);
    assert_eq!(bound.bindings[&second].node_id, nodes[1].id);
    assert!(manager.store.pending_auto_accounts().unwrap().is_empty());
    let old_port = bound.bindings[&first].listener_port;
    let mut failed = bound.clone();
    let node = failed.nodes.get_mut(&nodes[0].id).unwrap();
    node.delay_ms = None;
    node.delay_error_code = Some("proxy_test_failed".into());
    manager.router.publish_metadata(failed);
    assert!(manager.router.lease(first, false).is_err());
    let op = manager
        .enqueue(Action::ReconcileBindings, None)
        .await
        .unwrap();
    manager.wait(&op.id).await.unwrap();
    let recovered = manager.router.snapshot();
    assert_eq!(recovered.bindings[&first].node_id, nodes[2].id);
    assert_eq!(recovered.bindings[&first].listener_port, old_port);
    assert_eq!(recovered.bindings[&first].revision, 2);
    assert_eq!(recovered.bindings[&second].node_id, nodes[1].id);
    assert!(manager.ensure_account_assignment(first).is_ok());
    assert!(manager.router.lease(first, false).is_ok());
    manager.shutdown().await;
    std::fs::remove_dir_all(&manager.config().work_dir).unwrap();
}
