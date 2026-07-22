import test from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { git } from "../src/process.js";
import { readIdentity, sameProject, writeIdentity } from "../src/identity.js";

async function repository() {
  const root = await mkdtemp(join(tmpdir(), "resync-identity-test-"));
  const source = join(root, "source");
  await git(root, ["init", "--initial-branch=main", source]);
  await git(source, ["config", "user.name", "Test"]);
  await git(source, ["config", "user.email", "test@example.invalid"]);
  await writeFile(join(source, "README.md"), "same content\n");
  await git(source, ["add", "."]);
  await git(source, ["commit", "-m", "base"]);
  return { root, source };
}

test("ordinary Git clones do not inherit RepoSync project or checkout identity", async t => {
  const fixture = await repository();
  t.after(() => rm(fixture.root, { recursive: true, force: true }));
  const sourceIdentity = await writeIdentity(fixture.source, {
    service: "http://127.0.0.1:9876", projectId: "prj_original"
  });
  const clone = join(fixture.root, "clone");
  await git(fixture.root, ["clone", fixture.source, clone]);
  const clonedIdentity = await readIdentity(clone);
  assert.equal(clonedIdentity.projectId, null);
  assert.equal(clonedIdentity.checkoutId, null);
  assert.equal(sourceIdentity.projectId, "prj_original");
});

test("identical Git histories may have separate projects or separate checkouts of one project", async t => {
  const fixture = await repository();
  t.after(() => rm(fixture.root, { recursive: true, force: true }));
  const second = join(fixture.root, "second");
  const third = join(fixture.root, "third");
  await git(fixture.root, ["clone", fixture.source, second]);
  await git(fixture.root, ["clone", fixture.source, third]);
  const a = await writeIdentity(fixture.source, { service: "http://127.0.0.1:9876", projectId: "prj_shared" });
  const b = await writeIdentity(second, { service: "http://127.0.0.1:9876", projectId: "prj_shared" });
  const c = await writeIdentity(third, { service: "http://127.0.0.1:9876", projectId: "prj_independent" });
  assert.equal(sameProject(a, b), true);
  assert.notEqual(a.checkoutId, b.checkoutId);
  assert.equal(sameProject(a, c), false);
  assert.equal((await git(fixture.source, ["rev-parse", "HEAD"])).stdout, (await git(third, ["rev-parse", "HEAD"])).stdout);
});
