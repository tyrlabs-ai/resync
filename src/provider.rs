use crate::credentials::{
    DeviceIdentity, canonical_origin, generate_device_identity, store_credential,
};
use crate::protocol::PROTOCOL_VERSION;
use crate::state::{ProviderConfig, load_config_raw, state_paths, write_json};
use anyhow::{Result, bail};
use chrono::Utc;
use reqwest::Method;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use url::Url;

pub const DEFAULT_PROVIDER: &str = "https://resync-hosted-production.up.railway.app";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Discovery {
    pub protocol: String,
    pub service_id: String,
    #[serde(default)]
    pub auth_methods: Vec<String>,
    pub endpoints: BTreeMap<String, String>,
    #[serde(skip)]
    pub origin: String,
}

pub fn default_provider() -> String {
    std::env::var("RESYNC_DEFAULT_PROVIDER").unwrap_or_else(|_| DEFAULT_PROVIDER.into())
}

pub fn discover(provider: Option<&str>) -> Result<Discovery> {
    let origin = canonical_origin(provider.unwrap_or(&default_provider()))?;
    let url = Url::parse(&origin)?.join("/.well-known/resync")?;
    let response = Client::new()
        .get(url)
        .header("accept", "application/json")
        .send()?;
    if !response.status().is_success() {
        bail!("provider discovery failed ({})", response.status().as_u16());
    }
    let mut document: Discovery = response.json()?;
    if document.protocol != PROTOCOL_VERSION || document.endpoints.is_empty() {
        bail!("provider does not advertise a compatible resync.v1 service");
    }
    for value in document.endpoints.values_mut() {
        *value = Url::parse(&origin)?.join(value)?.to_string();
    }
    document.origin = origin;
    Ok(document)
}

pub fn provider_fetch(
    url: &str,
    token: Option<&str>,
    method: Method,
    body: Option<&Value>,
) -> Result<Value> {
    let client = Client::new();
    let mut request = client
        .request(method, url)
        .header("accept", "application/json");
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    if let Some(body) = body {
        request = request.json(body);
    }
    let response = request.send()?;
    let status = response.status();
    let text = response.text()?;
    let value = if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text)
            .unwrap_or_else(|_| json!({ "error": text.chars().take(500).collect::<String>() }))
    };
    if !status.is_success() {
        bail!(
            "{}",
            value
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| format!("provider request failed ({})", status.as_u16()))
        );
    }
    Ok(value)
}

pub fn enroll_with_token(
    provider: Option<&str>,
    bootstrap_token: &str,
    device_name: &str,
) -> Result<Value> {
    let discovery = discover(provider)?;
    let key = generate_device_identity(device_name)?;
    let enroll = discovery
        .endpoints
        .get("enroll")
        .ok_or_else(|| anyhow::anyhow!("provider discovery is missing enroll endpoint"))?;
    let catalog = discovery
        .endpoints
        .get("catalog")
        .ok_or_else(|| anyhow::anyhow!("provider discovery is missing catalog endpoint"))?;
    let enrollment = match provider_fetch(
        enroll,
        Some(bootstrap_token),
        Method::POST,
        Some(&json!({
            "device_name": device_name, "public_key": key.public_key
        })),
    ) {
        Ok(value) => value,
        Err(_) => {
            let catalog_value = provider_fetch(catalog, Some(bootstrap_token), Method::GET, None)?;
            json!({
                "device": { "id": Value::Null, "name": device_name,
                    "project_ids": catalog_value.get("projects").and_then(Value::as_array).into_iter().flatten().filter_map(|item| item.get("project_id")).collect::<Vec<_>>() },
                "token": bootstrap_token
            })
        }
    };
    let token = enrollment
        .get("token")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("provider enrollment returned no token"))?;
    provider_fetch(catalog, Some(token), Method::GET, None)?;
    store_provider_login(&discovery, &enrollment, &key)?;
    Ok(json!({
        "provider": discovery.origin,
        "device": enrollment.get("device").cloned().unwrap_or(Value::Null),
        "authMethods": discovery.auth_methods
    }))
}

pub fn store_provider_login(
    discovery: &Discovery,
    enrollment: &Value,
    key: &DeviceIdentity,
) -> Result<()> {
    let token = enrollment
        .get("token")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("enrollment token missing"))?;
    store_credential(&discovery.origin, token)?;
    let mut config = load_config_raw()?;
    config.server = None;
    config.token = None;
    config.active_provider = Some(discovery.origin.clone());
    let device = enrollment.get("device").unwrap_or(&Value::Null);
    config.providers.insert(
        discovery.origin.clone(),
        ProviderConfig {
            origin: discovery.origin.clone(),
            service_id: Some(discovery.service_id.clone()),
            device_id: device.get("id").and_then(Value::as_str).map(str::to_owned),
            device_name: device
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_owned),
            public_key_path: Some(key.public_key_path.clone()),
            logged_in_at: Some(Utc::now().to_rfc3339()),
        },
    );
    write_json(&state_paths().config, &config, 0o600)
}
