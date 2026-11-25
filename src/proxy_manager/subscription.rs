use super::{config::ProxyManagerConfig, crypto::Crypto, error, model::*};
use crate::{config::Settings, error::GatewayError};
use futures_util::StreamExt;
use serde_json::Value;
use std::{collections::HashSet, net::IpAddr, time::Duration};

fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            let [a, b, c, _] = v.octets();
            !v.is_private()
                && !v.is_loopback()
                && !v.is_link_local()
                && !v.is_unspecified()
                && !v.is_multicast()
                && !v.is_broadcast()
                && a != 0
                && a < 224
                && !(a == 100 && (64..=127).contains(&b))
                && !(a == 192 && b == 0 && (c == 0 || c == 2))
                && !(a == 198 && ((b == 18 || b == 19) || (b == 51 && c == 100)))
                && !(a == 203 && b == 0 && c == 113)
        }
        IpAddr::V6(v) => v.to_ipv4_mapped().map_or_else(
            || {
                (v.segments()[0] & 0xe000) == 0x2000
                    && !(v.segments()[0] == 0x2001 && v.segments()[1] == 0x0db8)
                    && !v.is_loopback()
                    && !v.is_unspecified()
                    && !v.is_multicast()
                    && (v.segments()[0] & 0xfe00) != 0xfc00
                    && (v.segments()[0] & 0xffc0) != 0xfe80
            },
            |v| public_ip(IpAddr::V4(v)),
        ),
    }
}
pub async fn download(url: &str, settings: &Settings) -> Result<Vec<u8>, GatewayError> {
    let config = &settings.proxy_manager;
    let mut url = reqwest::Url::parse(url)
        .map_err(|_| error("subscription_invalid", "invalid subscription URL"))?;
    let deadline =
        tokio::time::Instant::now() + Duration::from_secs(config.subscription_timeout_secs);
    let future = async {
        for hop in 0..=3 {
            if url.scheme() != "https"
                || !url.username().is_empty()
                || url.password().is_some()
                || url.fragment().is_some()
            {
                return Err(error(
                    "subscription_invalid",
                    "subscription URL must be HTTPS without credentials or fragment",
                ));
            }
            let host = url
                .host_str()
                .ok_or_else(|| error("subscription_invalid", "subscription URL has no host"))?
                .trim_matches(['[', ']'])
                .to_string();
            let addresses = tokio::net::lookup_host((
                host.as_str(),
                url.port_or_known_default().unwrap_or(443),
            ))
            .await
            .map_err(|_| {
                error(
                    "subscription_fetch_failed",
                    "subscription DNS resolution failed",
                )
            })?
            .collect::<Vec<_>>();
            let allowed = config
                .subscription_allowed_private_hosts
                .iter()
                .any(|x| x.eq_ignore_ascii_case(&host));
            if addresses.is_empty() || (!allowed && addresses.iter().any(|a| !public_ip(a.ip()))) {
                return Err(error(
                    "subscription_invalid",
                    "subscription address is outside allowed network scope",
                ));
            }
            let mut builder = reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(config.subscription_timeout_secs))
                .resolve_to_addrs(&host, &addresses);
            if config.subscription_download_route == "global"
                && settings.proxies_mode != crate::proxies::ProxyMode::Direct
            {
                // A remote proxy resolves CONNECT hostnames itself. Require explicit operator trust.
                if !allowed {
                    return Err(error(
                        "subscription_invalid",
                        "proxied subscription downloads require an explicit host allowlist entry",
                    ));
                }
                let proxy = settings
                    .proxies
                    .first()
                    .ok_or_else(|| error("subscription_invalid", "no global proxy configured"))?;
                builder =
                    builder
                        .proxy(reqwest::Proxy::all(proxy).map_err(|_| {
                            error("subscription_invalid", "invalid download proxy")
                        })?);
            }
            let client = builder.build().map_err(|_| {
                error(
                    "subscription_fetch_failed",
                    "download client initialization failed",
                )
            })?;
            let response = client.get(url.clone()).send().await.map_err(|_| {
                error(
                    "subscription_fetch_failed",
                    "subscription HTTPS request failed",
                )
            })?;
            if response.status().is_redirection() {
                if hop == 3 {
                    return Err(error(
                        "subscription_fetch_failed",
                        "subscription redirect limit exceeded",
                    ));
                }
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|x| x.to_str().ok())
                    .ok_or_else(|| {
                        error("subscription_fetch_failed", "invalid subscription redirect")
                    })?;
                url = url.join(location).map_err(|_| {
                    error("subscription_fetch_failed", "invalid subscription redirect")
                })?;
                continue;
            }
            if !response.status().is_success() {
                return Err(error(
                    "subscription_fetch_failed",
                    format!("subscription returned HTTP {}", response.status().as_u16()),
                ));
            }
            let mut bytes = Vec::new();
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|_| {
                    error(
                        "subscription_fetch_failed",
                        "subscription response interrupted",
                    )
                })?;
                if bytes.len().saturating_add(chunk.len()) > config.subscription_max_bytes {
                    return Err(error(
                        "subscription_invalid",
                        "subscription exceeds download size limit",
                    ));
                }
                bytes.extend_from_slice(&chunk);
            }
            return Ok(bytes);
        }
        unreachable!()
    };
    tokio::time::timeout_at(deadline, future)
        .await
        .map_err(|_| {
            error(
                "subscription_fetch_failed",
                "subscription download timed out",
            )
        })?
}
pub fn parse(
    data: &[u8],
    sub: &Subscription,
    old: &Snapshot,
    crypto: &Crypto,
    config: &ProxyManagerConfig,
) -> Result<Vec<Node>, GatewayError> {
    let fail = || {
        error(
            "subscription_invalid",
            "invalid or unsupported Clash subscription; previous version retained",
        )
    };
    let mut budget = serde_saphyr::Budget::default();
    budget.max_aliases = 128;
    budget.max_anchors = 64;
    budget.max_depth = 32;
    budget.max_documents = 1;
    budget.max_nodes = config
        .subscription_max_nodes
        .saturating_mul(200)
        .min(2_000_000);
    budget.max_total_scalar_bytes = config.subscription_max_bytes;
    let options = serde_saphyr::options! {budget:Some(budget),reject_unsupported_tags:true};
    let value: Value = serde_saphyr::from_str_with_options(
        std::str::from_utf8(data).map_err(|_| fail())?,
        options,
    )
    .map_err(|_| fail())?;
    let entries = value
        .get("proxies")
        .and_then(Value::as_array)
        .filter(|x| !x.is_empty() && x.len() <= config.subscription_max_nodes)
        .ok_or_else(fail)?;
    let mut names = HashSet::new();
    let mut result = Vec::new();
    for entry in entries {
        let mut object = entry.as_object().cloned().ok_or_else(fail)?;
        validate_fields(&Value::Object(object.clone()), 0).map_err(|_| fail())?;
        let name = object
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty() && s.len() <= 512)
            .ok_or_else(fail)?
            .to_string();
        if !names.insert(name.clone()) {
            return Err(fail());
        }
        let protocol = object
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(fail)?
            .to_ascii_lowercase();
        if ![
            "http",
            "socks5",
            "ss",
            "vmess",
            "vless",
            "trojan",
            "hysteria2",
            "tuic",
        ]
        .contains(&protocol.as_str())
        {
            return Err(fail());
        }
        let server = object
            .get("server")
            .and_then(Value::as_str)
            .filter(|x| !x.is_empty() && x.len() <= 253)
            .ok_or_else(fail)?
            .to_string();
        let port = object
            .get("port")
            .and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()))
            .and_then(|n| u16::try_from(n).ok())
            .filter(|n| *n > 0)
            .ok_or_else(fail)?;
        let identity = crypto.fingerprint(
            &serde_json::to_vec(&(&name, &protocol, server.to_ascii_lowercase(), port))
                .map_err(|_| fail())?,
        );
        let previous = old
            .nodes
            .values()
            .find(|n| n.subscription_id == sub.id && n.identity_key == identity);
        let node_id = previous.map_or_else(id, |n| n.id.clone());
        let mihomo_name = format!("rpnode_{node_id}");
        object.insert("name".into(), Value::String(mihomo_name.clone()));
        object.insert("type".into(), Value::String(protocol.clone()));
        object.insert("port".into(), Value::from(port));
        let bytes = serde_json::to_vec(&object).map_err(|_| fail())?;
        let now = now();
        result.push(Node {
            id: node_id.clone(),
            subscription_id: sub.id.clone(),
            mihomo_name,
            original_name: name.clone(),
            protocol,
            server,
            server_port: port,
            identity_key: identity,
            config_ciphertext: crypto
                .seal(&format!("node:{node_id}"), &bytes)
                .map_err(|_| fail())?,
            config_fingerprint: crypto.fingerprint(&bytes),
            presence: "present".into(),
            country: previous
                .and_then(|n| n.country.clone())
                .or_else(|| region_hint(&name).map(str::to_string)),
            country_source: previous
                .and_then(|n| n.country_source.clone())
                .or_else(|| region_hint(&name).map(|_| "name_hint".to_string())),
            delay_ms: None,
            delay_checked_at: None,
            delay_error_code: None,
            first_seen_at: previous.map_or(now, |n| n.first_seen_at),
            last_seen_at: now,
        });
    }
    Ok(result)
}
fn region_hint(name: &str) -> Option<&'static str> {
    let lower = name.to_lowercase();
    for (code, hints) in [
        ("JP", ["日本", "🇯🇵", "japan"]),
        ("US", ["美国", "🇺🇸", "united states"]),
        ("HK", ["香港", "🇭🇰", "hong kong"]),
        ("SG", ["新加坡", "🇸🇬", "singapore"]),
        ("TW", ["台湾", "🇹🇼", "taiwan"]),
        ("KR", ["韩国", "🇰🇷", "korea"]),
        ("DE", ["德国", "🇩🇪", "germany"]),
        ("GB", ["英国", "🇬🇧", "united kingdom"]),
    ] {
        if hints.iter().any(|h| lower.contains(h)) {
            return Some(code);
        }
    }
    None
}

fn validate_fields(value: &Value, depth: usize) -> Result<(), ()> {
    if depth > 32 {
        return Err(());
    }
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                if [
                    "dialer-proxy",
                    "proxy",
                    "private-key-path",
                    "certificate-path",
                    "plugin",
                    "plugin-opts",
                    "script",
                    "interface-name",
                    "routing-mark",
                    "include",
                    "<<",
                ]
                .contains(&k.as_str())
                {
                    return Err(());
                }
                validate_fields(v, depth + 1)?;
            }
        }
        Value::Array(xs) => {
            for x in xs {
                validate_fields(x, depth + 1)?;
            }
        }
        Value::String(s) if s.len() > 65536 => return Err(()),
        _ => {}
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    #[test]
    fn untrusted_yaml_cannot_override_control_plane_or_expand_aliases() {
        let crypto =
            Crypto::from_base64(&base64::engine::general_purpose::STANDARD.encode([1u8; 32]))
                .unwrap();
        let sub = Subscription {
            id: "s".into(),
            name: "test".into(),
            url_ciphertext: String::new(),
            enabled: true,
            refresh_interval_secs: 21600,
            revision: 1,
            next_refresh_at: None,
            last_attempt_at: None,
            last_success_at: None,
            last_error_code: None,
            created_at: 0,
            updated_at: 0,
        };
        for data in [
            "proxies: []",
            "proxies: &a [*a]",
            "proxies:\n- {name: n, type: direct, server: x, port: 1}",
            "proxies:\n- {name: n, type: ss, server: x, port: 1, dialer-proxy: DIRECT}",
            "proxies:\n- {name: n, name: other, type: ss, server: x, port: 1}",
        ] {
            assert!(
                parse(
                    data.as_bytes(),
                    &sub,
                    &Snapshot::default(),
                    &crypto,
                    &ProxyManagerConfig::default()
                )
                .is_err()
            );
        }
        let nodes = parse(
            b"proxies:\n- {name: JP, type: http, server: example.com, port: 443}",
            &sub,
            &Snapshot::default(),
            &crypto,
            &ProxyManagerConfig::default(),
        )
        .unwrap();
        assert_eq!(nodes.len(), 1);
    }
}
