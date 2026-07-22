import test from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { git, run } from "../src/process.js";
import { captureWorkspace } from "../src/git-state.js";
import { reconcileWorkspace } from "../src/transaction.js";

async function setup() {
  const root = await mkdtemp(join(tmpdir(), "resync-test-"));
  const remote = join(root, "remote.git");
  const seed = join(root, "seed");
  const local = join(root, "local");
  const upstream = join(root, "upstream");
  await git(root, ["init", "--bare", "--initial-branch=main", remote]);
  await git(root, ["init", "--initial-branch=main", seed]);
  await git(seed, ["config", "user.name", "Test"]);
  await git(seed, ["config", "user.email", "test@example.invalid"]);
  await writeFile(join(seed, "app.txt"), "base\n");
  await writeFile(join(seed, "other.txt"), "base\n");
  await git(seed, ["add", "."]);
  await git(seed, ["commit", "-m", "base"]);
  await git(seed, ["remote", "add", "origin", remote]);
  await git(seed, ["push", "-u", "origin", "main"]);
  await git(root, ["clone", remote, local]);
  await git(root, ["clone", remote, upstream]);
  for (const path of [local, upstream]) {
    await git(path, ["config", "user.name", "Test"]);
    await git(path, ["config", "user.email", "test@example.invalid"]);
  }
  const base = (await git(local, ["rev-parse", "HEAD"])).stdout.trim();
  return { root, remote, local, upstream, base };
}

async function advance(path, file, content, message = "upstream") {
  await mkdir(join(path, file, ".."), { recursive: true });
  await writeFile(join(path, file), content);
  await git(path, ["add", file]);
  await git(path, ["commit", "-m", message]);
  await git(path, ["push", "origin", "main"]);
  return (await git(path, ["rev-parse", "HEAD"])).stdout.trim();
}

test("clean reconciliation preserves staged and unstaged layers", async t => {
  const fixture = await setup();
  t.after(() => rm(fixture.root, { recursive: true, force: true }));
  await writeFile(join(fixture.local, "app.txt"), "local staged\n");
  await git(fixture.local, ["add", "app.txt"]);
  await writeFile(join(fixture.local, "app.txt"), "local unstaged\n");
  const target = await advance(fixture.upstream, "other.txt", "upstream\n");
  await git(fixture.local, ["fetch", "origin"]);

  const result = await reconcileWorkspace({
    projectPath: fixture.local,
    previousRemoteOid: fixture.base,
    targetRemoteOid: target,
    transactionRoot: join(fixture.root, "transactions")
  });

  assert.equal(result.outcome, "applied");
  assert.equal(await readFile(join(fixture.local, "other.txt"), "utf8"), "upstream\n");
  assert.equal(await readFile(join(fixture.local, "app.txt"), "utf8"), "local unstaged\n");
  assert.match((await git(fixture.local, ["diff", "--cached", "--", "app.txt"])).stdout, /local staged/);
  assert.match((await git(fixture.local, ["diff", "--", "app.txt"])).stdout, /local unstaged/);
  assert.equal((await git(fixture.local, ["rev-parse", "HEAD"])).stdout.trim(), target);
});

test("Git conflict leaves active workspace exactly unchanged", async t => {
  const fixture = await setup();
  t.after(() => rm(fixture.root, { recursive: true, force: true }));
  await writeFile(join(fixture.local, "app.txt"), "local\n");
  const before = await captureWorkspace(fixture.local);
  const target = await advance(fixture.upstream, "app.txt", "remote\n");
  await git(fixture.local, ["fetch", "origin"]);

  const result = await reconcileWorkspace({
    projectPath: fixture.local,
    previousRemoteOid: fixture.base,
    targetRemoteOid: target,
    transactionRoot: join(fixture.root, "transactions")
  });

  assert.equal(result.outcome, "conflict");
  assert.deepEqual(await captureWorkspace(fixture.local), before);
  assert.equal(await readFile(join(fixture.local, "app.txt"), "utf8"), "local\n");
});

test("incoming tracked path cannot overwrite an untracked local file", async t => {
  const fixture = await setup();
  t.after(() => rm(fixture.root, { recursive: true, force: true }));
  await writeFile(join(fixture.local, "collision.txt"), "precious local data\n");
  const before = await captureWorkspace(fixture.local);
  const target = await advance(fixture.upstream, "collision.txt", "remote data\n");
  await git(fixture.local, ["fetch", "origin"]);

  const result = await reconcileWorkspace({
    projectPath: fixture.local,
    previousRemoteOid: fixture.base,
    targetRemoteOid: target,
    transactionRoot: join(fixture.root, "transactions")
  });

  assert.equal(result.outcome, "conflict");
  assert.deepEqual(result.paths, ["collision.txt"]);
  assert.deepEqual(await captureWorkspace(fixture.local), before);
  assert.equal(await readFile(join(fixture.local, "collision.txt"), "utf8"), "precious local data\n");
});

test("incoming tracked path cannot overwrite an ignored local file", async t => {
  const fixture = await setup();
  t.after(() => rm(fixture.root, { recursive: true, force: true }));
  await writeFile(join(fixture.local, ".gitignore"), "ignored.txt\n");
  await git(fixture.local, ["add", ".gitignore"]);
  await git(fixture.local, ["commit", "-m", "ignore local artifact"]);
  const localHead = (await git(fixture.local, ["rev-parse", "HEAD"])).stdout.trim();
  await writeFile(join(fixture.local, "ignored.txt"), "ignored but precious\n");
  await git(fixture.upstream, ["pull", "--ff-only"]);
  const target = await advance(fixture.upstream, "ignored.txt", "remote tracked\n");
  await git(fixture.local, ["fetch", "origin"]);
  const result = await reconcileWorkspace({
    projectPath: fixture.local,
    previousRemoteOid: fixture.base,
    targetRemoteOid: target,
    transactionRoot: join(fixture.root, "transactions")
  });
  assert.equal(result.outcome, "conflict");
  assert.deepEqual(result.paths, ["ignored.txt"]);
  assert.equal(await readFile(join(fixture.local, "ignored.txt"), "utf8"), "ignored but precious\n");
  assert.equal((await git(fixture.local, ["rev-parse", "HEAD"])).stdout.trim(), localHead);
});

test("rewritten remote history is rejected before workspace mutation", async t => {
  const fixture = await setup();
  t.after(() => rm(fixture.root, { recursive: true, force: true }));
  const unrelated = (await git(fixture.local, ["commit-tree", "HEAD^{tree}"], {
    env: {
      GIT_AUTHOR_NAME: "Test", GIT_AUTHOR_EMAIL: "test@example.invalid",
      GIT_COMMITTER_NAME: "Test", GIT_COMMITTER_EMAIL: "test@example.invalid"
    },
    input: "unrelated\n"
  })).stdout.trim();
  const before = await captureWorkspace(fixture.local);
  await assert.rejects(() => reconcileWorkspace({
    projectPath: fixture.local,
    previousRemoteOid: fixture.base,
    targetRemoteOid: unrelated,
    transactionRoot: join(fixture.root, "transactions")
  }), /rewritten|does not descend/);
  assert.deepEqual(await captureWorkspace(fixture.local), before);
});
