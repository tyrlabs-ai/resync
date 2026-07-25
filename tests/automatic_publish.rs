mod common;

use common::{fixture, rev};
use resync::process::{RunOptions, git};
use resync::state::{LocalCatalog, LocalProject, StatePaths, write_json};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::Duration;

struct DaemonGuard {
    lock: std::path::PathBuf,
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        if let Ok(pid) = fs::read_to_string(&self.lock)
            && let Ok(pid) = pid.trim().parse::<i32>()
        {
            // SAFETY: the PID comes from the isolated daemon lock created by this test.
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
    }
}

fn command(
    binary: &Path,
    directory: &Path,
    state: &Path,
    arguments: &[&str],
) -> anyhow::Result<()> {
    let output = Command::new(binary)
        .args(arguments)
        .current_dir(directory)
        .env("RESYNC_STATE_DIR", state)
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "resync failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

#[test]
fn ordinary_commit_starts_daemon_and_publishes() -> anyhow::Result<()> {
    let fixture = fixture()?;
    let state = fixture.root.path().join("state");
    let paths = StatePaths {
        config: state.join("config.json"),
        credentials: state.join("credentials.json"),
        keys: state.join("keys"),
        catalog: state.join("catalog.json"),
        transactions: state.join("transactions"),
        daemon_lock: state.join("daemon.lock"),
        socket: state.join("daemon.sock"),
        root: state.clone(),
    };
    let project_id = "prj_automatic";
    write_json(
        &paths.catalog,
        &LocalCatalog {
            catalog_generation: 1,
            projects: vec![LocalProject {
                project_id: project_id.into(),
                local_path: fixture.local.clone(),
                remote_url: fixture.remote.to_string_lossy().into_owned(),
                remote_name: "origin".into(),
                default_branch: "main".into(),
                server_generation: 1,
                advertised_heads: BTreeMap::from([("main".into(), fixture.base.clone())]),
                last_applied_heads: BTreeMap::from([("main".into(), fixture.base.clone())]),
                active_branch: Some("main".into()),
                workspace_generation: 1,
                state: "CURRENT".into(),
                durability: "REMOTELY_DURABLE".into(),
                conflict: None,
                last_error: None,
                service: None,
                checkout_id: None,
                extra: BTreeMap::new(),
            }],
        },
        0o600,
    )?;
    let binary = Path::new(env!("CARGO_BIN_EXE_resync"));
    command(
        binary,
        &fixture.local,
        &state,
        &["install-adapters", project_id],
    )?;
    let skill = fs::read_to_string(fixture.local.join(".agents/skills/reposync/SKILL.md"))?;
    assert!(skill.contains("## RepoSync commit cadence"));
    let status = git(
        &fixture.local,
        ["status", "--porcelain"],
        RunOptions::default(),
    )?;
    assert!(
        status.stdout.trim().is_empty(),
        "adapter installation dirtied the checkout:\n{}",
        status.stdout
    );
    let _guard = DaemonGuard {
        lock: paths.daemon_lock.clone(),
    };

    fs::write(fixture.local.join("file"), "committed\n")?;
    let mut environment = BTreeMap::new();
    environment.insert("RESYNC_STATE_DIR".into(), state.as_os_str().to_owned());
    git(
        &fixture.local,
        ["commit", "-am", "publish me"],
        RunOptions {
            env: environment,
            ..RunOptions::default()
        },
    )?;
    let expected = rev(&fixture.local, "HEAD")?;
    for _ in 0..100 {
        let result = git(
            &fixture.remote,
            ["rev-parse", "refs/heads/main"],
            RunOptions {
                allow_failure: true,
                ..RunOptions::default()
            },
        )?;
        if result.stdout.trim() == expected {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }
    let log = fs::read_to_string(state.join("daemon.log")).unwrap_or_default();
    anyhow::bail!("automatic publication timed out; daemon log: {log}")
}
