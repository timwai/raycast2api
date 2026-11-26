use super::{config::ProxyManagerConfig, crypto::Crypto, error, model::Snapshot};
use crate::error::GatewayError;
use fs2::FileExt;
use serde_json::{Value, json};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

pub struct Core {
    config: ProxyManagerConfig,
    pub child: Option<tokio::process::Child>,
    _lock: File,
    http: reqwest::Client,
    secret: String,
    root: PathBuf,
}
#[derive(Clone)]
pub(crate) struct Controller {
    http: reqwest::Client,
    address: std::net::SocketAddr,
    secret: String,
}
impl Controller {
    pub async fn api(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, GatewayError> {
        self.api_with_timeout(method, path, body, Duration::from_secs(10))
            .await
    }
    pub async fn api_with_timeout(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
        timeout: Duration,
    ) -> Result<Value, GatewayError> {
        let mut request = self
            .http
            .request(method, format!("http://{}{}", self.address, path))
            .bearer_auth(&self.secret)
            .timeout(timeout);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request
            .send()
            .await
            .map_err(|_| error("proxy_unavailable", "managed controller is unavailable"))?;
        if !response.status().is_success() {
            return Err(error(
                "proxy_apply_failed",
                format!(
                    "managed controller returned HTTP {}",
                    response.status().as_u16()
                ),
            ));
        }
        if response.status() == reqwest::StatusCode::NO_CONTENT {
            return Ok(Value::Null);
        }
        response
            .json()
            .await
            .map_err(|_| error("proxy_unavailable", "invalid controller response"))
    }
}
impl Core {
    pub fn new(config: &ProxyManagerConfig) -> Result<Self, GatewayError> {
        std::fs::create_dir_all(&config.work_dir).map_err(io_error)?;
        let root = std::fs::canonicalize(&config.work_dir).map_err(io_error)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))
                .map_err(io_error)?;
        }
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join("manager.lock"))
            .map_err(io_error)?;
        lock.try_lock_exclusive().map_err(|_| {
            error(
                "proxy_instance_conflict",
                "proxy work directory is already owned by another instance",
            )
        })?;
        let secret_path = root.join("controller.secret");
        let secret = if secret_path.exists() {
            std::fs::read_to_string(&secret_path).map_err(io_error)?
        } else {
            let secret = format!("{}{}", super::id(), super::id());
            write_private(&secret_path, secret.as_bytes())?;
            secret
        };
        if secret.len() < 32 {
            return Err(error(
                "proxy_config_invalid",
                "invalid stored controller secret",
            ));
        }
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|_| {
                error(
                    "proxy_config_invalid",
                    "controller client initialization failed",
                )
            })?;
        Ok(Self {
            config: config.clone(),
            child: None,
            _lock: lock,
            http,
            secret,
            root,
        })
    }
    pub fn generate(&self, state: &Snapshot, crypto: &Crypto) -> Result<PathBuf, GatewayError> {
        let dir = self
            .root
            .join("generations")
            .join(state.generation.to_string());
        std::fs::create_dir_all(dir.join("providers")).map_err(io_error)?;
        let mut providers = serde_json::Map::new();
        for sub in state.subscriptions.values().filter(|s| s.enabled) {
            let nodes = state
                .nodes
                .values()
                .filter(|n| n.subscription_id == sub.id && n.presence == "present")
                .map(|n| {
                    crypto
                        .open(&format!("node:{}", n.id), &n.config_ciphertext)
                        .map_err(|_| {
                            error(
                                "proxy_decryption_failed",
                                "node configuration cannot be decrypted",
                            )
                        })
                        .and_then(|b| {
                            serde_json::from_slice::<Value>(&b).map_err(|_| {
                                error("proxy_storage_failed", "invalid stored node configuration")
                            })
                        })
                })
                .collect::<Result<Vec<_>, _>>()?;
            if nodes.is_empty() {
                continue;
            }
            let path = dir.join("providers").join(format!("{}.yaml", sub.id));
            write_immutable(
                &path,
                &serde_json::to_vec(&json!({"proxies":nodes})).unwrap(),
            )?;
            providers.insert(
                format!("rp_sub_{}", sub.id),
                json!({"type":"file","path":path,"health-check":{"enable":false}}),
            );
        }
        let mut groups = Vec::new();
        let mut listeners = Vec::new();
        for b in state.bindings.values() {
            let group = format!("rp_acc_{}", b.account_id);
            groups.push(if state.route_configured(b){
                let n=&state.nodes[&b.node_id];json!({"name":group,"type":"select","use":[format!("rp_sub_{}",n.subscription_id)],"filter":format!("^{}$",n.mihomo_name),"default-selected":n.mihomo_name,"empty-fallback":"REJECT","interval":0})
            }else{json!({"name":group,"type":"select","proxies":["REJECT"],"default-selected":"REJECT","empty-fallback":"REJECT","interval":0})});
            listeners.push(json!({"name":format!("rp_listener_{}",b.account_id),"type":"mixed","listen":"127.0.0.1","port":b.listener_port,"proxy":group}));
        }
        let value = json!({"port":0,"socks-port":0,"mixed-port":0,"redir-port":0,"tproxy-port":0,"allow-lan":false,"mode":"rule","log-level":"silent",
            "external-controller":self.config.controller_addr.to_string(),"secret":self.secret,"geo-auto-update":false,
            "tun":{"enable":false},"dns":{"enable":false},"sniffer":{"enable":false},"ntp":{"enable":false},"profile":{"store-selected":false,"store-fake-ip":false},
            "proxy-providers":providers,"proxy-groups":groups,"listeners":listeners,"rules":["MATCH,REJECT"]});
        let path = dir.join("config.yaml");
        write_immutable(&path, &serde_json::to_vec(&value).unwrap())?;
        Ok(path)
    }
    pub async fn validate(&self, path: &Path) -> Result<(), GatewayError> {
        let result = tokio::time::timeout(
            Duration::from_secs(15),
            tokio::process::Command::new(&self.config.binary_path)
                .args(["-t", "-d"])
                .arg(&self.root)
                .arg("-f")
                .arg(path)
                .kill_on_drop(true)
                .output(),
        )
        .await
        .map_err(|_| {
            error(
                "proxy_config_invalid",
                "core configuration validation timed out",
            )
        })?
        .map_err(io_error)?;
        if !result.status.success() {
            return Err(error(
                "proxy_config_invalid",
                "fixed core rejected generated configuration",
            ));
        }
        Ok(())
    }
    pub async fn start(&mut self, path: &Path, state: &Snapshot) -> Result<(), GatewayError> {
        let version = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::process::Command::new(&self.config.binary_path)
                .arg("-v")
                .kill_on_drop(true)
                .output(),
        )
        .await
        .map_err(|_| error("proxy_core_invalid", "core version check timed out"))?
        .map_err(io_error)?;
        if !version.status.success()
            || !String::from_utf8_lossy(&version.stdout)
                .split_whitespace()
                .any(|s| s == self.config.expected_version)
        {
            return Err(error(
                "proxy_core_invalid",
                "core version does not match lock file",
            ));
        }
        let probe = std::net::TcpListener::bind(self.config.controller_addr).map_err(|_| {
            error(
                "proxy_port_in_use",
                "controller port is already in use; existing processes will not be adopted",
            )
        })?;
        drop(probe);
        self.child = Some(
            tokio::process::Command::new(&self.config.binary_path)
                .arg("-d")
                .arg(&self.root)
                .arg("-f")
                .arg(path)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .map_err(io_error)?,
        );
        for _ in 0..100 {
            if let Some(child) = self.child.as_mut()
                && child.try_wait().map_err(io_error)?.is_some()
            {
                return Err(error(
                    "proxy_unavailable",
                    "managed core exited during startup",
                ));
            }
            if self.verify(state).await.is_ok() {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        self.stop().await;
        Err(error(
            "proxy_unavailable",
            "managed core did not become ready",
        ))
    }
    pub fn controller(&self) -> Controller {
        Controller {
            http: self.http.clone(),
            address: self.config.controller_addr,
            secret: self.secret.clone(),
        }
    }
    pub async fn api(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, GatewayError> {
        self.controller().api(method, path, body).await
    }
    pub async fn verify(&self, state: &Snapshot) -> Result<(), GatewayError> {
        let version = self.api(reqwest::Method::GET, "/version", None).await?;
        if version["version"].as_str() != Some(self.config.expected_version.as_str()) {
            return Err(error("proxy_core_invalid", "unexpected controller version"));
        }
        #[cfg(target_os = "linux")]
        {
            let pid = self
                .child
                .as_ref()
                .and_then(|c| c.id())
                .ok_or_else(|| error("proxy_unavailable", "managed core process is missing"))?;
            let owned = owned_tcp_listeners(pid)?;
            if !owned.contains(&self.config.controller_addr.port())
                || state
                    .bindings
                    .values()
                    .any(|b| !owned.contains(&b.listener_port))
            {
                return Err(error(
                    "proxy_port_in_use",
                    "controller or account listener is not owned by the managed core process",
                ));
            }
        }
        for b in state.bindings.values() {
            let group = self
                .api(
                    reqwest::Method::GET,
                    &format!("/proxies/rp_acc_{}", b.account_id),
                    None,
                )
                .await?;
            let expected = if state.route_configured(b) {
                state.nodes[&b.node_id].mihomo_name.as_str()
            } else {
                "REJECT"
            };
            if group["all"] != json!([expected]) || group["now"] != expected {
                return Err(error(
                    "proxy_route_blocked",
                    "core group does not match persisted binding",
                ));
            }
            // No request is sent through the listener: establish TCP only and close it.
            tokio::time::timeout(
                Duration::from_millis(500),
                tokio::net::TcpStream::connect(("127.0.0.1", b.listener_port)),
            )
            .await
            .map_err(|_| error("proxy_port_in_use", "account listener is not ready"))?
            .map_err(|_| error("proxy_port_in_use", "account listener cannot be reached"))?;
        }
        Ok(())
    }
    pub async fn reload(&self, path: &Path, state: &Snapshot) -> Result<(), GatewayError> {
        self.api(reqwest::Method::DELETE, "/connections", None)
            .await?;
        self.api(
            reqwest::Method::PUT,
            "/configs?force=true",
            Some(json!({"path":path})),
        )
        .await?;
        self.verify(state).await
    }
    pub fn cleanup_generations(&self, keep: &[u64]) {
        let Ok(entries) = std::fs::read_dir(self.root.join("generations")) else {
            return;
        };
        for entry in entries.flatten() {
            let Some(generation) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<u64>().ok())
            else {
                continue;
            };
            if !keep.contains(&generation)
                && entry
                    .file_type()
                    .is_ok_and(|kind| kind.is_dir() && !kind.is_symlink())
            {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
    pub async fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
    }
    pub fn exited(&mut self) -> bool {
        self.child
            .as_mut()
            .is_none_or(|c| !matches!(c.try_wait(), Ok(None)))
    }
}
#[cfg(target_os = "linux")]
fn owned_tcp_listeners(pid: u32) -> Result<std::collections::HashSet<u16>, GatewayError> {
    let descriptors = std::fs::read_dir(format!("/proc/{pid}/fd")).map_err(io_error)?;
    let inodes = descriptors
        .flatten()
        .filter_map(|fd| std::fs::read_link(fd.path()).ok())
        .filter_map(|path| {
            let name = path.to_string_lossy();
            name.strip_prefix("socket:[")
                .and_then(|x| x.strip_suffix(']'))
                .map(str::to_string)
        })
        .collect::<std::collections::HashSet<_>>();
    let mut ports = std::collections::HashSet::new();
    for protocol in ["tcp", "tcp6"] {
        let table =
            std::fs::read_to_string(format!("/proc/{pid}/net/{protocol}")).map_err(io_error)?;
        for line in table.lines().skip(1) {
            let columns = line.split_whitespace().collect::<Vec<_>>();
            if columns.len() > 9 && columns[3] == "0A" && inodes.contains(columns[9]) {
                if let Some(port) = columns[1]
                    .rsplit(':')
                    .next()
                    .and_then(|x| u16::from_str_radix(x, 16).ok())
                {
                    ports.insert(port);
                }
            }
        }
    }
    Ok(ports)
}

fn io_error(_: std::io::Error) -> GatewayError {
    error("proxy_io_failed", "proxy file or process operation failed")
}
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), GatewayError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(io_error)?;
    file.write_all(bytes).map_err(io_error)?;
    file.sync_all().map_err(io_error)
}
fn write_immutable(path: &Path, bytes: &[u8]) -> Result<(), GatewayError> {
    if path.exists() {
        if std::fs::read(path).map_err(io_error)? == bytes {
            return Ok(());
        }
        return Err(error(
            "proxy_generation_conflict",
            "immutable configuration generation already exists",
        ));
    }
    let temporary = path.with_extension(format!("{}.tmp", super::id()));
    write_private(&temporary, bytes)?;
    std::fs::rename(&temporary, path).map_err(io_error)?;
    if let Some(parent) = path.parent() {
        File::open(parent)
            .and_then(|f| f.sync_all())
            .map_err(io_error)?;
    }
    Ok(())
}
