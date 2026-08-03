use crate::credentials::{
    DeviceIdentity, canonical_origin, generate_device_identity, store_credential,
};
use crate::protocol::{NamedLeasePolicy, PROTOCOL_VERSION};
use crate::state::{ProviderConfig, load_config_raw, state_paths, write_json};
use anyhow::{Result, bail};
use chrono::Utc;
use reqwest::Method;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fmt;
use url::Url;

pub const DEFAULT_PROVIDER: &str = "https://resync-hosted-production.up.railway.app";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Discovery {
    pub protocol: String,
    pub service_id: String,
    #[serde(default)]
    pub auth_methods: Vec<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    pub endpoints: BTreeMap<String, String>,
    #[serde(default)]
    pub named_lease_policy: Option<NamedLeasePolicy>,
    #[serde(skip)]
    pub origin: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProviderHttpResponse {
    pub status: u16,
    pub body: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProviderHttpError {
    pub status: u16,
    pub body: Value,
}

impl fmt::Display for ProviderHttpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(message) = self.body.get("error").and_then(Value::as_str) {
            formatter.write_str(message)
        } else {
            write!(formatter, "provider request failed ({})", self.status)
        }
    }
}

impl std::error::Error for ProviderHttpError {}

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
    if let Some(policy) = &document.named_lease_policy {
        policy.validate()?;
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
    provider_request(url, token, method, body)
        .map(|response| response.body)
        .map_err(|error| match error.downcast::<ProviderHttpError>() {
            Ok(http_error) => anyhow::anyhow!(http_error.to_string()),
            Err(error) => error,
        })
}

pub fn provider_request(
    url: &str,
    token: Option<&str>,
    method: Method,
    body: Option<&Value>,
) -> Result<ProviderHttpResponse> {
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
        return Err(ProviderHttpError {
            status: status.as_u16(),
            body: value,
        }
        .into());
    }
    Ok(ProviderHttpResponse {
        status: status.as_u16(),
        body: value,
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn discovery_policy_remains_optional_for_older_providers() {
        let discovery: Discovery = serde_json::from_value(json!({
            "protocol": "resync.v1",
            "service_id": "svc_test",
            "endpoints": { "catalog": "/v1/catalog" }
        }))
        .unwrap();
        assert_eq!(discovery.named_lease_policy, None);
    }

    #[test]
    fn provider_request_preserves_structured_non_success_bodies() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request).unwrap();
            let body = r#"{"outcome":"HELD","lease":{"lease_id":"nls_1"}}"#;
            write!(
                stream,
                "HTTP/1.1 409 Conflict\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });

        let error = provider_request(&format!("http://{address}/lease"), None, Method::POST, None)
            .unwrap_err();
        let http = error.downcast_ref::<ProviderHttpError>().unwrap();
        assert_eq!(http.status, 409);
        assert_eq!(http.body["outcome"], "HELD");
        assert_eq!(http.body["lease"]["lease_id"], "nls_1");
        server.join().unwrap();
    }
}
