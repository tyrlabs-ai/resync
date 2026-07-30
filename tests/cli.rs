use assert_cmd::Command;
use predicates::prelude::*;
use resync::state::{LocalCatalog, LocalProject, write_json};
use std::collections::BTreeMap;
use std::fs;

fn local_project(project_id: &str, local_path: std::path::PathBuf) -> LocalProject {
    LocalProject {
        project_id: project_id.into(),
        local_path,
        remote_url: "https://example.invalid/project.git".into(),
        remote_name: "resync".into(),
        default_branch: "main".into(),
        server_generation: 1,
        advertised_heads: BTreeMap::new(),
        last_applied_heads: BTreeMap::new(),
        active_branch: Some("main".into()),
        workspace_generation: 1,
        state: "CURRENT".into(),
        durability: "REMOTELY_DURABLE".into(),
        conflict: None,
        last_error: None,
        service: None,
        checkout_id: None,
        extra: BTreeMap::new(),
    }
}

#[test]
fn version_and_help_are_available() {
    Command::cargo_bin("resync")
        .unwrap()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("protocol resync.v1"));
    for arguments in [
        vec!["--help"],
        vec!["auth", "--help"],
        vec!["auth", "login", "--help"],
        vec!["project", "--help"],
        vec!["project", "join", "--help"],
        vec!["device", "--help"],
        vec!["peer", "sync", "--help"],
        vec!["daemon", "--help"],
    ] {
        Command::cargo_bin("resync")
            .unwrap()
            .args(arguments)
            .assert()
            .success()
            .stdout(predicate::str::contains("Usage:"));
    }
}

#[test]
fn npm_install_runs_catalog_wide_adapter_repair() -> anyhow::Result<()> {
    let manifest: serde_json::Value = serde_json::from_str(include_str!("../package.json"))?;
    assert_eq!(
        manifest["scripts"]["postinstall"],
        "node ./bin/resync.js install-adapters"
    );
    Ok(())
}

#[test]
fn install_adapters_without_project_repairs_every_local_checkout() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let state = root.path().join("state");
    let first = root.path().join("first");
    let second = root.path().join("second");
    for repository in [&first, &second] {
        let status = std::process::Command::new("git")
            .args(["init", "--quiet", "--initial-branch=main"])
            .arg(repository)
            .status()?;
        anyhow::ensure!(status.success(), "could not initialize test repository");
    }
    write_json(
        &state.join("catalog.json"),
        &LocalCatalog {
            catalog_generation: 1,
            projects: vec![
                local_project("prj_first", first.clone()),
                local_project("prj_second", second.clone()),
            ],
        },
        0o600,
    )?;

    let assertion = Command::cargo_bin("resync")
        .unwrap()
        .env("RESYNC_STATE_DIR", &state)
        .arg("install-adapters")
        .assert()
        .success();
    let output: serde_json::Value = serde_json::from_slice(&assertion.get_output().stdout)?;
    assert_eq!(output["projectCount"], 2);
    assert_eq!(output["projects"].as_array().map(Vec::len), Some(2));
    assert_eq!(output["daemonSupervision"], "state-dir");

    for (repository, project_id) in [(&first, "prj_first"), (&second, "prj_second")] {
        let hooks = fs::read_to_string(repository.join(".codex/hooks.json"))?;
        let hook_configuration: serde_json::Value = serde_json::from_str(&hooks)?;
        for event in ["PreToolUse", "PostToolUse"] {
            let command = hook_configuration["hooks"][event][0]["hooks"][0]["command"]
                .as_str()
                .expect("generated RepoSync hook command");
            assert_ne!(command, format!("resync codex-hook '{project_id}'"));
            assert!(command.ends_with(&format!(" codex-hook '{project_id}'")));
        }
        assert!(
            repository
                .join(".agents/skills/reposync/SKILL.md")
                .is_file()
        );
        assert!(
            fs::read_to_string(repository.join(".git/hooks/post-commit"))?
                .contains(&format!("report-commit '{project_id}'"))
        );
        assert!(
            fs::read_to_string(repository.join(".git/hooks/post-checkout"))?
                .contains(&format!("report-checkout '{project_id}'"))
        );
    }
    Ok(())
}
