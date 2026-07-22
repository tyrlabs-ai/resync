import { randomUUID } from "node:crypto";
import { resolve } from "node:path";
import { git } from "./process.js";
import { canonicalOrigin } from "./credentials.js";

export async function repositoryRoot(path = process.cwd()) {
  const result = await git(resolve(path), ["rev-parse", "--show-toplevel"], { allowFailure: true });
  if (result.code !== 0) throw new Error("this command must run inside a Git worktree");
  return result.stdout.trim();
}

async function localConfig(root, key) {
  const result = await git(root, ["config", "--local", "--get", key], { allowFailure: true });
  return result.code === 0 ? result.stdout.trim() : null;
}

export async function readIdentity(path = process.cwd()) {
  const root = await repositoryRoot(path);
  const service = await localConfig(root, "resync.service");
  const projectId = await localConfig(root, "resync.projectId");
  const checkoutId = await localConfig(root, "resync.checkoutId");
  if (!service && !projectId) return { root, service: null, projectId: null, checkoutId: null };
  if (!service || !projectId) throw new Error("incomplete RepoSync identity in local Git configuration");
  return { root, service: canonicalOrigin(service), projectId, checkoutId };
}

export async function writeIdentity(path, { service, projectId, checkoutId = `chk_${randomUUID().replaceAll("-", "")}` }) {
  const root = await repositoryRoot(path);
  const origin = canonicalOrigin(service);
  await git(root, ["config", "--local", "resync.service", origin]);
  await git(root, ["config", "--local", "resync.projectId", projectId]);
  await git(root, ["config", "--local", "resync.checkoutId", checkoutId]);
  return { root, service: origin, projectId, checkoutId };
}

export async function clearIdentity(path) {
  const root = await repositoryRoot(path);
  for (const key of ["resync.service", "resync.projectId", "resync.checkoutId"]) {
    await git(root, ["config", "--local", "--unset-all", key], { allowFailure: true });
  }
}

export function sameProject(left, right) {
  return Boolean(left?.service && right?.service && left?.projectId && right?.projectId &&
    canonicalOrigin(left.service) === canonicalOrigin(right.service) && left.projectId === right.projectId);
}
