import { access, mkdir, realpath } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { assertCatalog } from "./protocol.js";
import { currentBranch } from "./git-state.js";
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

async function refs(projectPath, prefix) {
  const result = await git(projectPath, ["for-each-ref", "--format=%(refname:strip=2)\t%(objectname)", prefix], { allowFailure: true });
  if (result.code !== 0 || !result.stdout.trim()) return {};
  return Object.fromEntries(result.stdout.trim().split("\n").map(line => {
    const separator = line.indexOf("\t");
    return [line.slice(0, separator), line.slice(separator + 1)];
  }));
}

export async function localHeads(projectPath) {
  return refs(projectPath, "refs/heads");
}

export async function remoteHeads(projectPath, remoteName) {
  const values = await refs(projectPath, `refs/remotes/${remoteName}`);
  return Object.fromEntries(Object.entries(values)
    .filter(([name]) => name.startsWith(`${remoteName}/`) && name !== `${remoteName}/HEAD`)
    .map(([name, oid]) => [name.slice(remoteName.length + 1), oid]));
}

async function isAncestor(projectPath, ancestor, descendant) {
  return (await git(projectPath, ["merge-base", "--is-ancestor", ancestor, descendant], { allowFailure: true })).code === 0;
}

export async function fetchRemote(project, token) {
  const env = authorizationEnv(token);
  const refspec = `+refs/heads/*:refs/remotes/${project.remoteName}/*`;
  const result = await git(project.localPath, ["fetch", "--prune", "--no-tags", project.remoteName, refspec], { env, allowFailure: true });
  if (result.code !== 0) {
    if (/couldn't find remote ref/i.test(result.stderr)) return {};
    throw new Error(result.stderr.trim() || "Git fetch failed");
  }
  return remoteHeads(project.localPath, project.remoteName);
}

async function exists(path) {
  try { await access(path); return true; }
  catch (error) { if (error.code === "ENOENT") return false; throw error; }
}

async function configureRemote(destination, remoteName, remoteUrl) {
  const currentUrl = await git(destination, ["remote", "get-url", remoteName], { allowFailure: true });
  if (currentUrl.code === 0) await git(destination, ["remote", "set-url", remoteName, remoteUrl]);
  else await git(destination, ["remote", "add", remoteName, remoteUrl]);
  await git(destination, ["config", `remote.${remoteName}.fetch`, `+refs/heads/*:refs/remotes/${remoteName}/*`]);
}

export async function materializeProject({ catalogProject, localPath, token, remoteName = "resync" }) {
  const destination = resolve(localPath);
  const env = authorizationEnv(token);
  const advertised = Object.fromEntries(catalogProject.advertised_heads.map(head => [head.name, head.oid]));
  if (!await exists(destination)) {
    if (Object.keys(advertised).length) {
      await run("git", ["clone", "--origin", remoteName, catalogProject.remote_url, destination], {
        cwd: dirname(destination), env
      });
      const head = await git(destination, ["rev-parse", "--verify", "HEAD"], { allowFailure: true });
      if (head.code !== 0) {
        const branch = advertised[catalogProject.default_branch] ? catalogProject.default_branch : Object.keys(advertised).sort()[0];
        await git(destination, ["switch", "-c", branch, "--track", `${remoteName}/${branch}`]);
      }
    } else {
      await mkdir(destination, { recursive: true });
      await git(destination, ["init", `--initial-branch=${catalogProject.default_branch}`]);
      await git(destination, ["remote", "add", remoteName, catalogProject.remote_url]);
    }
  } else {
    const inside = await git(destination, ["rev-parse", "--is-inside-work-tree"], { allowFailure: true });
    if (inside.code !== 0) throw new Error(`${destination} exists but is not a Git worktree`);
  }

  await configureRemote(destination, remoteName, catalogProject.remote_url);
  let hostedHeads = await fetchRemote({ localPath: destination, remoteName }, token);
  let checkoutHeads = await localHeads(destination);
  const branch = await currentBranch(destination).catch(() => null);

  if (!Object.keys(hostedHeads).length) {
    if (Object.keys(checkoutHeads).length) {
      await git(destination, ["push", remoteName, "refs/heads/*:refs/heads/*"], { env });
      hostedHeads = await fetchRemote({ localPath: destination, remoteName }, token);
    }
  } else {
    const toPublish = [];
    for (const [name, localOid] of Object.entries(checkoutHeads)) {
      const remoteOid = hostedHeads[name];
      if (!remoteOid) {
        toPublish.push(`refs/heads/${name}:refs/heads/${name}`);
        continue;
      }
      if (localOid === remoteOid) continue;
      const remoteIsBase = await isAncestor(destination, remoteOid, localOid);
      const localIsBase = await isAncestor(destination, localOid, remoteOid);
      if (!remoteIsBase && !localIsBase) {
        throw new Error(`local and hosted histories for branch ${name} diverge; RepoSync will not choose a project identity or overwrite either history`);
      }
      if (remoteIsBase) toPublish.push(`refs/heads/${name}:refs/heads/${name}`);
    }
    if (toPublish.length) {
      await git(destination, ["push", remoteName, ...toPublish], { env });
      hostedHeads = await fetchRemote({ localPath: destination, remoteName }, token);
    }
    if (branch && checkoutHeads[branch] && hostedHeads[branch] && checkoutHeads[branch] !== hostedHeads[branch] &&
        await isAncestor(destination, checkoutHeads[branch], hostedHeads[branch])) {
      const result = await reconcileWorkspace({
        projectPath: destination, previousRemoteOid: checkoutHeads[branch], targetRemoteOid: hostedHeads[branch]
      });
      if (result.outcome !== "applied") throw new Error(`joining the hosted project conflicts: ${result.detail || result.outcome}`);
      checkoutHeads = await localHeads(destination);
    }
  }

  const lastAppliedHeads = {};
  for (const [name, remoteOid] of Object.entries(hostedHeads)) {
    const localOid = checkoutHeads[name];
    lastAppliedHeads[name] = localOid && localOid !== remoteOid && await isAncestor(destination, localOid, remoteOid)
      ? localOid
      : remoteOid;
  }
  return {
    projectId: catalogProject.project_id,
    localPath: await realpath(destination),
    remoteUrl: catalogProject.remote_url,
    remoteName,
    defaultBranch: catalogProject.default_branch,
    serverGeneration: catalogProject.project_generation,
    advertisedHeads: hostedHeads,
    lastAppliedHeads,
    activeBranch: branch,
    workspaceGeneration: Object.keys(hostedHeads).length ? 1 : 0,
    state: "CURRENT",
    durability: "REMOTELY_DURABLE",
    conflict: null,
    lastError: null
  };
}
