use anyhow::Result;
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

pub const DEFAULT_PROJECT_CONFIG: &str = r#"version = 1

[sync]
poll_interval = "10s"
preserve_index = true
conflict_behavior = "pause-writes"

[publish]
automatic = true
max_retries = 5
"#;

pub fn ensure_project_config(project_path: &Path) -> Result<PathBuf> {
    let path = project_path.join(".resync.toml");
    if !path.exists() {
        fs::write(&path, DEFAULT_PROJECT_CONFIG)?;
    }
    Ok(path)
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ProjectDocument {
    #[serde(default)]
    validation: Vec<Validation>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct Validation {
    #[serde(default)]
    command: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectConfig {
    pub validations: Vec<Vec<String>>,
}

pub fn read_project_config(project_path: &Path) -> Result<ProjectConfig> {
    let path = project_path.join(".resync.toml");
    if !path.exists() {
        return Ok(ProjectConfig::default());
    }
    let document: ProjectDocument = toml::from_str(&fs::read_to_string(path)?)?;
    Ok(ProjectConfig {
        validations: document
            .validation
            .into_iter()
            .map(|value| value.command)
            .filter(|value| !value.is_empty())
            .collect(),
    })
}
