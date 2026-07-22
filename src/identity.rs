use crate::credentials::canonical_origin;
use crate::process::{RunOptions, git};
use anyhow::{Result, bail};
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub root: PathBuf,
    pub service: Option<String>,
    pub project_id: Option<String>,
    pub checkout_id: Option<String>,
}

pub fn repository_root(path: &Path) -> Result<PathBuf> {
    let options = RunOptions {
        allow_failure: true,
        ..RunOptions::default()
    };
    let result = git(path, ["rev-parse", "--show-toplevel"], options)?;
    if result.code != 0 {
        bail!("this command must run inside a Git worktree");
    }
    Ok(PathBuf::from(result.stdout.trim()))
}

fn local_config(root: &Path, key: &str) -> Result<Option<String>> {
    let options = RunOptions {
        allow_failure: true,
        ..RunOptions::default()
    };
    let result = git(root, ["config", "--local", "--get", key], options)?;
    Ok((result.code == 0).then(|| result.stdout.trim().to_owned()))
}

pub fn read_identity(path: &Path) -> Result<Identity> {
    let root = repository_root(path)?;
    let service = local_config(&root, "resync.service")?;
    let project_id = local_config(&root, "resync.projectId")?;
    let checkout_id = local_config(&root, "resync.checkoutId")?;
    if service.is_some() != project_id.is_some() {
        bail!("incomplete RepoSync identity in local Git configuration");
    }
    Ok(Identity {
        root,
        service: service.map(|value| canonical_origin(&value)).transpose()?,
        project_id,
        checkout_id,
    })
}

pub fn write_identity(
    path: &Path,
    service: &str,
    project_id: &str,
    checkout_id: Option<&str>,
) -> Result<Identity> {
    let root = repository_root(path)?;
    let origin = canonical_origin(service)?;
    let checkout = checkout_id
        .map(str::to_owned)
        .unwrap_or_else(|| format!("chk_{}", Uuid::new_v4().simple()));
    for (key, value) in [
        ("resync.service", origin.as_str()),
        ("resync.projectId", project_id),
        ("resync.checkoutId", checkout.as_str()),
    ] {
        git(
            &root,
            ["config", "--local", key, value],
            RunOptions::default(),
        )?;
    }
    Ok(Identity {
        root,
        service: Some(origin),
        project_id: Some(project_id.into()),
        checkout_id: Some(checkout),
    })
}

pub fn clear_identity(path: &Path) -> Result<()> {
    let root = repository_root(path)?;
    for key in ["resync.service", "resync.projectId", "resync.checkoutId"] {
        let options = RunOptions {
            allow_failure: true,
            ..RunOptions::default()
        };
        git(&root, ["config", "--local", "--unset-all", key], options)?;
    }
    Ok(())
}

pub fn same_project(left: &Identity, right: &Identity) -> Result<bool> {
    Ok(
        match (
            &left.service,
            &left.project_id,
            &right.service,
            &right.project_id,
        ) {
            (Some(a), Some(b), Some(c), Some(d)) => {
                canonical_origin(a)? == canonical_origin(c)? && b == d
            }
            _ => false,
        },
    )
}
