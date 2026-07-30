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
fn npm_install_has_no_lifecycle_scripts() -> anyhow::Result<()> {
    let manifest: serde_json::Value = serde_json::from_str(include_str!("../package.json"))?;
    assert!(manifest.get("scripts").is_none());
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
    write_json(
        &first.join(".codex/hooks.json"),
        &serde_json::json!({
            "hooks": {
                "PreToolUse": [
                    { "matcher": "*", "hooks": [{
                        "type": "command",
                        "command": "'/obsolete/resync' codex-hook 'prj_first'"
                    }] },
                    { "matcher": "Bash", "hooks": [{
                        "type": "command",
                        "command": "echo unrelated"
                    }] }
                ],
                "PostToolUse": [{ "matcher": "*", "hooks": [{
                    "type": "command",
                    "command": "'/obsolete/resync' codex-hook 'prj_first'"
                }] }]
            }
        }),
        0o644,
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
            let groups = hook_configuration["hooks"][event]
                .as_array()
                .expect("hook groups");
            let generated = groups
                .iter()
                .flat_map(|group| group["hooks"].as_array().into_iter().flatten())
                .filter_map(|hook| hook["command"].as_str())
                .filter(|command| command.ends_with(&format!(" codex-hook '{project_id}'")))
                .collect::<Vec<_>>();
            assert_eq!(generated.len(), 1, "one current RepoSync hook per event");
            let command = generated[0];
            assert_ne!(command, format!("resync codex-hook '{project_id}'"));
            assert!(command.ends_with(&format!(" codex-hook '{project_id}'")));
            assert!(!command.contains("/obsolete/resync"));
        }
        if project_id == "prj_first" {
            assert_eq!(
                hook_configuration["hooks"]["PreToolUse"]
                    .as_array()
                    .map(Vec::len),
                Some(2),
                "unrelated hooks are preserved while the stale RepoSync hook is replaced"
            );
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

#[test]
fn workspace_install_hooks_writes_one_parent_dispatcher_for_explicit_projects() -> anyhow::Result<()>
{
    let root = tempfile::tempdir()?;
    let state = root.path().join("state");
    let workspace = root.path().join("workspace");
    let first = workspace.join("resync");
    let second = workspace.join("resync-hosted");
    fs::create_dir_all(&first)?;
    fs::create_dir_all(&second)?;
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
    fs::write(
        workspace.join(".resync-workspace.toml"),
        r#"version = 1

[[project]]
path = "resync"
project_id = "prj_first"

[[project]]
path = "resync-hosted"
project_id = "prj_second"
"#,
    )?;
    write_json(
        &workspace.join(".codex/hooks.json"),
        &serde_json::json!({
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [{ "type": "command", "command": "echo preserved" }]
                }]
            }
        }),
        0o644,
    )?;

    let assertion = Command::cargo_bin("resync")
        .unwrap()
        .env("RESYNC_STATE_DIR", &state)
        .args(["workspace", "install-hooks"])
        .arg(&workspace)
        .assert()
        .success();
    let output: serde_json::Value = serde_json::from_slice(&assertion.get_output().stdout)?;
    assert_eq!(output["projectCount"], 2);
    assert_eq!(output["requiresCodexTrustReview"], true);

    let hooks: serde_json::Value =
        serde_json::from_slice(&fs::read(workspace.join(".codex/hooks.json"))?)?;
    for event in ["PreToolUse", "PostToolUse"] {
        let commands = hooks["hooks"][event]
            .as_array()
            .expect("hook groups")
            .iter()
            .flat_map(|group| group["hooks"].as_array().into_iter().flatten())
            .filter_map(|hook| hook["command"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            commands
                .iter()
                .filter(|command| command.contains(" codex-workspace-hook "))
                .count(),
            1,
            "one umbrella RepoSync dispatcher per event"
        );
        assert!(commands.iter().any(|command| {
            command.contains(
                workspace
                    .join(".resync-workspace.toml")
                    .to_string_lossy()
                    .as_ref(),
            )
        }));
    }
    assert_eq!(
        hooks["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
        "echo preserved"
    );
    Ok(())
}

#[test]
fn workspace_install_hooks_rejects_projects_outside_the_workspace_root() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let state = root.path().join("state");
    let workspace = root.path().join("workspace");
    let outside = root.path().join("outside");
    fs::create_dir_all(&workspace)?;
    fs::create_dir_all(&outside)?;
    write_json(
        &state.join("catalog.json"),
        &LocalCatalog {
            catalog_generation: 1,
            projects: vec![local_project("prj_outside", outside)],
        },
        0o600,
    )?;
    fs::write(
        workspace.join(".resync-workspace.toml"),
        r#"version = 1

[[project]]
path = "../outside"
project_id = "prj_outside"
"#,
    )?;

    Command::cargo_bin("resync")
        .unwrap()
        .env("RESYNC_STATE_DIR", &state)
        .args(["workspace", "install-hooks"])
        .arg(&workspace)
        .assert()
        .failure()
        .stderr(predicate::str::contains("escapes the workspace root"));
    assert!(!workspace.join(".codex/hooks.json").exists());
    Ok(())
}
