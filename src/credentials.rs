use crate::process::{RunOptions, run};
use crate::state::{read_json, state_paths, write_json};
use anyhow::{Context, Result, bail};
use chrono::Utc;
use ed25519_dalek::SigningKey;
use ed25519_dalek::pkcs8::{EncodePrivateKey, EncodePublicKey};
use pkcs8::LineEnding;
use rand_core::OsRng;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use url::Url;
use uuid::Uuid;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct CredentialFile {
    #[serde(default = "one")]
    version: u32,
    #[serde(default)]
    providers: BTreeMap<String, StoredCredential>,
}

fn one() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredCredential {
    token: String,
    updated_at: String,
}

pub fn canonical_origin(value: &str) -> Result<String> {
    let url = Url::parse(value).with_context(|| format!("invalid provider URL {value}"))?;
    let local =
        url.scheme() == "http" && matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "::1"));
    if url.scheme() != "https" && !local {
        bail!("RepoSync providers must use HTTPS except on localhost");
    }
    Ok(url.origin().ascii_serialization())
}

fn helper(
    operation: &str,
    origin: &str,
    value: Option<serde_json::Value>,
) -> Result<Option<String>> {
    let Some(command) = std::env::var_os("RESYNC_CREDENTIAL_HELPER") else {
        return Ok(None);
    };
    let mut options = RunOptions {
        input: Some(serde_json::to_vec(
            &json!({ "operation": operation, "origin": origin, "value": value }),
        )?),
        allow_failure: true,
        ..RunOptions::default()
    };
    options.input.as_mut().unwrap().push(b'\n');
    let result = run("/bin/sh", ["-c".into(), command], options)?;
    if result.code != 0 {
        bail!(
            "credential helper failed: {}",
            if result.stderr.trim().is_empty() {
                format!("exit {}", result.code)
            } else {
                result.stderr.trim().into()
            }
        );
    }
    if operation != "get" {
        return Ok(Some(String::new()));
    }
    if result.stdout.trim().is_empty() {
        return Ok(None);
    }
    let response: serde_json::Value = serde_json::from_str(result.stdout.trim())?;
    Ok(response
        .get("token")
        .and_then(|value| value.as_str())
        .map(str::to_owned))
}

pub fn load_credential(provider: &str) -> Result<Option<String>> {
    let origin = canonical_origin(provider)?;
    if let Some(external) = helper("get", &origin, None)? {
        return Ok((!external.is_empty()).then_some(external));
    }
    if let Ok(token) = std::env::var("RESYNC_TOKEN") {
        let matches = std::env::var("RESYNC_PROVIDER")
            .ok()
            .map(|value| canonical_origin(&value))
            .transpose()?
            .is_none_or(|value| value == origin);
        if matches {
            return Ok(Some(token));
        }
    }
    let credentials: CredentialFile =
        read_json(&state_paths().credentials, CredentialFile::default())?;
    Ok(credentials
        .providers
        .get(&origin)
        .map(|value| value.token.clone()))
}

pub fn store_credential(provider: &str, token: &str) -> Result<()> {
    let origin = canonical_origin(provider)?;
    if token.is_empty() {
        bail!("cannot store an empty credential");
    }
    if helper("store", &origin, Some(json!({ "token": token })))?.is_some() {
        return Ok(());
    }
    let path = state_paths().credentials;
    let mut credentials: CredentialFile = read_json(&path, CredentialFile::default())?;
    credentials.providers.insert(
        origin,
        StoredCredential {
            token: token.into(),
            updated_at: Utc::now().to_rfc3339(),
        },
    );
    write_json(&path, &credentials, 0o600)
}

pub fn erase_credential(provider: &str) -> Result<()> {
    let origin = canonical_origin(provider)?;
    if helper("erase", &origin, None)?.is_some() {
        return Ok(());
    }
    let path = state_paths().credentials;
    let mut credentials: CredentialFile = read_json(&path, CredentialFile::default())?;
    credentials.providers.remove(&origin);
    write_json(&path, &credentials, 0o600)
}

#[derive(Debug, Clone)]
pub struct DeviceIdentity {
    pub name: String,
    pub private_key_path: PathBuf,
    pub public_key_path: PathBuf,
    pub public_key: String,
}

pub fn generate_device_identity(name: &str) -> Result<DeviceIdentity> {
    let id = format!("key_{}", Uuid::new_v4());
    let directory = state_paths().keys;
    fs::create_dir_all(&directory)?;
    #[cfg(unix)]
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    let private_key_path = directory.join(format!("{id}.pem"));
    let public_key_path = directory.join(format!("{id}.pub.pem"));
    let signing = SigningKey::generate(&mut OsRng);
    let private_key = signing.to_pkcs8_pem(LineEnding::LF)?.to_string();
    let public_key = signing.verifying_key().to_public_key_pem(LineEnding::LF)?;
    fs::write(&private_key_path, private_key)?;
    fs::write(&public_key_path, &public_key)?;
    #[cfg(unix)]
    {
        fs::set_permissions(&private_key_path, fs::Permissions::from_mode(0o600))?;
        fs::set_permissions(&public_key_path, fs::Permissions::from_mode(0o644))?;
    }
    Ok(DeviceIdentity {
        name: name.into(),
        private_key_path,
        public_key_path,
        public_key,
    })
}

pub fn remove_device_identity(public_key_path: Option<&Path>) -> Result<()> {
    let Some(public) = public_key_path else {
        return Ok(());
    };
    remove_if_exists(public)?;
    let value = public.to_string_lossy();
    if let Some(prefix) = value.strip_suffix(".pub.pem") {
        remove_if_exists(Path::new(&format!("{prefix}.pem")))?;
    }
    Ok(())
}

fn remove_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}
