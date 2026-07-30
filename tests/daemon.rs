mod common;

use common::{advance_remote, fixture, rev};
use resync::daemon::ResyncDaemon;
use resync::process::{RunOptions, git};
use resync::remote::fetch_remote;
use resync::state::{Config, Conflict, LocalCatalog, LocalProject, StatePaths};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;
use std::time::Duration;

#[test]
fn failed_automatic_reconciliation_grants_access_until_explicit_sync() -> anyhow::Result<()> {
    let fixture = fixture()?;
    fs::write(fixture.local.join("file"), "local\n")?;
    let target = advance_remote(&fixture, "remote\n")?;
    let project = LocalProject {
        project_id: "prj_reconciliation_mode".into(),
        local_path: fixture.local.clone(),
        remote_url: fixture.remote.to_string_lossy().into_owned(),
        remote_name: "origin".into(),
        default_branch: "main".into(),
        server_generation: 1,
        advertised_heads: BTreeMap::from([("main".into(), target)]),
        last_applied_heads: BTreeMap::from([("main".into(), fixture.base)]),
        active_branch: Some("main".into()),
        workspace_generation: 1,
        state: "REMOTE_AHEAD".into(),
        durability: "DIRTY_UNCHECKPOINTED".into(),
        conflict: None,
        last_error: None,
        service: None,
        checkout_id: None,
        extra: BTreeMap::new(),
    };
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
        paths.clone(),
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
        for call in ["first", "second"] {
            let access = daemon
                .handle(serde_json::json!({
                    "method": "acquireAccess",
                    "project_id": "prj_reconciliation_mode",
                    "session_id": "session",
                    "call_id": call
                }))
                .await?;
            assert_eq!(access["granted"], true);
            assert_eq!(access["reconciliation_mode"], true);
            assert_eq!(access["lease_id"], serde_json::Value::Null);
            assert_eq!(fs::read_to_string(fixture.local.join("file"))?, "local\n");
        }
        assert_eq!(fs::read_dir(&paths.transactions)?.count(), 1);

        git(
            &fixture.local,
            ["reset", "--hard", "origin/main"],
            RunOptions::default(),
        )?;
        let result = daemon
            .handle(serde_json::json!({
                "method": "sync",
                "project_id": "prj_reconciliation_mode"
            }))
            .await?;
        assert_eq!(result["outcome"], "current");
        let state = daemon
            .handle(serde_json::json!({
                "method": "accessState",
                "project_id": "prj_reconciliation_mode"
            }))
            .await?;
        assert_eq!(state["reconciliation_mode"], false);
        anyhow::Ok(())
    })
}

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

#[test]
fn conflicted_access_is_unleased_and_sync_clears_stale_conflict() -> anyhow::Result<()> {
    let fixture = fixture()?;
    let head = rev(&fixture.local, "HEAD")?;
    let project = LocalProject {
        project_id: "prj_stale_conflict".into(),
        local_path: fixture.local.clone(),
        remote_url: fixture.remote.to_string_lossy().into_owned(),
        remote_name: "origin".into(),
        default_branch: "main".into(),
        server_generation: 1,
        advertised_heads: BTreeMap::from([("main".into(), head.clone())]),
        last_applied_heads: BTreeMap::new(),
        active_branch: Some("main".into()),
        workspace_generation: 1,
        state: "CONFLICTED".into(),
        durability: "REMOTELY_DURABLE".into(),
        conflict: Some(Conflict {
            kind: "reconciliation".into(),
            detail: "obsolete conflict".into(),
            paths: Vec::new(),
        }),
        last_error: None,
        service: None,
        checkout_id: None,
        extra: BTreeMap::new(),
    };
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
        let access = daemon
            .handle(serde_json::json!({
                "method": "acquireAccess",
                "project_id": "prj_stale_conflict",
                "session_id": "session",
                "call_id": "call"
            }))
            .await?;
        assert_eq!(access["granted"], true);
        assert_eq!(access["reconciliation_mode"], true);
        assert_eq!(access["lease_id"], serde_json::Value::Null);
        assert_eq!(
            daemon.project_snapshot("prj_stale_conflict").await?.state,
            "CONFLICTED"
        );
        assert_eq!(
            daemon
                .handle(serde_json::json!({
                    "method": "sync",
                    "project_id": "prj_stale_conflict"
                }))
                .await?["outcome"],
            "current"
        );
        let snapshot = daemon.project_snapshot("prj_stale_conflict").await?;
        assert_eq!(snapshot.state, "CURRENT");
        assert_eq!(snapshot.conflict, None);
        anyhow::Ok(())
    })
}

#[test]
fn missing_provider_membership_becomes_access_lost() -> anyhow::Result<()> {
    let fixture = fixture()?;
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let address = listener.local_addr()?;
    let server = thread::spawn(move || -> anyhow::Result<()> {
        let (mut stream, _) = listener.accept()?;
        let mut request = [0_u8; 8192];
        let _ = stream.read(&mut request)?;
        let body = serde_json::to_string(&serde_json::json!({
            "protocol": "resync.v1",
            "projects": [{
                "project_id": "prj_access_lost",
                "checkout_id": "chk_access_lost",
                "active_branch": "main",
                "membership": "ACCESS_LOST"
            }]
        }))?;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        )?;
        Ok(())
    });
    let project = LocalProject {
        project_id: "prj_access_lost".into(),
        local_path: fixture.local.clone(),
        remote_url: fixture.remote.to_string_lossy().into_owned(),
        remote_name: "origin".into(),
        default_branch: "main".into(),
        server_generation: 1,
        advertised_heads: BTreeMap::from([("main".into(), fixture.base.clone())]),
        last_applied_heads: BTreeMap::from([("main".into(), fixture.base)]),
        active_branch: Some("main".into()),
        workspace_generation: 1,
        state: "CURRENT".into(),
        durability: "REMOTELY_DURABLE".into(),
        conflict: None,
        last_error: None,
        service: Some(format!("http://{address}")),
        checkout_id: Some("chk_access_lost".into()),
        extra: BTreeMap::new(),
    };
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
        Config {
            server: Some(format!("http://{address}")),
            token: Some("test-token".into()),
            ..Config::default()
        },
        LocalCatalog {
            catalog_generation: 1,
            projects: vec![project],
        },
        Duration::from_secs(10),
        Duration::from_secs(60),
    )?;
    let runtime = resync::daemon::runtime()?;
    runtime.block_on(async {
        daemon.refresh().await?;
        let snapshot = daemon.project_snapshot("prj_access_lost").await?;
        assert_eq!(snapshot.state, "ACCESS_LOST");
        assert!(
            snapshot
                .last_error
                .as_deref()
                .unwrap()
                .contains("no longer authorizes")
        );
        let access = daemon
            .handle(serde_json::json!({
                "method": "acquireAccess",
                "project_id": "prj_access_lost",
                "session_id": "session",
                "call_id": "call"
            }))
            .await?;
        assert_eq!(access["granted"], true);
        assert_eq!(access["reconciliation_mode"], false);
        daemon
            .handle(serde_json::json!({
                "method": "releaseAccess",
                "lease_id": access["lease_id"]
            }))
            .await?;
        anyhow::Ok(())
    })?;
    server.join().unwrap()?;
    Ok(())
}
