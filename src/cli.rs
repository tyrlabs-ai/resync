use crate::config::ensure_project_config;
use crate::credentials::{
    canonical_origin, erase_credential, load_credential, remove_device_identity,
};
use crate::daemon::{daemon_rpc, ensure_daemon_supervision, run_daemon, runtime};
use crate::identity::{clear_identity, read_identity, repository_root, write_identity};
use crate::peer::{AcceptOptions, peer_accept, peer_sync};
use crate::process::{RunOptions, git, run as run_process};
use crate::protocol::CatalogProject;
use crate::provider::{default_provider, discover, enroll_with_token, provider_fetch};
use crate::remote::{fetch_catalog, materialize_project, resolved_config};
use crate::rpc::rpc;
use crate::state::{
    LocalProject, load_catalog, load_config_raw, read_json, state_paths, write_json,
};
use crate::transaction::recover_transaction;
use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use reqwest::Method;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

const REPOSYNC_SKILL: &str = include_str!("../skills/reposync/SKILL.md");
const REPOSYNC_SKILL_OPENAI_METADATA: &str = include_str!("../skills/reposync/agents/openai.yaml");

#[derive(Parser)]
#[command(
    name = "resync",
    version,
    about = "Cooperative continuous Git synchronization across machines and agents."
)]
struct Cli {
    #[arg(
        long,
        global = true,
        value_name = "PATH",
        help = "Override the RepoSync state directory"
    )]
    state_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    #[command(about = "Manage provider-neutral login credentials and device identity")]
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    #[command(about = "Create, join, separate, synchronize, and inspect projects")]
    Project {
        #[command(subcommand)]
        command: ProjectCommand,
    },
    #[command(about = "Inspect and revoke independently enrolled machines")]
    Device {
        #[command(subcommand)]
        command: DeviceCommand,
    },
    #[command(about = "Enroll another machine in the same project over SSH")]
    Peer {
        #[command(subcommand)]
        command: PeerCommand,
    },
    #[command(about = "Run the per-user synchronization daemon")]
    Daemon {
        #[arg(long, default_value = "10s")]
        poll_interval: String,
    },
    #[command(about = "Check Git, provider, daemon, identity, and credential state")]
    Doctor,
    #[command(
        name = "install-adapters",
        about = "Repair lifecycle adapters for every local project or one selected project"
    )]
    InstallAdapters {
        #[arg(value_name = "PROJECT_ID")]
        project_id: Option<String>,
    },
    #[command(about = "Acquire one cooperative tool-call lease")]
    Acquire {
        project_id: String,
        session_id: Option<String>,
        call_id: Option<String>,
    },
    #[command(about = "Release one cooperative tool-call lease")]
    Release { lease_id: String },
    #[command(name = "codex-hook", hide = true)]
    CodexHook { project_id: String },
    #[command(name = "report-commit", hide = true)]
    ReportCommit {
        project_id: String,
        commit_oid: String,
        branch: String,
    },
    #[command(name = "report-checkout", hide = true)]
    ReportCheckout {
        project_id: String,
        previous_oid: String,
        new_oid: String,
        new_branch: String,
    },
}

#[derive(Subcommand)]
enum AuthCommand {
    #[command(about = "Enroll this machine with a provider")]
    Login(LoginArgs),
    #[command(about = "List configured providers without revealing credentials")]
    Status { provider: Option<String> },
    #[command(about = "Remove this machine's local provider credential and key")]
    Logout {
        provider: Option<String>,
        #[arg(long)]
        all: bool,
    },
}

#[derive(Args)]
struct LoginArgs {
    provider: Option<String>,
    #[arg(long, hide = true)]
    legacy_token: Option<String>,
    #[arg(long)]
    token: Option<String>,
    #[arg(long)]
    token_command: Option<String>,
    #[arg(long)]
    device_name: Option<String>,
}

#[derive(Subcommand)]
enum ProjectCommand {
    #[command(about = "Create a new synchronization universe for this Git repository")]
    Init(ProjectCreateArgs),
    #[command(about = "Join an account project with a distinct checkout ID")]
    Join {
        project_id: String,
        #[arg(long)]
        path: Option<PathBuf>,
        #[arg(long)]
        provider: Option<String>,
    },
    #[command(about = "Create an independent project ID from the current Git repository")]
    Fork(ProjectCreateArgs),
    #[command(about = "Show freshness, durability, project ID, and checkout ID")]
    Status { project: Option<String> },
    #[command(about = "Fetch and reconcile at an explicit barrier")]
    Sync { project: Option<String> },
    #[command(about = "Explicitly flush publication")]
    Publish { project: Option<String> },
    #[command(about = "List blocked reconciliation records")]
    Conflicts { project: Option<String> },
    #[command(about = "Recover interrupted reconciliation transactions")]
    Recover,
    #[command(about = "Stop managing a checkout without deleting its files")]
    Leave { project_id: Option<String> },
}

#[derive(Args)]
struct ProjectCreateArgs {
    #[arg(long)]
    name: Option<String>,
    #[arg(long)]
    provider: Option<String>,
}

#[derive(Subcommand)]
enum DeviceCommand {
    #[command(about = "List devices visible to the active provider account")]
    List {
        #[arg(long)]
        provider: Option<String>,
    },
    #[command(about = "Revoke a device without rotating other machines")]
    Revoke {
        device_id: String,
        #[arg(long)]
        provider: Option<String>,
    },
}

#[derive(Subcommand)]
enum PeerCommand {
    #[command(about = "Create a one-time ticket and converge an SSH peer")]
    Sync {
        target: String,
        #[arg(long)]
        into: Option<PathBuf>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        no_bootstrap: bool,
    },
    #[command(about = "Internal remote-side ticket redemption helper", hide = true)]
    Accept {
        #[arg(long)]
        provider: String,
        #[arg(long)]
        ticket: String,
        #[arg(long)]
        project: String,
        #[arg(long)]
        into: Option<PathBuf>,
        #[arg(long)]
        device_name: String,
    },
}

struct ConfiguredProvider {
    origin: String,
    token: String,
    discovery: crate::provider::Discovery,
}

fn configured_provider(provider: Option<&str>) -> Result<ConfiguredProvider> {
    let config = load_config_raw()?;
    let origin = canonical_origin(
        provider
            .or(config.active_provider.as_deref())
            .unwrap_or(&default_provider()),
    )?;
    let token = load_credential(&origin)?
        .or_else(|| {
            (config.server.as_deref() == Some(&origin))
                .then_some(config.token)
                .flatten()
        })
        .ok_or_else(|| {
            anyhow::anyhow!("not logged in to {origin}; run `resync auth login {origin}`")
        })?;
    let discovery = discover(Some(&origin))?;
    Ok(ConfiguredProvider {
        origin,
        token,
        discovery,
    })
}

fn print(value: &Value) -> Result<()> {
    serde_json::to_writer_pretty(std::io::stdout(), value)?;
    println!();
    Ok(())
}

fn preprocess_arguments() -> Vec<String> {
    let mut args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        return args;
    }
    let mut index = 1;
    while index < args.len() {
        if args[index] == "--state-dir" {
            index += 2;
            continue;
        }
        if args[index].starts_with("--state-dir=") {
            index += 1;
            continue;
        }
        break;
    }
    if index >= args.len() {
        return args;
    }
    let known = [
        "auth",
        "project",
        "device",
        "peer",
        "daemon",
        "doctor",
        "install-adapters",
        "acquire",
        "release",
        "codex-hook",
        "report-commit",
        "report-checkout",
        "help",
    ];
    let value = args[index].clone();
    let replacements: Option<Vec<&str>> = match value.as_str() {
        "login" => Some(vec!["auth", "login"]),
        "init" => Some(vec!["project", "init"]),
        "join" | "subscribe" => Some(vec!["project", "join"]),
        "fork" => Some(vec!["project", "fork"]),
        "unsubscribe" => Some(vec!["project", "leave"]),
        "status" => Some(vec!["project", "status"]),
        "sync" => Some(vec!["project", "sync"]),
        "publish" => Some(vec!["project", "publish"]),
        "conflicts" => Some(vec!["project", "conflicts"]),
        "recover" => Some(vec!["project", "recover"]),
        "devices" => Some(vec!["device", "list"]),
        _ => None,
    };
    if let Some(replacement) = replacements {
        args.splice(index..=index, replacement.into_iter().map(str::to_owned));
    } else if !known.contains(&value.as_str()) && !value.starts_with('-') {
        args.splice(index..index, ["peer".into(), "sync".into()]);
    }
    args
}

pub fn run() -> Result<()> {
    let args = preprocess_arguments();
    if args
        .iter()
        .any(|value| value == "--version" || value == "-V")
    {
        println!(
            "resync {} (protocol {})",
            env!("CARGO_PKG_VERSION"),
            crate::protocol::PROTOCOL_VERSION
        );
        return Ok(());
    }
    let cli = Cli::parse_from(args);
    if let Some(path) = cli.state_dir {
        // SAFETY: this happens before any worker threads or HTTP clients are created.
        unsafe { std::env::set_var("RESYNC_STATE_DIR", absolute(&path)) };
    }
    let value = dispatch(cli.command)?;
    if let Some(value) = value {
        print(&value)?;
    }
    Ok(())
}

fn dispatch(command: Command) -> Result<Option<Value>> {
    match command {
        Command::Auth { command } => Ok(Some(auth(command)?)),
        Command::Project { command } => Ok(Some(project(command)?)),
        Command::Device { command } => Ok(Some(device(command)?)),
        Command::Peer { command } => Ok(Some(peer(command)?)),
        Command::Daemon { poll_interval } => {
            runtime()?.block_on(run_daemon(parse_duration(&poll_interval)?))?;
            Ok(None)
        }
        Command::Doctor => Ok(Some(doctor())),
        Command::InstallAdapters { project_id } => {
            Ok(Some(install_adapters_command(project_id.as_deref())?))
        }
        Command::Acquire {
            project_id,
            session_id,
            call_id,
        } => Ok(Some(daemon_rpc(&json!({
            "method": "acquireAccess", "project_id": project_id,
            "session_id": session_id.unwrap_or_else(|| format!("cli-{}", std::process::id())),
            "call_id": call_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()), "access_hint": "unknown"
        }))?)),
        Command::Release { lease_id } => Ok(Some(daemon_rpc(
            &json!({ "method": "releaseAccess", "lease_id": lease_id, "outcome": "success", "mutation_hint": "possible" }),
        )?)),
        Command::CodexHook { project_id } => Ok(Some(codex_hook(&project_id)?)),
        Command::ReportCommit {
            project_id,
            commit_oid,
            branch,
        } => Ok(Some(daemon_rpc(&json!({
            "method": "reportCommit", "project_id": project_id, "commit_oid": commit_oid, "branch": branch
        }))?)),
        Command::ReportCheckout {
            project_id,
            previous_oid,
            new_oid,
            new_branch,
        } => Ok(Some(daemon_rpc(&json!({
            "method": "reportCheckout",
            "project_id": project_id,
            "previous_oid": previous_oid,
            "new_oid": new_oid,
            "new_branch": new_branch
        }))?)),
    }
}

fn auth(command: AuthCommand) -> Result<Value> {
    match command {
        AuthCommand::Login(arguments) => {
            let provider = arguments
                .provider
                .as_deref()
                .unwrap_or(&default_provider())
                .to_owned();
            let mut token = arguments
                .token
                .or(arguments.legacy_token)
                .or_else(|| std::env::var("RESYNC_BOOTSTRAP_TOKEN").ok());
            if let Some(command) = arguments.token_command {
                token = Some(
                    run_process("/bin/sh", ["-c", &command], RunOptions::default())?
                        .stdout
                        .trim()
                        .into(),
                );
            }
            let token = token
                .context("login requires --token, --token-command, or RESYNC_BOOTSTRAP_TOKEN")?;
            let name = arguments.device_name.unwrap_or_else(|| {
                hostname::get()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            });
            enroll_with_token(Some(&provider), &token, &name)
        }
        AuthCommand::Status { provider } => {
            let config = load_config_raw()?;
            let origins: Vec<String> = match provider {
                Some(value) => vec![canonical_origin(&value)?],
                None => config.providers.keys().cloned().collect(),
            };
            let providers = origins
                .into_iter()
                .map(|origin| {
                    let mut value = serde_json::to_value(
                        config.providers.get(&origin).cloned().unwrap_or_default(),
                    )?;
                    value["origin"] = Value::String(origin.clone());
                    value["active"] =
                        Value::Bool(config.active_provider.as_deref() == Some(&origin));
                    value["credentialAvailable"] = Value::Bool(load_credential(&origin)?.is_some());
                    Ok(value)
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(json!({ "activeProvider": config.active_provider, "providers": providers }))
        }
        AuthCommand::Logout { provider, all } => {
            let mut config = load_config_raw()?;
            let origins = if all {
                config.providers.keys().cloned().collect::<Vec<_>>()
            } else {
                vec![canonical_origin(
                    provider
                        .as_deref()
                        .or(config.active_provider.as_deref())
                        .unwrap_or(&default_provider()),
                )?]
            };
            for origin in &origins {
                erase_credential(origin)?;
                remove_device_identity(
                    config
                        .providers
                        .get(origin)
                        .and_then(|value| value.public_key_path.as_deref()),
                )?;
                config.providers.remove(origin);
            }
            if config
                .active_provider
                .as_ref()
                .is_some_and(|value| origins.contains(value))
            {
                config.active_provider = config.providers.keys().next().cloned();
            }
            config.server = None;
            config.token = None;
            write_json(&state_paths().config, &config, 0o600)?;
            Ok(json!({ "loggedOut": origins, "activeProvider": config.active_provider }))
        }
    }
}

fn project(command: ProjectCommand) -> Result<Value> {
    match command {
        ProjectCommand::Init(arguments) => project_create(arguments, false),
        ProjectCommand::Fork(arguments) => project_create(arguments, true),
        ProjectCommand::Join {
            project_id,
            path,
            provider,
        } => project_join(&project_id, path.as_deref(), provider.as_deref()),
        ProjectCommand::Leave { project_id } => unsubscribe(project_id.as_deref()),
        ProjectCommand::Status { project } => {
            daemon_rpc(&json!({ "method": "status", "project_id": project }))
        }
        ProjectCommand::Sync { project } => {
            daemon_rpc(&json!({ "method": "sync", "project_id": project_id_or_identity(project)? }))
        }
        ProjectCommand::Publish { project } => daemon_rpc(
            &json!({ "method": "publish", "project_id": project_id_or_identity(project)? }),
        ),
        ProjectCommand::Conflicts { project } => {
            let catalog = load_catalog()?;
            Ok(Value::Array(catalog.projects.into_iter().filter(|item| item.state == "CONFLICTED" && project.as_ref().is_none_or(|id| id == &item.project_id))
                .map(|item| json!({ "projectId": item.project_id, "localPath": item.local_path, "conflict": item.conflict })).collect()))
        }
        ProjectCommand::Recover => {
            let root = state_paths().transactions;
            let mut recovered = Vec::new();
            if root.exists() {
                for entry in fs::read_dir(root)? {
                    let path = entry?.path();
                    if path.extension().is_some_and(|value| value == "json") {
                        let transaction = recover_transaction(&path)?;
                        recovered.push(json!({ "id": transaction.id, "phase": transaction.phase }));
                    }
                }
            }
            Ok(Value::Array(recovered))
        }
    }
}

fn project_id_or_identity(value: Option<String>) -> Result<String> {
    match value {
        Some(value) => Ok(value),
        None => read_identity(&std::env::current_dir()?)?
            .project_id
            .context("current checkout has no RepoSync project identity"),
    }
}

fn project_create(arguments: ProjectCreateArgs, fork: bool) -> Result<Value> {
    let root = repository_root(&std::env::current_dir()?)?;
    let prior = read_identity(&root)?;
    if let Some(prior_project) = prior.project_id.as_deref().filter(|_| !fork) {
        let prior_service = prior
            .service
            .as_deref()
            .context("existing RepoSync identity has no provider")?;
        if let Some(requested) = arguments.provider.as_deref()
            && canonical_origin(requested)? != canonical_origin(prior_service)?
        {
            bail!(
                "checkout already belongs to {prior_service} / {prior_project}; use `resync project fork` to separate it"
            );
        }
        let auth = configured_provider(Some(prior_service))?;
        let remote = fetch_catalog(&crate::state::Config {
            server: Some(auth.origin.clone()),
            token: Some(auth.token.clone()),
            ..crate::state::Config::default()
        })?;
        let selected = remote
            .projects
            .iter()
            .find(|item| item.project_id == prior_project)
            .with_context(|| {
                format!(
                    "ACCESS_LOST: {prior_project} is not owned by the active device account; verify the provider login and device registration"
                )
            })?;
        let existing = load_catalog()?
            .projects
            .into_iter()
            .find(|item| item.project_id == prior_project && item.local_path == root);
        let mut local = match existing {
            Some(local) => local,
            None => materialize_project(selected, &root, Some(&auth.token), "resync")?,
        };
        local.remote_url = selected.remote_url.clone();
        local.default_branch = selected.default_branch.clone();
        local.server_generation = selected.project_generation;
        local.service = prior.service.clone();
        local.checkout_id = prior.checkout_id.clone();
        ensure_project_config(&root)?;
        register_project_with_daemon(&local, Some(remote.catalog_generation))?;
        install_adapters(prior_project)?;
        let _ = daemon_rpc(&json!({ "method": "sync", "project_id": prior_project }))?;
        let _ = daemon_rpc(&json!({ "method": "publish", "project_id": prior_project }))?;
        let mut value = serde_json::to_value(local)?;
        value["operation"] = Value::String("repaired".into());
        value["identityPreserved"] = Value::Bool(true);
        return Ok(value);
    }
    let auth = configured_provider(arguments.provider.as_deref())?;
    let default_branch = git(
        &root,
        ["symbolic-ref", "--short", "HEAD"],
        RunOptions::default(),
    )?
    .stdout
    .trim()
    .to_owned();
    let name = arguments.name.unwrap_or_else(|| {
        root.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    });
    let endpoint = auth
        .discovery
        .endpoints
        .get("projects")
        .context("provider discovery is missing projects endpoint")?;
    let creation_directory = state_paths().root.join("project-creations");
    let creation_name = format!(
        "{:x}",
        Sha256::digest(
            format!(
                "{}\0{}\0{}",
                auth.origin,
                root.display(),
                if fork { "fork" } else { "init" }
            )
            .as_bytes()
        )
    );
    let creation_path = creation_directory.join(format!("{creation_name}.json"));
    let mut creation: Value = read_json(&creation_path, json!({}))?;
    let idempotency_key = creation
        .get("idempotencyKey")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| format!("pcr_{}", uuid::Uuid::new_v4().simple()));
    creation["idempotencyKey"] = Value::String(idempotency_key.clone());
    creation["provider"] = Value::String(auth.origin.clone());
    creation["repository"] = Value::String(root.to_string_lossy().into_owned());
    write_json(&creation_path, &creation, 0o600)?;
    let created = provider_fetch(
        endpoint,
        Some(&auth.token),
        Method::POST,
        Some(&json!({
            "name": name,
            "default_branch": default_branch,
            "idempotency_key": idempotency_key
        })),
    )?;
    let catalog_project: CatalogProject = serde_json::from_value(created.clone())?;
    let mut local = materialize_project(&catalog_project, &root, Some(&auth.token), "resync")?;
    let identity = write_identity(&root, &auth.origin, &catalog_project.project_id, None)?;
    local.service = identity.service;
    local.checkout_id = identity.checkout_id;
    ensure_project_config(&root)?;
    register_project_with_daemon(&local, None)?;
    install_adapters(&local.project_id)?;
    fs::remove_file(&creation_path)?;
    if let Some(parent) = creation_path.parent() {
        fs::File::open(parent)?.sync_all()?;
    }
    let mut value = serde_json::to_value(local)?;
    value["operation"] = Value::String(if fork { "forked" } else { "initialized" }.into());
    value["previousProjectId"] = prior.project_id.map(Value::String).unwrap_or(Value::Null);
    Ok(value)
}

fn project_join(project_id: &str, path: Option<&Path>, provider: Option<&str>) -> Result<Value> {
    let auth = configured_provider(provider)?;
    let remote = fetch_catalog(&crate::state::Config {
        server: Some(auth.origin.clone()),
        token: Some(auth.token.clone()),
        ..crate::state::Config::default()
    })?;
    let selected = remote
        .projects
        .iter()
        .find(|item| item.project_id == project_id)
        .context("project is not in the active device account's catalog")?;
    let destination = join_destination(path, &std::env::current_dir()?)?;
    if destination.exists()
        && let Ok(existing) = read_identity(&destination)
        && let (Some(id), Some(service)) = (&existing.project_id, &existing.service)
        && (id != project_id || service != &auth.origin)
    {
        bail!(
            "checkout already belongs to {service} / {id}; use `resync project fork` for an independent project"
        );
    }
    let mut local = materialize_project(selected, &destination, Some(&auth.token), "resync")?;
    let identity = write_identity(&local.local_path, &auth.origin, project_id, None)?;
    local.service = identity.service;
    local.checkout_id = identity.checkout_id;
    ensure_project_config(&local.local_path)?;
    register_project_with_daemon(&local, Some(remote.catalog_generation))?;
    install_adapters(project_id)?;
    Ok(serde_json::to_value(local)?)
}

fn join_destination(path: Option<&Path>, current_directory: &Path) -> Result<PathBuf> {
    match path {
        Some(path) => Ok(path.to_owned()),
        None => repository_root(current_directory),
    }
}

pub(crate) fn register_project_with_daemon(
    project: &LocalProject,
    generation: Option<u64>,
) -> Result<()> {
    ensure_daemon_supervision(false)?;
    let mut request = json!({
        "method": "registerProject",
        "project": project
    });
    if let Some(generation) = generation {
        request["catalog_generation"] = Value::from(generation);
    }
    daemon_rpc(&request)?;
    Ok(())
}

fn unsubscribe(project_id: Option<&str>) -> Result<Value> {
    let id = match project_id {
        Some(value) => value.into(),
        None => read_identity(&std::env::current_dir()?)?
            .project_id
            .context("current checkout has no RepoSync project identity")?,
    };
    let mut catalog = load_catalog()?;
    let before = catalog.projects.len();
    catalog.projects.retain(|item| item.project_id != id);
    if before == catalog.projects.len() {
        bail!("project {id} is not subscribed");
    }
    write_json(&state_paths().catalog, &catalog, 0o600)?;
    if let Ok(identity) = read_identity(&std::env::current_dir()?)
        && identity.project_id.as_deref() == Some(&id)
    {
        clear_identity(&identity.root)?;
    }
    Ok(json!({ "unsubscribed": id, "filesPreserved": true }))
}

fn device(command: DeviceCommand) -> Result<Value> {
    match command {
        DeviceCommand::List { provider } => {
            let auth = configured_provider(provider.as_deref())?;
            let endpoint = auth
                .discovery
                .endpoints
                .get("devices")
                .cloned()
                .unwrap_or_else(|| format!("{}/v1/devices", auth.origin));
            provider_fetch(&endpoint, Some(&auth.token), Method::GET, None)
        }
        DeviceCommand::Revoke {
            device_id,
            provider,
        } => {
            let auth = configured_provider(provider.as_deref())?;
            let base = auth
                .discovery
                .endpoints
                .get("devices")
                .cloned()
                .unwrap_or_else(|| format!("{}/v1/devices", auth.origin));
            provider_fetch(
                &format!("{}/{device_id}/revoke", base.trim_end_matches('/')),
                Some(&auth.token),
                Method::POST,
                Some(&json!({})),
            )
        }
    }
}

fn peer(command: PeerCommand) -> Result<Value> {
    match command {
        PeerCommand::Sync {
            target,
            into,
            name,
            no_bootstrap,
        } => peer_sync(&target, into.as_deref(), name.as_deref(), no_bootstrap),
        PeerCommand::Accept {
            provider,
            ticket,
            project,
            into,
            device_name,
        } => peer_accept(AcceptOptions {
            provider: &provider,
            ticket: &ticket,
            project_id: &project,
            into: into.as_deref(),
            device_name: &device_name,
        }),
    }
}

fn doctor() -> Value {
    let mut checks = Vec::new();
    let result = run_process(
        "git",
        ["--version"],
        RunOptions {
            allow_failure: true,
            ..RunOptions::default()
        },
    );
    checks.push(match result { Ok(value) => json!({ "name": "git", "ok": value.code == 0, "detail": format!("{}{}", value.stdout, value.stderr).trim() }), Err(error) => json!({ "name": "git", "ok": false, "detail": error.to_string() }) });
    let config = resolved_config();
    checks.push(match config.and_then(|value| fetch_catalog(&value)) {
        Ok(_) => json!({ "name": "hosted catalog", "ok": true }),
        Err(error) => json!({ "name": "hosted catalog", "ok": false, "detail": error.to_string() }),
    });
    checks.push(match rpc(&json!({ "method": "ping" }), None) {
        Ok(_) => json!({ "name": "daemon", "ok": true }),
        Err(error) => json!({ "name": "daemon", "ok": false, "detail": error.to_string() }),
    });
    Value::Array(checks)
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn install_adapters_command(project_id: Option<&str>) -> Result<Value> {
    if let Some(project_id) = project_id {
        return install_adapters(project_id);
    }
    let catalog = load_catalog()?;
    let mut projects = Vec::with_capacity(catalog.projects.len());
    for project in &catalog.projects {
        projects.push(
            install_project_adapters(project)
                .with_context(|| format!("install adapters for {}", project.project_id))?,
        );
    }
    let supervision = ensure_daemon_supervision(false)?;
    Ok(json!({
        "projectCount": projects.len(),
        "projects": projects,
        "daemonSupervision": supervision,
        "requiresCodexTrustReview": !catalog.projects.is_empty()
    }))
}

pub fn install_adapters(project_id: &str) -> Result<Value> {
    let catalog = load_catalog()?;
    let project = catalog
        .projects
        .iter()
        .find(|item| item.project_id == project_id)
        .ok_or_else(|| anyhow::anyhow!("project {project_id} is not subscribed"))?;
    let mut result = install_project_adapters(project)?;
    result["daemonSupervision"] = Value::String(ensure_daemon_supervision(false)?.into());
    Ok(result)
}

pub(crate) fn install_project_adapters(project: &LocalProject) -> Result<Value> {
    let project_id = &project.project_id;
    let codex_directory = project.local_path.join(".codex");
    let hooks_path = codex_directory.join("hooks.json");
    let hooks_existed = hooks_path.exists();
    let mut hooks: Value = if hooks_existed {
        serde_json::from_slice(&fs::read(&hooks_path)?)?
    } else {
        json!({ "description": "RepoSync cooperative workspace leases", "hooks": {} })
    };
    let original_hooks = hooks.clone();
    let executable = std::env::current_exe()?;
    let legacy_marker = format!("resync codex-hook {}", shell_quote(project_id));
    let marker = format!(
        "{} codex-hook {}",
        shell_quote(&executable.to_string_lossy()),
        shell_quote(project_id)
    );
    for event in ["PreToolUse", "PostToolUse"] {
        let groups = hooks
            .pointer_mut(&format!("/hooks/{event}"))
            .and_then(Value::as_array_mut);
        if groups.is_none() {
            hooks["hooks"][event] = Value::Array(Vec::new());
        }
        let groups = hooks["hooks"][event].as_array_mut().unwrap();
        groups.retain(|group| {
            !group
                .get("hooks")
                .and_then(Value::as_array)
                .is_some_and(|items| {
                    items.iter().any(|item| {
                        item.get("command")
                            .and_then(Value::as_str)
                            .is_some_and(|command| command == marker || command == legacy_marker)
                    })
                })
        });
        groups.push(json!({ "matcher": "*", "hooks": [{
            "type": "command", "command": marker, "timeout": 3600,
            "statusMessage": if event == "PreToolUse" { "Refreshing RepoSync workspace" } else { "Releasing RepoSync workspace" }
        }] }));
    }
    fs::create_dir_all(&codex_directory)?;
    if !hooks_existed || hooks != original_hooks {
        write_json(&hooks_path, &hooks, 0o644)?;
    }
    let skill_directory = project
        .local_path
        .join(".agents")
        .join("skills")
        .join("reposync");
    let skill_path = skill_directory.join("SKILL.md");
    let skill_metadata_path = skill_directory.join("agents").join("openai.yaml");
    fs::create_dir_all(skill_metadata_path.parent().unwrap())?;
    fs::write(&skill_path, REPOSYNC_SKILL)?;
    fs::write(&skill_metadata_path, REPOSYNC_SKILL_OPENAI_METADATA)?;
    ensure_local_excludes(
        &project.local_path,
        &["/.codex/hooks.json", "/.agents/skills/reposync/"],
    )?;
    let hook_directory = PathBuf::from(
        git(
            &project.local_path,
            ["rev-parse", "--path-format=absolute", "--git-path", "hooks"],
            RunOptions::default(),
        )?
        .stdout
        .trim(),
    );
    let hook_path = hook_directory.join("post-commit");
    let previous = hook_directory.join("post-commit.pre-resync");
    fs::create_dir_all(&hook_directory)?;
    if hook_path.exists() && !fs::read_to_string(&hook_path)?.contains("# RepoSync dispatcher") {
        fs::rename(&hook_path, &previous)?;
    }
    let script = format!(
        "#!/bin/sh\n# RepoSync dispatcher\nset -u\n[ ! -x \"$0.pre-resync\" ] || \"$0.pre-resync\" \"$@\"\ncommit=\"$(git rev-parse HEAD 2>/dev/null)\" || exit 0\nbranch=\"$(git symbolic-ref --short HEAD 2>/dev/null)\" || exit 0\n{} report-commit {} \"$commit\" \"$branch\" >/dev/null || echo \"RepoSync could not schedule automatic publication\" >&2\n",
        shell_quote(&executable.to_string_lossy()),
        shell_quote(project_id)
    );
    fs::write(&hook_path, script)?;
    #[cfg(unix)]
    fs::set_permissions(&hook_path, fs::Permissions::from_mode(0o755))?;
    let checkout_hook = hook_directory.join("post-checkout");
    let previous_checkout = hook_directory.join("post-checkout.pre-resync");
    if checkout_hook.exists()
        && !fs::read_to_string(&checkout_hook)?.contains("# RepoSync dispatcher")
    {
        fs::rename(&checkout_hook, &previous_checkout)?;
    }
    let checkout_script = format!(
        "#!/bin/sh\n# RepoSync dispatcher\nset -u\n[ ! -x \"$0.pre-resync\" ] || \"$0.pre-resync\" \"$@\"\nnew_branch=\"$(git symbolic-ref --short HEAD 2>/dev/null)\" || exit 0\n{} report-checkout {} \"$1\" \"$2\" \"$new_branch\" >/dev/null || echo \"RepoSync could not record branch transition\" >&2\n",
        shell_quote(&executable.to_string_lossy()),
        shell_quote(project_id)
    );
    fs::write(&checkout_hook, checkout_script)?;
    #[cfg(unix)]
    fs::set_permissions(&checkout_hook, fs::Permissions::from_mode(0o755))?;
    Ok(
        json!({ "projectId": project_id, "codexHooks": hooks_path, "codexSkill": skill_path,
            "gitHooks": { "postCommit": hook_path, "postCheckout": checkout_hook },
            "requiresCodexTrustReview": true }),
    )
}

fn ensure_local_excludes(project_path: &Path, patterns: &[&str]) -> Result<()> {
    let exclude_path = PathBuf::from(
        git(
            project_path,
            [
                "rev-parse",
                "--path-format=absolute",
                "--git-path",
                "info/exclude",
            ],
            RunOptions::default(),
        )?
        .stdout
        .trim(),
    );
    let mut contents = fs::read_to_string(&exclude_path).unwrap_or_default();
    let mut changed = false;
    for pattern in patterns {
        if !contents.lines().any(|line| line.trim() == *pattern) {
            if !contents.is_empty() && !contents.ends_with('\n') {
                contents.push('\n');
            }
            contents.push_str(pattern);
            contents.push('\n');
            changed = true;
        }
    }
    if changed {
        if let Some(parent) = exclude_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(exclude_path, contents)?;
    }
    Ok(())
}

fn safe_hook_part(value: Option<&str>) -> String {
    value
        .unwrap_or("unknown")
        .chars()
        .map(|value| {
            if value.is_ascii_alphanumeric() || "._-".contains(value) {
                value
            } else {
                '_'
            }
        })
        .take(160)
        .collect()
}

fn hook_command(input: &Value) -> Option<&str> {
    (input.get("tool_name").and_then(Value::as_str) == Some("Bash"))
        .then(|| {
            input
                .get("tool_input")
                .and_then(|value| value.get("command"))
                .and_then(Value::as_str)
        })
        .flatten()
}

fn command_invokes_git(command: &str) -> bool {
    command
        .split(|value: char| value.is_whitespace() || ";|&()".contains(value))
        .map(|value| value.trim_matches(['\'', '"']))
        .any(|value| value == "git" || value.ends_with("/git"))
}

fn is_project_sync_command(input: &Value) -> bool {
    let Some(command) = hook_command(input) else {
        return false;
    };
    shell_command_segments(command).into_iter().any(|words| {
        words.len() >= 3
            && Path::new(&words[0])
                .file_name()
                .and_then(|value| value.to_str())
                == Some("resync")
            && words[1..3] == ["project", "sync"]
    })
}

fn shell_command_segments(command: &str) -> Vec<Vec<String>> {
    let mut segments = Vec::new();
    let mut segment = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut escaped = false;

    let flush_word = |word: &mut String, segment: &mut Vec<String>| {
        if !word.is_empty() {
            segment.push(std::mem::take(word));
        }
    };
    let flush_segment = |segment: &mut Vec<String>, segments: &mut Vec<Vec<String>>| {
        if !segment.is_empty() {
            segments.push(std::mem::take(segment));
        }
    };

    for character in command.chars() {
        if escaped {
            word.push(character);
            escaped = false;
            continue;
        }
        match quote {
            Some('\'') if character == '\'' => quote = None,
            Some('"') if character == '"' => quote = None,
            Some('"') if character == '\\' => escaped = true,
            Some(_) => word.push(character),
            None if character == '\\' => escaped = true,
            None if character == '\'' || character == '"' => quote = Some(character),
            None if character.is_whitespace() => flush_word(&mut word, &mut segment),
            None if ";|&()".contains(character) => {
                flush_word(&mut word, &mut segment);
                flush_segment(&mut segment, &mut segments);
            }
            None => word.push(character),
        }
    }
    if escaped {
        word.push('\\');
    }
    flush_word(&mut word, &mut segment);
    flush_segment(&mut segment, &mut segments);
    segments
}

fn reconciliation_marker(directory: &Path, project_id: &str) -> PathBuf {
    directory.join(format!(
        "reconciliation-{}.json",
        safe_hook_part(Some(project_id))
    ))
}

fn reconciliation_messages(
    marker: &Path,
    input: &Value,
    state: &Value,
) -> Result<(Option<String>, Option<String>)> {
    let active = state
        .get("reconciliation_mode")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if active {
        let branch = state
            .get("branch")
            .and_then(Value::as_str)
            .unwrap_or("current branch");
        let remote_ref = state
            .get("remote_ref")
            .and_then(Value::as_str)
            .unwrap_or("the RepoSync remote branch");
        if !marker.exists() {
            write_json(
                marker,
                &json!({ "branch": branch, "remoteRef": remote_ref }),
                0o600,
            )?;
            let message = format!(
                "RepoSync entered reconciliation mode after automatic reconciliation failed. Your workspace remains usable, but publication is paused. Before continuing with the user's task, reconcile `{branch}` with the latest `{remote_ref}`. After resolving any conflicts, run `resync project sync` to verify the result and leave reconciliation mode. Do not perform unrelated work until synchronization succeeds or reconciliation requires user input."
            );
            return Ok((Some(message.clone()), Some(message)));
        }
        if hook_command(input).is_some_and(command_invokes_git) {
            return Ok((
                None,
                Some(format!(
                    "RepoSync reconciliation mode: prioritize reconciling the latest `{remote_ref}` and run `resync project sync` before unrelated work."
                )),
            ));
        }
        return Ok((None, None));
    }
    if marker.exists() {
        fs::remove_file(marker)?;
        let message =
            "RepoSync reconciliation succeeded. Normal synchronization and publication have resumed."
                .to_owned();
        return Ok((Some(message.clone()), Some(message)));
    }
    Ok((None, None))
}

fn hook_output(system_message: Option<String>, additional_context: Option<String>) -> Value {
    let mut output = json!({});
    if let Some(message) = system_message {
        output["systemMessage"] = Value::String(message);
    }
    if let Some(context) = additional_context {
        output["hookSpecificOutput"] = json!({
            "hookEventName": "PreToolUse",
            "additionalContext": context
        });
    }
    output
}

fn codex_hook(project_id: &str) -> Result<Value> {
    let mut source = String::new();
    std::io::stdin()
        .take(1024 * 1024 + 1)
        .read_to_string(&mut source)?;
    if source.len() > 1024 * 1024 {
        bail!("hook input exceeds 1 MiB");
    }
    let input: Value = serde_json::from_str(&source)?;
    let directory = state_paths().root.join("codex-leases").join(safe_hook_part(
        input.get("session_id").and_then(Value::as_str),
    ));
    let lease_path = directory.join(format!(
        "{}.json",
        safe_hook_part(input.get("tool_use_id").and_then(Value::as_str))
    ));
    let mode_marker = reconciliation_marker(&directory, project_id);
    match input.get("hook_event_name").and_then(Value::as_str) {
        Some("PreToolUse") if is_project_sync_command(&input) => {
            match daemon_rpc(&json!({ "method": "accessState", "project_id": project_id })) {
                Ok(state) => {
                    fs::create_dir_all(&directory)?;
                    let (system_message, additional_context) =
                        reconciliation_messages(&mode_marker, &input, &state)?;
                    Ok(hook_output(system_message, additional_context))
                }
                Err(error) => Ok(json!({
                    "systemMessage": format!("RepoSync state is unavailable; the tool call will continue: {error:#}")
                })),
            }
        }
        Some("PreToolUse") => match daemon_rpc(&json!({
            "method": "acquireAccess", "project_id": project_id, "session_id": input.get("session_id"),
            "call_id": input.get("tool_use_id"), "access_hint": "unknown"
        })) {
            Ok(result) => {
                fs::create_dir_all(&directory)?;
                if let Some(lease_id) = result.get("lease_id").and_then(Value::as_str) {
                    write_json(
                        &lease_path,
                        &json!({
                            "leaseId": lease_id, "projectId": project_id, "toolUseId": input.get("tool_use_id")
                        }),
                        0o600,
                    )?;
                }
                let (system_message, mode_context) =
                    reconciliation_messages(&mode_marker, &input, &result)?;
                let normal_context = result
                    .get("model_message")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                Ok(hook_output(system_message, mode_context.or(normal_context)))
            }
            Err(error) => Ok(json!({
                "systemMessage": format!("RepoSync access coordination is unavailable; the tool call will continue: {error:#}")
            })),
        },
        Some("PostToolUse") if is_project_sync_command(&input) => {
            match daemon_rpc(&json!({ "method": "accessState", "project_id": project_id })) {
                Ok(state) => {
                    let (system_message, additional_context) =
                        reconciliation_messages(&mode_marker, &input, &state)?;
                    let mut output = hook_output(system_message, additional_context);
                    if let Some(value) = output.get_mut("hookSpecificOutput") {
                        value["hookEventName"] = Value::String("PostToolUse".into());
                    }
                    Ok(output)
                }
                Err(error) => Ok(json!({
                    "systemMessage": format!("RepoSync could not verify reconciliation state: {error:#}")
                })),
            }
        }
        Some("PostToolUse") => {
            if !lease_path.exists() {
                return Ok(json!({}));
            }
            match (|| -> Result<()> {
                let lease: Value = serde_json::from_slice(&fs::read(&lease_path)?)?;
                daemon_rpc(
                    &json!({ "method": "releaseAccess", "lease_id": lease.get("leaseId"), "outcome": "success", "mutation_hint": "possible" }),
                )?;
                fs::remove_file(lease_path)?;
                Ok(())
            })() {
                Ok(()) => Ok(json!({})),
                Err(error) => Ok(
                    json!({ "systemMessage": format!("RepoSync could not release the tool lease: {error:#}") }),
                ),
            }
        }
        _ => Ok(json!({})),
    }
}

fn parse_duration(value: &str) -> Result<Duration> {
    if let Some(ms) = value.strip_suffix("ms") {
        return Ok(Duration::from_millis(ms.parse()?));
    }
    if let Some(seconds) = value.strip_suffix('s') {
        return Ok(Duration::from_secs(seconds.parse()?));
    }
    Ok(Duration::from_secs(value.parse()?))
}

fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.into()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        command_invokes_git, is_project_sync_command, join_destination, reconciliation_messages,
    };
    use serde_json::json;

    #[test]
    fn explicit_join_path_does_not_require_a_git_worktree() -> anyhow::Result<()> {
        let outside_git = tempfile::tempdir()?;
        let destination = outside_git.path().join("checkout");
        assert_eq!(
            join_destination(Some(&destination), outside_git.path())?,
            destination
        );
        Ok(())
    }

    #[test]
    fn recognizes_git_and_reconciliation_command_segments() {
        assert!(command_invokes_git("git status --short"));
        assert!(command_invokes_git(
            "cd repo && /usr/bin/git rebase resync/main"
        ));
        assert!(!command_invokes_git("cargo test"));
        assert!(is_project_sync_command(&json!({
            "tool_name": "Bash",
            "tool_input": { "command": "resync project sync" }
        })));
        assert!(is_project_sync_command(&json!({
            "tool_name": "Bash",
            "tool_input": { "command": "git status && resync project sync" }
        })));
        assert!(is_project_sync_command(&json!({
            "tool_name": "Bash",
            "tool_input": {
                "command": "git add .gitignore && '/opt/RepoSync bin/resync' project sync prj_test"
            }
        })));
        assert!(!is_project_sync_command(&json!({
            "tool_name": "Bash",
            "tool_input": { "command": "echo resync project sync" }
        })));
    }

    #[test]
    fn reconciliation_messages_enter_remind_and_exit_once() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let marker = directory.path().join("mode.json");
        let state = json!({
            "reconciliation_mode": true,
            "branch": "main",
            "remote_ref": "resync/main"
        });
        let git_input = json!({
            "tool_name": "Bash",
            "tool_input": { "command": "git status" }
        });

        let (system, context) = reconciliation_messages(&marker, &git_input, &state)?;
        let entry = system.as_deref().unwrap();
        assert!(entry.contains("entered reconciliation mode"));
        assert!(entry.contains("`resync project sync`"));
        assert!(entry.contains("Before continuing with the user's task"));
        assert!(entry.contains("Do not perform unrelated work"));
        assert_eq!(context.as_deref(), Some(entry));

        let (system, context) = reconciliation_messages(&marker, &git_input, &state)?;
        assert_eq!(system, None);
        let reminder = context.as_deref().unwrap();
        assert!(reminder.starts_with("RepoSync reconciliation mode:"));
        assert!(reminder.contains("`resync project sync`"));
        assert!(reminder.contains("before unrelated work"));
        assert!(reminder.len() < entry.len());

        let non_git_input = json!({
            "tool_name": "Bash",
            "tool_input": { "command": "cargo test" }
        });
        assert_eq!(
            reconciliation_messages(&marker, &non_git_input, &state)?,
            (None, None)
        );

        let cleared = json!({ "reconciliation_mode": false });
        let (system, context) = reconciliation_messages(&marker, &non_git_input, &cleared)?;
        let exit = system.as_deref().unwrap();
        assert!(exit.contains("reconciliation succeeded"));
        assert_eq!(context.as_deref(), Some(exit));
        assert_eq!(
            reconciliation_messages(&marker, &non_git_input, &cleared)?,
            (None, None)
        );
        Ok(())
    }
}
