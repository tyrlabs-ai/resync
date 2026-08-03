use anyhow::{Result, bail};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

pub const PROTOCOL_VERSION: &str = "resync.v1";

pub const WORKSPACE_STATES: &[&str] = &[
    "MATERIALIZING",
    "CURRENT",
    "REMOTE_AHEAD",
    "WAITING_FOR_LEASES",
    "RECONCILING",
    "CONFLICTED",
    "OFFLINE",
    "ACCESS_LOST",
    "FAILED",
];

pub const DURABILITY_STATES: &[&str] = &[
    "NO_LOCAL_CHANGE",
    "DIRTY_UNCHECKPOINTED",
    "COMMITTED_UNPUBLISHED",
    "PUBLISHING",
    "REMOTELY_DURABLE",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NamedLeasePolicy {
    pub default_ttl_seconds: u64,
    pub min_ttl_seconds: u64,
    pub max_ttl_seconds: u64,
}

impl NamedLeasePolicy {
    pub fn validate(&self) -> Result<()> {
        if self.min_ttl_seconds == 0
            || self.min_ttl_seconds > self.default_ttl_seconds
            || self.default_ttl_seconds > self.max_ttl_seconds
        {
            bail!("provider advertised an invalid named-lease TTL policy");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamedLeaseOutcome {
    Acquired,
    Held,
    Renewed,
    Released,
    LeaseExpired,
    NotHolder,
    NotFound,
    InvalidResourceKey,
    InvalidTtl,
    RequestIdReused,
}

impl NamedLeaseOutcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Acquired => "ACQUIRED",
            Self::Held => "HELD",
            Self::Renewed => "RENEWED",
            Self::Released => "RELEASED",
            Self::LeaseExpired => "LEASE_EXPIRED",
            Self::NotHolder => "NOT_HOLDER",
            Self::NotFound => "NOT_FOUND",
            Self::InvalidResourceKey => "INVALID_RESOURCE_KEY",
            Self::InvalidTtl => "INVALID_TTL",
            Self::RequestIdReused => "REQUEST_ID_REUSED",
        }
    }

    pub const fn carries_lease(self) -> bool {
        matches!(
            self,
            Self::Acquired | Self::Held | Self::Renewed | Self::Released
        )
    }
}

impl fmt::Display for NamedLeaseOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for NamedLeaseOutcome {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "ACQUIRED" => Ok(Self::Acquired),
            "HELD" => Ok(Self::Held),
            "RENEWED" => Ok(Self::Renewed),
            "RELEASED" => Ok(Self::Released),
            "LEASE_EXPIRED" => Ok(Self::LeaseExpired),
            "NOT_HOLDER" => Ok(Self::NotHolder),
            "NOT_FOUND" => Ok(Self::NotFound),
            "INVALID_RESOURCE_KEY" => Ok(Self::InvalidResourceKey),
            "INVALID_TTL" => Ok(Self::InvalidTtl),
            "REQUEST_ID_REUSED" => Ok(Self::RequestIdReused),
            _ => bail!("unknown named-lease outcome {value:?}"),
        }
    }
}

impl Serialize for NamedLeaseOutcome {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for NamedLeaseOutcome {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NamedLease {
    pub lease_id: String,
    pub project_id: String,
    pub resource_key: String,
    pub holder_device_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holder_device_name: Option<String>,
    pub holder_id: String,
    pub generation: u64,
    pub acquired_at: chrono::DateTime<chrono::Utc>,
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

impl NamedLease {
    pub fn validate(&self) -> Result<()> {
        for (field, value) in [
            ("lease_id", self.lease_id.as_str()),
            ("project_id", self.project_id.as_str()),
            ("resource_key", self.resource_key.as_str()),
            ("holder_device_id", self.holder_device_id.as_str()),
            ("holder_id", self.holder_id.as_str()),
        ] {
            if value.is_empty() {
                bail!("named lease is missing {field}");
            }
        }
        if self.generation == 0 {
            bail!("named lease generation must be greater than zero");
        }
        if self.acquired_at >= self.expires_at {
            bail!("named lease expiry must be after acquisition");
        }
        Ok(())
    }

    pub fn validate_for_resource(&self, project_id: &str, resource_key: &str) -> Result<()> {
        self.validate()?;
        if self.project_id != project_id || self.resource_key != resource_key {
            bail!("provider returned a named lease for a different project or resource");
        }
        Ok(())
    }

    pub fn validate_for_lease(&self, project_id: &str, lease_id: &str) -> Result<()> {
        self.validate()?;
        if self.project_id != project_id || self.lease_id != lease_id {
            bail!("provider returned a different named lease than requested");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NamedLeaseMutationResponse {
    pub outcome: NamedLeaseOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease: Option<NamedLease>,
}

impl NamedLeaseMutationResponse {
    pub fn validate(&self) -> Result<()> {
        if self.outcome.carries_lease() != self.lease.is_some() {
            bail!("named-lease response has an invalid lease payload for its outcome");
        }
        if let Some(lease) = &self.lease {
            lease.validate()?;
        }
        Ok(())
    }

    pub fn validate_for_resource(&self, project_id: &str, resource_key: &str) -> Result<()> {
        self.validate()?;
        if let Some(lease) = &self.lease {
            lease.validate_for_resource(project_id, resource_key)?;
        }
        Ok(())
    }

    pub fn validate_for_lease(&self, project_id: &str, lease_id: &str) -> Result<()> {
        self.validate()?;
        if let Some(lease) = &self.lease {
            lease.validate_for_lease(project_id, lease_id)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NamedLeaseListResponse {
    pub leases: Vec<NamedLease>,
}

impl NamedLeaseListResponse {
    pub fn validate_for_project(&self, project_id: &str) -> Result<()> {
        let mut lease_ids = BTreeSet::new();
        let mut resource_keys = BTreeSet::new();
        for lease in &self.leases {
            lease.validate()?;
            if lease.project_id != project_id {
                bail!("provider returned a named lease for a different project");
            }
            if !lease_ids.insert(lease.lease_id.as_str())
                || !resource_keys.insert(lease.resource_key.as_str())
            {
                bail!("provider returned duplicate live named leases");
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdvertisedHead {
    pub name: String,
    pub oid: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CatalogProject {
    pub project_id: String,
    #[serde(default)]
    pub name: String,
    pub remote_url: String,
    #[serde(default)]
    pub default_branch: String,
    #[serde(default)]
    pub advertised_heads: Vec<AdvertisedHead>,
    #[serde(default)]
    pub project_generation: u64,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Catalog {
    pub protocol: String,
    pub catalog_generation: u64,
    pub projects: Vec<CatalogProject>,
}

impl Catalog {
    pub fn from_value(mut value: Value) -> Result<Self> {
        let object = value
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("server returned an incompatible catalog"))?;
        if object.get("protocol").and_then(Value::as_str) != Some(PROTOCOL_VERSION)
            || !object.get("catalog_generation").is_some_and(Value::is_u64)
        {
            bail!("server returned an incompatible catalog");
        }
        let projects = object
            .get_mut("projects")
            .and_then(Value::as_array_mut)
            .ok_or_else(|| anyhow::anyhow!("catalog.projects must be an array"))?;
        for project in projects {
            let item = project
                .as_object_mut()
                .ok_or_else(|| anyhow::anyhow!("catalog project must be an object"))?;
            if item
                .get("default_branch")
                .is_none_or(|v| v.as_str().is_none_or(|s| s.is_empty()))
                && let Some(value) = item.get("sync_branch").cloned()
            {
                item.insert("default_branch".into(), value);
            }
            if !item.contains_key("advertised_heads") {
                let heads = match item.get("advertised_head_oid").and_then(Value::as_str) {
                    Some(oid) => vec![serde_json::json!({
                        "name": item.get("default_branch").and_then(Value::as_str).unwrap_or(""),
                        "oid": oid
                    })],
                    None => Vec::new(),
                };
                item.insert("advertised_heads".into(), Value::Array(heads));
            }
            item.remove("sync_branch");
            item.remove("advertised_head_oid");
        }
        let catalog: Catalog = serde_json::from_value(value)?;
        catalog.validate()?;
        Ok(catalog)
    }

    pub fn validate(&self) -> Result<()> {
        let oid = Regex::new(r"^[0-9a-f]{40,64}$")?;
        for project in &self.projects {
            for (field, value) in [
                ("project_id", project.project_id.as_str()),
                ("remote_url", project.remote_url.as_str()),
                ("default_branch", project.default_branch.as_str()),
            ] {
                if value.is_empty() {
                    bail!("catalog project is missing {field}");
                }
            }
            let mut names = BTreeSet::new();
            for head in &project.advertised_heads {
                if head.name.is_empty() || !names.insert(&head.name) {
                    bail!("catalog project has an invalid or duplicate branch name");
                }
                if !oid.is_match(&head.oid) {
                    bail!("catalog project has an invalid advertised head OID");
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_preview_catalog_shape() {
        let catalog = Catalog::from_value(serde_json::json!({
            "protocol": "resync.v1", "catalog_generation": 2,
            "projects": [{
                "project_id": "prj_x", "remote_url": "https://example.invalid/x.git",
                "sync_branch": "main", "advertised_head_oid": "a".repeat(40)
            }]
        }))
        .unwrap();
        assert_eq!(catalog.projects[0].default_branch, "main");
        assert_eq!(catalog.projects[0].advertised_heads.len(), 1);
    }

    #[test]
    fn rejects_duplicate_heads() {
        let result = Catalog::from_value(serde_json::json!({
            "protocol": "resync.v1", "catalog_generation": 1,
            "projects": [{
                "project_id": "prj_x", "remote_url": "https://example.invalid/x.git",
                "default_branch": "main", "advertised_heads": [
                    {"name": "main", "oid": "a".repeat(40)},
                    {"name": "main", "oid": "b".repeat(40)}
                ]
            }]
        }));
        assert!(result.unwrap_err().to_string().contains("duplicate"));
    }

    #[test]
    fn named_lease_outcomes_are_exhaustively_parsed() {
        assert_eq!(
            serde_json::from_str::<NamedLeaseOutcome>(r#""HELD""#).unwrap(),
            NamedLeaseOutcome::Held
        );
        assert!(serde_json::from_str::<NamedLeaseOutcome>(r#""SURPRISE""#).is_err());
    }

    #[test]
    fn named_lease_authority_is_validated_against_the_request() {
        let response: NamedLeaseMutationResponse = serde_json::from_value(serde_json::json!({
            "outcome": "ACQUIRED",
            "lease": {
                "lease_id": "nls_1",
                "project_id": "prj_other",
                "resource_key": "waykeeper/maps/map/tickets/ticket",
                "holder_device_id": "dev_1",
                "holder_id": "worker_1",
                "generation": 1,
                "acquired_at": "2026-08-03T00:00:00Z",
                "expires_at": "2026-08-03T00:15:00Z"
            }
        }))
        .unwrap();
        assert!(
            response
                .validate_for_resource("prj_expected", "waykeeper/maps/map/tickets/ticket")
                .unwrap_err()
                .to_string()
                .contains("different project")
        );
    }

    #[test]
    fn named_lease_responses_reject_missing_authority_fields() {
        let response: NamedLeaseMutationResponse = serde_json::from_value(serde_json::json!({
            "outcome": "ACQUIRED"
        }))
        .unwrap();
        assert!(response.validate().is_err());

        let policy = NamedLeasePolicy {
            default_ttl_seconds: 15,
            min_ttl_seconds: 30,
            max_ttl_seconds: 3600,
        };
        assert!(policy.validate().is_err());
    }
}
