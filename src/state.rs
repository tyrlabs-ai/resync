use anyhow::{Context, Result};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

#[derive(Debug, Clone)]
pub struct StatePaths {
    pub root: PathBuf,
    pub config: PathBuf,
    pub credentials: PathBuf,
    pub keys: PathBuf,
    pub catalog: PathBuf,
    pub transactions: PathBuf,
    pub named_lease_receipts: PathBuf,
    pub daemon_lock: PathBuf,
    pub socket: PathBuf,
}

pub fn state_directory() -> PathBuf {
    if let Some(path) = std::env::var_os("RESYNC_STATE_DIR") {
        return absolute(PathBuf::from(path));
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    #[cfg(target_os = "macos")]
    return home.join("Library/Application Support/RepoSync");
    #[cfg(not(target_os = "macos"))]
    home.join(".local/state/resync")
}

fn absolute(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

pub fn state_paths() -> StatePaths {
    let root = state_directory();
    let socket = std::env::var_os("RESYNC_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("daemon.sock"));
    StatePaths {
        config: root.join("config.json"),
        credentials: root.join("credentials.json"),
        keys: root.join("keys"),
        catalog: root.join("catalog.json"),
        transactions: root.join("transactions"),
        named_lease_receipts: root.join("named-leases"),
        daemon_lock: root.join("daemon.lock"),
        socket,
        root,
    }
}

pub fn read_json<T: DeserializeOwned + Clone>(path: &Path, fallback: T) -> Result<T> {
    match fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid JSON in {}", path.display()))?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(fallback),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

pub fn write_json<T: Serialize>(path: &Path, value: &T, mode: u32) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        #[cfg(unix)]
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(mode);
    let mut file = options.open(&temporary)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    #[cfg(unix)]
    fs::set_permissions(&temporary, fs::Permissions::from_mode(mode))?;
    fs::rename(&temporary, path)?;
    if let Some(parent) = path.parent() {
        fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderConfig {
    #[serde(default)]
    pub origin: String,
    #[serde(default)]
    pub service_id: Option<String>,
    #[serde(default)]
    pub device_id: Option<String>,
    #[serde(default)]
    pub device_name: Option<String>,
    #[serde(default)]
    pub public_key_path: Option<PathBuf>,
    #[serde(default)]
    pub logged_in_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    #[serde(default = "protocol")]
    pub protocol: String,
    #[serde(default)]
    pub active_provider: Option<String>,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
    #[serde(default)]
    pub server: Option<String>,
    #[serde(default)]
    pub token: Option<String>,
}

fn protocol() -> String {
    crate::protocol::PROTOCOL_VERSION.into()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            protocol: protocol(),
            active_provider: None,
            providers: BTreeMap::new(),
            server: None,
            token: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Conflict {
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalProject {
    pub project_id: String,
    pub local_path: PathBuf,
    #[serde(default)]
    pub remote_url: String,
    #[serde(default = "remote_name")]
    pub remote_name: String,
    #[serde(default = "default_branch")]
    pub default_branch: String,
    #[serde(default)]
    pub server_generation: u64,
    #[serde(default)]
    pub advertised_heads: BTreeMap<String, String>,
    #[serde(default)]
    pub last_applied_heads: BTreeMap<String, String>,
    #[serde(default)]
    pub active_branch: Option<String>,
    #[serde(default = "one")]
    pub workspace_generation: u64,
    #[serde(default = "current")]
    pub state: String,
    #[serde(default = "durable")]
    pub durability: String,
    #[serde(default)]
    pub conflict: Option<Conflict>,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub service: Option<String>,
    #[serde(default)]
    pub checkout_id: Option<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

fn remote_name() -> String {
    "resync".into()
}
fn default_branch() -> String {
    "main".into()
}
fn one() -> u64 {
    1
}
fn current() -> String {
    "CURRENT".into()
}
fn durable() -> String {
    "REMOTELY_DURABLE".into()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalCatalog {
    #[serde(default)]
    pub catalog_generation: u64,
    #[serde(default)]
    pub projects: Vec<LocalProject>,
}

pub fn load_config_raw() -> Result<Config> {
    let paths = state_paths();
    let mut config: Config = read_json(&paths.config, Config::default())?;
    if config.active_provider.is_none()
        && let Some(server) = config.server.clone()
    {
        let origin = url::Url::parse(&server)?.origin().ascii_serialization();
        config.active_provider = Some(origin.clone());
        config.providers.insert(
            origin.clone(),
            ProviderConfig {
                origin,
                device_name: Some("legacy device".into()),
                ..ProviderConfig::default()
            },
        );
    }
    Ok(config)
}

pub fn load_catalog() -> Result<LocalCatalog> {
    read_json(&state_paths().catalog, LocalCatalog::default())
}
