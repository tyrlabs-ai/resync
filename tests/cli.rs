use assert_cmd::Command;
use predicates::prelude::*;
use resync::state::{LocalCatalog, LocalProject, write_json};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

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
        vec!["lease", "--help"],
        vec!["lease", "acquire", "--help"],
        vec!["lease", "get", "--help"],
        vec!["lease", "list", "--help"],
        vec!["lease", "renew", "--help"],
        vec!["lease", "release", "--help"],
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

fn json_files(root: &std::path::Path) -> anyhow::Result<Vec<std::path::PathBuf>> {
    let mut files = Vec::new();
    if !root.exists() {
        return Ok(files);
    }
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        if path.is_dir() {
            files.extend(json_files(&path)?);
        } else if path
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            files.push(path);
        }
    }
    Ok(files)
}

fn serve_response(
    stream: &mut std::net::TcpStream,
    status: &str,
    body: &str,
) -> anyhow::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    Ok(())
}

fn read_http_request(stream: &mut std::net::TcpStream) -> anyhow::Result<String> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..count]);
        let Some(headers_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let header_text = String::from_utf8_lossy(&bytes[..headers_end]);
        let content_length = header_text
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(str::trim)
                    .and_then(|value| value.parse::<usize>().ok())
            })
            .unwrap_or(0);
        if bytes.len() >= headers_end + 4 + content_length {
            break;
        }
    }
    Ok(String::from_utf8(bytes)?)
}

#[test]
fn named_lease_acquire_persists_pending_before_transport_and_active_before_output()
-> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let state = root.path().join("state");
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let origin = format!("http://{}", listener.local_addr()?);
    let project_id = "prj_cli_named_lease";
    let mut project = local_project(project_id, root.path().join("not-a-checkout"));
    project.service = Some(origin.clone());
    write_json(
        &state.join("catalog.json"),
        &LocalCatalog {
            catalog_generation: 1,
            projects: vec![project],
        },
        0o600,
    )?;
    write_json(
        &state.join("credentials.json"),
        &serde_json::json!({
            "version": 1,
            "providers": {
                origin.clone(): {
                    "token": "test-device-token",
                    "updatedAt": "2026-08-03T00:00:00Z"
                }
            }
        }),
        0o600,
    )?;
    let server_state = state.clone();
    let server_origin = origin.clone();
    let server = thread::spawn(move || -> anyhow::Result<()> {
        let (mut discovery, _) = listener.accept()?;
        let request = read_http_request(&mut discovery)?;
        anyhow::ensure!(request.starts_with("GET /.well-known/resync "));
        serve_response(
            &mut discovery,
            "200 OK",
            &serde_json::json!({
                "protocol": "resync.v1",
                "service_id": "svc_test",
                "capabilities": ["project-named-leases-v1"],
                "endpoints": {
                    "named_leases": format!("{server_origin}/v1/projects/{{project_id}}/named-leases")
                },
                "named_lease_policy": {
                    "default_ttl_seconds": 900,
                    "min_ttl_seconds": 30,
                    "max_ttl_seconds": 3600
                }
            })
            .to_string(),
        )?;
        let (mut acquire, _) = listener.accept()?;
        let request = read_http_request(&mut acquire)?;
        anyhow::ensure!(
            request.starts_with(&format!("POST /v1/projects/{project_id}/named-leases "))
        );
        anyhow::ensure!(request.contains("test-device-token"));
        let pending = json_files(&server_state.join("named-leases"))?;
        anyhow::ensure!(pending.len() == 1);
        anyhow::ensure!(
            pending[0]
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("pending-")
        );
        let pending_value: serde_json::Value = serde_json::from_slice(&fs::read(&pending[0])?)?;
        anyhow::ensure!(pending_value["state"] == "PENDING");
        let response = serde_json::json!({
            "outcome": "ACQUIRED",
            "lease": {
                "lease_id": "nls_cli_example",
                "project_id": project_id,
                "resource_key": "waykeeper/maps/map/tickets/ticket",
                "holder_device_id": "dev_cli",
                "holder_id": "worker-a",
                "generation": 1,
                "acquired_at": "2026-08-03T00:00:00Z",
                "expires_at": "2026-08-03T00:15:00Z"
            }
        });
        serve_response(&mut acquire, "201 Created", &response.to_string())?;
        Ok(())
    });

    let assertion = Command::cargo_bin("resync")
        .unwrap()
        .env("RESYNC_STATE_DIR", &state)
        .args([
            "lease",
            "acquire",
            "waykeeper/maps/map/tickets/ticket",
            "--project",
            project_id,
            "--holder",
            "worker-a",
            "--ttl",
            "15m",
        ])
        .assert()
        .success();
    server.join().unwrap()?;
    let stdout = String::from_utf8(assertion.get_output().stdout.clone())?;
    let stderr = String::from_utf8(assertion.get_output().stderr.clone())?;
    assert!(stdout.contains("nls_cli_example"));
    assert!(!stdout.contains("holder_secret"));
    assert!(!stderr.contains("holder_secret"));
    let active = json_files(&state.join("named-leases"))?;
    assert_eq!(active.len(), 1);
    assert!(
        active[0]
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("nls_cli_example-")
    );
    let active_value: serde_json::Value = serde_json::from_slice(&fs::read(&active[0])?)?;
    assert_eq!(active_value["state"], "ACTIVE");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&active[0])?.permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(active[0].parent().unwrap())?
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
    Ok(())
}

#[test]
fn named_lease_contention_is_json_on_stdout_with_exit_three() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let state = root.path().join("state");
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let origin = format!("http://{}", listener.local_addr()?);
    let project_id = "prj_cli_contention";
    let mut project = local_project(project_id, root.path().join("not-a-checkout"));
    project.service = Some(origin.clone());
    write_json(
        &state.join("catalog.json"),
        &LocalCatalog {
            catalog_generation: 1,
            projects: vec![project],
        },
        0o600,
    )?;
    write_json(
        &state.join("credentials.json"),
        &serde_json::json!({
            "version": 1,
            "providers": {
                origin.clone(): {
                    "token": "test-device-token",
                    "updatedAt": "2026-08-03T00:00:00Z"
                }
            }
        }),
        0o600,
    )?;
    let server_origin = origin.clone();
    let server = thread::spawn(move || -> anyhow::Result<()> {
        let (mut discovery, _) = listener.accept()?;
        read_http_request(&mut discovery)?;
        serve_response(
            &mut discovery,
            "200 OK",
            &serde_json::json!({
                "protocol": "resync.v1",
                "service_id": "svc_test",
                "capabilities": ["project-named-leases-v1"],
                "endpoints": {
                    "named_leases": format!("{server_origin}/v1/projects/{{project_id}}/named-leases")
                },
                "named_lease_policy": {
                    "default_ttl_seconds": 900,
                    "min_ttl_seconds": 30,
                    "max_ttl_seconds": 3600
                }
            })
            .to_string(),
        )?;
        let (mut acquire, _) = listener.accept()?;
        read_http_request(&mut acquire)?;
        serve_response(
            &mut acquire,
            "409 Conflict",
            &serde_json::json!({
                "outcome": "HELD",
                "lease": {
                    "lease_id": "nls_existing",
                    "project_id": project_id,
                    "resource_key": "waykeeper/maps/map/tickets/ticket",
                    "holder_device_id": "dev_other",
                    "holder_id": "worker-other",
                    "generation": 4,
                    "acquired_at": "2026-08-03T00:00:00Z",
                    "expires_at": "2026-08-03T00:15:00Z"
                }
            })
            .to_string(),
        )?;
        Ok(())
    });
    let assertion = Command::cargo_bin("resync")
        .unwrap()
        .env("RESYNC_STATE_DIR", &state)
        .args([
            "lease",
            "acquire",
            "waykeeper/maps/map/tickets/ticket",
            "--project",
            project_id,
            "--holder",
            "worker-a",
        ])
        .assert()
        .code(3)
        .stdout(predicate::str::contains("\"outcome\": \"HELD\""))
        .stderr(predicate::str::contains("HELD"));
    server.join().unwrap()?;
    let stdout = String::from_utf8(assertion.get_output().stdout.clone())?;
    let stderr = String::from_utf8(assertion.get_output().stderr.clone())?;
    assert!(!stdout.contains("holder_secret"));
    assert!(!stderr.contains("holder_secret"));
    assert!(json_files(&state.join("named-leases"))?.is_empty());
    Ok(())
}

#[test]
fn npm_install_has_no_lifecycle_scripts() -> anyhow::Result<()> {
    let manifest: serde_json::Value = serde_json::from_str(include_str!("../package.json"))?;
    for lifecycle in [
        "preinstall",
        "install",
        "postinstall",
        "prepare",
        "prepublish",
    ] {
        assert!(manifest["scripts"].get(lifecycle).is_none());
    }
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
fn adapter_repair_removes_every_reposync_hook_form_and_preserves_user_order() -> anyhow::Result<()>
{
    let root = tempfile::tempdir()?;
    let state = root.path().join("state");
    let repository = root.path().join("project");
    let status = std::process::Command::new("git")
        .args(["init", "--quiet", "--initial-branch=main"])
        .arg(&repository)
        .status()?;
    anyhow::ensure!(status.success(), "could not initialize test repository");
    let project_id = "prj_all_hook_forms";
    write_json(
        &state.join("catalog.json"),
        &LocalCatalog {
            catalog_generation: 1,
            projects: vec![local_project(project_id, repository.clone())],
        },
        0o600,
    )?;
    let stale_dispatcher = root
        .path()
        .join("old-state/hook-dispatchers/codex-deadbeef");
    write_json(
        &repository.join(".codex/hooks.json"),
        &serde_json::json!({
            "hooks": {
                "PreToolUse": [{
                    "matcher": "*",
                    "hooks": [
                        { "type": "command", "command": "echo first" },
                        {
                            "type": "command",
                            "command": format!("'{}'", stale_dispatcher.display())
                        },
                        {
                            "type": "command",
                            "command": format!(
                                "'/home/test/.nvm/versions/node/24.18.0/bin/resync' codex-hook {project_id}"
                            )
                        },
                        {
                            "type": "command",
                            "command": format!("resync codex-hook '{project_id}'")
                        },
                        { "type": "command", "command": "echo last" }
                    ]
                }],
                "PostToolUse": [{
                    "matcher": "*",
                    "hooks": [
                        {
                            "type": "command",
                            "command": format!("'{}'", stale_dispatcher.display())
                        },
                        {
                            "type": "command",
                            "command": format!(
                                "'/home/test/.nvm/versions/node/24.18.0/bin/resync' codex-hook '{project_id}'"
                            )
                        }
                    ]
                }]
            }
        }),
        0o644,
    )?;

    let install = || {
        Command::cargo_bin("resync")
            .unwrap()
            .env("RESYNC_STATE_DIR", &state)
            .args(["install-adapters", project_id])
            .assert()
            .success()
    };
    install();
    let hooks_path = repository.join(".codex/hooks.json");
    let first_repair = fs::read(&hooks_path)?;
    let hooks: serde_json::Value = serde_json::from_slice(&first_repair)?;
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
                .filter(|command| command.contains("hook-dispatchers/codex-"))
                .count(),
            1,
            "exactly one RepoSync dispatcher must remain for {event}"
        );
        assert!(
            commands
                .iter()
                .all(|command| !command.contains("codex-hook"))
        );
        assert!(
            commands
                .iter()
                .all(|command| !command.contains("old-state"))
        );
        if event == "PreToolUse" {
            let user_commands = commands
                .iter()
                .filter(|command| command.starts_with("echo "))
                .copied()
                .collect::<Vec<_>>();
            assert_eq!(user_commands, ["echo first", "echo last"]);
        }
    }

    install();
    assert_eq!(
        fs::read(hooks_path)?,
        first_repair,
        "repeated catalog repair must be byte-for-byte idempotent"
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
