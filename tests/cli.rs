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
fn transaction_debug_mode_is_explicit_and_gc_supports_dry_run() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let state = root.path().join("state");

    let status = Command::cargo_bin("resync")
        .unwrap()
        .env("RESYNC_STATE_DIR", &state)
        .args(["transaction-debug", "status"])
        .assert()
        .success();
    let output: serde_json::Value = serde_json::from_slice(&status.get_output().stdout)?;
    assert_eq!(output["enabled"], false);

    Command::cargo_bin("resync")
        .unwrap()
        .env("RESYNC_STATE_DIR", &state)
        .args(["transaction-debug", "enable"])
        .assert()
        .success();
    assert!(state.join("transaction-debug").is_file());

    let dry_run = Command::cargo_bin("resync")
        .unwrap()
        .env("RESYNC_STATE_DIR", &state)
        .args(["gc", "--dry-run"])
        .assert()
        .success();
    let output: serde_json::Value = serde_json::from_slice(&dry_run.get_output().stdout)?;
    assert_eq!(output["dryRun"], true);
    assert_eq!(output["debugHistory"], true);
    assert_eq!(output["scanned"], 0);

    Command::cargo_bin("resync")
        .unwrap()
        .env("RESYNC_STATE_DIR", &state)
        .args(["transaction-debug", "disable"])
        .assert()
        .success();
    assert!(!state.join("transaction-debug").exists());
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
                .filter(|command| command.contains("hook-dispatchers/codex-"))
                .collect::<Vec<_>>();
            assert_eq!(generated.len(), 1, "one current RepoSync hook per event");
            let command = generated[0];
            assert!(!command.contains(" codex-hook "));
            assert!(!command.contains("/obsolete/resync"));
            let dispatcher = std::path::PathBuf::from(command.trim_matches('\''));
            let script = fs::read_to_string(&dispatcher)?;
            assert!(script.contains("# RepoSync Codex hook dispatcher v1"));
            assert!(script.contains(&format!("codex-hook '{project_id}'")));
            assert!(dispatcher.with_extension("resync").exists());
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
fn adapter_upgrade_migrates_versioned_hooks_to_a_stable_dispatcher() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let state = root.path().join("state");
    let repository = root.path().join("project");
    let status = std::process::Command::new("git")
        .args(["init", "--quiet", "--initial-branch=main"])
        .arg(&repository)
        .status()?;
    anyhow::ensure!(status.success(), "could not initialize test repository");
    let project_id = "prj_upgrade";
    write_json(
        &state.join("catalog.json"),
        &LocalCatalog {
            catalog_generation: 1,
            projects: vec![local_project(project_id, repository.clone())],
        },
        0o600,
    )?;
    write_json(
        &repository.join(".codex/hooks.json"),
        &serde_json::json!({
            "hooks": {
                "PreToolUse": [{
                    "matcher": "*",
                    "hooks": [
                        {
                            "type": "command",
                            "command": "'/old/npm/resync' codex-hook 'prj_upgrade'"
                        },
                        {
                            "type": "command",
                            "command": "echo preserved"
                        }
                    ]
                }],
                "PostToolUse": [{
                    "matcher": "*",
                    "hooks": [{
                        "type": "command",
                        "command": "'/old/npm/resync' codex-hook 'prj_upgrade'"
                    }]
                }]
            }
        }),
        0o644,
    )?;

    let install = || -> anyhow::Result<serde_json::Value> {
        let assertion = Command::cargo_bin("resync")
            .unwrap()
            .env("RESYNC_STATE_DIR", &state)
            .args(["install-adapters", project_id])
            .assert()
            .success();
        Ok(serde_json::from_slice(&assertion.get_output().stdout)?)
    };
    let first_install = install()?;
    assert_eq!(first_install["requiresCodexTrustReview"], true);

    let hooks_path = repository.join(".codex/hooks.json");
    let hooks: serde_json::Value = serde_json::from_slice(&fs::read(&hooks_path)?)?;
    let mut dispatcher = None;
    for event in ["PreToolUse", "PostToolUse"] {
        let commands = hooks["hooks"][event]
            .as_array()
            .expect("generated hook groups")
            .iter()
            .flat_map(|group| group["hooks"].as_array().into_iter().flatten())
            .filter_map(|hook| hook["command"].as_str())
            .collect::<Vec<_>>();
        let generated = commands
            .iter()
            .filter(|command| command.contains("hook-dispatchers/codex-"))
            .collect::<Vec<_>>();
        assert_eq!(generated.len(), 1, "one stable dispatcher per event");
        assert!(
            commands
                .iter()
                .all(|command| !command.contains("/old/npm/resync")),
            "old versioned hook was not removed"
        );
        let path = std::path::PathBuf::from(generated[0].trim_matches('\''));
        assert!(
            path.is_file(),
            "dispatcher does not exist: {}",
            path.display()
        );
        assert_eq!(dispatcher.get_or_insert(path.clone()), &path);
    }
    assert!(
        hooks["hooks"]["PreToolUse"]
            .as_array()
            .expect("pre-tool groups")
            .iter()
            .flat_map(|group| group["hooks"].as_array().into_iter().flatten())
            .any(|hook| hook["command"] == "echo preserved"),
        "an unrelated handler in the migrated group was removed"
    );
    let dispatcher = dispatcher.expect("stable hook dispatcher");
    let script = fs::read_to_string(&dispatcher)?;
    assert!(script.contains("# RepoSync Codex hook dispatcher v1"));
    assert!(script.contains("codex-hook 'prj_upgrade'"));
    assert!(dispatcher.with_extension("resync").exists());

    let approved_definition = fs::read(&hooks_path)?;
    let reinstall = install()?;
    assert_eq!(reinstall["requiresCodexTrustReview"], false);
    assert_eq!(
        fs::read(&hooks_path)?,
        approved_definition,
        "reinstalling RepoSync changed the approved hook definition"
    );
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
                local_project("prj_stale", root.path().join("deleted-checkout")),
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
                    "hooks": [
                        { "type": "command", "command": "echo preserved" },
                        {
                            "type": "command",
                            "command": format!(
                                "'/old/npm/resync' codex-workspace-hook '{}'",
                                workspace.join(".resync-workspace.toml").display()
                            )
                        }
                    ]
                }],
                "PostToolUse": [{
                    "matcher": "*",
                    "hooks": [{
                        "type": "command",
                        "command": format!(
                            "'/old/npm/resync' codex-workspace-hook '{}'",
                            workspace.join(".resync-workspace.toml").display()
                        )
                    }]
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

    let hooks_path = workspace.join(".codex/hooks.json");
    let approved_definition = fs::read(&hooks_path)?;
    let hooks: serde_json::Value = serde_json::from_slice(&approved_definition)?;
    let mut dispatcher = None;
    for event in ["PreToolUse", "PostToolUse"] {
        let commands = hooks["hooks"][event]
            .as_array()
            .expect("hook groups")
            .iter()
            .flat_map(|group| group["hooks"].as_array().into_iter().flatten())
            .filter_map(|hook| hook["command"].as_str())
            .collect::<Vec<_>>();
        let generated = commands
            .iter()
            .filter(|command| command.contains("hook-dispatchers/codex-workspace-"))
            .collect::<Vec<_>>();
        assert_eq!(generated.len(), 1, "one umbrella dispatcher per event");
        assert!(!generated[0].contains(" codex-workspace-hook "));
        let path = std::path::PathBuf::from(generated[0].trim_matches('\''));
        assert!(path.is_file());
        assert_eq!(dispatcher.get_or_insert(path.clone()), &path);
    }
    let dispatcher = dispatcher.expect("stable workspace dispatcher");
    let script = fs::read_to_string(&dispatcher)?;
    assert!(script.contains("# RepoSync Codex workspace hook dispatcher v1"));
    assert!(script.contains("codex-workspace-hook"));
    assert!(
        script.contains(
            workspace
                .join(".resync-workspace.toml")
                .to_string_lossy()
                .as_ref()
        )
    );
    assert!(dispatcher.with_extension("resync").exists());
    assert_eq!(
        hooks["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
        "echo preserved"
    );

    let reinstall = Command::cargo_bin("resync")
        .unwrap()
        .env("RESYNC_STATE_DIR", &state)
        .args(["workspace", "install-hooks"])
        .arg(&workspace)
        .assert()
        .success();
    let reinstall_output: serde_json::Value =
        serde_json::from_slice(&reinstall.get_output().stdout)?;
    assert_eq!(reinstall_output["requiresCodexTrustReview"], false);
    assert_eq!(
        fs::read(&hooks_path)?,
        approved_definition,
        "reinstalling RepoSync changed the approved umbrella hook definition"
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
