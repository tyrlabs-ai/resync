import { access, mkdir, realpath } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { assertCatalog } from "./protocol.js";
import { git, run } from "./process.js";
import { reconcileWorkspace } from "./transaction.js";

export function authorizationEnv(token) {
  if (!token) return {};
  return {
    GIT_CONFIG_COUNT: "1",
    GIT_CONFIG_KEY_0: "http.extraHeader",
    GIT_CONFIG_VALUE_0: `Authorization: Basic ${Buffer.from(`resync:${token}`).toString("base64")}`
  };
}

export async function fetchCatalog(config) {
  if (!config.server || !config.token) throw new Error("run `resync login SERVER TOKEN` first");
  const response = await fetch(new URL("/v1/catalog", config.server), {
    headers: { authorization: `Bearer ${config.token}`, accept: "application/json" }
  });
  if (!response.ok) throw new Error(`catalog request failed (${response.status}): ${(await response.text()).slice(0, 500)}`);
  return assertCatalog(await response.json());
}

export async function remoteHead(projectPath, remoteName, branch, env = {}) {
  const result = await git(projectPath, ["rev-parse", "--verify", `refs/remotes/${remoteName}/${branch}`], { env, allowFailure: true });
  return result.code === 0 ? result.stdout.trim() : null;
}

export async function fetchRemote(project, token) {
  const env = authorizationEnv(token);
  const refspec = `+refs/heads/${project.syncBranch}:refs/remotes/${project.remoteName}/${project.syncBranch}`;
  const result = await git(project.localPath, ["fetch", "--no-tags", project.remoteName, refspec], { env, allowFailure: true });
  if (result.code !== 0) {
    if (/couldn't find remote ref/i.test(result.stderr)) return null;
    throw new Error(result.stderr.trim() || "Git fetch failed");
  }
  return remoteHead(project.localPath, project.remoteName, project.syncBranch, env);
}

async function exists(path) {
  try { await access(path); return true; }
  catch (error) { if (error.code === "ENOENT") return false; throw error; }
}

export async function materializeProject({ catalogProject, localPath, token, remoteName = "resync" }) {
  const destination = resolve(localPath);
  const env = authorizationEnv(token);
  if (!await exists(destination)) {
    if (catalogProject.advertised_head_oid) {
      await run("git", ["clone", "--branch", catalogProject.sync_branch, "--origin", remoteName, catalogProject.remote_url, destination], {
        cwd: dirname(destination), env
      });
    } else {
      await mkdir(destination, { recursive: true });
      await git(destination, ["init", `--initial-branch=${catalogProject.sync_branch}`]);
      await git(destination, ["remote", "add", remoteName, catalogProject.remote_url]);
    }
  } else {
    const inside = await git(destination, ["rev-parse", "--is-inside-work-tree"], { allowFailure: true });
    if (inside.code !== 0) throw new Error(`${destination} exists but is not a Git worktree`);
    const currentUrl = await git(destination, ["remote", "get-url", remoteName], { allowFailure: true });
    if (currentUrl.code === 0) await git(destination, ["remote", "set-url", remoteName, catalogProject.remote_url]);
    else await git(destination, ["remote", "add", remoteName, catalogProject.remote_url]);
    const fetchResult = await git(destination, ["fetch", "--no-tags", remoteName,
      `+refs/heads/${catalogProject.sync_branch}:refs/remotes/${remoteName}/${catalogProject.sync_branch}`], { env, allowFailure: true });
    if (fetchResult.code !== 0 && !/couldn't find remote ref|does not appear to be a git repository/i.test(fetchResult.stderr)) {
      throw new Error(fetchResult.stderr.trim());
    }
    const remoteOid = await remoteHead(destination, remoteName, catalogProject.sync_branch, env);
    if (!remoteOid) {
      const head = await git(destination, ["rev-parse", "--verify", "HEAD"], { allowFailure: true });
      if (head.code === 0) await git(destination, ["push", remoteName, `HEAD:refs/heads/${catalogProject.sync_branch}`], { env });
    } else {
      const head = await git(destination, ["rev-parse", "--verify", "HEAD"], { allowFailure: true });
      if (head.code !== 0) {
        await git(destination, ["switch", "-c", catalogProject.sync_branch, remoteOid]);
      } else {
        const localOid = head.stdout.trim();
        const remoteIsBase = (await git(destination, ["merge-base", "--is-ancestor", remoteOid, localOid], { allowFailure: true })).code === 0;
        const localIsBase = (await git(destination, ["merge-base", "--is-ancestor", localOid, remoteOid], { allowFailure: true })).code === 0;
        if (!remoteIsBase && !localIsBase) {
          throw new Error("local and hosted histories diverge; RepoSync will not choose a project identity or overwrite either history");
        }
        if (localIsBase && localOid !== remoteOid) {
          const result = await reconcileWorkspace({ projectPath: destination, previousRemoteOid: localOid, targetRemoteOid: remoteOid });
          if (result.outcome !== "applied") throw new Error(`joining the hosted project conflicts: ${result.detail || result.outcome}`);
        }
      }
    }
  }
  const fetchResult = await git(destination, ["fetch", "--no-tags", remoteName,
    `+refs/heads/${catalogProject.sync_branch}:refs/remotes/${remoteName}/${catalogProject.sync_branch}`], { env, allowFailure: true });
  if (fetchResult.code !== 0 && !/couldn't find remote ref/i.test(fetchResult.stderr)) throw new Error(fetchResult.stderr.trim());
  const remoteOid = await remoteHead(destination, remoteName, catalogProject.sync_branch, env);
  return {
    projectId: catalogProject.project_id,
    localPath: await realpath(destination),
    remoteUrl: catalogProject.remote_url,
    remoteName,
    syncBranch: catalogProject.sync_branch,
    serverGeneration: catalogProject.project_generation,
    advertisedRemoteOid: remoteOid,
    lastAppliedRemoteOid: remoteOid,
    workspaceGeneration: remoteOid ? 1 : 0,
    state: "CURRENT",
    durability: "REMOTELY_DURABLE",
    conflict: null,
    lastError: null
  };
}
