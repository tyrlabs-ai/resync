import { mkdir, mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { basename, join } from "node:path";
import { randomUUID } from "node:crypto";
import { captureWorkspace, createSyntheticChain, currentBranch, findUntrackedCollisions, readHeadRef, treePaths } from "./git-state.js";
import { git } from "./process.js";
import { statePaths, writeJson } from "./state.js";

async function isAncestor(projectPath, ancestor, descendant) {
  return (await git(projectPath, ["merge-base", "--is-ancestor", ancestor, descendant], { allowFailure: true })).code === 0;
}

async function changedPaths(projectPath, oldOid, newOid) {
  const output = (await git(projectPath, ["diff", "--name-only", "-z", oldOid, newOid])).stdout;
  return output.split("\0").filter(Boolean);
}

async function writeRecoveryRefs(projectPath, id, snapshot, chain) {
  const prefix = `refs/resync/recovery/${id}`;
  await git(projectPath, ["update-ref", `${prefix}/head`, snapshot.head]);
  await git(projectPath, ["update-ref", `${prefix}/index`, chain.indexCommit]);
  await git(projectPath, ["update-ref", `${prefix}/working`, chain.workingCommit]);
  return prefix;
}

async function installCandidate(projectPath, transaction, candidate) {
  const latest = await captureWorkspace(projectPath);
  const expected = transaction.snapshot;
  if (latest.head !== expected.head || latest.indexTree !== expected.indexTree || latest.workingTree !== expected.workingTree ||
      JSON.stringify(latest.untracked) !== JSON.stringify(expected.untracked)) {
    throw new Error("workspace changed while reconciliation was prepared; retry at a new lease boundary");
  }
  const tracked = await treePaths(projectPath, candidate.workingTree);
  const collisions = findUntrackedCollisions(expected.untracked, tracked);
  if (collisions.length) {
    const error = new Error(`incoming tracked paths collide with untracked files: ${collisions.join(", ")}`);
    error.kind = "untracked-collision";
    error.paths = collisions;
    throw error;
  }

  transaction.phase = "INSTALLING";
  await writeJson(transaction.path, transaction);
  await git(projectPath, ["read-tree", "--reset", "-u", candidate.workingTree]);
  await git(projectPath, ["read-tree", candidate.indexTree]);
  await git(projectPath, ["update-ref", transaction.headRef, candidate.realHead, expected.head]);
  transaction.phase = "COMPLETED";
  transaction.completedAt = new Date().toISOString();
  await writeJson(transaction.path, transaction);
}

export async function reconcileWorkspace({ projectPath, previousRemoteOid, targetRemoteOid, transactionRoot = statePaths().transactions }) {
  if (!previousRemoteOid || !targetRemoteOid) throw new Error("previousRemoteOid and targetRemoteOid are required");
  await currentBranch(projectPath).catch(() => { throw new Error("detached or unborn HEAD is unsupported in RepoSync V1"); });
  const unresolved = (await git(projectPath, ["ls-files", "--unmerged"])).stdout.trim();
  if (unresolved) throw new Error("an unmerged index is unsupported; resolve or abort the existing Git operation first");
  const snapshot = await captureWorkspace(projectPath);
  if (previousRemoteOid === targetRemoteOid) return { outcome: "current", head: snapshot.head, changedPaths: [] };
  if (!await isAncestor(projectPath, previousRemoteOid, snapshot.head)) {
    throw new Error("local HEAD does not descend from the last applied remote revision");
  }
  if (!await isAncestor(projectPath, previousRemoteOid, targetRemoteOid)) {
    throw new Error("remote branch was rewritten or does not descend from the last applied revision");
  }
  const merges = (await git(projectPath, ["rev-list", "--merges", `${previousRemoteOid}..${snapshot.head}`])).stdout.trim();
  if (merges) throw new Error("merge commits in unpublished local history are unsupported in RepoSync V1");

  const id = `${Date.now()}-${randomUUID()}`;
  const chain = await createSyntheticChain(projectPath, snapshot);
  const recoveryRef = await writeRecoveryRefs(projectPath, id, snapshot, chain);
  await mkdir(transactionRoot, { recursive: true, mode: 0o700 });
  const transaction = {
    id,
    projectPath,
    previousRemoteOid,
    targetRemoteOid,
    snapshot,
    recoveryRef,
    headRef: await readHeadRef(projectPath),
    phase: "PREPARING",
    createdAt: new Date().toISOString(),
    path: join(transactionRoot, `${id}.json`)
  };
  await writeJson(transaction.path, transaction);

  const temporary = await mkdtemp(join(tmpdir(), "resync-rebase-"));
  const branch = `resync-candidate-${id}`;
  try {
    await git(projectPath, ["worktree", "add", "--detach", temporary, chain.workingCommit]);
    await git(temporary, ["switch", "-c", branch]);
    const result = await git(temporary, ["rebase", "--keep-empty", "--onto", targetRemoteOid, previousRemoteOid, branch], {
      env: {
        GIT_SEQUENCE_EDITOR: ":",
        GIT_EDITOR: ":",
        GIT_COMMITTER_NAME: "RepoSync Reconciler",
        GIT_COMMITTER_EMAIL: "reconcile@reposync.local"
      },
      allowFailure: true
    });
    if (result.code !== 0) {
      await git(temporary, ["rebase", "--abort"], { allowFailure: true });
      transaction.phase = "CONFLICTED";
      transaction.conflict = { kind: "git-rebase", detail: result.stderr.trim().slice(-4000) };
      await writeJson(transaction.path, transaction);
      return { outcome: "conflict", transactionId: id, recoveryRef, detail: transaction.conflict.detail };
    }

    const workingCommit = (await git(temporary, ["rev-parse", "HEAD"])).stdout.trim();
    const indexCommit = (await git(temporary, ["rev-parse", "HEAD^"])).stdout.trim();
    const realHead = (await git(temporary, ["rev-parse", "HEAD^^"])).stdout.trim();
    const workingTree = (await git(temporary, ["rev-parse", `${workingCommit}^{tree}`])).stdout.trim();
    const indexTree = (await git(temporary, ["rev-parse", `${indexCommit}^{tree}`])).stdout.trim();
    const candidate = { workingCommit, indexCommit, realHead, workingTree, indexTree };
    transaction.candidate = candidate;
    await writeJson(transaction.path, transaction);
    try {
      await installCandidate(projectPath, transaction, candidate);
    } catch (error) {
      transaction.phase = "CONFLICTED";
      transaction.conflict = { kind: error.kind || "install-rejected", paths: error.paths || [], detail: error.message };
      await writeJson(transaction.path, transaction);
      return { outcome: "conflict", transactionId: id, recoveryRef, detail: error.message, paths: error.paths || [] };
    }
    return {
      outcome: "applied",
      transactionId: id,
      oldHead: snapshot.head,
      newHead: realHead,
      changedPaths: await changedPaths(projectPath, previousRemoteOid, targetRemoteOid)
    };
  } finally {
    await git(projectPath, ["worktree", "remove", "--force", temporary], { allowFailure: true });
    await git(projectPath, ["branch", "-D", branch], { allowFailure: true });
    await rm(temporary, { recursive: true, force: true });
  }
}

export async function recoverTransaction(path) {
  const transaction = JSON.parse(await readFile(path, "utf8"));
  if (transaction.phase === "PREPARING" || transaction.phase === "CONFLICTED" || transaction.phase === "COMPLETED") return transaction;
  if (transaction.phase !== "INSTALLING") throw new Error(`unknown transaction phase ${transaction.phase}`);
  const current = await captureWorkspace(transaction.projectPath);
  if (current.head === transaction.candidate.realHead) {
    transaction.phase = "COMPLETED";
    transaction.recoveredAt = new Date().toISOString();
    await writeJson(path, transaction);
    return transaction;
  }
  if (current.head === transaction.snapshot.head) {
    await git(transaction.projectPath, ["read-tree", "--reset", "-u", transaction.snapshot.workingTree]);
    await git(transaction.projectPath, ["read-tree", transaction.snapshot.indexTree]);
    transaction.phase = "ROLLED_BACK";
    transaction.recoveredAt = new Date().toISOString();
    await writeJson(path, transaction);
    return transaction;
  }
  throw new Error(`transaction ${basename(path)} needs manual recovery; HEAD matches neither preserved state`);
}
