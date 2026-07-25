use crate::config::read_project_config;
use crate::git_state::{capture_workspace, current_branch};
use crate::process::{RunOptions, git, run};
use crate::protocol::PROTOCOL_VERSION;
use crate::remote::{authorization_env, fetch_catalog, fetch_remote, resolved_config};
use crate::state::{
    Config, LocalCatalog, LocalProject, StatePaths, load_catalog, read_json, state_paths,
    write_json,
};
use crate::transaction::{ReconcileResult, reconcile_workspace};
use anyhow::{Context, Result, bail};
use rand::Rng;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, Notify};
use uuid::Uuid;

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

#[derive(Debug, Default)]
struct GateState {
    exclusive: bool,
    leases: HashMap<String, Value>,
}

#[derive(Debug)]
struct LeaseGate {
    state: Mutex<GateState>,
    changed: Notify,
    max_lease: Duration,
}

impl LeaseGate {
    fn new(max_lease: Duration) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(GateState::default()),
            changed: Notify::new(),
            max_lease,
        })
    }

    async fn begin_exclusive(&self) {
        // First serialize exclusive owners. Only the task that changes the flag
        // from false to true may proceed to the lease-draining phase.
        loop {
            let notified = self.changed.notified();
            {
                let mut state = self.state.lock().await;
                if !state.exclusive {
                    state.exclusive = true;
                    break;
                }
            }
            notified.await;
        }
        loop {
            let notified = self.changed.notified();
            if self.state.lock().await.leases.is_empty() {
                return;
            }
            notified.await;
        }
    }

    async fn end_exclusive(&self) {
        self.state.lock().await.exclusive = false;
        self.changed.notify_waiters();
    }

    async fn activity(self: &Arc<Self>, metadata: Value) -> String {
        loop {
            let notified = self.changed.notified();
            {
                let mut state = self.state.lock().await;
                if !state.exclusive {
                    let id = Uuid::new_v4().to_string();
                    state.leases.insert(id.clone(), metadata);
                    let gate = Arc::clone(self);
                    let lease = id.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(gate.max_lease).await;
                        gate.release(&lease).await;
                    });
                    return id;
                }
            }
            notified.await;
        }
    }

    async fn release(&self, id: &str) -> bool {
        let removed = self.state.lock().await.leases.remove(id).is_some();
        if removed {
            self.changed.notify_waiters();
        }
        removed
    }
}

struct DaemonInner {
    paths: StatePaths,
    config: Config,
    binary_id: String,
    catalog: Mutex<LocalCatalog>,
    gates: StdMutex<HashMap<String, Arc<LeaseGate>>>,
    background_reconciliations: StdMutex<HashSet<String>>,
    lease_projects: Mutex<HashMap<String, String>>,
    poll_interval: Duration,
    max_lease: Duration,
}

#[derive(Clone)]
pub struct ResyncDaemon {
    inner: Arc<DaemonInner>,
}

impl ResyncDaemon {
    pub fn load(
        paths: Option<StatePaths>,
        poll_interval: Duration,
        max_lease: Duration,
    ) -> Result<Self> {
        let paths = paths.unwrap_or_else(state_paths);
        let config = resolved_config()?;
        let mut catalog: LocalCatalog = read_json(&paths.catalog, LocalCatalog::default())?;
        migrate_projects(&mut catalog.projects);
        write_json(&paths.catalog, &catalog, 0o600)?;
        Ok(Self {
            inner: Arc::new(DaemonInner {
                paths,
                config,
                binary_id: executable_fingerprint()?,
                catalog: Mutex::new(catalog),
                gates: StdMutex::new(HashMap::new()),
                background_reconciliations: StdMutex::new(HashSet::new()),
                lease_projects: Mutex::new(HashMap::new()),
                poll_interval,
                max_lease,
            }),
        })
    }

    pub fn from_parts(
        paths: StatePaths,
        config: Config,
        mut catalog: LocalCatalog,
        poll_interval: Duration,
        max_lease: Duration,
    ) -> Result<Self> {
        migrate_projects(&mut catalog.projects);
        write_json(&paths.catalog, &catalog, 0o600)?;
        Ok(Self {
            inner: Arc::new(DaemonInner {
                paths,
                config,
                binary_id: executable_fingerprint()?,
                catalog: Mutex::new(catalog),
                gates: StdMutex::new(HashMap::new()),
                background_reconciliations: StdMutex::new(HashSet::new()),
                lease_projects: Mutex::new(HashMap::new()),
                poll_interval,
                max_lease,
            }),
        })
    }

    pub async fn reconcile_now(&self, project_id: &str) -> Result<ReconcileResult> {
        self.reconcile_exclusive(project_id).await
    }

    pub async fn project_snapshot(&self, project_id: &str) -> Result<LocalProject> {
        let catalog = self.inner.catalog.lock().await;
        Ok(find_project(&catalog, project_id)?.clone())
    }

    fn gate(&self, project_id: &str) -> Arc<LeaseGate> {
        let mut gates = self.inner.gates.lock().expect("gate lock poisoned");
        gates
            .entry(project_id.into())
            .or_insert_with(|| LeaseGate::new(self.inner.max_lease))
            .clone()
    }

    async fn persist(&self, catalog: &LocalCatalog) -> Result<()> {
        write_json(&self.inner.paths.catalog, catalog, 0o600)
    }

    pub async fn refresh(&self) -> Result<()> {
        let remote = fetch_catalog(&self.inner.config)?;
        let token = self.inner.config.token.as_deref();
        let mut catalog = self.inner.catalog.lock().await;
        catalog.catalog_generation = remote.catalog_generation;
        for project in &mut catalog.projects {
            let Some(update) = remote
                .projects
                .iter()
                .find(|item| item.project_id == project.project_id)
            else {
                continue;
            };
            project.remote_url = update.remote_url.clone();
            project.default_branch = update.default_branch.clone();
            project.server_generation = update.project_generation;
            match fetch_remote(project, token) {
                Ok(fetched) => {
                    project.advertised_heads = fetched;
                    project.active_branch = current_branch(&project.local_path).ok();
                    let target = project
                        .active_branch
                        .as_ref()
                        .and_then(|branch| project.advertised_heads.get(branch));
                    let applied = project
                        .active_branch
                        .as_ref()
                        .and_then(|branch| project.last_applied_heads.get(branch));
                    if target.is_some() && target != applied && project.state != "CONFLICTED" {
                        project.state = "REMOTE_AHEAD".into();
                    } else if project.state != "CONFLICTED" {
                        project.state = "CURRENT".into();
                    }
                    project.last_error = None;
                }
                Err(error) => {
                    project.state = "OFFLINE".into();
                    project.last_error = Some(format!("{error:#}"));
                }
            }
        }
        let pending = catalog
            .projects
            .iter()
            .filter(|project| project.state == "REMOTE_AHEAD")
            .map(|project| project.project_id.clone())
            .collect::<Vec<_>>();
        self.persist(&catalog).await?;
        drop(catalog);
        for project_id in pending {
            self.schedule_reconcile(project_id);
        }
        Ok(())
    }

    fn schedule_reconcile(&self, project_id: String) {
        {
            let mut running = self
                .inner
                .background_reconciliations
                .lock()
                .expect("background reconciliation lock poisoned");
            if !running.insert(project_id.clone()) {
                return;
            }
        }
        let daemon = self.clone();
        tokio::spawn(async move {
            if let Err(error) = daemon.reconcile_exclusive(&project_id).await {
                let mut catalog = daemon.inner.catalog.lock().await;
                if let Ok(project) = find_project_mut(&mut catalog, &project_id) {
                    if project.state == "RECONCILING" {
                        project.state = "REMOTE_AHEAD".into();
                    }
                    project.last_error = Some(format!("{error:#}"));
                    let _ = daemon.persist(&catalog).await;
                }
            }
            daemon
                .inner
                .background_reconciliations
                .lock()
                .expect("background reconciliation lock poisoned")
                .remove(&project_id);
        });
    }

    async fn reconcile_exclusive(&self, project_id: &str) -> Result<ReconcileResult> {
        let gate = self.gate(project_id);
        gate.begin_exclusive().await;
        let result = async {
            let mut catalog = self.inner.catalog.lock().await;
            let project = find_project_mut(&mut catalog, project_id)?;
            let result = self.reconcile_project(project);
            self.persist(&catalog).await?;
            result
        }
        .await;
        gate.end_exclusive().await;
        result
    }

    fn reconcile_project(&self, project: &mut LocalProject) -> Result<ReconcileResult> {
        let branch = current_branch(&project.local_path).map_err(|_| {
            anyhow::anyhow!("detached or unborn HEAD is unsupported in RepoSync V1")
        })?;
        project.active_branch = Some(branch.clone());
        let Some(target) = project.advertised_heads.get(&branch).cloned() else {
            project.state = "CURRENT".into();
            project.conflict = None;
            return Ok(simple_outcome("current"));
        };
        let snapshot = capture_workspace(&project.local_path)?;
        if snapshot.head == target {
            project.last_applied_heads.insert(branch, target);
            project.state = "CURRENT".into();
            project.conflict = None;
            return Ok(simple_outcome("current"));
        }
        let target_is_base = is_ancestor(&project.local_path, &target, &snapshot.head)?;
        if target_is_base {
            project.last_applied_heads.insert(branch, target);
            project.state = "CURRENT".into();
            project.conflict = None;
            return Ok(simple_outcome("current"));
        }
        let head_is_base = is_ancestor(&project.local_path, &snapshot.head, &target)?;
        let mut previous = project.last_applied_heads.get(&branch).cloned();
        if previous.is_none() || previous.as_deref() == Some(&target) {
            if head_is_base {
                previous = Some(snapshot.head.clone());
            } else {
                let detail = format!("local and hosted histories for branch {branch} diverge");
                project.state = "CONFLICTED".into();
                project.conflict = Some(crate::state::Conflict {
                    kind: "diverged-history".into(),
                    detail: detail.clone(),
                    paths: Vec::new(),
                });
                return Ok(ReconcileResult {
                    outcome: "conflict".into(),
                    detail: Some(detail),
                    ..simple_outcome("")
                });
            }
        }
        project.state = "RECONCILING".into();
        let result = reconcile_workspace(
            &project.local_path,
            previous.as_deref().unwrap(),
            &target,
            Some(&self.inner.paths.transactions),
        )?;
        if result.outcome == "applied" {
            project.last_applied_heads.insert(branch, target);
            project.workspace_generation += 1;
            project.state = "CURRENT".into();
            project.conflict = None;
        } else if result.outcome == "conflict" {
            project.state = "CONFLICTED".into();
            project.conflict = Some(crate::state::Conflict {
                kind: "reconciliation".into(),
                detail: result
                    .detail
                    .clone()
                    .unwrap_or_else(|| "reconciliation conflict".into()),
                paths: result.paths.clone(),
            });
        }
        Ok(result)
    }

    async fn acquire(&self, request: &Value) -> Result<Value> {
        let project_id = string_field(request, "project_id")?;
        {
            let catalog = self.inner.catalog.lock().await;
            let project = find_project(&catalog, project_id)?;
            if matches!(project.state.as_str(), "CONFLICTED" | "FAILED") {
                return Ok(blocked(project));
            }
        }
        let gate = self.gate(project_id);
        gate.begin_exclusive().await;
        let reconciliation = {
            let mut catalog = self.inner.catalog.lock().await;
            let project = find_project_mut(&mut catalog, project_id)?;
            let result = self.reconcile_project(project);
            self.persist(&catalog).await?;
            result
        };
        gate.end_exclusive().await;
        let reconciliation = reconciliation?;
        {
            let catalog = self.inner.catalog.lock().await;
            let project = find_project(&catalog, project_id)?;
            if project.state == "CONFLICTED" {
                return Ok(blocked(project));
            }
        }
        let lease_id = gate
            .activity(json!({
                "sessionId": request.get("session_id"), "callId": request.get("call_id")
            }))
            .await;
        self.inner
            .lease_projects
            .lock()
            .await
            .insert(lease_id.clone(), project_id.into());
        let catalog = self.inner.catalog.lock().await;
        let project = find_project(&catalog, project_id)?;
        let head = capture_workspace(&project.local_path)?.head;
        Ok(json!({
            "granted": true, "lease_id": lease_id, "workspace_generation": project.workspace_generation,
            "branch_oid": head, "reconciliation": if reconciliation.outcome == "applied" { "applied" } else { "none" },
            "changed_paths_summary": reconciliation.changed_paths.iter().take(50).collect::<Vec<_>>(),
            "model_message": (reconciliation.outcome == "applied").then(|| format!(
                "RepoSync advanced the workspace and preserved local work. Upstream changed: {}.",
                if reconciliation.changed_paths.is_empty() { "metadata only".into() } else { reconciliation.changed_paths.iter().take(20).cloned().collect::<Vec<_>>().join(", ") }
            ))
        }))
    }

    async fn release(&self, request: &Value) -> Result<Value> {
        let lease_id = string_field(request, "lease_id")?;
        let project_id = self
            .inner
            .lease_projects
            .lock()
            .await
            .remove(lease_id)
            .ok_or_else(|| anyhow::anyhow!("unknown or already released lease"))?;
        if !self.gate(&project_id).release(lease_id).await {
            bail!("unknown or already released lease");
        }
        Ok(json!({ "released": true }))
    }

    async fn sync(&self, request: &Value) -> Result<Value> {
        let project_id = string_field(request, "project_id")?;
        self.refresh().await?;
        let gate = self.gate(project_id);
        gate.begin_exclusive().await;
        let result = {
            let mut catalog = self.inner.catalog.lock().await;
            let project = find_project_mut(&mut catalog, project_id)?;
            let result = self.reconcile_project(project);
            self.persist(&catalog).await?;
            result
        };
        gate.end_exclusive().await;
        Ok(serde_json::to_value(result?)?)
    }

    async fn publish(&self, request: &Value) -> Result<Value> {
        let project_id = string_field(request, "project_id")?.to_owned();
        let requested_branch = request
            .get("branch")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let gate = self.gate(&project_id);
        gate.begin_exclusive().await;
        let result = self
            .publish_exclusive(&project_id, requested_branch.as_deref())
            .await;
        gate.end_exclusive().await;
        result
    }

    async fn publish_exclusive(
        &self,
        project_id: &str,
        requested_branch: Option<&str>,
    ) -> Result<Value> {
        for attempt in 1..=5 {
            let mut catalog = self.inner.catalog.lock().await;
            let project = find_project_mut(&mut catalog, project_id)?;
            project.advertised_heads = fetch_remote(project, self.inner.config.token.as_deref())?;
            let branch = current_branch(&project.local_path)?;
            if requested_branch.is_some_and(|value| value != branch) {
                bail!(
                    "cannot publish {}; active branch is now {branch}",
                    requested_branch.unwrap()
                );
            }
            let reconciliation = self.reconcile_project(project)?;
            if reconciliation.outcome == "conflict" {
                self.persist(&catalog).await?;
                return Ok(serde_json::to_value(reconciliation)?);
            }
            let head = git(
                &project.local_path,
                ["rev-parse", "HEAD"],
                RunOptions::default(),
            )?
            .stdout
            .trim()
            .to_owned();
            let configuration = read_project_config(&project.local_path)?;
            if !configuration.validations.is_empty() {
                let validation_path = self
                    .inner
                    .paths
                    .root
                    .join(format!("validation-{}", Uuid::new_v4()));
                let validation = (|| -> Result<()> {
                    git(
                        &project.local_path,
                        [
                            "worktree",
                            "add",
                            "--detach",
                            validation_path.to_string_lossy().as_ref(),
                            &head,
                        ],
                        RunOptions::default(),
                    )?;
                    for command in configuration.validations {
                        run(
                            &command[0],
                            command.iter().skip(1),
                            RunOptions {
                                cwd: Some(validation_path.clone()),
                                ..RunOptions::default()
                            },
                        )?;
                    }
                    Ok(())
                })();
                let cleanup = RunOptions {
                    allow_failure: true,
                    ..RunOptions::default()
                };
                let _ = git(
                    &project.local_path,
                    [
                        "worktree",
                        "remove",
                        "--force",
                        validation_path.to_string_lossy().as_ref(),
                    ],
                    cleanup,
                );
                validation?;
            }
            project.durability = "PUBLISHING".into();
            let local_path = project.local_path.clone();
            let remote_name = project.remote_name.clone();
            self.persist(&catalog).await?;
            let options = RunOptions {
                env: authorization_env(self.inner.config.token.as_deref()),
                allow_failure: true,
                ..RunOptions::default()
            };
            let pushed = git(
                &local_path,
                ["push", &remote_name, &format!("HEAD:refs/heads/{branch}")],
                options,
            )?;
            if pushed.code == 0 {
                let project = find_project_mut(&mut catalog, project_id)?;
                project
                    .last_applied_heads
                    .insert(branch.clone(), head.clone());
                project.advertised_heads.insert(branch, head.clone());
                project.state = "CURRENT".into();
                project.durability = "REMOTELY_DURABLE".into();
                self.persist(&catalog).await?;
                return Ok(json!({ "outcome": "published", "head": head, "attempt": attempt }));
            }
            if !["non-fast-forward", "fetch first", "rejected"]
                .iter()
                .any(|value| pushed.stderr.to_lowercase().contains(value))
            {
                bail!("{}", pushed.stderr.trim());
            }
        }
        bail!("publication lost the remote race five times")
    }

    pub async fn handle(&self, request: Value) -> Result<Value> {
        match request.get("method").and_then(Value::as_str).unwrap_or("") {
            "ping" => Ok(json!({
                "protocol": PROTOCOL_VERSION,
                "pid": std::process::id(),
                "binaryId": self.inner.binary_id
            })),
            "reload" => {
                let mut loaded = load_catalog()?;
                migrate_projects(&mut loaded.projects);
                *self.inner.catalog.lock().await = loaded;
                self.refresh().await?;
                Ok(json!({ "reloaded": self.inner.catalog.lock().await.projects.len() }))
            }
            "status" => {
                let catalog = self.inner.catalog.lock().await;
                let projects: Vec<&LocalProject> =
                    match request.get("project_id").and_then(Value::as_str) {
                        Some(id) => vec![find_project(&catalog, id)?],
                        None => catalog.projects.iter().collect(),
                    };
                Ok(json!({ "projects": projects }))
            }
            "acquireAccess" => self.acquire(&request).await,
            "releaseAccess" => self.release(&request).await,
            "sync" => self.sync(&request).await,
            "publish" => self.publish(&request).await,
            "reportCommit" => {
                let project_id = string_field(&request, "project_id")?.to_owned();
                {
                    let mut catalog = self.inner.catalog.lock().await;
                    find_project_mut(&mut catalog, &project_id)?.durability =
                        "COMMITTED_UNPUBLISHED".into();
                    self.persist(&catalog).await?;
                }
                let daemon = self.clone();
                let branch = request
                    .get("branch")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                tokio::spawn(async move {
                    if let Err(error) = daemon
                        .publish(&json!({ "project_id": project_id, "branch": branch }))
                        .await
                    {
                        eprintln!("automatic publication failed: {error:#}");
                    }
                });
                Ok(json!({ "scheduled": true, "commit_oid": request.get("commit_oid") }))
            }
            method => bail!("unknown daemon method {method}"),
        }
    }
}

fn migrate_projects(projects: &mut [LocalProject]) {
    for project in projects {
        if let Some(Value::String(branch)) = project.extra.remove("syncBranch") {
            project.default_branch = branch;
        }
        if let Some(Value::String(oid)) = project.extra.remove("advertisedRemoteOid") {
            project
                .advertised_heads
                .insert(project.default_branch.clone(), oid);
        }
        if let Some(Value::String(oid)) = project.extra.remove("lastAppliedRemoteOid") {
            project
                .last_applied_heads
                .insert(project.default_branch.clone(), oid);
        }
    }
}

fn find_project<'a>(catalog: &'a LocalCatalog, id: &str) -> Result<&'a LocalProject> {
    catalog
        .projects
        .iter()
        .find(|project| project.project_id == id || project.local_path.to_string_lossy() == id)
        .ok_or_else(|| anyhow::anyhow!("unknown subscribed project {id}"))
}

fn find_project_mut<'a>(catalog: &'a mut LocalCatalog, id: &str) -> Result<&'a mut LocalProject> {
    catalog
        .projects
        .iter_mut()
        .find(|project| project.project_id == id || project.local_path.to_string_lossy() == id)
        .ok_or_else(|| anyhow::anyhow!("unknown subscribed project {id}"))
}

fn string_field<'a>(request: &'a Value, name: &str) -> Result<&'a str> {
    request
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("request is missing {name}"))
}

fn is_ancestor(path: &std::path::Path, ancestor: &str, descendant: &str) -> Result<bool> {
    Ok(git(
        path,
        ["merge-base", "--is-ancestor", ancestor, descendant],
        RunOptions {
            allow_failure: true,
            ..RunOptions::default()
        },
    )?
    .code
        == 0)
}

fn simple_outcome(outcome: &str) -> ReconcileResult {
    ReconcileResult {
        outcome: outcome.into(),
        transaction_id: None,
        recovery_ref: None,
        head: None,
        old_head: None,
        new_head: None,
        changed_paths: Vec::new(),
        detail: None,
        paths: Vec::new(),
    }
}

fn blocked(project: &LocalProject) -> Value {
    json!({
        "blocked": true, "state": project.state.to_lowercase(),
        "model_message": project.conflict.as_ref().map(|value| value.detail.clone()).or(project.last_error.clone()).unwrap_or_else(|| "workspace unavailable".into()),
        "recovery_actions": ["resync conflicts", "resync recover"]
    })
}

fn acquire_process_lock(paths: &StatePaths) -> Result<()> {
    if let Some(parent) = paths.daemon_lock.parent() {
        fs::create_dir_all(parent)?;
    }
    for _ in 0..2 {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        match options.open(&paths.daemon_lock) {
            Ok(mut file) => {
                writeln!(file, "{}", std::process::id())?;
                file.sync_all()?;
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let pid = fs::read_to_string(&paths.daemon_lock)
                    .ok()
                    .and_then(|value| value.trim().parse::<i32>().ok());
                if let Some(pid) = pid {
                    // SAFETY: kill with signal 0 only probes whether the recorded process exists.
                    if unsafe { libc::kill(pid, 0) } == 0 {
                        bail!("RepoSync daemon is already running as PID {pid}");
                    }
                }
                let _ = fs::remove_file(&paths.daemon_lock);
            }
            Err(error) => return Err(error.into()),
        }
    }
    bail!("could not acquire RepoSync daemon lock")
}

async fn serve_connection(daemon: ResyncDaemon, stream: UnixStream) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    loop {
        line.clear();
        let length = reader.read_line(&mut line).await?;
        if length == 0 {
            return Ok(());
        }
        if line.len() > 1024 * 1024 {
            bail!("daemon request exceeds 1 MiB");
        }
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Value>(&line) {
            Ok(request) => match daemon.handle(request).await {
                Ok(result) => json!({ "ok": true, "result": result }),
                Err(error) => json!({ "ok": false, "error": format!("{error:#}") }),
            },
            Err(error) => json!({ "ok": false, "error": error.to_string() }),
        };
        writer
            .write_all(serde_json::to_string(&response)?.as_bytes())
            .await?;
        writer.write_all(b"\n").await?;
    }
}

pub async fn run_daemon(poll_interval: Duration) -> Result<()> {
    let daemon = ResyncDaemon::load(None, poll_interval, Duration::from_secs(3600))?;
    let paths = daemon.inner.paths.clone();
    acquire_process_lock(&paths)?;
    if paths.socket.exists() {
        fs::remove_file(&paths.socket)?;
    }
    if let Some(parent) = paths.socket.parent() {
        fs::create_dir_all(parent)?;
    }
    let listener = UnixListener::bind(&paths.socket)
        .with_context(|| format!("bind {}", paths.socket.display()))?;
    #[cfg(unix)]
    fs::set_permissions(&paths.socket, fs::Permissions::from_mode(0o600))?;
    let refresh = daemon.clone();
    tokio::spawn(async move {
        loop {
            let jitter = rand::thread_rng().gen_range(0..1000);
            tokio::time::sleep(refresh.inner.poll_interval + Duration::from_millis(jitter)).await;
            let _ = refresh.refresh().await;
        }
    });
    let _ = daemon.refresh().await;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        tokio::select! {
            incoming = listener.accept() => {
                let (stream, _) = incoming?;
                let service = daemon.clone();
                tokio::spawn(async move { if let Err(error) = serve_connection(service, stream).await { eprintln!("daemon connection failed: {error:#}"); } });
            }
            _ = tokio::signal::ctrl_c() => break,
            _ = terminate.recv() => break,
        }
    }
    let _ = fs::remove_file(&paths.socket);
    let _ = fs::remove_file(&paths.daemon_lock);
    Ok(())
}

pub fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?)
}

fn executable_fingerprint() -> Result<String> {
    let executable = std::env::current_exe()?;
    let mut file = File::open(&executable)
        .with_context(|| format!("open current executable {}", executable.display()))?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn daemon_matches_client(ping: &Value, client_binary_id: &str) -> bool {
    ping.get("binaryId").and_then(Value::as_str) == Some(client_binary_id)
}

fn terminate_stale_daemon(ping: &Value) -> Result<()> {
    let pid = ping
        .get("pid")
        .and_then(Value::as_u64)
        .and_then(|value| libc::pid_t::try_from(value).ok())
        .ok_or_else(|| anyhow::anyhow!("stale RepoSync daemon did not report a valid pid"))?;
    if pid == libc::pid_t::try_from(std::process::id()).unwrap_or_default() {
        bail!("refusing to terminate the current RepoSync process");
    }
    // SAFETY: pid came from the daemon that answered on this user's mode-0600
    // Unix socket. SIGTERM asks that daemon to perform its normal cleanup.
    let result = unsafe { libc::kill(pid, libc::SIGTERM) };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error).context("terminate stale RepoSync daemon");
        }
    }
    for _ in 0..50 {
        std::thread::sleep(Duration::from_millis(100));
        match crate::rpc::rpc(&json!({ "method": "ping" }), None) {
            Err(_) => return Ok(()),
            Ok(current) if current.get("pid").and_then(Value::as_u64) != Some(pid as u64) => {
                return Ok(());
            }
            Ok(_) => {}
        }
    }
    bail!("stale RepoSync daemon {pid} did not stop after SIGTERM")
}

pub fn daemon_rpc(request: &Value) -> Result<Value> {
    let binary_id = executable_fingerprint()?;
    if let Ok(ping) = crate::rpc::rpc(&json!({ "method": "ping" }), None) {
        if daemon_matches_client(&ping, &binary_id) {
            return crate::rpc::rpc(request, None);
        }
        terminate_stale_daemon(&ping)?;
    }
    let paths = state_paths();
    fs::create_dir_all(&paths.root)?;
    let log_path = paths.root.join("daemon.log");
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let executable = std::env::current_exe()?;
    let mut command = std::process::Command::new(executable);
    command
        .arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid has no memory-safety preconditions and is called after fork before exec.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    command.spawn()?;
    let mut last = None;
    for _ in 0..50 {
        std::thread::sleep(Duration::from_millis(100));
        match crate::rpc::rpc(&json!({ "method": "ping" }), None) {
            Ok(ping) if daemon_matches_client(&ping, &binary_id) => {
                return crate::rpc::rpc(request, None);
            }
            Ok(_) => {
                last = Some(anyhow::anyhow!(
                    "another RepoSync daemon with a different binary is running"
                ));
            }
            Err(error) => last = Some(error),
        }
    }
    let detail = fs::read_to_string(log_path)
        .unwrap_or_default()
        .lines()
        .rev()
        .take(5)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");
    bail!(
        "{}{}",
        last.map(|error| error.to_string())
            .unwrap_or_else(|| "RepoSync daemon did not become ready".into()),
        if detail.is_empty() {
            String::new()
        } else {
            format!("; daemon log: {detail}")
        }
    )
}

#[cfg(test)]
mod tests {
    use super::{LeaseGate, daemon_matches_client};
    use serde_json::json;
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn stale_daemon_builds_are_not_reused() {
        assert!(daemon_matches_client(
            &json!({ "binaryId": "current", "pid": 42 }),
            "current"
        ));
        assert!(!daemon_matches_client(
            &json!({ "binaryId": "stale", "pid": 42 }),
            "current"
        ));
        assert!(!daemon_matches_client(&json!({ "pid": 42 }), "current"));
    }

    #[tokio::test]
    async fn exclusive_owners_are_serialized() {
        let gate = LeaseGate::new(Duration::from_secs(60));
        let lease = gate.activity(json!({})).await;
        let first_gate = Arc::clone(&gate);
        let mut first = tokio::spawn(async move { first_gate.begin_exclusive().await });
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(!first.is_finished());

        let second_gate = Arc::clone(&gate);
        let mut second = tokio::spawn(async move { second_gate.begin_exclusive().await });
        gate.release(&lease).await;
        tokio::time::timeout(Duration::from_secs(1), &mut first)
            .await
            .expect("first owner should acquire the gate")
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut second)
                .await
                .is_err()
        );

        gate.end_exclusive().await;
        tokio::time::timeout(Duration::from_secs(1), &mut second)
            .await
            .expect("second owner should acquire after release")
            .unwrap();
        gate.end_exclusive().await;
    }
}
