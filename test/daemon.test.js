import test from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { ResyncDaemon } from "../src/daemon.js";
import { git } from "../src/process.js";
import { fetchRemote } from "../src/remote.js";

test("reconciliation applies only the currently checked-out branch", async t => {
  const root = await mkdtemp(join(tmpdir(), "resync-daemon-branches-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const remote = join(root, "remote.git");
  const seed = join(root, "seed");
  const local = join(root, "local");
  await git(root, ["init", "--bare", "--initial-branch=main", remote]);
  await git(root, ["init", "--initial-branch=main", seed]);
  for (const repository of [seed]) {
    await git(repository, ["config", "user.name", "Test"]);
    await git(repository, ["config", "user.email", "test@example.invalid"]);
  }
  await writeFile(join(seed, "file"), "base\n");
  await git(seed, ["add", "."]);
  await git(seed, ["commit", "-m", "base"]);
  await git(seed, ["branch", "feature"]);
  await git(seed, ["remote", "add", "origin", remote]);
  await git(seed, ["push", "origin", "main", "feature"]);
  await git(root, ["clone", remote, local]);
  await git(local, ["config", "user.name", "Test"]);
  await git(local, ["config", "user.email", "test@example.invalid"]);
  await git(local, ["switch", "-c", "feature", "--track", "origin/feature"]);
  const initial = (await git(local, ["rev-parse", "HEAD"])).stdout.trim();

  await git(seed, ["switch", "feature"]);
  await writeFile(join(seed, "file"), "feature\n");
  await git(seed, ["commit", "-am", "advance feature"]);
  await git(seed, ["push", "origin", "feature"]);
  await git(seed, ["switch", "main"]);
  await writeFile(join(seed, "main"), "main\n");
  await git(seed, ["add", "."]);
  await git(seed, ["commit", "-m", "advance main"]);
  await git(seed, ["push", "origin", "main"]);

  const project = {
    projectId: "prj_test", localPath: local, remoteName: "origin", defaultBranch: "main",
    advertisedHeads: {}, lastAppliedHeads: { feature: initial, main: initial },
    workspaceGeneration: 1, state: "CURRENT", conflict: null
  };
  project.advertisedHeads = await fetchRemote(project, null);
  const paths = {
    root: join(root, "state"), catalog: join(root, "state", "catalog.json"),
    transactions: join(root, "state", "transactions"), socket: join(root, "state", "daemon.sock")
  };
  const daemon = new ResyncDaemon({ paths });
  daemon.catalog = { catalogGeneration: 1, projects: [project] };
  const mainBefore = (await git(local, ["rev-parse", "refs/heads/main"])).stdout.trim();
  const result = await daemon.reconcile(project);

  assert.equal(result.outcome, "applied");
  assert.equal((await git(local, ["rev-parse", "HEAD"])).stdout.trim(), project.advertisedHeads.feature);
  assert.equal((await git(local, ["rev-parse", "refs/heads/main"])).stdout.trim(), mainBefore);
  assert.notEqual(project.advertisedHeads.main, mainBefore);
});
