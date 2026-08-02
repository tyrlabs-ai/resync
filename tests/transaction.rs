mod common;

use common::{advance_remote, fixture, rev};
use resync::git_state::capture_workspace;
use resync::process::{RunOptions, git};
use resync::transaction::{
    TransactionRetention, gc_transactions, reconcile_workspace, reconcile_workspace_with_retention,
    recover_transaction,
};
use std::collections::BTreeSet;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[test]
fn clean_reconciliation_preserves_staged_and_unstaged_layers() -> anyhow::Result<()> {
    let fixture = fixture()?;
    let transactions = fixture.root.path().join("transactions");
    fs::write(fixture.local.join("local"), "staged\n")?;
    git(&fixture.local, ["add", "local"], RunOptions::default())?;
    fs::write(fixture.local.join("local"), "unstaged\n")?;
    let target = advance_remote(&fixture, "remote\n")?;
    let result = reconcile_workspace(&fixture.local, &fixture.base, &target, Some(&transactions))?;
    assert_eq!(result.outcome, "applied");
    assert_eq!(fs::read_to_string(fixture.local.join("file"))?, "remote\n");
    assert_eq!(
        fs::read_to_string(fixture.local.join("local"))?,
        "unstaged\n"
    );
    let staged = git(&fixture.local, ["show", ":local"], RunOptions::default())?.stdout;
    assert_eq!(staged, "staged\n");
    assert_eq!(fs::read_dir(transactions)?.count(), 0);
    Ok(())
}

#[cfg(unix)]
#[test]
fn internal_candidate_checkout_does_not_run_project_hooks() -> anyhow::Result<()> {
    let fixture = fixture()?;
    let marker = fixture.root.path().join("post-checkout-ran");
    let hook = fixture.local.join(".git/hooks/post-checkout");
    fs::write(
        &hook,
        format!("#!/bin/sh\nprintf ran > \"{}\"\n", marker.display()),
    )?;
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755))?;

    let target = advance_remote(&fixture, "remote\n")?;
    let result = reconcile_workspace(
        &fixture.local,
        &fixture.base,
        &target,
        Some(&fixture.root.path().join("transactions")),
    )?;

    assert_eq!(result.outcome, "applied");
    assert!(
        !marker.exists(),
        "RepoSync's internal candidate checkout invoked the project's post-checkout hook"
    );
    Ok(())
}

#[test]
fn git_conflict_leaves_workspace_unchanged() -> anyhow::Result<()> {
    let fixture = fixture()?;
    fs::write(fixture.local.join("file"), "local\n")?;
    git(
        &fixture.local,
        ["commit", "-am", "local"],
        RunOptions::default(),
    )?;
    fs::write(fixture.local.join("untracked"), "keep\n")?;
    let before = capture_workspace(&fixture.local)?;
    let target = advance_remote(&fixture, "remote\n")?;
    let result = reconcile_workspace(
        &fixture.local,
        &fixture.base,
        &target,
        Some(&fixture.root.path().join("transactions")),
    )?;
    assert_eq!(result.outcome, "conflict");
    assert_eq!(capture_workspace(&fixture.local)?, before);
    assert_eq!(
        fs::read_to_string(fixture.local.join("untracked"))?,
        "keep\n"
    );
    Ok(())
}

#[test]
fn incoming_tracked_path_cannot_overwrite_untracked_file() -> anyhow::Result<()> {
    let fixture = fixture()?;
    let transactions = fixture.root.path().join("transactions");
    let ignored = fixture.local.join("ignored-build");
    fs::create_dir(&ignored)?;
    fs::write(fixture.local.join(".git/info/exclude"), "ignored-build/\n")?;
    for index in 0..2_000 {
        fs::write(
            ignored.join(format!("artifact-{index:04}.o")),
            "generated\n",
        )?;
    }
    fs::write(fixture.local.join("local-only"), "local contents\n")?;
    fs::write(fixture.seed.join("local-only"), "remote tracked\n")?;
    git(&fixture.seed, ["add", "local-only"], RunOptions::default())?;
    git(
        &fixture.seed,
        ["commit", "-m", "track colliding path"],
        RunOptions::default(),
    )?;
    git(
        &fixture.seed,
        ["push", "origin", "main"],
        RunOptions::default(),
    )?;
    let target = rev(&fixture.seed, "HEAD")?;
    git(&fixture.local, ["fetch", "origin"], RunOptions::default())?;
    let result = reconcile_workspace(&fixture.local, &fixture.base, &target, Some(&transactions))?;
    assert_eq!(result.outcome, "conflict");
    assert_eq!(result.paths, vec!["local-only"]);
    assert_eq!(
        fs::read_to_string(fixture.local.join("local-only"))?,
        "local contents\n"
    );
    let transaction_path = fs::read_dir(transactions)?.next().unwrap()?.path();
    let transaction = fs::read_to_string(&transaction_path)?;
    assert!(
        transaction.len() < 16 * 1024,
        "transaction record was {} bytes",
        transaction.len()
    );
    assert!(!transaction.contains("\"untracked\""));
    Ok(())
}

#[test]
fn rewritten_remote_history_is_rejected_before_mutation() -> anyhow::Result<()> {
    let fixture = fixture()?;
    fs::write(fixture.local.join("local"), "dirty\n")?;
    let before = capture_workspace(&fixture.local)?;
    git(
        &fixture.seed,
        ["checkout", "--orphan", "rewritten"],
        RunOptions::default(),
    )?;
    git(&fixture.seed, ["rm", "-rf", "."], RunOptions::default())?;
    fs::write(fixture.seed.join("other"), "other\n")?;
    git(&fixture.seed, ["add", "."], RunOptions::default())?;
    git(
        &fixture.seed,
        ["commit", "-m", "rewrite"],
        RunOptions::default(),
    )?;
    let target = rev(&fixture.seed, "HEAD")?;
    let error = reconcile_workspace(
        &fixture.local,
        &fixture.base,
        &target,
        Some(&fixture.root.path().join("transactions")),
    )
    .unwrap_err();
    assert!(error.to_string().contains("remote branch was rewritten"));
    assert_eq!(capture_workspace(&fixture.local)?, before);
    Ok(())
}

#[test]
fn debug_transaction_history_is_bounded_to_128_records() -> anyhow::Result<()> {
    let fixture = fixture()?;
    let transactions = fixture.root.path().join("transactions");
    let mut previous = fixture.base.clone();

    for index in 0..130 {
        let target = advance_remote(&fixture, &format!("remote {index}\n"))?;
        let result = reconcile_workspace_with_retention(
            &fixture.local,
            &previous,
            &target,
            Some(&transactions),
            TransactionRetention {
                debug_history: true,
            },
        )?;
        assert_eq!(result.outcome, "applied");
        previous = target;
    }

    assert_eq!(fs::read_dir(transactions)?.count(), 128);
    Ok(())
}

#[test]
fn transaction_gc_compacts_retained_legacy_snapshots() -> anyhow::Result<()> {
    let fixture = fixture()?;
    let transactions = fixture.root.path().join("transactions");
    let target = advance_remote(&fixture, "remote\n")?;
    reconcile_workspace_with_retention(
        &fixture.local,
        &fixture.base,
        &target,
        Some(&transactions),
        TransactionRetention {
            debug_history: true,
        },
    )?;

    let path = fs::read_dir(&transactions)?.next().unwrap()?.path();
    let mut legacy: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
    legacy["snapshot"]["untracked"] = serde_json::json!(
        (0..2_000)
            .map(|index| format!("ignored-build/artifact-{index:04}.o"))
            .collect::<Vec<_>>()
    );
    fs::write(&path, serde_json::to_vec_pretty(&legacy)?)?;
    let bloated_size = fs::metadata(&path)?.len();
    git(
        &fixture.local,
        ["update-ref", "refs/resync/recovery/orphan/head", "HEAD"],
        RunOptions::default(),
    )?;
    let managed_projects = BTreeSet::from([fixture.local.clone()]);

    let report = gc_transactions(
        &transactions,
        &managed_projects,
        &BTreeSet::new(),
        TransactionRetention {
            debug_history: true,
        },
        false,
    )?;
    assert_eq!(report.scanned, 1);
    assert_eq!(report.retained, 1);
    assert_eq!(report.compacted, 1);
    assert_eq!(report.recovery_refs_prunable, 1);
    assert_eq!(report.recovery_refs_removed, 1);
    assert!(fs::metadata(&path)?.len() < bloated_size / 10);
    assert!(!fs::read_to_string(path)?.contains("\"untracked\""));
    let refs = git(
        &fixture.local,
        [
            "for-each-ref",
            "--format=%(refname)",
            "refs/resync/recovery",
        ],
        RunOptions::default(),
    )?
    .stdout;
    assert!(!refs.contains("orphan"));
    assert_eq!(refs.lines().count(), 3);
    Ok(())
}

#[test]
fn recovering_a_terminal_transaction_removes_ordinary_history() -> anyhow::Result<()> {
    let fixture = fixture()?;
    let transactions = fixture.root.path().join("transactions");
    let target = advance_remote(&fixture, "remote\n")?;
    reconcile_workspace_with_retention(
        &fixture.local,
        &fixture.base,
        &target,
        Some(&transactions),
        TransactionRetention {
            debug_history: true,
        },
    )?;
    let path = fs::read_dir(&transactions)?.next().unwrap()?.path();

    let recovered = recover_transaction(&path)?;
    assert_eq!(recovered.phase, "COMPLETED");
    assert!(!path.exists());
    Ok(())
}
