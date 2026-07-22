import test from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { installAdapters } from "../src/cli.js";
import { git } from "../src/process.js";
import { writeJson } from "../src/state.js";

function delay(ms) { return new Promise(resolveDelay => setTimeout(resolveDelay, ms)); }

test("an ordinary Git commit starts the daemon and publishes automatically", async t => {
  const root = await mkdtemp(join(tmpdir(), "resync-auto-publish-"));
  const priorState = process.env.RESYNC_STATE_DIR;
  process.env.RESYNC_STATE_DIR = join(root, "state");
  t.after(async () => {
    try {
      const pid = Number((await readFile(join(root, "state", "daemon.lock"), "utf8")).trim());
      if (pid && pid !== process.pid) process.kill(pid, "SIGTERM");
    } catch {}
    await delay(100);
    if (priorState == null) delete process.env.RESYNC_STATE_DIR;
    else process.env.RESYNC_STATE_DIR = priorState;
    await rm(root, { recursive: true, force: true });
  });

  const remote = join(root, "remote.git");
  const local = join(root, "local");
  await git(root, ["init", "--bare", "--initial-branch=main", remote]);
  await git(root, ["init", "--initial-branch=main", local]);
  await git(local, ["config", "user.name", "Test"]);
  await git(local, ["config", "user.email", "test@example.invalid"]);
  await writeFile(join(local, "file"), "base\n");
  await git(local, ["add", "."]);
  await git(local, ["commit", "-m", "base"]);
  await git(local, ["remote", "add", "resync", remote]);
  await git(local, ["push", "resync", "main"]);
  const base = (await git(local, ["rev-parse", "HEAD"])).stdout.trim();
  const projectId = "prj_automatic";
  await writeJson(join(root, "state", "catalog.json"), {
    catalogGeneration: 1,
    projects: [{
      projectId, localPath: local, remoteUrl: remote, remoteName: "resync", defaultBranch: "main",
      serverGeneration: 1, advertisedHeads: { main: base }, lastAppliedHeads: { main: base },
      activeBranch: "main", workspaceGeneration: 1, state: "CURRENT", durability: "REMOTELY_DURABLE",
      conflict: null, lastError: null
    }]
  });
  await installAdapters(projectId, resolve("bin/resync.js"));

  await writeFile(join(local, "file"), "committed\n");
  const commit = await git(local, ["commit", "-am", "publish me"]);
  const expected = (await git(local, ["rev-parse", "HEAD"])).stdout.trim();
  let published = null;
  for (let attempt = 0; attempt < 50; attempt += 1) {
    published = (await git(remote, ["rev-parse", "refs/heads/main"], { allowFailure: true })).stdout.trim() || null;
    if (published === expected) break;
    await delay(100);
  }
  const daemonLog = await readFile(join(root, "state", "daemon.log"), "utf8").catch(() => "no daemon log");
  assert.equal(published, expected, `${commit.stderr}\n${daemonLog}`);
});
