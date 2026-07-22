mod common;

use common::{advance_remote, fixture, rev};
use resync::git_state::capture_workspace;
use resync::process::{RunOptions, git};
use resync::transaction::reconcile_workspace;
use std::fs;

#[test]
fn clean_reconciliation_preserves_staged_and_unstaged_layers() -> anyhow::Result<()> {
    let fixture = fixture()?;
    fs::write(fixture.local.join("local"), "staged\n")?;
    git(&fixture.local, ["add", "local"], RunOptions::default())?;
    fs::write(fixture.local.join("local"), "unstaged\n")?;
    let target = advance_remote(&fixture, "remote\n")?;
    let result = reconcile_workspace(
        &fixture.local,
        &fixture.base,
        &target,
        Some(&fixture.root.path().join("transactions")),
    )?;
    assert_eq!(result.outcome, "applied");
    assert_eq!(fs::read_to_string(fixture.local.join("file"))?, "remote\n");
    assert_eq!(
        fs::read_to_string(fixture.local.join("local"))?,
        "unstaged\n"
    );
    let staged = git(&fixture.local, ["show", ":local"], RunOptions::default())?.stdout;
    assert_eq!(staged, "staged\n");
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
    fs::write(fixture.local.join("secret"), "local secret\n")?;
    fs::write(fixture.seed.join("secret"), "remote tracked\n")?;
    git(&fixture.seed, ["add", "secret"], RunOptions::default())?;
    git(
        &fixture.seed,
        ["commit", "-m", "track secret"],
        RunOptions::default(),
    )?;
    git(
        &fixture.seed,
        ["push", "origin", "main"],
        RunOptions::default(),
    )?;
    let target = rev(&fixture.seed, "HEAD")?;
    git(&fixture.local, ["fetch", "origin"], RunOptions::default())?;
    let result = reconcile_workspace(
        &fixture.local,
        &fixture.base,
        &target,
        Some(&fixture.root.path().join("transactions")),
    )?;
    assert_eq!(result.outcome, "conflict");
    assert_eq!(result.paths, vec!["secret"]);
    assert_eq!(
        fs::read_to_string(fixture.local.join("secret"))?,
        "local secret\n"
    );
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
