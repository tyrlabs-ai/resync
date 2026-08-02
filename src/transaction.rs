use crate::git_state::{
    SyntheticChain, WorkspaceSnapshot, capture_workspace, create_synthetic_chain, current_branch,
    find_untracked_collisions, read_head_ref, tree_paths,
};
use crate::process::{RunOptions, git};
use crate::state::{state_paths, write_json};
use anyhow::{Result, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TransactionRetention {
    pub debug_history: bool,
}

pub const DEBUG_HISTORY_MAX_RECORDS: usize = 128;
pub const DEBUG_HISTORY_MAX_BYTES: u64 = 256 * 1024 * 1024;

pub fn transaction_retention(state_root: &Path) -> TransactionRetention {
    TransactionRetention {
        debug_history: state_root.join("transaction-debug").is_file(),
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TransactionGcReport {
    pub dry_run: bool,
    pub debug_history: bool,
    pub scanned: usize,
    pub retained: usize,
    pub removed: usize,
    pub compacted: usize,
    pub invalid_retained: usize,
    pub recovery_refs_prunable: usize,
    pub recovery_refs_removed: usize,
    pub recovery_refs_skipped: usize,
    pub bytes_reclaimed: u64,
}

struct GcEntry {
    transaction: Transaction,
    original_bytes: u64,
    compact_bytes: u64,
    retain: bool,
}

pub fn gc_transactions(
    root: &Path,
    managed_project_paths: &BTreeSet<PathBuf>,
    active_conflict_paths: &BTreeSet<PathBuf>,
    retention: TransactionRetention,
    dry_run: bool,
) -> Result<TransactionGcReport> {
    let mut entries = Vec::new();
    let mut scanned = 0;
    let mut invalid_retained = 0;
    if let Ok(directory) = fs::read_dir(root) {
        for entry in directory {
            let path = entry?.path();
            if path.extension().is_none_or(|extension| extension != "json") {
                continue;
            }
            scanned += 1;
            let contents = fs::read(&path)?;
            let Ok(mut transaction) = serde_json::from_slice::<Transaction>(&contents) else {
                invalid_retained += 1;
                continue;
            };
            transaction.path = path;
            let compact_bytes = serde_json::to_vec_pretty(&transaction)?.len() as u64 + 1;
            let retain = transaction.phase == "INSTALLING"
                || (transaction.phase == "CONFLICTED"
                    && active_conflict_paths.contains(&transaction.project_path));
            entries.push(GcEntry {
                transaction,
                original_bytes: contents.len() as u64,
                compact_bytes,
                retain,
            });
        }
    }

    if retention.debug_history {
        let mut history = entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| !entry.retain)
            .map(|(index, entry)| (index, entry.transaction.created_at.clone()))
            .collect::<Vec<_>>();
        history.sort_by(|left, right| right.1.cmp(&left.1));
        let mut retained_records = 0;
        let mut retained_bytes = 0_u64;
        for (index, _) in history {
            let entry = &mut entries[index];
            if retained_records < DEBUG_HISTORY_MAX_RECORDS
                && retained_bytes.saturating_add(entry.compact_bytes) <= DEBUG_HISTORY_MAX_BYTES
            {
                entry.retain = true;
                retained_records += 1;
                retained_bytes += entry.compact_bytes;
            }
        }
    }

    let retained = entries.iter().filter(|entry| entry.retain).count() + invalid_retained;
    let removed = entries.iter().filter(|entry| !entry.retain).count();
    let compacted = entries
        .iter()
        .filter(|entry| entry.retain && entry.compact_bytes < entry.original_bytes)
        .count();
    let bytes_reclaimed = entries
        .iter()
        .map(|entry| {
            if entry.retain {
                entry.original_bytes.saturating_sub(entry.compact_bytes)
            } else {
                entry.original_bytes
            }
        })
        .sum();

    let mut project_paths = managed_project_paths.clone();
    let mut required_refs = BTreeMap::<PathBuf, BTreeSet<String>>::new();
    for entry in &entries {
        project_paths.insert(entry.transaction.project_path.clone());
        if !entry.retain {
            continue;
        }
        validate_recovery_ref(&entry.transaction.recovery_ref)?;
        for suffix in ["head", "index", "working"] {
            required_refs
                .entry(entry.transaction.project_path.clone())
                .or_default()
                .insert(format!("{}/{suffix}", entry.transaction.recovery_ref));
        }
    }
    let mut prunable_refs = BTreeMap::<PathBuf, Vec<String>>::new();
    let mut recovery_refs_skipped = 0;
    for project_path in project_paths {
        let Some(actual_refs) = list_recovery_refs(&project_path)? else {
            continue;
        };
        let required = required_refs.get(&project_path);
        let unneeded = actual_refs
            .into_iter()
            .filter(|reference| required.is_none_or(|refs| !refs.contains(reference)))
            .collect::<Vec<_>>();
        if invalid_retained == 0 {
            if !unneeded.is_empty() {
                prunable_refs.insert(project_path, unneeded);
            }
        } else {
            recovery_refs_skipped += unneeded.len();
        }
    }
    let recovery_refs_prunable = prunable_refs.values().map(Vec::len).sum();
    let mut recovery_refs_removed = 0;
    if !dry_run {
        for (project_path, references) in prunable_refs {
            delete_recovery_refs_batch(&project_path, &references)?;
            recovery_refs_removed += references.len();
            remove_empty_recovery_directories(&project_path)?;
        }

        for entry in &entries {
            if entry.retain {
                if entry.compact_bytes < entry.original_bytes {
                    write_json(&entry.transaction.path, &entry.transaction, 0o600)?;
                }
            } else {
                match fs::remove_file(&entry.transaction.path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
        }
    }

    Ok(TransactionGcReport {
        dry_run,
        debug_history: retention.debug_history,
        scanned,
        retained,
        removed,
        compacted,
        invalid_retained,
        recovery_refs_prunable,
        recovery_refs_removed,
        recovery_refs_skipped,
        bytes_reclaimed,
    })
}

fn delete_recovery_refs_batch(project_path: &Path, references: &[String]) -> Result<()> {
    let mut input = String::from("start\n");
    for reference in references {
        validate_recovery_ref(reference)?;
        writeln!(input, "delete {reference}")?;
    }
    input.push_str("prepare\ncommit\n");
    git(
        project_path,
        ["update-ref", "--stdin"],
        RunOptions {
            input: Some(input.into_bytes()),
            ..RunOptions::default()
        },
    )?;
    Ok(())
}

fn validate_recovery_ref(reference: &str) -> Result<()> {
    if !reference.starts_with("refs/resync/recovery/")
        || reference.chars().any(|character| character.is_whitespace())
    {
        bail!("refusing to operate on invalid recovery ref {reference}");
    }
    Ok(())
}

fn list_recovery_refs(project_path: &Path) -> Result<Option<Vec<String>>> {
    let output = match git(
        project_path,
        [
            "for-each-ref",
            "--format=%(refname)",
            "refs/resync/recovery",
        ],
        RunOptions {
            allow_failure: true,
            ..RunOptions::default()
        },
    ) {
        Ok(output) if output.code == 0 => output,
        Ok(_) | Err(_) => return Ok(None),
    };
    Ok(Some(
        output
            .stdout
            .lines()
            .filter(|reference| !reference.is_empty())
            .map(str::to_owned)
            .collect(),
    ))
}

fn remove_empty_recovery_directories(project_path: &Path) -> Result<()> {
    let output = git(
        project_path,
        [
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "refs/resync/recovery",
        ],
        RunOptions::default(),
    )?;
    let root = PathBuf::from(output.stdout.trim());
    let Ok(_) = fs::read_dir(&root) else {
        return Ok(());
    };
    let mut directories = Vec::new();
    let mut pending = vec![root.clone()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory)? {
            let path = entry?.path();
            if path.is_dir() {
                pending.push(path.clone());
                directories.push(path);
            }
        }
    }
    directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for directory in directories {
        match fs::remove_dir(directory) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
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

fn delete_recovery_refs(project_path: &Path, prefix: &str) -> Result<()> {
    for suffix in ["head", "index", "working"] {
        git(
            project_path,
            ["update-ref", "-d", &format!("{prefix}/{suffix}")],
            RunOptions::default(),
        )?;
    }
    Ok(())
}

fn remove_transaction_artifacts(transaction: &Transaction) -> Result<()> {
    delete_recovery_refs(&transaction.project_path, &transaction.recovery_ref)?;
    match fs::remove_file(&transaction.path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn persist_debug_transaction(transaction: &Transaction) -> Result<()> {
    write_json(&transaction.path, transaction, 0o600)?;
    prune_debug_history(
        transaction.path.parent().unwrap_or_else(|| Path::new(".")),
        Some(&transaction.id),
    )
}

fn prune_debug_history(root: &Path, protected_id: Option<&str>) -> Result<()> {
    let Ok(entries) = fs::read_dir(root) else {
        return Ok(());
    };
    let mut history = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        let Ok(transaction) = serde_json::from_slice::<Transaction>(&fs::read(&path)?) else {
            continue;
        };
        if matches!(transaction.phase.as_str(), "INSTALLING" | "CONFLICTED")
            || protected_id == Some(transaction.id.as_str())
        {
            continue;
        }
        history.push((
            transaction.created_at.clone(),
            fs::metadata(&path)?.len(),
            transaction,
        ));
    }
    history.sort_by(|left, right| right.0.cmp(&left.0));

    let protected_count = usize::from(protected_id.is_some());
    let mut retained_count = protected_count;
    let mut retained_bytes = 0_u64;
    for (_, bytes, transaction) in history {
        if retained_count < DEBUG_HISTORY_MAX_RECORDS
            && retained_bytes.saturating_add(bytes) <= DEBUG_HISTORY_MAX_BYTES
        {
            retained_count += 1;
            retained_bytes += bytes;
        } else {
            remove_transaction_artifacts(&transaction)?;
        }
    }
    Ok(())
}

pub fn clear_resolved_conflicts(
    root: &Path,
    project_path: &Path,
    retention: TransactionRetention,
) -> Result<()> {
    if retention.debug_history {
        return prune_debug_history(root, None);
    }
    let Ok(entries) = fs::read_dir(root) else {
        return Ok(());
    };
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        let transaction: Transaction = serde_json::from_slice(&fs::read(path)?)?;
        if transaction.project_path == project_path && transaction.phase == "CONFLICTED" {
            remove_transaction_artifacts(&transaction)?;
        }
    }
    Ok(())
}

fn install_candidate(
    project_path: &Path,
    transaction: &mut Transaction,
    candidate: &Candidate,
    retention: TransactionRetention,
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
    let collisions = find_untracked_collisions(project_path, &tracked)?;
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
    if retention.debug_history {
        persist_debug_transaction(transaction)?;
    } else {
        remove_transaction_artifacts(transaction)?;
    }
    Ok(None)
}

pub fn reconcile_workspace(
    project_path: &Path,
    previous_remote_oid: &str,
    target_remote_oid: &str,
    transaction_root: Option<&Path>,
) -> Result<ReconcileResult> {
    reconcile_workspace_with_retention(
        project_path,
        previous_remote_oid,
        target_remote_oid,
        transaction_root,
        TransactionRetention::default(),
    )
}

pub fn reconcile_workspace_with_retention(
    project_path: &Path,
    previous_remote_oid: &str,
    target_remote_oid: &str,
    transaction_root: Option<&Path>,
    retention: TransactionRetention,
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
    if retention.debug_history {
        persist_debug_transaction(&transaction)?;
    }

    let temporary = tempdir()?;
    let temporary_path = temporary.path();
    let branch = format!("resync-candidate-{id}");
    let outcome = (|| -> Result<ReconcileResult> {
        git(
            project_path,
            [
                "-c",
                "core.hooksPath=/dev/null",
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
            ["-c", "core.hooksPath=/dev/null", "switch", "-c", &branch],
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
                "-c",
                "core.hooksPath=/dev/null",
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
            git(
                temporary_path,
                ["-c", "core.hooksPath=/dev/null", "rebase", "--abort"],
                abort,
            )?;
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
        if retention.debug_history {
            persist_debug_transaction(&transaction)?;
        }
        if let Some(conflict) =
            install_candidate(project_path, &mut transaction, &candidate, retention)?
        {
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
    if outcome.is_err() && !retention.debug_history {
        let _ = remove_transaction_artifacts(&transaction);
    }
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
    recover_transaction_with_retention(path, TransactionRetention::default())
}

pub fn recover_transaction_with_retention(
    path: &Path,
    retention: TransactionRetention,
) -> Result<Transaction> {
    let mut transaction: Transaction = serde_json::from_slice(&fs::read(path)?)?;
    if matches!(transaction.phase.as_str(), "PREPARING" | "CONFLICTED") {
        return Ok(transaction);
    }
    if matches!(transaction.phase.as_str(), "COMPLETED" | "ROLLED_BACK") {
        finalize_recovered_transaction(&transaction, retention)?;
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
        finalize_recovered_transaction(&transaction, retention)?;
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
        finalize_recovered_transaction(&transaction, retention)?;
        return Ok(transaction);
    }
    bail!(
        "transaction {} needs manual recovery; HEAD matches neither preserved state",
        path.file_name().unwrap_or_default().to_string_lossy()
    )
}

fn finalize_recovered_transaction(
    transaction: &Transaction,
    retention: TransactionRetention,
) -> Result<()> {
    if retention.debug_history {
        persist_debug_transaction(transaction)
    } else {
        remove_transaction_artifacts(transaction)
    }
}
