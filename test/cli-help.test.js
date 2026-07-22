import test from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createServer } from "node:http";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { commandTree } from "../src/help.js";

const cli = resolve("bin/resync.js");

function invoke(args, env = {}) {
  return new Promise((resolveResult, reject) => {
    const child = spawn(process.execPath, [cli, ...args], { env: { ...process.env, ...env }, stdio: ["ignore", "pipe", "pipe"] });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", chunk => { stdout += chunk; });
    child.stderr.on("data", chunk => { stderr += chunk; });
    child.on("error", reject);
    child.on("exit", code => resolveResult({ code, stdout, stderr }));
  });
}

test("-h and --help work at every public command level", async () => {
  for (const flag of ["-h", "--help"]) {
    const root = await invoke([flag]);
    assert.equal(root.code, 0);
    for (const group of ["auth", "project", "device", "peer"]) assert.match(root.stdout, new RegExp(`\\b${group}\\b`));
    for (const [name, node] of Object.entries(commandTree.commands)) {
      const level = await invoke([name, flag]);
      assert.equal(level.code, 0, `${name} ${flag}: ${level.stderr}`);
      assert.match(level.stdout, /Usage:/);
      if (node.commands) {
        for (const command of Object.keys(node.commands)) {
          assert.match(level.stdout, new RegExp(`\\b${command}\\b`));
          const leaf = await invoke([name, command, flag]);
          assert.equal(leaf.code, 0, `${name} ${command} ${flag}: ${leaf.stderr}`);
          assert.match(leaf.stdout, new RegExp(`Usage:\\n  resync ${name} ${command.replace(/[.*+?^${}()|[\\]\\]/g, "\\$&")}`));
        }
      }
    }
    for (const alias of ["login", "subscribe", "unsubscribe", "init", "join", "fork", "devices", "status", "sync", "publish", "conflicts", "recover"]) {
      const result = await invoke([alias, flag]);
      assert.equal(result.code, 0, `${alias} ${flag}: ${result.stderr}`);
      assert.match(result.stdout, /Usage:/);
    }
  }
});

test("auth login discovers a provider, enrolls a device, stores secrets separately, and logs out", async t => {
  const state = await mkdtemp(join(tmpdir(), "resync-auth-test-"));
  t.after(() => rm(state, { recursive: true, force: true }));
  let origin;
  const server = createServer(async (request, response) => {
    const chunks = [];
    for await (const chunk of request) chunks.push(chunk);
    const body = chunks.length ? JSON.parse(Buffer.concat(chunks)) : {};
    const send = (status, value) => { response.writeHead(status, { "content-type": "application/json" }); response.end(JSON.stringify(value)); };
    if (request.url === "/.well-known/resync") return send(200, {
      protocol: "resync.v1", service_id: "svc_test", auth_methods: ["bootstrap-token", "join-ticket"],
      endpoints: { enroll: "/v1/auth/enroll", catalog: "/v1/catalog", projects: "/v1/projects", devices: "/v1/devices" }
    });
    if (request.url === "/v1/auth/enroll" && request.method === "POST") {
      assert.equal(request.headers.authorization, "Bearer bootstrap-secret");
      assert.equal(body.device_name, "test-laptop");
      assert.match(body.public_key, /BEGIN PUBLIC KEY/);
      return send(201, { device: { id: "dev_test", name: body.device_name, project_ids: [] }, token: "device-secret" });
    }
    if (request.url === "/v1/catalog") {
      assert.equal(request.headers.authorization, "Bearer device-secret");
      return send(200, { protocol: "resync.v1", catalog_generation: 1, projects: [] });
    }
    return send(404, { error: "not found" });
  });
  await new Promise(resolveListen => server.listen(0, "127.0.0.1", resolveListen));
  t.after(() => new Promise(resolveClose => server.close(resolveClose)));
  origin = `http://127.0.0.1:${server.address().port}`;

  const loggedIn = await invoke(["auth", "login", origin, "--token", "bootstrap-secret", "--device-name", "test-laptop"], { RESYNC_STATE_DIR: state });
  assert.equal(loggedIn.code, 0, loggedIn.stderr);
  assert.equal(JSON.parse(loggedIn.stdout).device.id, "dev_test");
  const configuration = JSON.parse(await readFile(join(state, "config.json"), "utf8"));
  const credentials = JSON.parse(await readFile(join(state, "credentials.json"), "utf8"));
  assert.equal(JSON.stringify(configuration).includes("device-secret"), false);
  assert.equal(credentials.providers[origin].token, "device-secret");

  const status = await invoke(["auth", "status"], { RESYNC_STATE_DIR: state });
  assert.equal(status.code, 0);
  assert.equal(JSON.parse(status.stdout).providers[0].credentialAvailable, true);
  assert.equal(status.stdout.includes("device-secret"), false);

  const logout = await invoke(["auth", "logout", origin], { RESYNC_STATE_DIR: state });
  assert.equal(logout.code, 0, logout.stderr);
  const after = await invoke(["auth", "status"], { RESYNC_STATE_DIR: state });
  assert.deepEqual(JSON.parse(after.stdout).providers, []);
});
