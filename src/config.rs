use anyhow::Result;
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

pub const DEFAULT_PROJECT_CONFIG: &str = r#"version = 1

# Run commands against the exact commit before RepoSync publishes it. Keep this
# file committed so every checkout applies the same publication policy.
#
# [[validation]]
# command = ["cargo", "test", "--locked"]
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

#[cfg(test)]
mod tests {
    use super::{DEFAULT_PROJECT_CONFIG, ProjectDocument, read_project_config};
    use std::fs;

    #[test]
    fn generated_policy_is_valid_and_has_no_commands() -> anyhow::Result<()> {
        assert_eq!(DEFAULT_PROJECT_CONFIG, include_str!("../.resync.toml"));
        let document: ProjectDocument = toml::from_str(DEFAULT_PROJECT_CONFIG)?;
        assert!(document.validation.is_empty());
        Ok(())
    }

    #[test]
    fn reads_validation_commands_in_declaration_order() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        fs::write(
            directory.path().join(".resync.toml"),
            r#"version = 1

[[validation]]
command = ["cargo", "fmt", "--check"]

[[validation]]
command = []

[[validation]]
command = ["cargo", "test", "--locked"]
"#,
        )?;

        let policy = read_project_config(directory.path())?;
        assert_eq!(
            policy.validations,
            vec![
                vec!["cargo", "fmt", "--check"],
                vec!["cargo", "test", "--locked"],
            ]
        );
        Ok(())
    }
}
