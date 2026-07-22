import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { git } from "./process.js";

async function rev(projectPath, name) {
  return (await git(projectPath, ["rev-parse", "--verify", name])).stdout.trim();
}

export async function currentBranch(projectPath) {
  return (await git(projectPath, ["symbolic-ref", "--short", "HEAD"])).stdout.trim();
}

export async function listUntracked(projectPath) {
  // Include ignored files: an incoming tracked file must never silently replace
  // a local build artifact or secret merely because Git ignores it.
  const output = (await git(projectPath, ["ls-files", "--others", "-z"])).stdout;
  return output.split("\0").filter(Boolean);
}

async function temporaryIndex(projectPath, seedTree, updateTracked) {
  const directory = await mkdtemp(join(tmpdir(), "resync-index-"));
  const index = join(directory, "index");
  const env = { GIT_INDEX_FILE: index };
  try {
    await git(projectPath, ["read-tree", seedTree], { env });
    if (updateTracked) await git(projectPath, ["add", "-u", "--", "."], { env });
    const tree = (await git(projectPath, ["write-tree"], { env })).stdout.trim();
    return tree;
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
}

export async function captureWorkspace(projectPath) {
  const head = await rev(projectPath, "HEAD");
  const indexTree = (await git(projectPath, ["write-tree"])).stdout.trim();
  const workingTree = await temporaryIndex(projectPath, indexTree, true);
  const untracked = await listUntracked(projectPath);
  return { head, indexTree, workingTree, untracked };
}

export async function createSyntheticChain(projectPath, snapshot) {
  const env = {
    GIT_AUTHOR_NAME: "RepoSync Recovery",
    GIT_AUTHOR_EMAIL: "recovery@reposync.local",
    GIT_COMMITTER_NAME: "RepoSync Recovery",
    GIT_COMMITTER_EMAIL: "recovery@reposync.local"
  };
  const indexCommit = (await git(projectPath, ["commit-tree", snapshot.indexTree, "-p", snapshot.head], {
    env, input: "RepoSync synthetic index\n"
  })).stdout.trim();
  const workingCommit = (await git(projectPath, ["commit-tree", snapshot.workingTree, "-p", indexCommit], {
    env, input: "RepoSync synthetic working tree\n"
  })).stdout.trim();
  return { indexCommit, workingCommit };
}

export async function treePaths(projectPath, tree) {
  const output = (await git(projectPath, ["ls-tree", "-r", "--name-only", "-z", tree])).stdout;
  return output.split("\0").filter(Boolean);
}

export function findUntrackedCollisions(untracked, tracked) {
  const collisions = new Set();
  const trackedSet = new Set(tracked);
  for (const path of untracked) {
    if (trackedSet.has(path)) collisions.add(path);
    const pieces = path.split("/");
    for (let index = 1; index < pieces.length; index += 1) {
      if (trackedSet.has(pieces.slice(0, index).join("/"))) collisions.add(path);
    }
    for (const candidate of tracked) {
      if (candidate.startsWith(`${path}/`)) collisions.add(path);
    }
  }
  return [...collisions].sort();
}

export async function gitDir(projectPath) {
  const raw = (await git(projectPath, ["rev-parse", "--git-dir"])).stdout.trim();
  return raw.startsWith("/") ? raw : join(projectPath, raw);
}

export async function readHeadRef(projectPath) {
  return (await git(projectPath, ["symbolic-ref", "HEAD"])).stdout.trim();
}
