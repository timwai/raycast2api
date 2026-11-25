//! Authenticated control plane. Controller secrets and raw node credentials never leave here.
use crate::{
    error::GatewayError,
    proxy_manager::{Action, Operation, SubscriptionInput},
    server::middleware::ManagementIdentity,
    server::state::SharedState,
};
use axum::{
    Extension, Json,
    extract::State,
    http::{HeaderMap, StatusCode},
};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct List {
    page: Option<u32>,
    page_size: Option<u32>,
    subscription_id: Option<String>,
    country: Option<String>,
    keyword: Option<String>,
    status: Option<String>,
    sort_by_delay: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Create {
    name: String,
    url: String,
    #[serde(default = "yes")]
    enabled: bool,
    refresh_interval_secs: Option<u64>,
}
fn yes() -> bool {
    true
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Update {
    id: String,
    expected_revision: u64,
    name: Option<String>,
    url: Option<String>,
    enabled: Option<bool>,
    refresh_interval_secs: Option<u64>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Id {
    id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Delete {
    id: String,
    expected_revision: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetBinding {
    email: String,
    node_id: String,
    expected_revision: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteBinding {
    email: String,
    expected_revision: u64,
}
fn key(
    headers: &HeaderMap,
    identity: &ManagementIdentity,
    route: &str,
) -> Result<Option<String>, GatewayError> {
    use sha2::Digest;
    let Some(value) = headers.get("idempotency-key") else {
        return Ok(None);
    };
    let value = value
        .to_str()
        .map_err(|_| GatewayError::BadRequest("invalid idempotency key".into()))?;
    if value.is_empty() || value.len() > 200 || !value.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(GatewayError::BadRequest(
            "idempotency key must contain 1 to 200 visible ASCII characters".into(),
        ));
    }
    let scope = serde_json::to_vec(&(&identity.0, route, value))?;
    Ok(Some(format!(
        "request:{}",
        hex::encode(sha2::Sha256::digest(scope))
    )))
}
async fn enqueue(
    state: SharedState,
    headers: HeaderMap,
    identity: ManagementIdentity,
    route: &str,
    action: Action,
) -> Result<(StatusCode, Json<Operation>), GatewayError> {
    let key = key(&headers, &identity, route)?;
    Ok((
        StatusCode::ACCEPTED,
        Json(state.proxy_manager.enqueue_request(action, key).await?),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchCreate {
    items: Vec<SubscriptionInput>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchTest {
    ids: Vec<String>,
}
pub async fn create_batch(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Extension(identity): Extension<ManagementIdentity>,
    Json(p): Json<BatchCreate>,
) -> Result<(StatusCode, Json<Operation>), GatewayError> {
    if p.items.is_empty() || p.items.len() > 50 {
        return Err(GatewayError::BadRequest(
            "batch must contain 1 to 50 subscriptions".into(),
        ));
    }
    enqueue(
        state,
        headers,
        identity,
        "subscriptions/create-batch",
        Action::CreateSubscriptions { items: p.items },
    )
    .await
}
pub async fn test_batch(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Extension(identity): Extension<ManagementIdentity>,
    Json(p): Json<BatchTest>,
) -> Result<(StatusCode, Json<Operation>), GatewayError> {
    if p.ids.is_empty() || p.ids.len() > 20 {
        return Err(GatewayError::BadRequest(
            "select 1 to 20 nodes for batch testing".into(),
        ));
    }
    enqueue(
        state,
        headers,
        identity,
        "nodes/test-batch",
        Action::TestNodes { ids: p.ids },
    )
    .await
}
pub async fn test_all(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Extension(identity): Extension<ManagementIdentity>,
) -> Result<(StatusCode, Json<Operation>), GatewayError> {
    enqueue(
        state,
        headers,
        identity,
        "nodes/test-all",
        Action::TestAllNodes,
    )
    .await
}
fn page(items: Vec<Value>, p: List) -> Result<Json<Value>, GatewayError> {
    let page = p.page.unwrap_or(1);
    let size = p.page_size.unwrap_or(50);
    if page == 0 || size == 0 || size > 200 {
        return Err(GatewayError::BadRequest(
            "page must be positive and page_size <= 200".into(),
        ));
    }
    let total = items.len();
    let selected = items
        .into_iter()
        .skip((page as usize - 1) * size as usize)
        .take(size as usize)
        .collect::<Vec<_>>();
    Ok(Json(
        json!({"items":selected,"total":total,"page":page,"page_size":size,"pages":total.div_ceil(size as usize)}),
    ))
}
pub async fn subscriptions(
    State(state): State<SharedState>,
    Json(p): Json<List>,
) -> Result<Json<Value>, GatewayError> {
    let snap = state.proxy_manager.snapshot();
    let items = snap
        .subscriptions
        .values()
        .map(|s| {
            let mut v = serde_json::to_value(s).unwrap();
            v["node_count"] = json!(
                snap.nodes
                    .values()
                    .filter(|n| n.subscription_id == s.id && n.presence == "present")
                    .count()
            );
            v
        })
        .collect();
    page(items, p)
}
pub async fn create(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Extension(identity): Extension<ManagementIdentity>,
    Json(p): Json<Create>,
) -> Result<(StatusCode, Json<Operation>), GatewayError> {
    let interval = p
        .refresh_interval_secs
        .unwrap_or(state.proxy_manager.config().refresh_interval_secs);
    enqueue(
        state,
        headers,
        identity,
        "subscriptions/create",
        Action::CreateSubscription {
            name: p.name,
            url: p.url,
            enabled: p.enabled,
            refresh_interval_secs: interval,
        },
    )
    .await
}
pub async fn update(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Extension(identity): Extension<ManagementIdentity>,
    Json(p): Json<Update>,
) -> Result<(StatusCode, Json<Operation>), GatewayError> {
    enqueue(
        state,
        headers,
        identity,
        "subscriptions/update",
        Action::UpdateSubscription {
            id: p.id,
            expected_revision: p.expected_revision,
            name: p.name,
            url: p.url,
            enabled: p.enabled,
            refresh_interval_secs: p.refresh_interval_secs,
        },
    )
    .await
}
pub async fn delete(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Extension(identity): Extension<ManagementIdentity>,
    Json(p): Json<Delete>,
) -> Result<(StatusCode, Json<Operation>), GatewayError> {
    enqueue(
        state,
        headers,
        identity,
        "subscriptions/delete",
        Action::DeleteSubscription {
            id: p.id,
            expected_revision: p.expected_revision,
        },
    )
    .await
}
pub async fn refresh(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Extension(identity): Extension<ManagementIdentity>,
    Json(p): Json<Id>,
) -> Result<(StatusCode, Json<Operation>), GatewayError> {
    enqueue(
        state,
        headers,
        identity,
        "subscriptions/refresh",
        Action::Refresh { id: p.id },
    )
    .await
}
pub async fn refresh_all(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Extension(identity): Extension<ManagementIdentity>,
) -> Result<(StatusCode, Json<Operation>), GatewayError> {
    enqueue(
        state,
        headers,
        identity,
        "subscriptions/refresh-all",
        Action::RefreshAll,
    )
    .await
}
pub async fn nodes(
    State(state): State<SharedState>,
    Json(p): Json<List>,
) -> Result<Json<Value>, GatewayError> {
    let presence = p.status.as_deref().unwrap_or("present");
    if !matches!(presence, "present" | "missing" | "all") {
        return Err(GatewayError::BadRequest(
            "node status must be present, missing, or all".into(),
        ));
    }
    let snap = state.proxy_manager.snapshot();
    let mut nodes = snap
        .nodes
        .values()
        .filter(|n| {
            p.subscription_id
                .as_ref()
                .is_none_or(|id| *id == n.subscription_id)
                && p.country
                    .as_ref()
                    .is_none_or(|c| n.country.as_ref() == Some(c))
                && (presence == "all" || presence == n.presence)
                && p.keyword.as_ref().is_none_or(|k| {
                    n.original_name.to_lowercase().contains(&k.to_lowercase())
                        || n.server.to_lowercase().contains(&k.to_lowercase())
                })
        })
        .map(|n| {
            let mut v = serde_json::to_value(n).unwrap();
            v["subscription_enabled"] = json!(
                snap.subscriptions
                    .get(&n.subscription_id)
                    .is_some_and(|s| s.enabled)
            );
            v["subscription_name"] =
                json!(snap.subscriptions.get(&n.subscription_id).map(|s| &s.name));
            v["binding_count"] =
                json!(snap.bindings.values().filter(|b| b.node_id == n.id).count());
            v["test_status"] = json!(n.test_status());
            v["selectable"] = json!(
                n.presence == "present" && v["subscription_enabled"] == true && n.test_passed()
            );
            v
        })
        .collect::<Vec<_>>();
    if p.sort_by_delay {
        nodes.sort_by_key(|v| v["delay_ms"].as_u64().unwrap_or(u64::MAX));
    }
    page(nodes, p)
}
pub async fn node_detail(
    State(state): State<SharedState>,
    Json(p): Json<Id>,
) -> Result<Json<Value>, GatewayError> {
    Ok(Json(
        serde_json::to_value(
            state
                .proxy_manager
                .snapshot()
                .nodes
                .get(&p.id)
                .ok_or_else(|| GatewayError::NotFound("node not found".into()))?,
        )
        .unwrap(),
    ))
}
pub async fn test(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Extension(identity): Extension<ManagementIdentity>,
    Json(p): Json<Id>,
) -> Result<(StatusCode, Json<Operation>), GatewayError> {
    enqueue(
        state,
        headers,
        identity,
        "nodes/test",
        Action::TestNode { id: p.id },
    )
    .await
}
pub async fn bindings(
    State(state): State<SharedState>,
    Json(p): Json<List>,
) -> Result<Json<Value>, GatewayError> {
    page(state.proxy_manager.bindings()?, p)
}
pub async fn set_binding(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Extension(identity): Extension<ManagementIdentity>,
    Json(p): Json<SetBinding>,
) -> Result<(StatusCode, Json<Operation>), GatewayError> {
    enqueue(
        state,
        headers,
        identity,
        "bindings/set",
        Action::SetBinding {
            email: p.email,
            node_id: p.node_id,
            expected_revision: p.expected_revision,
        },
    )
    .await
}
pub async fn delete_binding(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Extension(identity): Extension<ManagementIdentity>,
    Json(p): Json<DeleteBinding>,
) -> Result<(StatusCode, Json<Operation>), GatewayError> {
    enqueue(
        state,
        headers,
        identity,
        "bindings/delete",
        Action::DeleteBinding {
            email: p.email,
            expected_revision: p.expected_revision,
        },
    )
    .await
}
pub async fn operation(
    State(state): State<SharedState>,
    Json(p): Json<Id>,
) -> Result<Json<Operation>, GatewayError> {
    Ok(Json(state.proxy_manager.operation(&p.id)?))
}
pub async fn status(State(state): State<SharedState>) -> Json<Value> {
    Json(state.proxy_manager.status())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::{Method, Request},
    };
    use tower::ServiceExt;
    #[tokio::test]
    async fn node_list_exposes_test_eligibility_and_test_all_requires_authentication() {
        let state = crate::server::state::compact_test_state();
        let mut snapshot = state.proxy_manager.router.snapshot();
        snapshot.subscriptions.insert(
            "sub".into(),
            serde_json::from_value(json!({
                "id":"sub","name":"sub","enabled":true,"refresh_interval_secs":21600,
                "revision":1,"created_at":1,"updated_at":1
            }))
            .unwrap(),
        );
        for (id, status) in [
            ("passed", "passed"),
            ("failed", "failed"),
            ("untested", "untested"),
        ] {
            let mut node: crate::proxy_manager::Node = serde_json::from_value(json!({
                "id":id,"subscription_id":"sub","mihomo_name":id,"original_name":id,
                "protocol":"http","server":"fixture.invalid","server_port":443,
                "identity_key":id,"presence":"present","first_seen_at":1,"last_seen_at":1
            }))
            .unwrap();
            if status != "untested" {
                node.delay_checked_at = Some(100);
            }
            if status == "passed" {
                node.delay_ms = Some(42);
            }
            if status == "failed" {
                node.delay_error_code = Some("proxy_test_failed".into());
            }
            snapshot.nodes.insert(id.into(), node);
        }
        state.proxy_manager.router.publish_metadata(snapshot);
        let app = crate::server::build_router(state);
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/proxy/nodes/list")
                    .header("Authorization", "Bearer test-key")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap();
        for item in body["data"]["items"].as_array().unwrap() {
            assert_eq!(item["test_status"], item["id"]);
            assert_eq!(item["selectable"], item["id"] == "passed");
        }
        for authenticated in [false, true] {
            let mut request = Request::builder()
                .method(Method::POST)
                .uri("/v1/proxy/nodes/test-all");
            if authenticated {
                request = request.header("Authorization", "Bearer test-key");
            }
            let response = app
                .clone()
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                if authenticated {
                    StatusCode::SERVICE_UNAVAILABLE
                } else {
                    StatusCode::UNAUTHORIZED
                }
            );
        }
    }
    #[test]
    fn idempotency_keys_are_isolated_by_authenticated_caller_and_endpoint() {
        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", "same-key".parse().unwrap());
        let first = ManagementIdentity("caller-one".into());
        let second = ManagementIdentity("caller-two".into());
        let scoped = key(&headers, &first, "subscriptions/create").unwrap();
        assert_eq!(
            scoped,
            key(&headers, &first, "subscriptions/create").unwrap()
        );
        assert_ne!(
            scoped,
            key(&headers, &second, "subscriptions/create").unwrap()
        );
        assert_ne!(
            scoped,
            key(&headers, &first, "subscriptions/create-batch").unwrap()
        );
        assert!(!scoped.unwrap().contains("same-key"));
        for invalid in ["", "two words", &"a".repeat(201)] {
            headers.insert("idempotency-key", invalid.parse().unwrap());
            assert!(key(&headers, &first, "subscriptions/create").is_err());
        }
    }
    #[tokio::test]
    async fn node_list_defaults_to_current_nodes_and_can_show_bound_missing_nodes() {
        let state = crate::server::state::compact_test_state();
        let mut snapshot = state.proxy_manager.router.snapshot();
        for (id, presence) in [("current", "present"), ("bound-old", "missing")] {
            let node =
                serde_json::from_value(json!({"id":id,"subscription_id":"sub","mihomo_name":id,
                "original_name":id,"protocol":"http","server":"example.invalid","server_port":443,
                "identity_key":id,"presence":presence,"first_seen_at":1,"last_seen_at":1}))
                .unwrap();
            snapshot.nodes.insert(id.into(), node);
        }
        state.proxy_manager.router.publish_metadata(snapshot);
        let app = crate::server::build_router(state);
        for (body, total, expected) in [
            (json!({}), 1, Some("current")),
            (json!({"status":"missing"}), 1, Some("bound-old")),
            (json!({"status":"all"}), 2, None),
        ] {
            let request = Request::builder()
                .method(Method::POST)
                .uri("/v1/proxy/nodes/list")
                .header("Authorization", "Bearer test-key")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap();
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body: Value =
                serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap())
                    .unwrap();
            assert_eq!(body["data"]["total"], total);
            if let Some(expected) = expected {
                assert_eq!(body["data"]["items"][0]["id"], expected);
            }
        }
    }

    #[tokio::test]
    async fn management_authentication_and_disabled_binding_are_explicit() {
        let state = crate::server::state::compact_test_state();
        let app = crate::server::build_router(state.clone());
        let request = |key: Option<&str>| {
            let mut request = Request::builder()
                .method(Method::GET)
                .uri("/v1/proxy/status");
            if let Some(key) = key {
                request = request.header("Authorization", format!("Bearer {key}"));
            }
            request.body(Body::empty()).unwrap()
        };
        assert_eq!(
            app.clone().oneshot(request(None)).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
        let response = app
            .clone()
            .oneshot(request(Some("test-key")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["cache-control"], "no-store");
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap();
        assert_eq!(body["data"]["enabled"], false);
        let managed = Request::builder()
            .method(Method::POST)
            .uri("/v1/api-keys/create")
            .header("Authorization", "Bearer test-key")
            .header("content-type", "application/json")
            .body(Body::from("{\"mode\":\"random\"}"))
            .unwrap();
        let response = app.clone().oneshot(managed).await.unwrap();
        assert!(response.status().is_success());
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap();
        let key = body["data"]["api_key"].as_str().unwrap();
        assert_eq!(
            app.oneshot(request(Some(key))).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
    }
}
