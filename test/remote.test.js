import test from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { git } from "../src/process.js";
import { fetchRemote, materializeProject, remoteHeads } from "../src/remote.js";

test("materialization and fetch track every hosted branch", async t => {
  const root = await mkdtemp(join(tmpdir(), "resync-branches-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const remote = join(root, "remote.git");
  const source = join(root, "source");
  const clone = join(root, "clone");
  await git(root, ["init", "--bare", "--initial-branch=main", remote]);
  await git(root, ["init", "--initial-branch=main", source]);
  await git(source, ["config", "user.name", "Test"]);
  await git(source, ["config", "user.email", "test@example.invalid"]);
  await writeFile(join(source, "main.txt"), "main\n");
  await git(source, ["add", "."]);
  await git(source, ["commit", "-m", "main"]);
  await git(source, ["switch", "-c", "feature/one"]);
  await writeFile(join(source, "feature.txt"), "one\n");
  await git(source, ["add", "."]);
  await git(source, ["commit", "-m", "feature"]);
  await git(source, ["switch", "main"]);

  const emptyCatalog = {
    project_id: "prj_test", remote_url: remote, default_branch: "main",
    advertised_heads: [], project_generation: 1
  };
  const first = await materializeProject({ catalogProject: emptyCatalog, localPath: source, token: null });
  assert.deepEqual(Object.keys(first.advertisedHeads).sort(), ["feature/one", "main"]);

  const populatedCatalog = {
    ...emptyCatalog,
    advertised_heads: Object.entries(first.advertisedHeads).map(([name, oid]) => ({ name, oid }))
  };
  const second = await materializeProject({ catalogProject: populatedCatalog, localPath: clone, token: null });
  assert.deepEqual(Object.keys(await remoteHeads(clone, "resync")).sort(), ["feature/one", "main"]);
  assert.equal(second.activeBranch, "main");

  await git(source, ["switch", "feature/one"]);
  await writeFile(join(source, "feature.txt"), "two\n");
  await git(source, ["commit", "-am", "feature two"]);
  await git(source, ["push", "resync", "feature/one"]);
  const fetched = await fetchRemote(second, null);
  assert.equal(fetched["feature/one"], (await git(source, ["rev-parse", "HEAD"])).stdout.trim());
});
