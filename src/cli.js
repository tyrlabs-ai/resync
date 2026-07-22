import { randomUUID } from "node:crypto";
import { chmod, mkdir, readFile, readdir, rename, rm, writeFile } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { ensureProjectConfig } from "./config.js";
import { runDaemon } from "./daemon.js";
import { fetchCatalog, materializeProject } from "./remote.js";
import { rpc } from "./rpc.js";
import { loadCatalog, loadConfig, statePaths, writeJson } from "./state.js";
import { recoverTransaction } from "./transaction.js";

const help = `RepoSync cooperative continuous rebase

Usage:
  resync login <server-url> <device-token>
  resync subscribe <project-id> <local-path>
  resync unsubscribe <project-id>
  resync daemon
  resync status [project-id]
  resync sync <project-id>
  resync publish <project-id>
  resync acquire <project-id> [session-id] [call-id]
  resync release <lease-id>
  resync install-adapters <project-id>
  resync codex-hook <project-id>       # invoked by Codex hooks on stdin
  resync report-commit <project-id>    # invoked by the Git hook
  resync conflicts [project-id]
  resync recover
  resync doctor
`;

function print(value) { process.stdout.write(`${JSON.stringify(value, null, 2)}\n`); }

async function login(server, token) {
  if (!server || !token) throw new Error("usage: resync login <server-url> <device-token>");
  const config = { protocol: "resync.v1", server: new URL(server).origin, token };
  await fetchCatalog(config);
  await writeJson(statePaths().config, config);
  return { server: config.server, authenticated: true };
}

async function subscribe(projectId, localPath) {
  if (!projectId || !localPath) throw new Error("usage: resync subscribe <project-id> <local-path>");
  const config = await loadConfig();
  const remote = await fetchCatalog(config);
  const selected = remote.projects.find(project => project.project_id === projectId);
  if (!selected) throw new Error(`project ${projectId} is not in this device's catalog`);
  const project = await materializeProject({ catalogProject: selected, localPath, token: config.token });
  await ensureProjectConfig(project.localPath, project.syncBranch);
  const catalog = await loadCatalog();
  catalog.projects = catalog.projects.filter(item => item.projectId !== projectId && item.localPath !== project.localPath);
  catalog.projects.push(project);
  catalog.catalogGeneration = remote.catalog_generation;
  await writeJson(statePaths().catalog, catalog);
  return project;
}

async function unsubscribe(projectId) {
  const catalog = await loadCatalog();
  const before = catalog.projects.length;
  catalog.projects = catalog.projects.filter(project => project.projectId !== projectId);
  if (catalog.projects.length === before) throw new Error(`project ${projectId} is not subscribed`);
  await writeJson(statePaths().catalog, catalog);
  return { unsubscribed: projectId, filesPreserved: true };
}

async function conflicts(projectId) {
  const catalog = await loadCatalog();
  return catalog.projects.filter(project => (!projectId || project.projectId === projectId) && project.state === "CONFLICTED")
    .map(project => ({ projectId: project.projectId, localPath: project.localPath, conflict: project.conflict }));
}

async function recover() {
  const paths = statePaths();
  let names;
  try { names = await readdir(paths.transactions); }
  catch (error) { if (error.code === "ENOENT") return []; throw error; }
  const recovered = [];
  for (const name of names.filter(name => name.endsWith(".json"))) recovered.push(await recoverTransaction(resolve(paths.transactions, name)));
  return recovered.map(item => ({ id: item.id, phase: item.phase }));
}

async function doctor() {
  const checks = [];
  for (const program of ["git", "node"]) {
    const { run } = await import("./process.js");
    const result = await run(program, ["--version"], { allowFailure: true });
    checks.push({ name: program, ok: result.code === 0, detail: (result.stdout || result.stderr).trim() });
  }
  const config = await loadConfig();
  try { await fetchCatalog(config); checks.push({ name: "hosted catalog", ok: true }); }
  catch (error) { checks.push({ name: "hosted catalog", ok: false, detail: error.message }); }
  try { await rpc({ method: "ping" }); checks.push({ name: "daemon", ok: true }); }
  catch (error) { checks.push({ name: "daemon", ok: false, detail: error.message }); }
  return checks;
}

function safeHookPart(value) {
  return String(value || "unknown").replace(/[^A-Za-z0-9._-]/g, "_").slice(0, 160);
}

function shellQuote(value) {
  return `'${String(value).replaceAll("'", `'\\''`)}'`;
}

async function codexHook(projectId) {
  const input = JSON.parse(await new Promise((resolveInput, reject) => {
    let value = "";
    process.stdin.setEncoding("utf8");
    process.stdin.on("data", chunk => { value += chunk; if (value.length > 1024 * 1024) reject(new Error("hook input exceeds 1 MiB")); });
    process.stdin.on("end", () => resolveInput(value));
    process.stdin.on("error", reject);
  }));
  const directory = join(statePaths().root, "codex-leases", safeHookPart(input.session_id));
  const leasePath = join(directory, `${safeHookPart(input.tool_use_id)}.json`);
  if (input.hook_event_name === "PreToolUse") {
    try {
      const result = await rpc({
        method: "acquireAccess", project_id: projectId, session_id: input.session_id,
        call_id: input.tool_use_id, access_hint: "unknown"
      });
      if (result.blocked) {
        return {
          hookSpecificOutput: {
            hookEventName: "PreToolUse", permissionDecision: "deny",
            permissionDecisionReason: result.model_message || `RepoSync blocked the workspace in ${result.state}`
          }
        };
      }
      await mkdir(directory, { recursive: true, mode: 0o700 });
      await writeJson(leasePath, { leaseId: result.lease_id, projectId, toolUseId: input.tool_use_id });
      return {
        hookSpecificOutput: {
          hookEventName: "PreToolUse",
          ...(result.model_message ? { additionalContext: result.model_message } : {})
        }
      };
    } catch (error) {
      return {
        hookSpecificOutput: {
          hookEventName: "PreToolUse", permissionDecision: "deny",
          permissionDecisionReason: `RepoSync access barrier failed: ${error.message}`
        }
      };
    }
  }
  if (input.hook_event_name === "PostToolUse") {
    try {
      const lease = JSON.parse(await readFile(leasePath, "utf8"));
      await rpc({ method: "releaseAccess", lease_id: lease.leaseId, outcome: "success", mutation_hint: "possible" });
      await rm(leasePath, { force: true });
      return {};
    } catch (error) {
      return { systemMessage: `RepoSync could not release the tool lease: ${error.message}` };
    }
  }
  return {};
}

async function installAdapters(projectId) {
  const catalog = await loadCatalog();
  const project = catalog.projects.find(item => item.projectId === projectId);
  if (!project) throw new Error(`project ${projectId} is not subscribed`);
  const codexDirectory = join(project.localPath, ".codex");
  const hooksPath = join(codexDirectory, "hooks.json");
  let hooks = { description: "RepoSync cooperative workspace leases", hooks: {} };
  try { hooks = JSON.parse(await readFile(hooksPath, "utf8")); }
  catch (error) { if (error.code !== "ENOENT") throw error; }
  hooks.hooks ||= {};
  const marker = `resync codex-hook ${shellQuote(projectId)}`;
  const definition = event => ({
    matcher: "*",
    hooks: [{ type: "command", command: marker, timeout: 3600, statusMessage: event === "PreToolUse" ? "Refreshing RepoSync workspace" : "Releasing RepoSync workspace" }]
  });
  for (const event of ["PreToolUse", "PostToolUse"]) {
    hooks.hooks[event] ||= [];
    hooks.hooks[event] = hooks.hooks[event].filter(group => !group.hooks?.some(item => item.command === marker));
    hooks.hooks[event].push(definition(event));
  }
  await mkdir(codexDirectory, { recursive: true });
  await writeJson(hooksPath, hooks, 0o644);

  const { git } = await import("./process.js");
  const hookDirectory = (await git(project.localPath, ["rev-parse", "--path-format=absolute", "--git-path", "hooks"])).stdout.trim();
  const hookPath = join(hookDirectory, "post-commit");
  const previous = `${hookPath}.pre-resync`;
  await mkdir(dirname(hookPath), { recursive: true });
  try {
    const existing = await readFile(hookPath, "utf8");
    if (!existing.includes("# RepoSync dispatcher")) await rename(hookPath, previous);
  } catch (error) { if (error.code !== "ENOENT") throw error; }
  const script = `#!/bin/sh\n# RepoSync dispatcher\nset -u\n[ ! -x \"$0.pre-resync\" ] || \"$0.pre-resync\" \"$@\"\ncommit=\"$(git rev-parse HEAD 2>/dev/null)\" || exit 0\nresync report-commit ${shellQuote(projectId)} \"$commit\" >/dev/null 2>&1 || true\n`;
  await writeFile(hookPath, script, { mode: 0o755 });
  await chmod(hookPath, 0o755);
  return { projectId, codexHooks: hooksPath, gitHook: hookPath, requiresCodexTrustReview: true };
}

export async function main(args) {
  const [command, ...rest] = args;
  if (!command || command === "help" || command === "--help" || command === "-h") {
    process.stdout.write(help);
    return;
  }
  let result;
  switch (command) {
    case "login": result = await login(rest[0], rest[1]); break;
    case "subscribe": result = await subscribe(rest[0], rest[1]); break;
    case "unsubscribe": result = await unsubscribe(rest[0]); break;
    case "daemon": await runDaemon(); return;
    case "status": result = await rpc({ method: "status", project_id: rest[0] }); break;
    case "sync": result = await rpc({ method: "sync", project_id: rest[0] }); break;
    case "publish": result = await rpc({ method: "publish", project_id: rest[0] }); break;
    case "acquire": result = await rpc({
      method: "acquireAccess", project_id: rest[0], session_id: rest[1] || randomUUID(), call_id: rest[2] || randomUUID(), access_hint: "unknown"
    }); break;
    case "release": result = await rpc({ method: "releaseAccess", lease_id: rest[0], outcome: "success", mutation_hint: "possible" }); break;
    case "install-adapters": result = await installAdapters(rest[0]); break;
    case "codex-hook": result = await codexHook(rest[0]); break;
    case "report-commit": result = await rpc({ method: "reportCommit", project_id: rest[0], commit_oid: rest[1] }); break;
    case "conflicts": result = await conflicts(rest[0]); break;
    case "recover": result = await recover(); break;
    case "doctor": result = await doctor(); break;
    default: throw new Error(`unknown command ${command}\n\n${help}`);
  }
  print(result);
}
