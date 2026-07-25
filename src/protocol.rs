use anyhow::{Result, bail};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;

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
}
