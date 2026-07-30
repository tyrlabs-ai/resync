use crate::config::read_project_config;
use crate::git_state::{capture_workspace, current_branch};
use crate::process::{RunOptions, git, run};
use crate::protocol::PROTOCOL_VERSION;
use crate::publication::{
    ACKNOWLEDGED, ProviderPublication, Publication, PublicationAck, PublicationQueue,
    acknowledge_with_provider,
};
use crate::remote::{authorization_env, fetch_remote_branch, heartbeat, resolved_config};
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
    publications: Mutex<PublicationQueue>,
    publication_processing: Mutex<()>,
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
        let publications = PublicationQueue::load(&paths)?;
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
                publications: Mutex::new(publications),
                publication_processing: Mutex::new(()),
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
        let publications = PublicationQueue::load(&paths)?;
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
                publications: Mutex::new(publications),
                publication_processing: Mutex::new(()),
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
        if self.inner.config.server.is_none() || self.inner.config.token.is_none() {
            return Ok(());
        }
        let snapshots = self.inner.catalog.lock().await.projects.clone();
        let config = self.inner.config.clone();
        let remote = tokio::task::spawn_blocking(move || heartbeat(&config, &snapshots)).await??;
        let token = self.inner.config.token.as_deref();
        let mut catalog = self.inner.catalog.lock().await;
        let mut pending_reconciliation = Vec::new();
        let mut missing_publications = Vec::new();
        for project in &mut catalog.projects {
            let actual_branch = current_branch(&project.local_path)
                .ok()
                .or_else(|| project.active_branch.clone())
                .unwrap_or_else(|| project.default_branch.clone());
            project.active_branch = Some(actual_branch.clone());
            let Some(update) = remote
                .iter()
                .find(|item| item.project_id == project.project_id)
            else {
                project.state = "ACCESS_LOST".into();
                project.last_error =
                    Some("provider heartbeat omitted a locally subscribed project".into());
                continue;
            };
            if update.membership != "ACTIVE" {
                project.state = "ACCESS_LOST".into();
                project.last_error =
                    Some("device credential no longer authorizes this subscribed project".into());
                continue;
            }
            project.server_generation = update.project_generation;
            match fetch_remote_branch(project, &actual_branch, token) {
                Ok(fetched) => {
                    match fetched.or_else(|| update.head.clone()) {
                        Some(head) => {
                            project.advertised_heads.insert(actual_branch.clone(), head);
                        }
                        None => {
                            project.advertised_heads.remove(&actual_branch);
                        }
                    }
                    let local_head = git(
                        &project.local_path,
                        ["rev-parse", "HEAD"],
                        RunOptions::default(),
                    )?
                    .stdout
                    .trim()
                    .to_owned();
                    let target = project.advertised_heads.get(&actual_branch).cloned();
                    if project.state != "CONFLICTED" {
                        match target {
                            None => {
                                project.state = "CURRENT".into();
                                missing_publications.push((
                                    project.project_id.clone(),
                                    actual_branch.clone(),
                                    local_head,
                                ));
                            }
                            Some(target) if target == local_head => {
                                project.state = "CURRENT".into();
                                project
                                    .last_applied_heads
                                    .insert(actual_branch.clone(), target);
                            }
                            Some(target)
                                if is_ancestor(&project.local_path, &target, &local_head)? =>
                            {
                                project.state = "CURRENT".into();
                                missing_publications.push((
                                    project.project_id.clone(),
                                    actual_branch.clone(),
                                    local_head,
                                ));
                            }
                            Some(_) => {
                                project.state = "REMOTE_AHEAD".into();
                                pending_reconciliation.push(project.project_id.clone());
                            }
                        }
                    }
                    if project.durability == "REMOTELY_DURABLE" {
                        project.last_error = None;
                    }
                }
                Err(error) => {
                    project.state = "OFFLINE".into();
                    project.last_error = Some(format!("{error:#}"));
                }
            }
        }
        self.persist(&catalog).await?;
        drop(catalog);
        for (project_id, branch, head) in missing_publications {
            self.enqueue_publication(&project_id, &branch, &head)
                .await?;
        }
        for project_id in pending_reconciliation {
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

    async fn ensure_branch_transition(&self, project_id: &str) -> Result<()> {
        let project = {
            let catalog = self.inner.catalog.lock().await;
            find_project(&catalog, project_id)?.clone()
        };
        let actual = current_branch(&project.local_path)?;
        let Some(previous) = project.active_branch else {
            let mut catalog = self.inner.catalog.lock().await;
            find_project_mut(&mut catalog, project_id)?.active_branch = Some(actual);
            self.persist(&catalog).await?;
            return Ok(());
        };
        if previous == actual {
            return Ok(());
        }
        let outgoing = git(
            &project.local_path,
            ["rev-parse", "--verify", &format!("refs/heads/{previous}")],
            RunOptions {
                allow_failure: true,
                ..RunOptions::default()
            },
        )?;
        if outgoing.code == 0 {
            self.enqueue_publication(project_id, &previous, outgoing.stdout.trim())
                .await?;
        }
        {
            let mut catalog = self.inner.catalog.lock().await;
            find_project_mut(&mut catalog, project_id)?.active_branch = Some(actual);
            self.persist(&catalog).await?;
        }
        if self.inner.config.server.is_some() && self.inner.config.token.is_some() {
            self.refresh().await?;
        }
        Ok(())
    }

    async fn acquire(&self, request: &Value) -> Result<Value> {
        let project_id = string_field(request, "project_id")?;
        self.ensure_branch_transition(project_id).await?;
        let should_reconcile = {
            let catalog = self.inner.catalog.lock().await;
            let project = find_project(&catalog, project_id)?;
            matches!(
                project.state.as_str(),
                "REMOTE_AHEAD" | "WAITING_FOR_LEASES" | "RECONCILING"
            )
        };
        let gate = self.gate(project_id);
        let reconciliation = if should_reconcile {
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
            result?
        } else {
            simple_outcome("deferred")
        };
        let reconciliation_mode = {
            let catalog = self.inner.catalog.lock().await;
            find_project(&catalog, project_id)?.state == "CONFLICTED"
        };
        let lease_id = if reconciliation_mode {
            None
        } else {
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
            Some(lease_id)
        };
        let catalog = self.inner.catalog.lock().await;
        let project = find_project(&catalog, project_id)?;
        let head = git(
            &project.local_path,
            ["rev-parse", "HEAD"],
            RunOptions::default(),
        )?
        .stdout
        .trim()
        .to_owned();
        let branch = project
            .active_branch
            .as_deref()
            .unwrap_or(&project.default_branch);
        Ok(json!({
            "granted": true, "lease_id": lease_id, "workspace_generation": project.workspace_generation,
            "branch_oid": head, "reconciliation": if reconciliation.outcome == "applied" { "applied" } else { "none" },
            "reconciliation_mode": reconciliation_mode,
            "branch": branch,
            "remote_ref": format!("{}/{}", project.remote_name, branch),
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

    async fn enqueue_publication(
        &self,
        project_id: &str,
        branch: &str,
        commit_oid: &str,
    ) -> Result<String> {
        let project = {
            let catalog = self.inner.catalog.lock().await;
            find_project(&catalog, project_id)?.clone()
        };
        let verified = git(
            &project.local_path,
            ["rev-parse", "--verify", &format!("{commit_oid}^{{commit}}")],
            RunOptions::default(),
        )?
        .stdout
        .trim()
        .to_owned();
        if verified != commit_oid {
            bail!("reported commit does not resolve to the requested OID");
        }
        {
            let queue = self.inner.publications.lock().await;
            if let Some(existing) = queue.publications.iter().find(|value| {
                value.pending()
                    && value.project_id == project.project_id
                    && value.branch == branch
                    && value.source_commit_oid == commit_oid
            }) {
                return Ok(existing.publication_id.clone());
            }
        }
        let publication = Publication::new(
            &project.project_id,
            project.checkout_id.clone(),
            branch,
            commit_oid,
            project.advertised_heads.get(branch).cloned(),
        );
        git(
            &project.local_path,
            [
                "update-ref",
                &format!("refs/resync/publications/{}", publication.publication_id),
                commit_oid,
            ],
            RunOptions::default(),
        )?;
        {
            let mut queue = self.inner.publications.lock().await;
            queue.publications.push(publication.clone());
            queue.persist(&self.inner.paths)?;
        }
        {
            let mut catalog = self.inner.catalog.lock().await;
            let project = find_project_mut(&mut catalog, project_id)?;
            project.durability = "COMMITTED_UNPUBLISHED".into();
            self.persist(&catalog).await?;
        }
        self.schedule_publications();
        Ok(publication.publication_id)
    }

    fn schedule_publications(&self) {
        let daemon = self.clone();
        tokio::spawn(async move {
            let _ = daemon.process_publications(false).await;
        });
    }

    async fn process_publications(&self, force: bool) -> Result<()> {
        let _processing = self.inner.publication_processing.lock().await;
        let mut pending = {
            let queue = self.inner.publications.lock().await;
            queue
                .publications
                .iter()
                .filter(|value| value.pending() && (force || value.due()))
                .map(|value| (value.created_at.clone(), value.publication_id.clone()))
                .collect::<Vec<_>>()
        };
        pending.sort_by(|left, right| right.0.cmp(&left.0));
        for (_, publication_id) in pending {
            let publication = {
                let queue = self.inner.publications.lock().await;
                queue
                    .publications
                    .iter()
                    .find(|value| value.publication_id == publication_id && value.pending())
                    .cloned()
            };
            let Some(publication) = publication else {
                continue;
            };
            if let Err(error) = self.deliver_publication(&publication).await {
                let mut queue = self.inner.publications.lock().await;
                if let Some(value) = queue
                    .publications
                    .iter_mut()
                    .find(|value| value.publication_id == publication_id)
                {
                    value.record_failure(&error, Duration::from_secs(60));
                }
                queue.persist(&self.inner.paths)?;
                drop(queue);
                let mut catalog = self.inner.catalog.lock().await;
                if let Ok(project) = find_project_mut(&mut catalog, &publication.project_id) {
                    project.durability = "COMMITTED_UNPUBLISHED".into();
                    project.last_error = Some(format!("{error:#}"));
                }
                self.persist(&catalog).await?;
            }
        }
        Ok(())
    }

    async fn deliver_publication(&self, publication: &Publication) -> Result<()> {
        let gate = self.gate(&publication.project_id);
        gate.begin_exclusive().await;
        let result = self.deliver_publication_exclusive(publication).await;
        gate.end_exclusive().await;
        result
    }

    async fn deliver_publication_exclusive(&self, initial: &Publication) -> Result<()> {
        for _ in 0..5 {
            let mut publication = {
                let queue = self.inner.publications.lock().await;
                queue
                    .publications
                    .iter()
                    .find(|value| value.publication_id == initial.publication_id)
                    .cloned()
                    .context("publication disappeared from queue")?
            };
            if !publication.pending() {
                return Ok(());
            }
            let project = {
                let catalog = self.inner.catalog.lock().await;
                find_project(&catalog, &publication.project_id)?.clone()
            };
            if project.state == "CONFLICTED" {
                bail!("publication is paused until `resync project sync` completes reconciliation");
            }
            let remote_head = fetch_remote_branch(
                &project,
                &publication.branch,
                self.inner.config.token.as_deref(),
            )?;
            let candidate_reachable = remote_head.as_deref().is_some_and(|remote| {
                is_ancestor(
                    &project.local_path,
                    &publication.candidate_commit_oid,
                    remote,
                )
                .unwrap_or(false)
            });
            if !candidate_reachable && remote_head != publication.expected_remote_oid {
                if current_branch(&project.local_path).ok().as_deref() != Some(&publication.branch)
                {
                    bail!(
                        "remote branch {} advanced while it is inactive; publication remains queued",
                        publication.branch
                    );
                }
                let candidate = {
                    let mut catalog = self.inner.catalog.lock().await;
                    let project = find_project_mut(&mut catalog, &publication.project_id)?;
                    match &remote_head {
                        Some(value) => {
                            project
                                .advertised_heads
                                .insert(publication.branch.clone(), value.clone());
                        }
                        None => {
                            project.advertised_heads.remove(&publication.branch);
                        }
                    }
                    let reconciliation = self.reconcile_project(project)?;
                    if reconciliation.outcome == "conflict" {
                        self.persist(&catalog).await?;
                        bail!("publication is blocked by a reconciliation conflict");
                    }
                    let candidate = git(
                        &project.local_path,
                        ["rev-parse", "HEAD"],
                        RunOptions::default(),
                    )?
                    .stdout
                    .trim()
                    .to_owned();
                    self.persist(&catalog).await?;
                    candidate
                };
                let mut queue = self.inner.publications.lock().await;
                let queued = queue
                    .publications
                    .iter_mut()
                    .find(|value| value.publication_id == publication.publication_id)
                    .context("publication disappeared from queue")?;
                queued.attempt += 1;
                queued.candidate_commit_oid = candidate;
                queued.expected_remote_oid = remote_head;
                queued.last_error = None;
                queued.next_attempt_at = Some(chrono::Utc::now().to_rfc3339());
                queue.persist(&self.inner.paths)?;
                continue;
            }
            if !candidate_reachable {
                self.validate_candidate(&project, &publication.candidate_commit_oid)?;
                let expected = publication.expected_remote_oid.as_deref().unwrap_or("");
                let pushed = git(
                    &project.local_path,
                    [
                        "push",
                        &format!(
                            "--force-with-lease=refs/heads/{}:{expected}",
                            publication.branch
                        ),
                        &project.remote_name,
                        &format!(
                            "{}:refs/heads/{}",
                            publication.candidate_commit_oid, publication.branch
                        ),
                    ],
                    RunOptions {
                        env: authorization_env(self.inner.config.token.as_deref()),
                        allow_failure: true,
                        ..RunOptions::default()
                    },
                )?;
                if pushed.code != 0 {
                    bail!("{}", pushed.stderr.trim());
                }
            }
            let confirmed_head = fetch_remote_branch(
                &project,
                &publication.branch,
                self.inner.config.token.as_deref(),
            )?
            .context("hosted branch is missing after publication")?;
            if !is_ancestor(
                &project.local_path,
                &publication.candidate_commit_oid,
                &confirmed_head,
            )? {
                bail!("published candidate is not reachable from the hosted branch");
            }
            publication.last_attempt_at = Some(chrono::Utc::now().to_rfc3339());
            let ack = match (
                project.service.as_deref(),
                self.inner.config.token.as_deref(),
            ) {
                (Some(service), Some(token)) => {
                    let service = service.to_owned();
                    let token = token.to_owned();
                    let request = publication.clone();
                    match tokio::task::spawn_blocking(move || {
                        acknowledge_with_provider(&service, &token, &request)
                    })
                    .await??
                    {
                        ProviderPublication::Acknowledged(ack) => ack,
                        ProviderPublication::RemoteAdvanced {
                            remote_head,
                            project_generation,
                        } => {
                            bail!(
                                "provider reported REMOTE_ADVANCED at {:?} (generation {})",
                                remote_head,
                                project_generation
                            )
                        }
                    }
                }
                _ => PublicationAck {
                    publication_id: publication.publication_id.clone(),
                    attempt: publication.attempt,
                    project_id: publication.project_id.clone(),
                    branch: publication.branch.clone(),
                    accepted_oid: publication.candidate_commit_oid.clone(),
                    remote_head: confirmed_head,
                    project_generation: project.server_generation,
                    durable: true,
                },
            };
            self.complete_publication(&publication, ack).await?;
            return Ok(());
        }
        bail!("publication lost the remote race five times")
    }

    fn validate_candidate(&self, project: &LocalProject, candidate: &str) -> Result<()> {
        let configuration = read_project_config(&project.local_path)?;
        if configuration.validations.is_empty() {
            return Ok(());
        }
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
                    candidate,
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
        let _ = git(
            &project.local_path,
            [
                "worktree",
                "remove",
                "--force",
                validation_path.to_string_lossy().as_ref(),
            ],
            RunOptions {
                allow_failure: true,
                ..RunOptions::default()
            },
        );
        validation
    }

    async fn complete_publication(
        &self,
        publication: &Publication,
        ack: PublicationAck,
    ) -> Result<()> {
        let mut completed = Vec::new();
        let has_pending = {
            let mut queue = self.inner.publications.lock().await;
            for value in &mut queue.publications {
                if value.pending()
                    && value.project_id == publication.project_id
                    && value.branch == publication.branch
                    && value.created_at <= publication.created_at
                {
                    value.state = ACKNOWLEDGED.into();
                    value.last_attempt_at = Some(chrono::Utc::now().to_rfc3339());
                    value.next_attempt_at = None;
                    value.last_error = None;
                    value.ack = Some(ack.clone());
                    completed.push(value.publication_id.clone());
                }
            }
            queue.compact();
            queue.persist(&self.inner.paths)?;
            queue
                .publications
                .iter()
                .any(|value| value.pending() && value.project_id == publication.project_id)
        };
        let project_path = {
            let mut catalog = self.inner.catalog.lock().await;
            let project = find_project_mut(&mut catalog, &publication.project_id)?;
            project
                .advertised_heads
                .insert(publication.branch.clone(), ack.remote_head.clone());
            project
                .last_applied_heads
                .insert(publication.branch.clone(), ack.remote_head.clone());
            project.server_generation = project.server_generation.max(ack.project_generation);
            project.last_error = None;
            if !has_pending {
                project.durability = "REMOTELY_DURABLE".into();
            }
            let path = project.local_path.clone();
            self.persist(&catalog).await?;
            path
        };
        for id in completed {
            let _ = git(
                &project_path,
                [
                    "update-ref",
                    "-d",
                    &format!("refs/resync/publications/{id}"),
                ],
                RunOptions {
                    allow_failure: true,
                    ..RunOptions::default()
                },
            );
        }
        Ok(())
    }

    async fn publish(&self, request: &Value) -> Result<Value> {
        let project_id = string_field(request, "project_id")?.to_owned();
        let project = {
            let catalog = self.inner.catalog.lock().await;
            find_project(&catalog, &project_id)?.clone()
        };
        let branch = request
            .get("branch")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or(current_branch(&project.local_path)?);
        let head = git(
            &project.local_path,
            ["rev-parse", &format!("refs/heads/{branch}")],
            RunOptions::default(),
        )?
        .stdout
        .trim()
        .to_owned();
        self.enqueue_publication(&project_id, &branch, &head)
            .await?;
        self.process_publications(true).await?;
        let queue = self.inner.publications.lock().await;
        let pending = queue.publications.iter().find(|value| {
            value.pending() && value.project_id == project_id && value.branch == branch
        });
        if let Some(value) = pending {
            bail!(
                "{}",
                value
                    .last_error
                    .as_deref()
                    .unwrap_or("publication remains pending")
            );
        }
        Ok(json!({ "outcome": "published", "head": head, "queue_depth": 0 }))
    }

    pub async fn handle(&self, request: Value) -> Result<Value> {
        match request.get("method").and_then(Value::as_str).unwrap_or("") {
            "ping" => Ok(json!({
                "protocol": PROTOCOL_VERSION,
                "pid": std::process::id(),
                "binaryId": self.inner.binary_id
            })),
            "registerProject" => {
                let project: LocalProject = serde_json::from_value(
                    request
                        .get("project")
                        .cloned()
                        .context("registerProject requires project")?,
                )?;
                let mut catalog = self.inner.catalog.lock().await;
                catalog.projects.retain(|item| {
                    item.project_id != project.project_id && item.local_path != project.local_path
                });
                catalog.projects.push(project.clone());
                if let Some(generation) = request.get("catalog_generation").and_then(Value::as_u64)
                {
                    catalog.catalog_generation = catalog.catalog_generation.max(generation);
                }
                self.persist(&catalog).await?;
                Ok(json!({
                    "projectId": project.project_id,
                    "registered": true,
                    "catalogGeneration": catalog.catalog_generation
                }))
            }
            "reload" => {
                let mut loaded = load_catalog()?;
                migrate_projects(&mut loaded.projects);
                *self.inner.catalog.lock().await = loaded;
                self.refresh().await?;
                let daemon = self.clone();
                tokio::spawn(async move {
                    let _ = daemon.process_publications(true).await;
                });
                Ok(json!({ "reloaded": self.inner.catalog.lock().await.projects.len() }))
            }
            "status" => {
                let catalog = self.inner.catalog.lock().await;
                let projects: Vec<LocalProject> =
                    match request.get("project_id").and_then(Value::as_str) {
                        Some(id) => vec![find_project(&catalog, id)?.clone()],
                        None => catalog.projects.clone(),
                    };
                let queue = self.inner.publications.lock().await;
                let projects = projects
                    .into_iter()
                    .map(|project| {
                        let pending = queue
                            .publications
                            .iter()
                            .filter(|value| {
                                value.pending() && value.project_id == project.project_id
                            })
                            .collect::<Vec<_>>();
                        let mut value = serde_json::to_value(&project)?;
                        value["queueDepth"] = json!(pending.len());
                        value["oldestPendingAt"] = pending
                            .iter()
                            .map(|item| item.created_at.as_str())
                            .min()
                            .map(|item| Value::String(item.into()))
                            .unwrap_or(Value::Null);
                        value["lastPublicationAttemptAt"] = pending
                            .iter()
                            .filter_map(|item| item.last_attempt_at.as_deref())
                            .max()
                            .map(|item| Value::String(item.into()))
                            .unwrap_or(Value::Null);
                        value["nextPublicationAttemptAt"] = pending
                            .iter()
                            .filter_map(|item| item.next_attempt_at.as_deref())
                            .min()
                            .map(|item| Value::String(item.into()))
                            .unwrap_or(Value::Null);
                        anyhow::Ok(value)
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(json!({ "projects": projects }))
            }
            "acquireAccess" => self.acquire(&request).await,
            "accessState" => {
                let project_id = string_field(&request, "project_id")?;
                let catalog = self.inner.catalog.lock().await;
                let project = find_project(&catalog, project_id)?;
                let branch = project
                    .active_branch
                    .as_deref()
                    .unwrap_or(&project.default_branch);
                Ok(json!({
                    "state": project.state.to_lowercase(),
                    "reconciliation_mode": project.state == "CONFLICTED",
                    "branch": branch,
                    "remote_ref": format!("{}/{}", project.remote_name, branch)
                }))
            }
            "releaseAccess" => self.release(&request).await,
            "sync" => self.sync(&request).await,
            "publish" => self.publish(&request).await,
            "reportCommit" => {
                let project_id = string_field(&request, "project_id")?.to_owned();
                let branch = string_field(&request, "branch")?;
                let commit_oid = string_field(&request, "commit_oid")?;
                let publication_id = self
                    .enqueue_publication(&project_id, branch, commit_oid)
                    .await?;
                Ok(json!({
                    "queued": true,
                    "publication_id": publication_id,
                    "commit_oid": commit_oid
                }))
            }
            "reportCheckout" => {
                let project_id = string_field(&request, "project_id")?.to_owned();
                let new_branch = string_field(&request, "new_branch")?.to_owned();
                let previous_oid = string_field(&request, "previous_oid")?.to_owned();
                let previous_branch = {
                    let catalog = self.inner.catalog.lock().await;
                    find_project(&catalog, &project_id)?.active_branch.clone()
                };
                if let Some(previous_branch) = previous_branch
                    && previous_branch != new_branch
                    && previous_oid.chars().any(|value| value != '0')
                {
                    self.enqueue_publication(&project_id, &previous_branch, &previous_oid)
                        .await?;
                }
                {
                    let mut catalog = self.inner.catalog.lock().await;
                    find_project_mut(&mut catalog, &project_id)?.active_branch =
                        Some(new_branch.clone());
                    self.persist(&catalog).await?;
                }
                let _ = self.refresh().await;
                Ok(json!({ "recorded": true, "active_branch": new_branch }))
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
    let projects = daemon.inner.catalog.lock().await.projects.clone();
    match current_adapter_build_id(&daemon.inner.binary_id)
        .and_then(|build_id| repair_adapters_for_build(&paths, &projects, &build_id))
    {
        Ok(_) => {}
        Err(error) => eprintln!("daemon adapter repair failed: {error:#}"),
    }
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
            let _ = refresh.process_publications(false).await;
        }
    });
    let _ = daemon.refresh().await;
    let replay = daemon.clone();
    tokio::spawn(async move {
        let _ = replay.process_publications(true).await;
    });
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

fn current_adapter_build_id(binary_id: &str) -> Result<String> {
    Ok(format!(
        "{}:{binary_id}:{}",
        env!("CARGO_PKG_VERSION"),
        std::env::current_exe()?.display()
    ))
}

fn repair_adapters_for_build(
    paths: &StatePaths,
    projects: &[LocalProject],
    build_id: &str,
) -> Result<bool> {
    let marker_path = paths.root.join("adapter-build.json");
    let installed: Value = read_json(&marker_path, Value::Null).unwrap_or(Value::Null);
    if installed.get("buildId").and_then(Value::as_str) == Some(build_id) {
        return Ok(false);
    }

    let failures = projects
        .iter()
        .filter_map(|project| {
            crate::cli::install_project_adapters(project)
                .err()
                .map(|error| format!("{}: {error:#}", project.project_id))
        })
        .collect::<Vec<_>>();
    if !failures.is_empty() {
        bail!(
            "could not refresh every managed checkout: {}",
            failures.join("; ")
        );
    }

    write_json(
        &marker_path,
        &json!({ "buildId": build_id, "version": env!("CARGO_PKG_VERSION") }),
        0o600,
    )?;
    Ok(true)
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

pub(crate) fn ensure_daemon_supervision(restart: bool) -> Result<&'static str> {
    // An explicit state directory is an isolated CLI/test instance. It must not
    // rewrite or start the user's global systemd service with this executable.
    if std::env::var_os("RESYNC_STATE_DIR").is_some() {
        return Ok("state-dir");
    }
    #[cfg(target_os = "linux")]
    {
        let available = run(
            "systemctl",
            ["--user", "show-environment"],
            RunOptions {
                allow_failure: true,
                ..RunOptions::default()
            },
        )
        .is_ok_and(|value| value.code == 0);
        if available {
            let home = std::env::var_os("HOME")
                .map(std::path::PathBuf::from)
                .context("HOME is unavailable")?;
            let directory = home.join(".config/systemd/user");
            fs::create_dir_all(&directory)?;
            let executable = std::env::current_exe()?;
            fs::write(
                directory.join("resync.service"),
                format!(
                    "[Unit]\nDescription=RepoSync convergence daemon\nAfter=network-online.target\n\n[Service]\nExecStart={} daemon\nRestart=always\nRestartSec=5\n\n[Install]\nWantedBy=default.target\n",
                    executable.display()
                ),
            )?;
            run(
                "systemctl",
                ["--user", "daemon-reload"],
                RunOptions::default(),
            )?;
            run(
                "systemctl",
                if restart {
                    vec!["--user", "restart", "resync.service"]
                } else {
                    vec!["--user", "enable", "--now", "resync.service"]
                },
                RunOptions::default(),
            )?;
            return Ok("systemd-user");
        }
    }
    Ok("hook-started")
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
    if ensure_daemon_supervision(true)? != "systemd-user" {
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
    }
    let mut last = None;
    for _ in 0..150 {
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
    use super::{LeaseGate, ResyncDaemon, daemon_matches_client, repair_adapters_for_build};
    use crate::state::{Config, LocalCatalog, LocalProject, StatePaths, read_json};
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::fs;
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn new_daemon_build_repairs_adapters_once() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let repository = temporary.path().join("project");
        let status = std::process::Command::new("git")
            .args(["init", "--quiet", "--initial-branch=main"])
            .arg(&repository)
            .status()?;
        anyhow::ensure!(status.success(), "could not initialize test repository");
        let root = temporary.path().join("state");
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
        let project = LocalProject {
            project_id: "prj_adapter_upgrade".into(),
            local_path: repository.clone(),
            remote_url: String::new(),
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
        };
        let hooks = repository.join(".codex/hooks.json");

        assert!(repair_adapters_for_build(
            &paths,
            std::slice::from_ref(&project),
            "build-one"
        )?);
        assert!(fs::read_to_string(&hooks)?.contains("codex-hook"));

        fs::write(&hooks, "{}\n")?;
        assert!(!repair_adapters_for_build(
            &paths,
            std::slice::from_ref(&project),
            "build-one"
        )?);
        assert_eq!(fs::read_to_string(&hooks)?, "{}\n");

        assert!(repair_adapters_for_build(&paths, &[project], "build-two")?);
        assert!(fs::read_to_string(hooks)?.contains("codex-hook"));
        Ok(())
    }

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

    #[tokio::test]
    async fn daemon_registration_atomically_persists_a_project() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path().join("state");
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
                catalog_generation: 3,
                projects: Vec::new(),
            },
            Duration::from_secs(10),
            Duration::from_secs(60),
        )?;
        let project: LocalProject = serde_json::from_value(json!({
            "projectId": "prj_registered",
            "localPath": temporary.path().join("checkout")
        }))?;

        daemon
            .handle(json!({
                "method": "registerProject",
                "project": project,
                "catalog_generation": 4
            }))
            .await?;

        let persisted: LocalCatalog = read_json(&paths.catalog, LocalCatalog::default())?;
        assert_eq!(persisted.catalog_generation, 4);
        assert_eq!(persisted.projects.len(), 1);
        assert_eq!(persisted.projects[0].project_id, "prj_registered");
        assert_eq!(
            daemon.project_snapshot("prj_registered").await?.project_id,
            "prj_registered"
        );
        Ok(())
    }
}
