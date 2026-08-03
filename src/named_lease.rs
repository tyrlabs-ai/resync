use crate::credentials::{canonical_origin, load_credential};
use crate::identity::read_identity;
use crate::protocol::{
    NamedLease, NamedLeaseListResponse, NamedLeaseMutationResponse, NamedLeaseOutcome,
    NamedLeasePolicy,
};
use crate::provider::{Discovery, ProviderHttpError, discover, provider_request};
use crate::state::{LocalProject, load_catalog, load_config_raw, state_paths, write_json};
use anyhow::{Context, Result, bail};
use base64::Engine;
use chrono::{DateTime, Utc};
use rand::RngCore;
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

const CAPABILITY: &str = "project-named-leases-v1";

#[derive(Debug, Clone)]
pub struct NamedLeaseProject {
    pub provider_origin: String,
    pub project_id: String,
    pub token: String,
    pub endpoint: String,
    pub policy: NamedLeasePolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum NamedLeaseReceiptState {
    Pending,
    Active,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingMutation {
    pub operation: String,
    pub request_id: String,
    pub ttl_seconds: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NamedLeaseReceipt {
    pub version: u32,
    pub state: NamedLeaseReceiptState,
    pub provider_origin: String,
    pub project_id: String,
    pub request_id: String,
    pub resource_key: String,
    pub holder_id: String,
    pub holder_secret: String,
    pub requested_ttl_seconds: u64,
    #[serde(default)]
    pub lease: Option<NamedLease>,
    #[serde(default)]
    pub pending_mutation: Option<PendingMutation>,
}

#[derive(Debug)]
pub struct NamedLeaseProviderOutcome {
    pub status: u16,
    pub response: NamedLeaseMutationResponse,
}

impl std::fmt::Display for NamedLeaseProviderOutcome {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "provider returned named-lease outcome {} ({})",
            self.response.outcome, self.status
        )
    }
}

impl std::error::Error for NamedLeaseProviderOutcome {}

pub fn resolve_project(project_id: Option<&str>, current_dir: &Path) -> Result<NamedLeaseProject> {
    let catalog = load_catalog()?;
    let (project, origin) = if let Some(project_id) = project_id {
        let project = catalog
            .projects
            .iter()
            .find(|project| project.project_id == project_id)
            .with_context(|| format!("project {project_id} is not in the local catalog"))?;
        let config = load_config_raw()?;
        let origin = project
            .service
            .as_deref()
            .or(config.active_provider.as_deref())
            .context("project has no provider and no active provider is configured")?;
        (project, canonical_origin(origin)?)
    } else {
        let identity = read_identity(current_dir)?;
        let project_id = identity
            .project_id
            .as_deref()
            .context("current checkout is not enrolled with RepoSync")?;
        let origin = identity
            .service
            .as_deref()
            .context("current checkout has no RepoSync service")?;
        let project = catalog
            .projects
            .iter()
            .find(|project| project.project_id == project_id)
            .context("current project is no longer in the local catalog")?;
        (project, canonical_origin(origin)?)
    };
    resolve_discovered_project(project, &origin, discover(Some(&origin))?)
}

fn resolve_discovered_project(
    project: &LocalProject,
    origin: &str,
    discovery: Discovery,
) -> Result<NamedLeaseProject> {
    validate_identifier(&project.project_id, "project", "prj_")?;
    if !discovery
        .capabilities
        .iter()
        .any(|capability| capability == CAPABILITY)
    {
        bail!("provider does not support {CAPABILITY}");
    }
    let policy = discovery
        .named_lease_policy
        .context("provider omitted named_lease_policy")?;
    policy.validate()?;
    let template = discovery
        .endpoints
        .get("named_leases")
        .context("provider omitted the named_leases endpoint")?;
    if !template.contains("{project_id}") && !template.contains("%7Bproject_id%7D") {
        bail!("provider named_leases endpoint omits the project_id template");
    }
    let encoded_project =
        url::form_urlencoded::byte_serialize(project.project_id.as_bytes()).collect::<String>();
    let endpoint = template
        .replace("{project_id}", &encoded_project)
        .replace("%7Bproject_id%7D", &encoded_project);
    let token = load_credential(origin)?
        .with_context(|| format!("not logged in to {origin}; run `resync auth login {origin}`"))?;
    Ok(NamedLeaseProject {
        provider_origin: origin.into(),
        project_id: project.project_id.clone(),
        token,
        endpoint,
        policy,
    })
}

pub fn acquire(
    project: &NamedLeaseProject,
    resource_key: &str,
    holder_id: Option<&str>,
    ttl_seconds: Option<u64>,
) -> Result<NamedLeaseMutationResponse> {
    validate_resource_key(resource_key)?;
    let ttl = ttl_seconds.unwrap_or(project.policy.default_ttl_seconds);
    validate_ttl(&project.policy, ttl)?;
    let pending_directory = receipt_project_directory(project);
    let existing = find_pending_receipt(
        &pending_directory,
        &project.provider_origin,
        &project.project_id,
        resource_key,
        holder_id,
    )?;
    let (pending_path, receipt) = if let Some(existing) = existing {
        existing
    } else {
        let holder_id = holder_id
            .map(str::to_owned)
            .unwrap_or_else(|| format!("holder_{}", Uuid::new_v4().simple()));
        validate_holder_id(&holder_id)?;
        let receipt = NamedLeaseReceipt {
            version: 1,
            state: NamedLeaseReceiptState::Pending,
            provider_origin: project.provider_origin.clone(),
            project_id: project.project_id.clone(),
            request_id: format!("req_{}", Uuid::new_v4().simple()),
            resource_key: resource_key.into(),
            holder_id,
            holder_secret: random_secret(),
            requested_ttl_seconds: ttl,
            lease: None,
            pending_mutation: None,
        };
        let path = pending_receipt_path(&pending_directory, &receipt);
        write_json(&path, &receipt, 0o600)?;
        (path, receipt)
    };
    let body = json!({
        "request_id": receipt.request_id,
        "resource_key": receipt.resource_key,
        "holder_id": receipt.holder_id,
        "holder_secret": receipt.holder_secret,
        "ttl_seconds": receipt.requested_ttl_seconds,
    });
    let response = match mutation_request(project, &project.endpoint, Method::POST, &body) {
        Ok(response) => response,
        Err(error) => {
            if error.downcast_ref::<NamedLeaseProviderOutcome>().is_some() {
                remove_receipt(&pending_path)?;
            }
            return Err(error);
        }
    };
    response.validate_for_resource(&project.project_id, resource_key)?;
    if response.outcome != NamedLeaseOutcome::Acquired {
        remove_receipt(&pending_path)?;
        return Err(NamedLeaseProviderOutcome {
            status: mutation_status(response.outcome),
            response,
        }
        .into());
    }
    let lease = response
        .lease
        .clone()
        .context("ACQUIRED response omitted its lease")?;
    validate_identifier(&lease.lease_id, "lease", "nls_")?;
    let active = NamedLeaseReceipt {
        state: NamedLeaseReceiptState::Active,
        lease: Some(lease.clone()),
        ..receipt
    };
    let active_path = active_receipt_path(&pending_directory, &lease.lease_id, &active.holder_id);
    write_json(&active_path, &active, 0o600)?;
    remove_receipt(&pending_path)?;
    Ok(response)
}

pub fn get(project: &NamedLeaseProject, resource_key: &str) -> Result<Option<NamedLease>> {
    validate_resource_key(resource_key)?;
    let url = format!(
        "{}?resource_key={}",
        project.endpoint,
        url::form_urlencoded::byte_serialize(resource_key.as_bytes()).collect::<String>()
    );
    let response = provider_request(&url, Some(&project.token), Method::GET, None);
    match response {
        Ok(response) => {
            let lease: NamedLease = serde_json::from_value(
                response
                    .body
                    .get("lease")
                    .cloned()
                    .context("provider response omitted lease")?,
            )?;
            lease.validate_for_resource(&project.project_id, resource_key)?;
            Ok(Some(lease))
        }
        Err(error) => {
            let Some(http) = error.downcast_ref::<ProviderHttpError>() else {
                return Err(error);
            };
            if http.status == 404
                && http.body.get("outcome") == Some(&Value::String("NOT_FOUND".into()))
            {
                Ok(None)
            } else {
                Err(anyhow::anyhow!(http.clone()))
            }
        }
    }
}

pub fn list(project: &NamedLeaseProject, prefix: &str) -> Result<Vec<NamedLease>> {
    validate_resource_prefix(prefix)?;
    let url = format!(
        "{}?prefix={}",
        project.endpoint,
        url::form_urlencoded::byte_serialize(prefix.as_bytes()).collect::<String>()
    );
    let response = provider_request(&url, Some(&project.token), Method::GET, None)?;
    let response: NamedLeaseListResponse = serde_json::from_value(response.body)?;
    response.validate_for_project(&project.project_id)?;
    if response
        .leases
        .iter()
        .any(|lease| !lease.resource_key.starts_with(prefix))
    {
        bail!("provider returned a named lease outside the requested prefix");
    }
    Ok(response.leases)
}

pub fn renew(
    project: &NamedLeaseProject,
    lease_id: &str,
    holder_id: &str,
    ttl_seconds: Option<u64>,
) -> Result<NamedLeaseMutationResponse> {
    mutate_active_receipt(project, lease_id, holder_id, "renew", ttl_seconds)
}

pub fn release(
    project: &NamedLeaseProject,
    lease_id: &str,
    holder_id: &str,
) -> Result<NamedLeaseMutationResponse> {
    mutate_active_receipt(project, lease_id, holder_id, "release", None)
}

fn mutate_active_receipt(
    project: &NamedLeaseProject,
    lease_id: &str,
    holder_id: &str,
    operation: &str,
    ttl_seconds: Option<u64>,
) -> Result<NamedLeaseMutationResponse> {
    validate_holder_id(holder_id)?;
    validate_identifier(lease_id, "lease", "nls_")?;
    if operation == "renew" {
        validate_ttl(
            &project.policy,
            ttl_seconds.unwrap_or(project.policy.default_ttl_seconds),
        )?;
    }
    let directory = receipt_project_directory(project);
    let path = active_receipt_path(&directory, lease_id, holder_id);
    let mut receipt: NamedLeaseReceipt = crate::state::read_json(&path, None::<NamedLeaseReceipt>)?
        .context("no active named-lease receipt matches that lease and holder")?;
    validate_receipt_binding(&receipt, project, lease_id, holder_id)?;
    let request = match &receipt.pending_mutation {
        Some(pending) if pending.operation == operation && pending.ttl_seconds == ttl_seconds => {
            pending.clone()
        }
        Some(_) => bail!("a different named-lease mutation is awaiting an unambiguous response"),
        None => PendingMutation {
            operation: operation.into(),
            request_id: format!("req_{}", Uuid::new_v4().simple()),
            ttl_seconds,
        },
    };
    receipt.pending_mutation = Some(request.clone());
    write_json(&path, &receipt, 0o600)?;
    let mut body = json!({
        "request_id": request.request_id,
        "holder_secret": receipt.holder_secret,
    });
    if operation == "renew" {
        body["ttl_seconds"] = json!(ttl_seconds.unwrap_or(project.policy.default_ttl_seconds));
    }
    let url = format!(
        "{}/{}/{}",
        project.endpoint,
        url::form_urlencoded::byte_serialize(lease_id.as_bytes()).collect::<String>(),
        operation
    );
    let response = mutation_request(project, &url, Method::POST, &body);
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            if let Some(outcome) = error.downcast_ref::<NamedLeaseProviderOutcome>()
                && matches!(
                    outcome.response.outcome,
                    NamedLeaseOutcome::LeaseExpired | NamedLeaseOutcome::NotHolder
                )
            {
                remove_receipt(&path)?;
            }
            return Err(error);
        }
    };
    response.validate_for_lease(&project.project_id, lease_id)?;
    match response.outcome {
        NamedLeaseOutcome::Renewed => {
            receipt.lease = response.lease.clone();
            receipt.pending_mutation = None;
            write_json(&path, &receipt, 0o600)?;
        }
        NamedLeaseOutcome::Released => remove_receipt(&path)?,
        _ => {
            remove_receipt(&path)?;
            return Err(NamedLeaseProviderOutcome {
                status: mutation_status(response.outcome),
                response,
            }
            .into());
        }
    }
    Ok(response)
}

fn mutation_request(
    project: &NamedLeaseProject,
    url: &str,
    method: Method,
    body: &Value,
) -> Result<NamedLeaseMutationResponse> {
    match provider_request(url, Some(&project.token), method, Some(body)) {
        Ok(response) => Ok(serde_json::from_value(response.body)?),
        Err(error) => {
            let Some(http) = error.downcast_ref::<ProviderHttpError>() else {
                return Err(error);
            };
            let response: NamedLeaseMutationResponse = serde_json::from_value(http.body.clone())?;
            response.validate()?;
            Err(NamedLeaseProviderOutcome {
                status: http.status,
                response,
            }
            .into())
        }
    }
}

fn mutation_status(outcome: NamedLeaseOutcome) -> u16 {
    match outcome {
        NamedLeaseOutcome::Acquired => 201,
        NamedLeaseOutcome::Renewed | NamedLeaseOutcome::Released => 200,
        NamedLeaseOutcome::Held
        | NamedLeaseOutcome::LeaseExpired
        | NamedLeaseOutcome::RequestIdReused => 409,
        NamedLeaseOutcome::NotHolder => 403,
        NamedLeaseOutcome::NotFound => 404,
        NamedLeaseOutcome::InvalidResourceKey | NamedLeaseOutcome::InvalidTtl => 400,
    }
}

fn receipt_project_directory(project: &NamedLeaseProject) -> PathBuf {
    state_paths()
        .named_lease_receipts
        .join(hash(&[&project.provider_origin]))
        .join(&project.project_id)
}

fn pending_receipt_path(directory: &Path, receipt: &NamedLeaseReceipt) -> PathBuf {
    directory.join(format!(
        "pending-{}.json",
        hash(&[
            &receipt.project_id,
            &receipt.resource_key,
            &receipt.holder_id,
        ])
    ))
}

fn active_receipt_path(directory: &Path, lease_id: &str, holder_id: &str) -> PathBuf {
    directory.join(format!("{}-{}.json", lease_id, hash(&[holder_id])))
}

fn find_pending_receipt(
    directory: &Path,
    provider_origin: &str,
    project_id: &str,
    resource_key: &str,
    holder_id: Option<&str>,
) -> Result<Option<(PathBuf, NamedLeaseReceipt)>> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut matches = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if !path
            .file_name()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value.starts_with("pending-") && value.ends_with(".json"))
        {
            continue;
        }
        let receipt: NamedLeaseReceipt = serde_json::from_slice(&fs::read(&path)?)?;
        if receipt.state == NamedLeaseReceiptState::Pending
            && receipt.provider_origin == provider_origin
            && receipt.project_id == project_id
            && receipt.resource_key == resource_key
            && holder_id.is_none_or(|holder_id| receipt.holder_id == holder_id)
        {
            matches.push((path, receipt));
        }
    }
    if matches.len() > 1 {
        bail!("multiple pending named-lease receipts match this acquisition");
    }
    Ok(matches.pop())
}

fn validate_receipt_binding(
    receipt: &NamedLeaseReceipt,
    project: &NamedLeaseProject,
    lease_id: &str,
    holder_id: &str,
) -> Result<()> {
    let lease = receipt
        .lease
        .as_ref()
        .context("active receipt has no lease")?;
    if receipt.version != 1
        || receipt.state != NamedLeaseReceiptState::Active
        || receipt.provider_origin != project.provider_origin
        || receipt.project_id != project.project_id
        || receipt.holder_id != holder_id
        || lease.lease_id != lease_id
    {
        bail!("named-lease receipt does not match the requested authority");
    }
    lease.validate_for_lease(&project.project_id, lease_id)
}

fn validate_ttl(policy: &NamedLeasePolicy, ttl: u64) -> Result<()> {
    if ttl < policy.min_ttl_seconds || ttl > policy.max_ttl_seconds {
        bail!(
            "named-lease TTL must be {}-{} seconds",
            policy.min_ttl_seconds,
            policy.max_ttl_seconds
        );
    }
    Ok(())
}

pub fn validate_resource_key(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 255
        || value.starts_with('/')
        || value.ends_with('/')
        || !value.split('/').all(valid_resource_segment)
    {
        bail!("invalid named-lease resource key");
    }
    Ok(())
}

pub fn validate_resource_prefix(value: &str) -> Result<()> {
    let Some(key) = value.strip_suffix('/') else {
        bail!("named-lease prefix must end at a slash boundary");
    };
    if value.len() > 255 {
        bail!("named-lease prefix is too long");
    }
    validate_resource_key(key)
}

fn valid_resource_segment(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 63
        && value != "."
        && value != ".."
        && value
            .as_bytes()
            .first()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)
        })
}

fn validate_holder_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 255
        || value
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        bail!("invalid named-lease holder ID");
    }
    Ok(())
}

fn validate_identifier(value: &str, field: &str, prefix: &str) -> Result<()> {
    if !value.starts_with(prefix)
        || value.len() > 100
        || !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
    {
        bail!("invalid named-lease {field} ID");
    }
    Ok(())
}

fn random_secret() -> String {
    let mut secret = [0_u8; 32];
    rand::thread_rng().fill_bytes(&mut secret);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(secret)
}

fn hash(fields: &[&str]) -> String {
    let mut digest = Sha256::new();
    for field in fields {
        digest.update(field.len().to_be_bytes());
        digest.update(field.as_bytes());
    }
    format!("{:x}", digest.finalize())
}

fn remove_receipt(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub fn receipt_is_locally_expired(receipt: &NamedLeaseReceipt, now: DateTime<Utc>) -> bool {
    receipt
        .lease
        .as_ref()
        .is_some_and(|lease| now >= lease.expires_at)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_keys_and_prefixes_use_complete_lowercase_segments() {
        assert!(validate_resource_key("waykeeper/maps/map-1/tickets/ticket-1").is_ok());
        assert!(validate_resource_prefix("waykeeper/maps/").is_ok());
        for invalid in ["", "/a", "a/", "a//b", "a/../b", "A/b", "a/b c"] {
            assert!(validate_resource_key(invalid).is_err(), "{invalid}");
        }
        assert!(validate_resource_prefix("waykeeper2/").is_ok());
        assert!(validate_resource_prefix("waykeeper//").is_err());
    }

    #[test]
    fn receipt_paths_hash_sensitive_and_path_like_inputs() {
        let project = NamedLeaseProject {
            provider_origin: "https://provider.example".into(),
            project_id: "prj_example".into(),
            token: "secret".into(),
            endpoint: "https://provider.example/v1/projects/prj_example/named-leases".into(),
            policy: NamedLeasePolicy {
                default_ttl_seconds: 900,
                min_ttl_seconds: 30,
                max_ttl_seconds: 3600,
            },
        };
        let directory = receipt_project_directory(&project);
        assert!(!directory.to_string_lossy().contains("provider.example"));
        let path = active_receipt_path(&directory, "nls_example", "../../holder");
        assert_eq!(path.parent(), Some(directory.as_path()));
        assert!(
            !path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .contains("holder")
        );
    }

    #[test]
    fn local_expiry_is_observation_not_receipt_deletion() {
        let receipt = NamedLeaseReceipt {
            version: 1,
            state: NamedLeaseReceiptState::Active,
            provider_origin: "https://provider.example".into(),
            project_id: "prj_example".into(),
            request_id: "req_example".into(),
            resource_key: "waykeeper/item".into(),
            holder_id: "holder".into(),
            holder_secret: "secret".into(),
            requested_ttl_seconds: 900,
            lease: Some(NamedLease {
                lease_id: "nls_example".into(),
                project_id: "prj_example".into(),
                resource_key: "waykeeper/item".into(),
                holder_device_id: "dev_example".into(),
                holder_device_name: None,
                holder_id: "holder".into(),
                generation: 1,
                acquired_at: DateTime::parse_from_rfc3339("2026-08-03T00:00:00Z")
                    .unwrap()
                    .to_utc(),
                expires_at: DateTime::parse_from_rfc3339("2026-08-03T00:15:00Z")
                    .unwrap()
                    .to_utc(),
            }),
            pending_mutation: None,
        };
        let now = DateTime::parse_from_rfc3339("2026-08-03T00:15:00Z")
            .unwrap()
            .to_utc();
        assert!(receipt_is_locally_expired(&receipt, now));
        assert!(receipt.lease.is_some());
    }
}
