use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Serialize, Deserialize)]
pub struct Subscription {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub url_ciphertext: String,
    pub enabled: bool,
    pub refresh_interval_secs: u64,
    pub revision: u64,
    #[serde(default)]
    pub next_refresh_at: Option<u64>,
    pub last_attempt_at: Option<u64>,
    pub last_success_at: Option<u64>,
    pub last_error_code: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    pub subscription_id: String,
    pub mihomo_name: String,
    pub original_name: String,
    pub protocol: String,
    pub server: String,
    pub server_port: u16,
    pub identity_key: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub config_ciphertext: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub config_fingerprint: String,
    pub presence: String,
    pub country: Option<String>,
    pub country_source: Option<String>,
    pub delay_ms: Option<u64>,
    pub delay_checked_at: Option<u64>,
    pub delay_error_code: Option<String>,
    pub first_seen_at: u64,
    pub last_seen_at: u64,
}
impl Node {
    pub fn test_passed(&self) -> bool {
        self.delay_checked_at.is_some()
            && self.delay_ms.is_some_and(|delay| delay > 0)
            && self.delay_error_code.is_none()
    }
    pub fn test_status(&self) -> &'static str {
        if self.test_passed() {
            "passed"
        } else if self.delay_checked_at.is_some() || self.delay_error_code.is_some() {
            "failed"
        } else {
            "untested"
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Binding {
    pub account_id: i64,
    pub node_id: String,
    pub listener_port: u16,
    pub revision: u64,
    pub created_at: u64,
    pub updated_at: u64,
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub generation: u64,
    pub subscriptions: BTreeMap<String, Subscription>,
    pub nodes: BTreeMap<String, Node>,
    pub bindings: BTreeMap<i64, Binding>,
}
impl Snapshot {
    /// A successful refresh replaces current nodes and keeps only bound tombstones.
    pub fn replace_subscription_nodes(&mut self, subscription_id: &str, nodes: Vec<Node>) {
        for node in self
            .nodes
            .values_mut()
            .filter(|node| node.subscription_id == subscription_id)
        {
            node.presence = "missing".into();
        }
        for mut node in nodes {
            if let Some(previous) = self.nodes.get(&node.id)
                && previous.config_fingerprint == node.config_fingerprint
            {
                node.config_ciphertext = previous.config_ciphertext.clone();
                node.delay_ms = previous.delay_ms;
                node.delay_checked_at = previous.delay_checked_at;
                node.delay_error_code = previous.delay_error_code.clone();
            }
            self.nodes.insert(node.id.clone(), node);
        }
        self.prune_unbound_missing_nodes();
    }

    pub fn prune_unbound_missing_nodes(&mut self) {
        let bound = self
            .bindings
            .values()
            .map(|binding| binding.node_id.as_str())
            .collect::<std::collections::HashSet<_>>();
        self.nodes
            .retain(|id, node| node.presence != "missing" || bound.contains(id.as_str()));
    }

    pub fn route_active(&self, binding: &Binding) -> bool {
        self.nodes.get(&binding.node_id).is_some_and(|node| {
            node.presence == "present"
                && self
                    .subscriptions
                    .get(&node.subscription_id)
                    .is_some_and(|sub| sub.enabled)
        })
    }
    pub fn public(&self) -> Self {
        let mut copy = self.clone();
        for sub in copy.subscriptions.values_mut() {
            sub.url_ciphertext.clear();
        }
        for node in copy.nodes.values_mut() {
            node.config_ciphertext.clear();
            node.config_fingerprint.clear();
            node.identity_key.clear();
        }
        copy
    }
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Operation {
    pub id: String,
    pub phase: String,
    pub code: Option<String>,
    pub message: Option<String>,
    pub generation: Option<u64>,
    pub revision: Option<u64>,
    pub created_at: u64,
    pub updated_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate: Option<Snapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_ciphertext: Option<String>,
    pub idempotency_key: Option<String>,
    pub request_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
}
pub fn id() -> String {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).expect("OS random source");
    hex::encode(bytes)
}
pub fn now() -> u64 {
    crate::util::unix_secs()
}

#[cfg(test)]
pub(crate) mod test_data {
    use super::*;
    use serde_json::json;
    pub fn node(id: &str, subscription_id: &str, presence: &str) -> Node {
        serde_json::from_value(json!({
            "id":id,"subscription_id":subscription_id,"mihomo_name":format!("rpnode_{id}"),
            "original_name":id,"protocol":"http","server":"example.invalid","server_port":443,
            "identity_key":id,"config_ciphertext":format!("encrypted-{id}"),"config_fingerprint":"unchanged",
            "presence":presence,"first_seen_at":1,"last_seen_at":1
        })).unwrap()
    }
    pub fn subscription(id: &str) -> Subscription {
        serde_json::from_value(
            json!({"id":id,"name":id,"url_ciphertext":"encrypted-url","enabled":true,
            "refresh_interval_secs":21600,"revision":1,"created_at":1,"updated_at":1}),
        )
        .unwrap()
    }
    pub fn binding(account_id: i64, node_id: &str) -> Binding {
        Binding {
            account_id,
            node_id: node_id.into(),
            listener_port: 18000,
            revision: 1,
            created_at: 1,
            updated_at: 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{test_data::*, *};
    #[test]
    fn only_tested_successful_nodes_can_be_selected_and_config_changes_reset_results() {
        let mut tested = node("node", "sub", "present");
        assert_eq!(tested.test_status(), "untested");
        tested.delay_ms = Some(42);
        assert!(
            !tested.test_passed(),
            "a delay alone is not a completed test"
        );
        tested.delay_checked_at = Some(100);
        assert!(tested.test_passed());
        tested.delay_error_code = Some("proxy_test_failed".into());
        assert_eq!(tested.test_status(), "failed");
        let mut snapshot = Snapshot::default();
        snapshot.nodes.insert(tested.id.clone(), tested);
        let mut changed = node("node", "sub", "present");
        changed.config_fingerprint = "new-config".into();
        snapshot.replace_subscription_nodes("sub", vec![changed]);
        assert_eq!(snapshot.nodes["node"].test_status(), "untested");
        assert!(!snapshot.nodes["node"].test_passed());
    }
    #[test]
    fn refresh_replaces_unbound_nodes_but_preserves_bound_tombstones_and_other_subscriptions() {
        let mut snapshot = Snapshot::default();
        for node in [
            node("removed", "sub", "present"),
            node("bound", "sub", "present"),
            node("current", "sub", "present"),
            node("other", "other-sub", "present"),
        ] {
            snapshot.nodes.insert(node.id.clone(), node);
        }
        snapshot.bindings.insert(1, binding(1, "bound"));
        snapshot.nodes.get_mut("current").unwrap().delay_ms = Some(23);
        let mut current = node("current", "sub", "present");
        current.config_ciphertext = "fresh-ciphertext".into();
        snapshot.replace_subscription_nodes("sub", vec![current, node("next", "sub", "present")]);
        assert!(!snapshot.nodes.contains_key("removed"));
        assert_eq!(snapshot.nodes["bound"].presence, "missing");
        assert_eq!(snapshot.bindings[&1].node_id, "bound");
        assert!(!snapshot.route_active(&snapshot.bindings[&1]));
        assert_eq!(snapshot.nodes["other"].presence, "present");
        assert_eq!(
            snapshot.nodes["current"].config_ciphertext,
            "encrypted-current"
        );
        assert_eq!(snapshot.nodes["current"].delay_ms, Some(23));
        snapshot.replace_subscription_nodes("sub", vec![node("next", "sub", "present")]);
        assert_eq!(
            snapshot.nodes.len(),
            3,
            "repeated refreshes must not accumulate obsolete nodes"
        );
        snapshot.bindings.clear();
        snapshot.prune_unbound_missing_nodes();
        assert!(!snapshot.nodes.contains_key("bound"));
        assert!(snapshot.nodes.contains_key("other"));
    }
}
