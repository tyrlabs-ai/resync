import { chmod, mkdir, rm } from "node:fs/promises";
import { createServer } from "node:net";
import { randomUUID } from "node:crypto";
import { dirname } from "node:path";
import { captureWorkspace, currentBranch } from "./git-state.js";
import { git } from "./process.js";
import { fetchCatalog, fetchRemote, authorizationEnv } from "./remote.js";
import { loadCatalog, loadConfig, statePaths, writeJson } from "./state.js";
import { reconcileWorkspace } from "./transaction.js";
import { readProjectConfig } from "./config.js";

function delay(ms) { return new Promise(resolve => setTimeout(resolve, ms)); }

function migrateProject(project) {
  project.defaultBranch ||= project.syncBranch || "main";
  project.advertisedHeads ||= project.advertisedRemoteOid ? { [project.defaultBranch]: project.advertisedRemoteOid } : {};
  project.lastAppliedHeads ||= project.lastAppliedRemoteOid ? { [project.defaultBranch]: project.lastAppliedRemoteOid } : {};
  delete project.syncBranch;
  delete project.advertisedRemoteOid;
  delete project.lastAppliedRemoteOid;
  return project;
}

class LeaseGate {
  constructor(maxLeaseMs = 60 * 60_000) {
    this.leases = new Map();
    this.exclusive = false;
    this.waiters = [];
    this.maxLeaseMs = maxLeaseMs;
  }

  async activity(metadata) {
    while (this.exclusive) await new Promise(resolve => this.waiters.push(resolve));
    const id = randomUUID();
    const timer = setTimeout(() => this.release(id), this.maxLeaseMs);
    timer.unref();
    this.leases.set(id, { ...metadata, acquiredAt: new Date().toISOString(), timer });
    return id;
  }

  release(id) {
    const lease = this.leases.get(id);
    if (lease?.timer) clearTimeout(lease.timer);
    const removed = this.leases.delete(id);
    if (removed && this.leases.size === 0) this.wake();
    return removed;
  }

  wake() {
    const waiters = this.waiters.splice(0);
    for (const resolve of waiters) resolve();
  }

  async runExclusive(callback) {
    while (this.exclusive) await new Promise(resolve => this.waiters.push(resolve));
    this.exclusive = true;
    try {
      while (this.leases.size) await new Promise(resolve => this.waiters.push(resolve));
      return await callback();
    } finally {
      this.exclusive = false;
      this.wake();
    }
  }
}

export class ResyncDaemon {
  constructor(options = {}) {
    this.paths = options.paths || statePaths();
    this.pollIntervalMs = options.pollIntervalMs || 10_000;
    this.maxLeaseMs = options.maxLeaseMs || 60 * 60_000;
    this.catalog = null;
    this.config = null;
    this.gates = new Map();
    this.leaseProjects = new Map();
    this.stopping = false;
    this.server = null;
  }

  gate(projectId) {
    if (!this.gates.has(projectId)) this.gates.set(projectId, new LeaseGate(this.maxLeaseMs));
    return this.gates.get(projectId);
  }

  project(projectId) {
    const project = this.catalog.projects.find(item => item.projectId === projectId || item.localPath === projectId);
    if (!project) throw new Error(`unknown subscribed project ${projectId}`);
    return project;
  }

  async persist() {
    await writeJson(this.paths.catalog, this.catalog);
  }

  async refresh() {
    const remote = await fetchCatalog(this.config);
    this.catalog.catalogGeneration = remote.catalog_generation;
    for (const project of this.catalog.projects) {
      const update = remote.projects.find(item => item.project_id === project.projectId);
      if (!update) continue;
      project.remoteUrl = update.remote_url;
      project.defaultBranch = update.default_branch;
      project.serverGeneration = update.project_generation;
      try {
        const fetched = await fetchRemote(project, this.config.token);
        project.advertisedHeads = fetched;
        project.activeBranch = await currentBranch(project.localPath).catch(() => null);
        const target = project.activeBranch && fetched[project.activeBranch];
        if (target && target !== project.lastAppliedHeads[project.activeBranch] && project.state !== "CONFLICTED") {
          project.state = "REMOTE_AHEAD";
        } else if (project.state !== "CONFLICTED") project.state = "CURRENT";
        project.lastError = null;
      } catch (error) {
        project.state = "OFFLINE";
        project.lastError = error.message;
      }
    }
    await this.persist();
  }

  async reconcile(project) {
    const branch = await currentBranch(project.localPath).catch(() => null);
    if (!branch) throw new Error("detached or unborn HEAD is unsupported in RepoSync V1");
    project.activeBranch = branch;
    const target = project.advertisedHeads[branch];
    if (!target) {
      project.state = "CURRENT";
      return { outcome: "current", changedPaths: [] };
    }
    const snapshot = await captureWorkspace(project.localPath);
    if (snapshot.head === target) {
      project.lastAppliedHeads[branch] = target;
      project.state = "CURRENT";
      return { outcome: "current", changedPaths: [] };
    }
    let previous = project.lastAppliedHeads[branch];
    const targetIsBase = (await git(project.localPath, ["merge-base", "--is-ancestor", target, snapshot.head], { allowFailure: true })).code === 0;
    if (targetIsBase) {
      project.lastAppliedHeads[branch] = target;
      project.state = "CURRENT";
      return { outcome: "current", changedPaths: [] };
    }
    const headIsBase = (await git(project.localPath, ["merge-base", "--is-ancestor", snapshot.head, target], { allowFailure: true })).code === 0;
    if (!previous || previous === target) {
      if (headIsBase) previous = snapshot.head;
      else {
        project.state = "CONFLICTED";
        project.conflict = { kind: "diverged-history", detail: `local and hosted histories for branch ${branch} diverge` };
        await this.persist();
        return { outcome: "conflict", detail: project.conflict.detail };
      }
    }
    project.state = "RECONCILING";
    await this.persist();
    const result = await reconcileWorkspace({
      projectPath: project.localPath,
      previousRemoteOid: previous,
      targetRemoteOid: target,
      transactionRoot: this.paths.transactions
    });
    if (result.outcome === "applied") {
      project.lastAppliedHeads[branch] = target;
      project.workspaceGeneration += 1;
      project.state = "CURRENT";
      project.conflict = null;
    } else if (result.outcome === "conflict") {
      project.state = "CONFLICTED";
      project.conflict = result;
    }
    await this.persist();
    return result;
  }

  async acquire(request) {
    const project = this.project(request.project_id);
    if (project.state === "CONFLICTED" || project.state === "FAILED") {
      return { blocked: true, state: project.state.toLowerCase(), model_message: project.conflict?.detail || project.lastError || "workspace unavailable", recovery_actions: ["resync conflicts", "resync recover"] };
    }
    const reconciliation = await this.gate(project.projectId).runExclusive(() => this.reconcile(project));
    if (project.state === "CONFLICTED") return this.acquire(request);
    const leaseId = await this.gate(project.projectId).activity({ sessionId: request.session_id, callId: request.call_id });
    this.leaseProjects.set(leaseId, project.projectId);
    return {
      granted: true,
      lease_id: leaseId,
      workspace_generation: project.workspaceGeneration,
      branch_oid: (await captureWorkspace(project.localPath)).head,
      reconciliation: reconciliation.outcome === "applied" ? "applied" : "none",
      changed_paths_summary: reconciliation.changedPaths?.slice(0, 50),
      model_message: reconciliation.outcome === "applied"
        ? `RepoSync advanced the workspace and preserved local work. Upstream changed: ${reconciliation.changedPaths.slice(0, 20).join(", ") || "metadata only"}.`
        : undefined
    };
  }

  release(request) {
    const projectId = this.leaseProjects.get(request.lease_id);
    if (!projectId) throw new Error("unknown or already released lease");
    this.leaseProjects.delete(request.lease_id);
    this.gate(projectId).release(request.lease_id);
    return { released: true };
  }

  async sync(request) {
    const project = this.project(request.project_id);
    await this.refresh();
    return this.gate(project.projectId).runExclusive(() => this.reconcile(project));
  }

  async publish(request) {
    const project = this.project(request.project_id);
    return this.gate(project.projectId).runExclusive(async () => {
      for (let attempt = 1; attempt <= 5; attempt += 1) {
        const fetched = await fetchRemote(project, this.config.token);
        project.advertisedHeads = fetched;
        const branch = await currentBranch(project.localPath);
        if (request.branch && request.branch !== branch) throw new Error(`cannot publish ${request.branch}; active branch is now ${branch}`);
        const result = await this.reconcile(project);
        if (result.outcome === "conflict") return result;
        const head = (await git(project.localPath, ["rev-parse", "HEAD"])).stdout.trim();
        const configuration = await readProjectConfig(project.localPath);
        if (configuration.validations.length) {
          const validationPath = `${this.paths.root}/validation-${randomUUID()}`;
          try {
            await git(project.localPath, ["worktree", "add", "--detach", validationPath, head]);
            for (const command of configuration.validations) {
              const [program, ...args] = command;
              const { run } = await import("./process.js");
              await run(program, args, { cwd: validationPath });
            }
          } finally {
            await git(project.localPath, ["worktree", "remove", "--force", validationPath], { allowFailure: true });
          }
        }
        project.durability = "PUBLISHING";
        await this.persist();
        const pushed = await git(project.localPath, ["push", project.remoteName, `HEAD:refs/heads/${branch}`], {
          env: authorizationEnv(this.config.token), allowFailure: true
        });
        if (pushed.code === 0) {
          project.lastAppliedHeads[branch] = head;
          project.advertisedHeads[branch] = head;
          project.state = "CURRENT";
          project.durability = "REMOTELY_DURABLE";
          await this.persist();
          return { outcome: "published", head, attempt };
        }
        if (!/non-fast-forward|fetch first|rejected/i.test(pushed.stderr)) throw new Error(pushed.stderr.trim());
      }
      throw new Error("publication lost the remote race five times");
    });
  }

  async handle(request) {
    switch (request.method) {
      case "ping": return { protocol: "resync.v1", pid: process.pid };
      case "status": return { projects: request.project_id ? [this.project(request.project_id)] : this.catalog.projects };
      case "acquireAccess": return this.acquire(request);
      case "releaseAccess": return this.release(request);
      case "sync": return this.sync(request);
      case "publish": return this.publish(request);
      case "reportCommit": {
        const project = this.project(request.project_id);
        project.durability = "COMMITTED_UNPUBLISHED";
        await this.persist();
        setImmediate(() => this.publish({ project_id: project.projectId, branch: request.branch }).catch(error => {
          project.lastError = error.message;
          this.persist().catch(() => {});
        }));
        return { scheduled: true, commit_oid: request.commit_oid };
      }
      default: throw new Error(`unknown daemon method ${request.method}`);
    }
  }

  async start() {
    this.config = await loadConfig();
    this.catalog = await loadCatalog();
    for (const project of this.catalog.projects) migrateProject(project);
    await this.persist();
    await mkdir(dirname(this.paths.socket), { recursive: true, mode: 0o700 });
    await rm(this.paths.socket, { force: true });
    this.server = createServer(socket => {
      let buffered = "";
      socket.on("data", chunk => {
        buffered += chunk;
        if (Buffer.byteLength(buffered) > 1024 * 1024) {
          socket.destroy(new Error("daemon request exceeds 1 MiB"));
          return;
        }
        let newline;
        while ((newline = buffered.indexOf("\n")) >= 0) {
          const line = buffered.slice(0, newline);
          buffered = buffered.slice(newline + 1);
          if (!line.trim()) continue;
          Promise.resolve().then(() => this.handle(JSON.parse(line))).then(
            result => socket.write(`${JSON.stringify({ ok: true, result })}\n`),
            error => socket.write(`${JSON.stringify({ ok: false, error: error.message })}\n`)
          );
        }
      });
    });
    await new Promise((resolve, reject) => {
      this.server.once("error", reject);
      this.server.listen(this.paths.socket, resolve);
    });
    await chmod(this.paths.socket, 0o600);
    this.refresh().catch(() => {});
    this.pollLoop = this.poll();
    return this;
  }

  async poll() {
    while (!this.stopping) {
      await delay(this.pollIntervalMs + Math.floor(Math.random() * 1000));
      if (!this.stopping) await this.refresh().catch(() => {});
    }
  }

  async stop() {
    this.stopping = true;
    if (this.server) await new Promise(resolve => this.server.close(resolve));
    await rm(this.paths.socket, { force: true });
  }
}

export async function runDaemon() {
  const daemon = await new ResyncDaemon().start();
  const stop = async () => { await daemon.stop(); process.exit(0); };
  process.once("SIGINT", stop);
  process.once("SIGTERM", stop);
}
