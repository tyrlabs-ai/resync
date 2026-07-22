mod common;

use common::{fixture, rev};
use resync::daemon::ResyncDaemon;
use resync::process::{RunOptions, git};
use resync::remote::fetch_remote;
use resync::state::{Config, LocalCatalog, LocalProject, StatePaths};
use std::collections::BTreeMap;
use std::fs;
use std::time::Duration;

#[test]
fn idle_reconciliation_applies_only_current_branch() -> anyhow::Result<()> {
    let fixture = fixture()?;
    git(&fixture.seed, ["branch", "feature"], RunOptions::default())?;
    git(
        &fixture.seed,
        ["push", "origin", "feature"],
        RunOptions::default(),
    )?;
    git(&fixture.local, ["fetch", "origin"], RunOptions::default())?;
    git(
        &fixture.local,
        ["switch", "-c", "feature", "--track", "origin/feature"],
        RunOptions::default(),
    )?;
    let initial = rev(&fixture.local, "HEAD")?;
    git(&fixture.seed, ["switch", "feature"], RunOptions::default())?;
    fs::write(fixture.seed.join("file"), "feature\n")?;
    git(
        &fixture.seed,
        ["commit", "-am", "feature"],
        RunOptions::default(),
    )?;
    git(
        &fixture.seed,
        ["push", "origin", "feature"],
        RunOptions::default(),
    )?;
    git(&fixture.seed, ["switch", "main"], RunOptions::default())?;
    fs::write(fixture.seed.join("main"), "main\n")?;
    git(&fixture.seed, ["add", "."], RunOptions::default())?;
    git(
        &fixture.seed,
        ["commit", "-m", "main"],
        RunOptions::default(),
    )?;
    git(
        &fixture.seed,
        ["push", "origin", "main"],
        RunOptions::default(),
    )?;

    let mut project = LocalProject {
        project_id: "prj_test".into(),
        local_path: fixture.local.clone(),
        remote_url: fixture.remote.to_string_lossy().into_owned(),
        remote_name: "origin".into(),
        default_branch: "main".into(),
        server_generation: 1,
        advertised_heads: BTreeMap::new(),
        last_applied_heads: BTreeMap::from([
            ("feature".into(), initial.clone()),
            ("main".into(), initial),
        ]),
        active_branch: Some("feature".into()),
        workspace_generation: 1,
        state: "CURRENT".into(),
        durability: "REMOTELY_DURABLE".into(),
        conflict: None,
        last_error: None,
        service: None,
        checkout_id: None,
        extra: BTreeMap::new(),
    };
    project.advertised_heads = fetch_remote(&project, None)?;
    let main_before = rev(&fixture.local, "refs/heads/main")?;
    let root = fixture.root.path().join("state");
    let paths = StatePaths {
        config: root.join("config.json"),
        credentials: root.join("credentials.json"),
        keys: root.join("keys"),
        catalog: root.join("catalog.json"),
        transactions: root.join("transactions"),
        daemon_lock: root.join("daemon.lock"),
        socket: root.join("daemon.sock"),
        root,
    };
    let daemon = ResyncDaemon::from_parts(
        paths,
        Config::default(),
        LocalCatalog {
            catalog_generation: 1,
            projects: vec![project],
        },
        Duration::from_secs(10),
        Duration::from_secs(60),
    )?;
    let runtime = resync::daemon::runtime()?;
    runtime.block_on(async {
        let result = daemon.reconcile_now("prj_test").await?;
        assert_eq!(result.outcome, "applied");
        let snapshot = daemon.project_snapshot("prj_test").await?;
        assert_eq!(
            rev(&fixture.local, "HEAD")?,
            snapshot.advertised_heads["feature"]
        );
        assert_eq!(rev(&fixture.local, "refs/heads/main")?, main_before);
        anyhow::Ok(())
    })
}
