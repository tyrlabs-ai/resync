use crate::config::ensure_project_config;
use crate::credentials::{generate_device_identity, load_credential, remove_device_identity};
use crate::daemon::daemon_rpc;
use crate::git_state::current_branch;
use crate::identity::{read_identity, same_project, write_identity};
use crate::process::{RunOptions, git, run};
use crate::provider::{Discovery, discover, provider_fetch, store_provider_login};
use crate::remote::{fetch_catalog, materialize_project, resolved_config};
use crate::state::{LocalProject, load_catalog, state_paths, write_json};
use anyhow::{Context, Result, bail};
use reqwest::Method;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

pub fn ticket_endpoint(discovery: &Discovery, project_id: &str) -> Result<String> {
    match discovery.endpoints.get("join_tickets") {
        Some(template) => Ok(template
            .replace("{project_id}", project_id)
            .replace("%7Bproject_id%7D", project_id)),
        None => Ok(format!(
            "{}/v1/projects/{project_id}/join-tickets",
            discovery.origin
        )),
    }
}

fn parse_target(value: &str, explicit_path: Option<&Path>) -> Result<(String, Option<PathBuf>)> {
    if value.is_empty() {
        bail!("peer sync requires an SSH target");
    }
    if let Some(path) = explicit_path {
        return Ok((value.into(), Some(path.into())));
    }
    if !value.starts_with("ssh://")
        && let Some((host, path)) = value.split_once(':')
        && !host.is_empty()
    {
        return Ok((host.into(), (!path.is_empty()).then(|| PathBuf::from(path))));
    }
    Ok((value.into(), None))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn remote_home_path(relative: &str) -> String {
    format!("\"$HOME\"/{}", shell_quote(relative))
}

fn remote_install_script(remote: &str) -> String {
    format!(
        "set -eu\nchmod 755 {}\nmv {} {}\nmkdir -p {}\nln -sfn {} {}\n",
        remote_home_path(&format!("{remote}.tmp")),
        remote_home_path(&format!("{remote}.tmp")),
        remote_home_path(remote),
        remote_home_path(".local/bin"),
        remote_home_path(remote),
        remote_home_path(".local/bin/resync")
    )
}

fn remote_target_with_ssh(ssh: &Path, host: &str) -> Result<String> {
    let output = run(
        ssh,
        [host, "sh", "-s"],
        RunOptions {
            input: Some(b"printf '%s\\n%s\\n' \"$(uname -s)\" \"$(uname -m)\"\n".to_vec()),
            ..RunOptions::default()
        },
    )?;
    let values: Vec<&str> = output.stdout.lines().collect();
    let os = values.first().copied().unwrap_or("").to_ascii_lowercase();
    let arch = values.get(1).copied().unwrap_or("").to_ascii_lowercase();
    match (os.as_str(), arch.as_str()) {
        ("darwin", "arm64" | "aarch64") => Ok("aarch64-apple-darwin".into()),
        ("darwin", "x86_64") => Ok("x86_64-apple-darwin".into()),
        ("linux", "arm64" | "aarch64") => Ok("aarch64-unknown-linux-gnu".into()),
        ("linux", "x86_64" | "amd64") => Ok("x86_64-unknown-linux-gnu".into()),
        _ => bail!("unsupported peer platform {os}/{arch}"),
    }
}

fn remote_target(host: &str) -> Result<String> {
    remote_target_with_ssh(Path::new("ssh"), host)
}

fn current_target() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-gnu"),
        ("linux", "x86_64") => Some("x86_64-unknown-linux-gnu"),
        _ => None,
    }
}

fn linux_binary_is_static(binary: &[u8]) -> Result<bool> {
    if binary.get(..6) != Some(b"\x7fELF\x02\x01") {
        bail!("Linux peer bootstrap requires a 64-bit little-endian ELF binary");
    }
    let program_offset = u64::from_le_bytes(
        binary
            .get(32..40)
            .context("ELF header is missing the program table offset")?
            .try_into()
            .unwrap(),
    ) as usize;
    let entry_size = u16::from_le_bytes(
        binary
            .get(54..56)
            .context("ELF header is missing the program entry size")?
            .try_into()
            .unwrap(),
    ) as usize;
    let entry_count = u16::from_le_bytes(
        binary
            .get(56..58)
            .context("ELF header is missing the program entry count")?
            .try_into()
            .unwrap(),
    ) as usize;
    if entry_size < 4 {
        bail!("ELF program entries are too small");
    }
    for index in 0..entry_count {
        let offset = program_offset
            .checked_add(
                index
                    .checked_mul(entry_size)
                    .context("ELF program table overflow")?,
            )
            .context("ELF program table overflow")?;
        let program_type = u32::from_le_bytes(
            binary
                .get(offset..offset + 4)
                .context("ELF program table is truncated")?
                .try_into()
                .unwrap(),
        );
        if program_type == 3 {
            return Ok(false);
        }
    }
    Ok(true)
}

fn release_binary(target: &str) -> Result<Vec<u8>> {
    if current_target() == Some(target) {
        let binary = fs::read(std::env::current_exe()?)?;
        #[cfg(target_os = "linux")]
        if linux_binary_is_static(&binary)? {
            return Ok(binary);
        }
        #[cfg(not(target_os = "linux"))]
        return Ok(binary);
    }
    let version = env!("CARGO_PKG_VERSION");
    let base = std::env::var("RESYNC_RELEASE_BASE").unwrap_or_else(|_| {
        format!("https://github.com/tyrlabs-ai/resync/releases/download/v{version}")
    });
    let name = format!("resync-{target}");
    let client = reqwest::blocking::Client::new();
    let binary = client
        .get(format!("{base}/{name}"))
        .send()?
        .error_for_status()?
        .bytes()?
        .to_vec();
    let sums = client
        .get(format!("{base}/SHA256SUMS"))
        .send()?
        .error_for_status()?
        .text()?;
    let expected = sums
        .lines()
        .find_map(|line| {
            let mut values = line.split_whitespace();
            let digest = values.next()?;
            let file = values.next()?.trim_start_matches('*');
            (file == name).then_some(digest)
        })
        .ok_or_else(|| anyhow::anyhow!("release checksum is missing {name}"))?;
    let actual = format!("{:x}", Sha256::digest(&binary));
    if actual != expected {
        bail!("release checksum mismatch for {name}");
    }
    Ok(binary)
}

fn install_remote_binary(host: &str) -> Result<String> {
    let target = remote_target(host)?;
    let binary = release_binary(&target)?;
    let remote = format!(
        ".local/share/resync/bin/{}/resync",
        env!("CARGO_PKG_VERSION")
    );
    let directory = remote.rsplit_once('/').unwrap().0;
    run(
        "ssh",
        [host, "sh", "-s"],
        RunOptions {
            input: Some(
                format!("set -eu\nmkdir -p {}\n", remote_home_path(directory)).into_bytes(),
            ),
            ..RunOptions::default()
        },
    )?;
    let local = tempfile::NamedTempFile::new()?;
    fs::write(local.path(), binary)?;
    run(
        "rsync",
        [
            "--".into(),
            local.path().to_string_lossy().into_owned(),
            format!("{host}:{remote}.tmp"),
        ],
        RunOptions::default(),
    )?;
    run(
        "ssh",
        [host, "sh", "-s"],
        RunOptions {
            input: Some(remote_install_script(&remote).into_bytes()),
            ..RunOptions::default()
        },
    )?;
    Ok(remote_home_path(&remote))
}

fn persist_project(project: LocalProject, generation: Option<u64>) -> Result<()> {
    let paths = state_paths();
    let mut catalog = load_catalog()?;
    catalog.projects.retain(|item| {
        item.project_id != project.project_id && item.local_path != project.local_path
    });
    catalog.projects.push(project);
    if let Some(generation) = generation {
        catalog.catalog_generation = generation;
    }
    write_json(&paths.catalog, &catalog, 0o600)
}

pub fn peer_sync(
    target_value: &str,
    into: Option<&Path>,
    name: Option<&str>,
    no_bootstrap: bool,
) -> Result<Value> {
    let (host, target_path) = parse_target(target_value, into)?;
    let identity = read_identity(&std::env::current_dir()?)?;
    let project_id = identity.project_id.as_deref().ok_or_else(|| {
        anyhow::anyhow!("current checkout is not enrolled; run `resync project init` first")
    })?;
    let service = identity.service.as_deref().unwrap();
    let config = resolved_config()?;
    let token = load_credential(service)?
        .or_else(|| {
            (config.server.as_deref() == Some(service))
                .then(|| config.token.clone())
                .flatten()
        })
        .ok_or_else(|| anyhow::anyhow!("no credential for {service}"))?;
    let discovery = discover(Some(service))?;
    let mut catalog = fetch_catalog(&crate::state::Config {
        server: Some(service.into()),
        token: Some(token.clone()),
        ..config.clone()
    })?;
    let mut project = catalog
        .projects
        .iter()
        .find(|item| item.project_id == project_id)
        .cloned()
        .ok_or_else(|| {
            anyhow::anyhow!("current device is no longer authorized for this project")
        })?;
    let branch = current_branch(&identity.root)?;
    let local_head = git(&identity.root, ["rev-parse", "HEAD"], RunOptions::default())?
        .stdout
        .trim()
        .to_owned();
    let advertised = |project: &crate::protocol::CatalogProject| {
        project
            .advertised_heads
            .iter()
            .find(|head| head.name == branch)
            .map(|head| head.oid.clone())
    };
    if advertised(&project).as_deref() != Some(&local_head) {
        daemon_rpc(&json!({ "method": "publish", "project_id": project_id }))?;
        catalog = fetch_catalog(&crate::state::Config {
            server: Some(service.into()),
            token: Some(token.clone()),
            ..config
        })?;
        project = catalog
            .projects
            .iter()
            .find(|item| item.project_id == project_id)
            .cloned()
            .context("project disappeared after publication")?;
        if advertised(&project).as_deref() != Some(&local_head) {
            bail!("hosted branch {branch} did not advance to the current commit");
        }
    }
    let ticket = provider_fetch(
        &ticket_endpoint(&discovery, project_id)?,
        Some(&token),
        Method::POST,
        Some(&json!({ "ttl_seconds": 300 })),
    )?;
    let executable = if no_bootstrap {
        shell_quote("resync")
    } else {
        install_remote_binary(&host)?
    };
    let mut remote_arguments = vec![
        "peer".into(),
        "accept".into(),
        "--provider".into(),
        service.into(),
        "--ticket".into(),
        ticket
            .get("ticket")
            .and_then(Value::as_str)
            .context("provider returned no join ticket")?
            .into(),
        "--project".into(),
        project_id.into(),
        "--device-name".into(),
        name.unwrap_or(&host).into(),
    ];
    if let Some(path) = target_path {
        remote_arguments.extend(["--into".into(), path.to_string_lossy().into_owned()]);
    }
    let script = format!(
        "set -eu\nexec {} {}\n",
        executable,
        remote_arguments
            .iter()
            .map(|value| shell_quote(value))
            .collect::<Vec<_>>()
            .join(" ")
    );
    let options = RunOptions {
        input: Some(script.into_bytes()),
        ..RunOptions::default()
    };
    let response = run("ssh", [&host, "sh", "-s"], options)?;
    let remote: Value =
        serde_json::from_str(&response.stdout).context("peer returned an invalid response")?;
    if remote.get("projectId").and_then(Value::as_str) != Some(project_id)
        || remote.get("service").and_then(Value::as_str) != Some(service)
    {
        bail!("peer enrolled into an unexpected project identity");
    }
    let dirty = !git(
        &identity.root,
        ["status", "--porcelain"],
        RunOptions::default(),
    )?
    .stdout
    .trim()
    .is_empty();
    Ok(json!({
        "projectId": project_id, "service": service, "localCheckoutId": identity.checkout_id,
        "peer": host, "peerPath": remote.get("localPath"), "peerCheckoutId": remote.get("checkoutId"),
        "committedHead": local_head, "warning": dirty.then_some("local uncommitted work remains only on this checkout")
    }))
}

pub struct AcceptOptions<'a> {
    pub provider: &'a str,
    pub ticket: &'a str,
    pub project_id: &'a str,
    pub into: Option<&'a Path>,
    pub device_name: &'a str,
}

fn enrollment_token<'a>(
    enrollment: &'a Value,
    existing_token: Option<&'a str>,
) -> Result<(&'a str, bool)> {
    if enrollment
        .get("reused_device")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Ok((
            existing_token.context("provider reused a device without an existing credential")?,
            false,
        ));
    }
    Ok((
        enrollment
            .get("token")
            .and_then(Value::as_str)
            .context("ticket enrollment returned no token")?,
        true,
    ))
}

fn peer_destination(
    explicit: Option<&Path>,
    managed_project_path: Option<&Path>,
    home: Option<&Path>,
    project_id: &str,
    project_name: &str,
) -> PathBuf {
    explicit
        .or(managed_project_path)
        .map(Path::to_owned)
        .unwrap_or_else(|| {
            home.unwrap_or_else(|| Path::new("."))
                .join(".local/share/resync/checkouts")
                .join(project_id)
                .join(project_name)
        })
}

pub fn peer_accept(options: AcceptOptions<'_>) -> Result<Value> {
    let discovery = discover(Some(options.provider))?;
    let existing_token = load_credential(&discovery.origin)?;
    if existing_token.is_some()
        && !discovery
            .capabilities
            .iter()
            .any(|value| value == "membership-extension-v1")
    {
        bail!(
            "provider cannot guarantee membership extension; refusing to replace the active device identity"
        );
    }
    let key = generate_device_identity(options.device_name)?;
    let endpoint = discovery
        .endpoints
        .get("redeem_join_ticket")
        .context("provider discovery is missing ticket redemption endpoint")?;
    let enrollment = provider_fetch(
        endpoint,
        existing_token.as_deref(),
        Method::POST,
        Some(&json!({
            "ticket": options.ticket, "device_name": options.device_name, "public_key": key.public_key
        })),
    )?;
    let (token, replaced_device) = enrollment_token(&enrollment, existing_token.as_deref())?;
    if replaced_device {
        store_provider_login(&discovery, &enrollment, &key)?;
    } else {
        remove_device_identity(Some(&key.public_key_path))?;
    }
    let catalog = fetch_catalog(&crate::state::Config {
        server: Some(discovery.origin.clone()),
        token: Some(token.into()),
        ..crate::state::Config::default()
    })?;
    let selected = catalog
        .projects
        .iter()
        .find(|item| item.project_id == options.project_id)
        .context("join ticket did not authorize the expected project")?;
    let managed_project_path = std::env::var_os("RESYNC_PEER_PROJECT_PATH")
        .or_else(|| std::env::var_os("YOLOPODS_PROJECT_PATH"))
        .map(PathBuf::from);
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let destination = peer_destination(
        options.into,
        managed_project_path.as_deref(),
        home.as_deref(),
        options.project_id,
        &selected.name,
    );
    if destination.exists() {
        if let Ok(current) = read_identity(&destination) {
            let expected = crate::identity::Identity {
                root: destination.clone(),
                service: Some(discovery.origin.clone()),
                project_id: Some(options.project_id.into()),
                checkout_id: None,
            };
            if current.project_id.is_some() && !same_project(&current, &expected)? {
                bail!("target path belongs to another RepoSync project; refusing to relink it");
            }
        }
    } else if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut local = materialize_project(selected, &destination, Some(token), "resync")?;
    let identity = write_identity(
        &local.local_path,
        &discovery.origin,
        options.project_id,
        None,
    )?;
    local.service = identity.service.clone();
    local.checkout_id = identity.checkout_id.clone();
    ensure_project_config(&local.local_path)?;
    persist_project(local.clone(), Some(catalog.catalog_generation))?;
    crate::cli::install_adapters(options.project_id)?;
    daemon_rpc(&json!({ "method": "reload" }))?;
    Ok(
        json!({ "projectId": options.project_id, "service": discovery.origin, "checkoutId": identity.checkout_id, "localPath": local.local_path }),
    )
}

#[cfg(test)]
mod tests {
    use super::{
        enrollment_token, linux_binary_is_static, peer_destination, remote_home_path,
        remote_install_script, remote_target_with_ssh, ticket_endpoint,
    };
    use crate::provider::Discovery;
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    #[test]
    fn repeated_peer_enrollment_keeps_the_existing_device_token() {
        let enrollment = json!({
            "device": { "id": "dev_existing" },
            "reused_device": true
        });
        let (token, replaced) = enrollment_token(&enrollment, Some("existing-token")).unwrap();
        assert_eq!(token, "existing-token");
        assert!(!replaced);
    }

    #[test]
    fn linux_peer_bootstrap_rejects_dynamic_elf_binaries() {
        let mut elf = vec![0_u8; 64 + 56];
        elf[..6].copy_from_slice(b"\x7fELF\x02\x01");
        elf[32..40].copy_from_slice(&64_u64.to_le_bytes());
        elf[54..56].copy_from_slice(&56_u16.to_le_bytes());
        elf[56..58].copy_from_slice(&1_u16.to_le_bytes());

        assert!(linux_binary_is_static(&elf).unwrap());
        elf[64..68].copy_from_slice(&3_u32.to_le_bytes());
        assert!(!linux_binary_is_static(&elf).unwrap());
    }

    #[test]
    fn remote_install_makes_bootstrapped_binary_the_stable_cli() {
        let script = remote_install_script(".local/share/resync/bin/0.2.0/resync");
        assert!(script.contains(
            "ln -sfn \"$HOME\"/'.local/share/resync/bin/0.2.0/resync' \
             \"$HOME\"/'.local/bin/resync'"
        ));
    }

    #[test]
    fn ticket_endpoint_substitutes_encoded_templates() {
        let discovery = Discovery {
            protocol: "resync.v1".into(),
            service_id: "test".into(),
            auth_methods: Vec::new(),
            capabilities: Vec::new(),
            endpoints: BTreeMap::from([(
                "join_tickets".into(),
                "https://provider.example/v1/projects/%7Bproject_id%7D/join-tickets".into(),
            )]),
            origin: "https://provider.example".into(),
        };
        assert_eq!(
            ticket_endpoint(&discovery, "prj_example").unwrap(),
            "https://provider.example/v1/projects/prj_example/join-tickets"
        );
    }

    #[test]
    fn platform_probe_uses_streamed_shell_transport() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let ssh = directory.path().join("ssh");
        fs::write(
            &ssh,
            "#!/bin/sh\ntest \"$1\" = peer\ntest \"$2\" = sh\ntest \"$3\" = -s\ntest \"$#\" -eq 3\ngrep -q 'uname -s'\nprintf 'Linux\\nx86_64\\n'\n",
        )?;
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755))?;

        assert_eq!(
            remote_target_with_ssh(&ssh, "peer")?,
            "x86_64-unknown-linux-gnu"
        );
        Ok(())
    }

    #[test]
    fn peer_destination_prefers_managed_project_path() {
        assert_eq!(
            peer_destination(
                None,
                Some(Path::new("/home/pod/yolopods")),
                Some(Path::new("/home/pod")),
                "prj_example",
                "yolopods"
            ),
            Path::new("/home/pod/yolopods")
        );
    }

    #[test]
    fn bootstrap_paths_are_rooted_in_remote_home() {
        assert_eq!(
            remote_home_path(".local/share/resync/bin/0.2.0"),
            "\"$HOME\"/'.local/share/resync/bin/0.2.0'"
        );
    }
}
