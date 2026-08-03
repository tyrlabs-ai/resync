use crate::hook_dispatcher::{
    ensure_workspace_dispatcher, is_reposync_workspace_command, shell_quote,
};
use crate::state::{load_catalog, state_paths, write_json};
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

pub const WORKSPACE_MANIFEST_NAME: &str = ".resync-workspace.toml";

#[derive(Debug, Deserialize)]
struct WorkspaceDocument {
    version: u32,
    #[serde(default)]
    project: Vec<WorkspaceProjectDocument>,
}

#[derive(Debug, Deserialize)]
struct WorkspaceProjectDocument {
    path: PathBuf,
    #[serde(default)]
    project_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceProject {
    pub project_id: String,
    pub local_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    pub root: PathBuf,
    pub manifest_path: PathBuf,
    pub projects: Vec<WorkspaceProject>,
}

pub fn load_workspace(manifest_path: &Path) -> Result<Workspace> {
    let manifest_path = fs::canonicalize(manifest_path)
        .with_context(|| format!("read workspace manifest {}", manifest_path.display()))?;
    let root = manifest_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("workspace manifest has no parent directory"))?
        .to_path_buf();
    let document: WorkspaceDocument = toml::from_str(&fs::read_to_string(&manifest_path)?)?;
    ensure!(
        document.version == 1,
        "unsupported workspace manifest version {}; expected 1",
        document.version
    );
    ensure!(
        !document.project.is_empty(),
        "workspace manifest must configure at least one project"
    );

    let catalog = load_catalog()?;
    let catalog_projects = catalog
        .projects
        .iter()
        .filter_map(|project| match fs::canonicalize(&project.local_path) {
            Ok(path) => Some(Ok((path, project))),
            Err(error) if error.kind() == ErrorKind::NotFound => None,
            Err(error) => Some(Err(error).with_context(|| {
                format!(
                    "resolve subscribed project {}",
                    project.local_path.display()
                )
            })),
        })
        .collect::<Result<Vec<_>>>()?;
    let mut seen_paths = BTreeSet::new();
    let mut seen_project_ids = BTreeSet::new();
    let mut projects = Vec::with_capacity(document.project.len());
    for configured in document.project {
        ensure!(
            !configured.path.is_absolute(),
            "workspace project paths must be relative: {}",
            configured.path.display()
        );
        let local_path = fs::canonicalize(root.join(&configured.path)).with_context(|| {
            format!(
                "resolve workspace project path {}",
                configured.path.display()
            )
        })?;
        ensure!(
            local_path != root && local_path.starts_with(&root),
            "workspace project path escapes the workspace root: {}",
            configured.path.display()
        );
        let project = catalog_projects
            .iter()
            .find_map(|(path, project)| (path == &local_path).then_some(*project))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "workspace project {} is not a locally subscribed RepoSync checkout",
                    configured.path.display()
                )
            })?;
        if let Some(expected_project_id) = configured.project_id {
            ensure!(
                expected_project_id == project.project_id,
                "workspace project {} expected ID {}, but the local checkout is {}",
                configured.path.display(),
                expected_project_id,
                project.project_id
            );
        }
        ensure!(
            seen_paths.insert(local_path.clone()),
            "workspace project path is configured more than once: {}",
            configured.path.display()
        );
        ensure!(
            seen_project_ids.insert(project.project_id.clone()),
            "workspace project ID is configured more than once: {}",
            project.project_id
        );
        projects.push(WorkspaceProject {
            project_id: project.project_id.clone(),
            local_path,
        });
    }

    Ok(Workspace {
        root,
        manifest_path,
        projects,
    })
}

pub fn install_hooks(root: &Path) -> Result<Value> {
    let root = fs::canonicalize(root)
        .with_context(|| format!("resolve workspace root {}", root.display()))?;
    let workspace = load_workspace(&root.join(WORKSPACE_MANIFEST_NAME))?;
    ensure!(
        workspace.root == root,
        "workspace manifest must be located directly beneath the workspace root"
    );

    let codex_directory = root.join(".codex");
    let hooks_path = codex_directory.join("hooks.json");
    let hooks_existed = hooks_path.exists();
    let mut hooks: Value = if hooks_existed {
        serde_json::from_slice(&fs::read(&hooks_path)?)?
    } else {
        json!({
            "description": "RepoSync multi-project workspace leases",
            "hooks": {}
        })
    };
    if !hooks.is_object() {
        bail!("{} must contain a JSON object", hooks_path.display());
    }
    if !hooks.get("hooks").is_some_and(Value::is_object) {
        hooks["hooks"] = json!({});
    }

    let original_hooks = hooks.clone();
    let executable = std::env::current_exe()?;
    let dispatcher =
        ensure_workspace_dispatcher(&state_paths().root, &workspace.manifest_path, &executable)?;
    let command = shell_quote(&dispatcher.to_string_lossy());
    for event in ["PreToolUse", "PostToolUse"] {
        if !hooks["hooks"].get(event).is_some_and(Value::is_array) {
            hooks["hooks"][event] = Value::Array(Vec::new());
        }
        let groups = hooks["hooks"][event].as_array_mut().unwrap();
        let mut command_found = false;
        groups.retain_mut(|group| {
            let Some(handlers) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
                return true;
            };
            handlers.retain(|handler| {
                let Some(value) = handler.get("command").and_then(Value::as_str) else {
                    return true;
                };
                if value == command {
                    if command_found {
                        return false;
                    }
                    command_found = true;
                    return true;
                }
                !is_reposync_workspace_command(value)
            });
            !handlers.is_empty()
        });
        if !command_found {
            groups.push(json!({
                "matcher": "*",
                "hooks": [{
                    "type": "command",
                    "command": command,
                    "timeout": 3600,
                    "statusMessage": if event == "PreToolUse" {
                        "Refreshing RepoSync workspace projects"
                    } else {
                        "Releasing RepoSync workspace projects"
                    }
                }]
            }));
        }
    }
    fs::create_dir_all(&codex_directory)?;
    let requires_codex_trust_review = !hooks_existed || hooks != original_hooks;
    if requires_codex_trust_review {
        write_json(&hooks_path, &hooks, 0o644)?;
    }

    Ok(json!({
        "workspaceRoot": workspace.root,
        "manifestPath": workspace.manifest_path,
        "hooksPath": hooks_path,
        "codexHookDispatcher": dispatcher,
        "projectCount": workspace.projects.len(),
        "projects": workspace.projects.iter().map(|project| json!({
            "projectId": project.project_id,
            "path": project.local_path
        })).collect::<Vec<_>>(),
        "requiresCodexTrustReview": requires_codex_trust_review
    }))
}

#[cfg(test)]
mod tests {
    use super::WorkspaceDocument;

    #[test]
    fn workspace_document_requires_explicit_project_entries() -> anyhow::Result<()> {
        let document: WorkspaceDocument = toml::from_str(
            r#"version = 1

[[project]]
path = "resync"
"#,
        )?;
        assert_eq!(document.project.len(), 1);
        assert_eq!(document.project[0].path.to_string_lossy(), "resync");
        assert_eq!(document.project[0].project_id, None);
        Ok(())
    }
}
