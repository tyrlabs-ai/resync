use crate::git_state::{
    SyntheticChain, WorkspaceSnapshot, capture_workspace, create_synthetic_chain, current_branch,
    find_untracked_collisions, read_head_ref, tree_paths,
};
use crate::process::{RunOptions, git};
use crate::state::{state_paths, write_json};
use anyhow::{Result, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::tempdir;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    pub working_commit: String,
    pub index_commit: String,
    pub real_head: String,
    pub working_tree: String,
    pub index_tree: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TransactionConflict {
    pub kind: String,
    #[serde(default)]
    pub paths: Vec<String>,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Transaction {
    pub id: String,
    pub project_path: PathBuf,
    pub previous_remote_oid: String,
    pub target_remote_oid: String,
    pub snapshot: WorkspaceSnapshot,
    pub recovery_ref: String,
    pub head_ref: String,
    pub phase: String,
    pub created_at: String,
    pub path: PathBuf,
    #[serde(default)]
    pub candidate: Option<Candidate>,
    #[serde(default)]
    pub conflict: Option<TransactionConflict>,
    #[serde(default)]
    pub completed_at: Option<String>,
    #[serde(default)]
    pub recovered_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ReconcileResult {
    pub outcome: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transaction_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_head: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_head: Option<String>,
    #[serde(default)]
    pub changed_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default)]
    pub paths: Vec<String>,
}

fn is_ancestor(project_path: &Path, ancestor: &str, descendant: &str) -> Result<bool> {
    let options = RunOptions {
        allow_failure: true,
        ..RunOptions::default()
    };
    Ok(git(
        project_path,
        ["merge-base", "--is-ancestor", ancestor, descendant],
        options,
    )?
    .code
        == 0)
}

fn changed_paths(project_path: &Path, old_oid: &str, new_oid: &str) -> Result<Vec<String>> {
    let output = git(
        project_path,
        ["diff", "--name-only", "-z", old_oid, new_oid],
        RunOptions::default(),
    )?
    .stdout;
    Ok(output
        .split('\0')
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect())
}

fn write_recovery_refs(
    project_path: &Path,
    id: &str,
    snapshot: &WorkspaceSnapshot,
    chain: &SyntheticChain,
) -> Result<String> {
    let prefix = format!("refs/resync/recovery/{id}");
    for (suffix, oid) in [
        ("head", &snapshot.head),
        ("index", &chain.index_commit),
        ("working", &chain.working_commit),
    ] {
        git(
            project_path,
            ["update-ref", &format!("{prefix}/{suffix}"), oid],
            RunOptions::default(),
        )?;
    }
    Ok(prefix)
}

fn install_candidate(
    project_path: &Path,
    transaction: &mut Transaction,
    candidate: &Candidate,
) -> Result<Option<TransactionConflict>> {
    let latest = capture_workspace(project_path)?;
    if latest != transaction.snapshot {
        return Ok(Some(TransactionConflict {
            kind: "install-rejected".into(),
            paths: Vec::new(),
            detail:
                "workspace changed while reconciliation was prepared; retry at a new lease boundary"
                    .into(),
        }));
    }
    let tracked = tree_paths(project_path, &candidate.working_tree)?;
    let collisions = find_untracked_collisions(&transaction.snapshot.untracked, &tracked);
    if !collisions.is_empty() {
        return Ok(Some(TransactionConflict {
            kind: "untracked-collision".into(),
            detail: format!(
                "incoming tracked paths collide with untracked files: {}",
                collisions.join(", ")
            ),
            paths: collisions,
        }));
    }
    transaction.phase = "INSTALLING".into();
    write_json(&transaction.path, transaction, 0o600)?;
    git(
        project_path,
        ["read-tree", "--reset", "-u", &candidate.working_tree],
        RunOptions::default(),
    )?;
    git(
        project_path,
        ["read-tree", &candidate.index_tree],
        RunOptions::default(),
    )?;
    git(
        project_path,
        [
            "update-ref",
            &transaction.head_ref,
            &candidate.real_head,
            &transaction.snapshot.head,
        ],
        RunOptions::default(),
    )?;
    transaction.phase = "COMPLETED".into();
    transaction.completed_at = Some(Utc::now().to_rfc3339());
    write_json(&transaction.path, transaction, 0o600)?;
    Ok(None)
}

pub fn reconcile_workspace(
    project_path: &Path,
    previous_remote_oid: &str,
    target_remote_oid: &str,
    transaction_root: Option<&Path>,
) -> Result<ReconcileResult> {
    if previous_remote_oid.is_empty() || target_remote_oid.is_empty() {
        bail!("previousRemoteOid and targetRemoteOid are required");
    }
    current_branch(project_path)
        .map_err(|_| anyhow::anyhow!("detached or unborn HEAD is unsupported in RepoSync V1"))?;
    let unresolved = git(
        project_path,
        ["ls-files", "--unmerged"],
        RunOptions::default(),
    )?
    .stdout;
    if !unresolved.trim().is_empty() {
        bail!(
            "an unmerged index is unsupported; resolve or abort the existing Git operation first"
        );
    }
    let snapshot = capture_workspace(project_path)?;
    if previous_remote_oid == target_remote_oid {
        return Ok(ReconcileResult {
            outcome: "current".into(),
            head: Some(snapshot.head),
            ..empty_result()
        });
    }
    if !is_ancestor(project_path, previous_remote_oid, &snapshot.head)? {
        bail!("local HEAD does not descend from the last applied remote revision");
    }
    if !is_ancestor(project_path, previous_remote_oid, target_remote_oid)? {
        bail!("remote branch was rewritten or does not descend from the last applied revision");
    }
    let merges = git(
        project_path,
        [
            "rev-list",
            "--merges",
            &format!("{previous_remote_oid}..{}", snapshot.head),
        ],
        RunOptions::default(),
    )?
    .stdout;
    if !merges.trim().is_empty() {
        bail!("merge commits in unpublished local history are unsupported in RepoSync V1");
    }

    let id = format!("{}-{}", Utc::now().timestamp_millis(), Uuid::new_v4());
    let chain = create_synthetic_chain(project_path, &snapshot)?;
    let recovery_ref = write_recovery_refs(project_path, &id, &snapshot, &chain)?;
    let root = transaction_root
        .map(Path::to_owned)
        .unwrap_or_else(|| state_paths().transactions);
    fs::create_dir_all(&root)?;
    let path = root.join(format!("{id}.json"));
    let mut transaction = Transaction {
        id: id.clone(),
        project_path: project_path.to_owned(),
        previous_remote_oid: previous_remote_oid.into(),
        target_remote_oid: target_remote_oid.into(),
        snapshot: snapshot.clone(),
        recovery_ref: recovery_ref.clone(),
        head_ref: read_head_ref(project_path)?,
        phase: "PREPARING".into(),
        created_at: Utc::now().to_rfc3339(),
        path,
        candidate: None,
        conflict: None,
        completed_at: None,
        recovered_at: None,
    };
    write_json(&transaction.path, &transaction, 0o600)?;

    let temporary = tempdir()?;
    let temporary_path = temporary.path();
    let branch = format!("resync-candidate-{id}");
    let outcome = (|| -> Result<ReconcileResult> {
        git(
            project_path,
            [
                "worktree",
                "add",
                "--detach",
                temporary_path.to_string_lossy().as_ref(),
                &chain.working_commit,
            ],
            RunOptions::default(),
        )?;
        git(
            temporary_path,
            ["switch", "-c", &branch],
            RunOptions::default(),
        )?;
        let mut options = RunOptions {
            allow_failure: true,
            ..RunOptions::default()
        };
        for (key, value) in [
            ("GIT_SEQUENCE_EDITOR", ":"),
            ("GIT_EDITOR", ":"),
            ("GIT_COMMITTER_NAME", "RepoSync Reconciler"),
            ("GIT_COMMITTER_EMAIL", "reconcile@reposync.local"),
        ] {
            options.env.insert(key.into(), value.into());
        }
        let result = git(
            temporary_path,
            [
                "rebase",
                "--keep-empty",
                "--onto",
                target_remote_oid,
                previous_remote_oid,
                &branch,
            ],
            options,
        )?;
        if result.code != 0 {
            let abort = RunOptions {
                allow_failure: true,
                ..RunOptions::default()
            };
            git(temporary_path, ["rebase", "--abort"], abort)?;
            let detail: String = result
                .stderr
                .chars()
                .rev()
                .take(4000)
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            transaction.phase = "CONFLICTED".into();
            transaction.conflict = Some(TransactionConflict {
                kind: "git-rebase".into(),
                paths: Vec::new(),
                detail: detail.trim().into(),
            });
            write_json(&transaction.path, &transaction, 0o600)?;
            return Ok(ReconcileResult {
                outcome: "conflict".into(),
                transaction_id: Some(id.clone()),
                recovery_ref: Some(recovery_ref.clone()),
                detail: Some(detail.trim().into()),
                ..empty_result()
            });
        }
        let working_commit = rev(temporary_path, "HEAD")?;
        let index_commit = rev(temporary_path, "HEAD^")?;
        let real_head = rev(temporary_path, "HEAD^^")?;
        let working_tree = rev(temporary_path, &format!("{working_commit}^{{tree}}"))?;
        let index_tree = rev(temporary_path, &format!("{index_commit}^{{tree}}"))?;
        let candidate = Candidate {
            working_commit,
            index_commit,
            real_head: real_head.clone(),
            working_tree,
            index_tree,
        };
        transaction.candidate = Some(candidate.clone());
        write_json(&transaction.path, &transaction, 0o600)?;
        if let Some(conflict) = install_candidate(project_path, &mut transaction, &candidate)? {
            transaction.phase = "CONFLICTED".into();
            transaction.conflict = Some(conflict.clone());
            write_json(&transaction.path, &transaction, 0o600)?;
            return Ok(ReconcileResult {
                outcome: "conflict".into(),
                transaction_id: Some(id.clone()),
                recovery_ref: Some(recovery_ref.clone()),
                detail: Some(conflict.detail),
                paths: conflict.paths,
                ..empty_result()
            });
        }
        Ok(ReconcileResult {
            outcome: "applied".into(),
            transaction_id: Some(id.clone()),
            old_head: Some(snapshot.head.clone()),
            new_head: Some(real_head),
            changed_paths: changed_paths(project_path, previous_remote_oid, target_remote_oid)?,
            ..empty_result()
        })
    })();
    let cleanup = RunOptions {
        allow_failure: true,
        ..RunOptions::default()
    };
    let _ = git(
        project_path,
        [
            "worktree",
            "remove",
            "--force",
            temporary_path.to_string_lossy().as_ref(),
        ],
        cleanup.clone(),
    );
    let _ = git(project_path, ["branch", "-D", &branch], cleanup);
    outcome
}

fn rev(path: &Path, value: &str) -> Result<String> {
    Ok(git(path, ["rev-parse", value], RunOptions::default())?
        .stdout
        .trim()
        .into())
}

fn empty_result() -> ReconcileResult {
    ReconcileResult {
        outcome: String::new(),
        transaction_id: None,
        recovery_ref: None,
        head: None,
        old_head: None,
        new_head: None,
        changed_paths: Vec::new(),
        detail: None,
        paths: Vec::new(),
    }
}

pub fn recover_transaction(path: &Path) -> Result<Transaction> {
    let mut transaction: Transaction = serde_json::from_slice(&fs::read(path)?)?;
    if matches!(
        transaction.phase.as_str(),
        "PREPARING" | "CONFLICTED" | "COMPLETED"
    ) {
        return Ok(transaction);
    }
    if transaction.phase != "INSTALLING" {
        bail!("unknown transaction phase {}", transaction.phase);
    }
    let current = capture_workspace(&transaction.project_path)?;
    let candidate = transaction
        .candidate
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("installing transaction has no candidate"))?;
    if current.head == candidate.real_head {
        transaction.phase = "COMPLETED".into();
        transaction.recovered_at = Some(Utc::now().to_rfc3339());
        write_json(path, &transaction, 0o600)?;
        return Ok(transaction);
    }
    if current.head == transaction.snapshot.head {
        git(
            &transaction.project_path,
            [
                "read-tree",
                "--reset",
                "-u",
                &transaction.snapshot.working_tree,
            ],
            RunOptions::default(),
        )?;
        git(
            &transaction.project_path,
            ["read-tree", &transaction.snapshot.index_tree],
            RunOptions::default(),
        )?;
        transaction.phase = "ROLLED_BACK".into();
        transaction.recovered_at = Some(Utc::now().to_rfc3339());
        write_json(path, &transaction, 0o600)?;
        return Ok(transaction);
    }
    bail!(
        "transaction {} needs manual recovery; HEAD matches neither preserved state",
        path.file_name().unwrap_or_default().to_string_lossy()
    )
}
