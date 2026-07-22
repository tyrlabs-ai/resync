use crate::process::{RunOptions, git};
use anyhow::Result;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceSnapshot {
    pub head: String,
    pub index_tree: String,
    pub working_tree: String,
    pub untracked: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyntheticChain {
    pub index_commit: String,
    pub working_commit: String,
}

fn rev(project_path: &Path, name: &str) -> Result<String> {
    Ok(git(
        project_path,
        ["rev-parse", "--verify", name],
        RunOptions::default(),
    )?
    .stdout
    .trim()
    .into())
}

pub fn current_branch(project_path: &Path) -> Result<String> {
    Ok(git(
        project_path,
        ["symbolic-ref", "--short", "HEAD"],
        RunOptions::default(),
    )?
    .stdout
    .trim()
    .into())
}

pub fn list_untracked(project_path: &Path) -> Result<Vec<String>> {
    let output = git(
        project_path,
        ["ls-files", "--others", "-z"],
        RunOptions::default(),
    )?
    .stdout;
    Ok(output
        .split('\0')
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect())
}

fn temporary_index(project_path: &Path, seed_tree: &str, update_tracked: bool) -> Result<String> {
    let directory = tempdir()?;
    let index = directory.path().join("index");
    let mut options = RunOptions::default();
    options
        .env
        .insert("GIT_INDEX_FILE".into(), index.into_os_string());
    git(project_path, ["read-tree", seed_tree], options.clone())?;
    if update_tracked {
        git(project_path, ["add", "-u", "--", "."], options.clone())?;
    }
    Ok(git(project_path, ["write-tree"], options)?
        .stdout
        .trim()
        .into())
}

pub fn capture_workspace(project_path: &Path) -> Result<WorkspaceSnapshot> {
    let head = rev(project_path, "HEAD")?;
    let index_tree = git(project_path, ["write-tree"], RunOptions::default())?
        .stdout
        .trim()
        .to_owned();
    let working_tree = temporary_index(project_path, &index_tree, true)?;
    let untracked = list_untracked(project_path)?;
    Ok(WorkspaceSnapshot {
        head,
        index_tree,
        working_tree,
        untracked,
    })
}

pub fn create_synthetic_chain(
    project_path: &Path,
    snapshot: &WorkspaceSnapshot,
) -> Result<SyntheticChain> {
    let mut options = RunOptions::default();
    for (key, value) in [
        ("GIT_AUTHOR_NAME", "RepoSync Recovery"),
        ("GIT_AUTHOR_EMAIL", "recovery@reposync.local"),
        ("GIT_COMMITTER_NAME", "RepoSync Recovery"),
        ("GIT_COMMITTER_EMAIL", "recovery@reposync.local"),
    ] {
        options.env.insert(key.into(), value.into());
    }
    options.input = Some(b"RepoSync synthetic index\n".to_vec());
    let index_commit = git(
        project_path,
        ["commit-tree", &snapshot.index_tree, "-p", &snapshot.head],
        options.clone(),
    )?
    .stdout
    .trim()
    .to_owned();
    options.input = Some(b"RepoSync synthetic working tree\n".to_vec());
    let working_commit = git(
        project_path,
        ["commit-tree", &snapshot.working_tree, "-p", &index_commit],
        options,
    )?
    .stdout
    .trim()
    .to_owned();
    Ok(SyntheticChain {
        index_commit,
        working_commit,
    })
}

pub fn tree_paths(project_path: &Path, tree: &str) -> Result<Vec<String>> {
    let output = git(
        project_path,
        ["ls-tree", "-r", "--name-only", "-z", tree],
        RunOptions::default(),
    )?
    .stdout;
    Ok(output
        .split('\0')
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect())
}

pub fn find_untracked_collisions(untracked: &[String], tracked: &[String]) -> Vec<String> {
    let tracked_set: BTreeSet<&str> = tracked.iter().map(String::as_str).collect();
    let mut collisions = BTreeSet::new();
    for path in untracked {
        if tracked_set.contains(path.as_str()) {
            collisions.insert(path.clone());
        }
        let pieces: Vec<&str> = path.split('/').collect();
        for index in 1..pieces.len() {
            if tracked_set.contains(pieces[..index].join("/").as_str()) {
                collisions.insert(path.clone());
            }
        }
        if tracked
            .iter()
            .any(|candidate| candidate.starts_with(&format!("{path}/")))
        {
            collisions.insert(path.clone());
        }
    }
    collisions.into_iter().collect()
}

pub fn git_dir(project_path: &Path) -> Result<PathBuf> {
    let raw = git(
        project_path,
        ["rev-parse", "--git-dir"],
        RunOptions::default(),
    )?
    .stdout
    .trim()
    .to_owned();
    let path = PathBuf::from(raw);
    Ok(if path.is_absolute() {
        path
    } else {
        project_path.join(path)
    })
}

pub fn read_head_ref(project_path: &Path) -> Result<String> {
    Ok(git(
        project_path,
        ["symbolic-ref", "HEAD"],
        RunOptions::default(),
    )?
    .stdout
    .trim()
    .into())
}
