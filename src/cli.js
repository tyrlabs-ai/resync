import { randomUUID } from "node:crypto";
import { chmod, mkdir, readFile, readdir, rename, rm, writeFile } from "node:fs/promises";
import { hostname } from "node:os";
import { basename, dirname, join, resolve } from "node:path";
import { ensureProjectConfig } from "./config.js";
import { canonicalOrigin, eraseCredential, loadCredential, removeDeviceIdentity } from "./credentials.js";
import { runDaemon } from "./daemon.js";
import { commandTree, parseOptions, renderHelp } from "./help.js";
import { clearIdentity, readIdentity, repositoryRoot, writeIdentity } from "./identity.js";
import { DEFAULT_PROVIDER, discover, enrollWithToken, providerFetch } from "./provider.js";
import { fetchCatalog, materializeProject } from "./remote.js";
import { rpc } from "./rpc.js";
import { daemonRpc } from "./daemon-client.js";
import { loadCatalog, loadConfig, statePaths, writeJson } from "./state.js";
import { recoverTransaction } from "./transaction.js";
import { git, run } from "./process.js";

function print(value) { process.stdout.write(`${JSON.stringify(value, null, 2)}\n`); }

async function configuredProvider(provider) {
  const config = await loadConfig();
  const origin = canonicalOrigin(provider || config.activeProvider || DEFAULT_PROVIDER);
  const token = await loadCredential(origin) || (config.server === origin ? config.token : null);
  if (!token) throw new Error(`not logged in to ${origin}; run \`resync auth login ${origin}\``);
  return { config, origin, token, discovery: await discover(origin) };
}

async function authLogin(args) {
  const { options, positional } = parseOptions(args, { "--token": "string", "--token-command": "string", "--device-name": "string" });
  const provider = positional[0] || DEFAULT_PROVIDER;
  let token = options.token || process.env.RESYNC_BOOTSTRAP_TOKEN;
  if (options.token_command) {
    const result = await run("/bin/sh", ["-c", options.token_command]);
    token = result.stdout.trim();
  }
  // Compatibility with the original `resync login SERVER TOKEN` spelling.
  token ||= positional[1];
  if (!token) throw new Error("login requires --token, --token-command, or RESYNC_BOOTSTRAP_TOKEN");
  return enrollWithToken({ provider, bootstrapToken: token, deviceName: options.device_name || hostname() });
}

async function authStatus(provider) {
  const config = await loadConfig();
  const origins = provider ? [canonicalOrigin(provider)] : Object.keys(config.providers || {});
  return {
    activeProvider: config.activeProvider,
    providers: await Promise.all(origins.map(async origin => ({
      ...config.providers[origin],
      origin,
      active: origin === config.activeProvider,
      credentialAvailable: Boolean(await loadCredential(origin))
    })))
  };
}

async function authLogout(args) {
  const { options, positional } = parseOptions(args, { "--all": "boolean" });
  const config = await loadConfig();
  const origins = options.all ? Object.keys(config.providers || {}) : [canonicalOrigin(positional[0] || config.activeProvider || DEFAULT_PROVIDER)];
  for (const origin of origins) {
    await eraseCredential(origin);
    await removeDeviceIdentity(config.providers?.[origin]?.publicKeyPath);
    if (config.providers) delete config.providers[origin];
  }
  if (origins.includes(config.activeProvider)) config.activeProvider = Object.keys(config.providers || {})[0] || null;
  delete config.server;
  delete config.token;
  await writeJson(statePaths().config, config, 0o600);
  return { loggedOut: origins, activeProvider: config.activeProvider };
}

async function persistProject(project, remoteCatalogGeneration) {
  const catalog = await loadCatalog();
  catalog.projects = catalog.projects.filter(item => item.projectId !== project.projectId && item.localPath !== project.localPath);
  catalog.projects.push(project);
  if (remoteCatalogGeneration != null) catalog.catalogGeneration = remoteCatalogGeneration;
  await writeJson(statePaths().catalog, catalog);
  return project;
}

async function projectJoin(projectId, localPath, provider) {
  if (!projectId) throw new Error("project join requires a project ID");
  const auth = await configuredProvider(provider);
  const remote = await fetchCatalog({ server: auth.origin, token: auth.token });
  const selected = remote.projects.find(project => project.project_id === projectId);
  if (!selected) throw new Error(`project ${projectId} is not in this device's catalog`);
  const destination = localPath || await repositoryRoot();
  const existingIdentity = await readIdentity(destination).catch(error => {
    if (localPath) return null;
    throw error;
  });
  if (existingIdentity?.projectId && (existingIdentity.projectId !== projectId || existingIdentity.service !== auth.origin)) {
    throw new Error(`checkout already belongs to ${existingIdentity.service} / ${existingIdentity.projectId}; use \`resync project fork\` for an independent project`);
  }
  const project = await materializeProject({ catalogProject: selected, localPath: destination, token: auth.token });
  const identity = await writeIdentity(project.localPath, { service: auth.origin, projectId });
  Object.assign(project, { service: auth.origin, checkoutId: identity.checkoutId });
  await ensureProjectConfig(project.localPath);
  await persistProject(project, remote.catalog_generation);
  await installAdapters(project.projectId);
  await daemonRpc({ method: "reload" });
  return project;
}

async function projectInit(args, fork = false) {
  const { options } = parseOptions(args, { "--name": "string", "--provider": "string" });
  const root = await repositoryRoot();
  const prior = await readIdentity(root);
  if (prior.projectId && !fork) throw new Error(`checkout already belongs to ${prior.service} / ${prior.projectId}; use \`resync project fork\` to separate it`);
  const auth = await configuredProvider(options.provider);
  const defaultBranch = (await git(root, ["symbolic-ref", "--short", "HEAD"])).stdout.trim();
  const name = options.name || basename(root);
  const created = await providerFetch(auth.discovery.endpoints.projects, auth.token, {
    method: "POST", body: { name, default_branch: defaultBranch }
  });
  if (!created?.project_id) throw new Error("provider returned an invalid project creation response");
  const project = await materializeProject({ catalogProject: created, localPath: root, token: auth.token });
  const identity = await writeIdentity(root, { service: auth.origin, projectId: created.project_id });
  Object.assign(project, { service: auth.origin, checkoutId: identity.checkoutId });
  await ensureProjectConfig(root);
  await persistProject(project);
  await installAdapters(project.projectId);
  await daemonRpc({ method: "reload" });
  return { ...project, operation: fork ? "forked" : "initialized", previousProjectId: prior.projectId || null };
}

async function unsubscribe(projectId) {
  const catalog = await loadCatalog();
  const before = catalog.projects.length;
  catalog.projects = catalog.projects.filter(project => project.projectId !== projectId);
  if (catalog.projects.length === before) throw new Error(`project ${projectId} is not subscribed`);
  await writeJson(statePaths().catalog, catalog);
  const identity = await readIdentity().catch(() => null);
  if (identity?.projectId === projectId) await clearIdentity(identity.root);
  return { unsubscribed: projectId, filesPreserved: true };
}

async function deviceList(provider) {
  const auth = await configuredProvider(provider);
  return providerFetch(auth.discovery.endpoints.devices || new URL("/v1/devices", auth.origin), auth.token);
}

async function deviceRevoke(deviceId, provider) {
  if (!deviceId) throw new Error("device revoke requires a device ID");
  const auth = await configuredProvider(provider);
  const base = auth.discovery.endpoints.devices || new URL("/v1/devices", auth.origin);
  return providerFetch(new URL(`${String(base).replace(/\/$/, "")}/${encodeURIComponent(deviceId)}/revoke`), auth.token, { method: "POST" });
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
      const result = await daemonRpc({
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
      await daemonRpc({ method: "releaseAccess", lease_id: lease.leaseId, outcome: "success", mutation_hint: "possible" });
      await rm(leasePath, { force: true });
      return {};
    } catch (error) {
      return { systemMessage: `RepoSync could not release the tool lease: ${error.message}` };
    }
  }
  return {};
}

export async function installAdapters(projectId, cliScript = resolve(process.argv[1])) {
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
  const hookCli = `${shellQuote(process.execPath)} ${shellQuote(cliScript)}`;
  const script = `#!/bin/sh\n# RepoSync dispatcher\nset -u\n[ ! -x \"$0.pre-resync\" ] || \"$0.pre-resync\" \"$@\"\ncommit=\"$(git rev-parse HEAD 2>/dev/null)\" || exit 0\nbranch=\"$(git symbolic-ref --short HEAD 2>/dev/null)\" || exit 0\n${hookCli} report-commit ${shellQuote(projectId)} \"$commit\" \"$branch\" >/dev/null || echo \"RepoSync could not schedule automatic publication\" >&2\n`;
  await writeFile(hookPath, script, { mode: 0o755 });
  await chmod(hookPath, 0o755);
  return { projectId, codexHooks: hooksPath, gitHook: hookPath, requiresCodexTrustReview: true };
}

function helpFor(path) {
  let node = commandTree;
  for (const part of path) node = node.commands?.[part];
  process.stdout.write(renderHelp({ ...node, name: ["resync", ...path].join(" ") }));
}

async function dispatch(group, command, args) {
  let result;
  if (group === "auth") {
    if (command === "login") result = await authLogin(args);
    else if (command === "status") result = await authStatus(parseOptions(args).positional[0]);
    else if (command === "logout") result = await authLogout(args);
  } else if (group === "project") {
    if (command === "init") result = await projectInit(args, false);
    else if (command === "fork") result = await projectInit(args, true);
    else if (command === "join") {
      const parsed = parseOptions(args, { "--path": "string", "--provider": "string" });
      result = await projectJoin(parsed.positional[0], parsed.options.path, parsed.options.provider);
    } else if (command === "leave") result = await unsubscribe(parseOptions(args).positional[0] || (await readIdentity()).projectId);
    else if (command === "status") result = await daemonRpc({ method: "status", project_id: parseOptions(args).positional[0] });
    else if (command === "sync") result = await daemonRpc({ method: "sync", project_id: parseOptions(args).positional[0] || (await readIdentity()).projectId });
    else if (command === "publish") result = await daemonRpc({ method: "publish", project_id: parseOptions(args).positional[0] || (await readIdentity()).projectId });
    else if (command === "conflicts") result = await conflicts(parseOptions(args).positional[0]);
    else if (command === "recover") result = await recover();
  } else if (group === "device") {
    const parsed = parseOptions(args, { "--provider": "string" });
    if (command === "list") result = await deviceList(parsed.options.provider);
    else if (command === "revoke") result = await deviceRevoke(parsed.positional[0], parsed.options.provider);
  } else if (group === "peer") {
    const peer = await import("./peer.js");
    if (command === "sync") result = await peer.peerSync(args);
    else if (command === "accept") result = await peer.peerAccept(args);
  }
  if (result === undefined) throw new Error(`unimplemented command: ${group} ${command}`);
  return result;
}

function extractGlobals(input) {
  const args = [...input];
  for (let index = 0; index < args.length;) {
    if (args[index] === "--state-dir") {
      if (!args[index + 1]) throw new Error("--state-dir requires a value");
      process.env.RESYNC_STATE_DIR = resolve(args[index + 1]);
      args.splice(index, 2);
    } else if (args[index].startsWith("--state-dir=")) {
      process.env.RESYNC_STATE_DIR = resolve(args[index].slice("--state-dir=".length));
      args.splice(index, 1);
    } else index += 1;
  }
  return args;
}

export async function main(input) {
  const args = extractGlobals(input);
  if (args.includes("--version")) { process.stdout.write("resync 0.2.0 (protocol resync.v1)\n"); return; }
  if (!args.length || args[0] === "help" || args[0] === "-h" || args[0] === "--help") { helpFor([]); return; }
  const root = args[0];
  const groupNode = commandTree.commands[root];
  if (groupNode?.commands) {
    if (args.length === 1 || ["-h", "--help", "help"].includes(args[1])) { helpFor([root]); return; }
    const command = args[1];
    if (!groupNode.commands[command]) throw new Error(`unknown ${root} command ${command}\n\n${renderHelp({ ...groupNode, name: `resync ${root}` })}`);
    if (args.slice(2).some(value => value === "-h" || value === "--help")) { helpFor([root, command]); return; }
    print(await dispatch(root, command, args.slice(2)));
    return;
  }
  if (groupNode && args.slice(1).some(value => value === "-h" || value === "--help")) { helpFor([root]); return; }

  // Engineering-preview aliases remain accepted while grouped commands are canonical.
  const aliases = {
    login: async rest => authLogin(rest[1] ? [rest[0], "--token", rest[1]] : rest),
    subscribe: async rest => projectJoin(rest[0], rest[1]),
    unsubscribe: async rest => unsubscribe(rest[0]),
    init: async rest => projectInit(rest, false),
    join: async rest => {
      const parsed = parseOptions(rest, { "--path": "string", "--provider": "string" });
      return projectJoin(parsed.positional[0], parsed.options.path, parsed.options.provider);
    },
    fork: async rest => projectInit(rest, true),
    devices: async rest => {
      if (rest[0] === "revoke") {
        const parsed = parseOptions(rest.slice(1), { "--provider": "string" });
        return deviceRevoke(parsed.positional[0], parsed.options.provider);
      }
      const parsed = parseOptions(rest, { "--provider": "string" });
      return deviceList(parsed.options.provider);
    },
    status: async rest => daemonRpc({ method: "status", project_id: rest[0] }),
    sync: async rest => daemonRpc({ method: "sync", project_id: rest[0] || (await readIdentity()).projectId }),
    publish: async rest => daemonRpc({ method: "publish", project_id: rest[0] || (await readIdentity()).projectId }),
    conflicts: async rest => conflicts(rest[0]),
    recover: async () => recover()
  };
  const aliasHelp = {
    login: ["auth", "login"], subscribe: ["project", "join"], unsubscribe: ["project", "leave"],
    init: ["project", "init"], join: ["project", "join"], fork: ["project", "fork"], devices: ["device"],
    status: ["project", "status"], sync: ["project", "sync"], publish: ["project", "publish"],
    conflicts: ["project", "conflicts"], recover: ["project", "recover"]
  };
  if (aliasHelp[root] && args.slice(1).some(value => value === "-h" || value === "--help")) { helpFor(aliasHelp[root]); return; }
  if (aliases[root]) { print(await aliases[root](args.slice(1))); return; }

  let result;
  if (root === "daemon") { await runDaemon(); return; }
  else if (root === "doctor") result = await doctor();
  else if (root === "install-adapters") result = await installAdapters(args[1]);
  else if (root === "acquire") result = await daemonRpc({ method: "acquireAccess", project_id: args[1], session_id: args[2] || randomUUID(), call_id: args[3] || randomUUID(), access_hint: "unknown" });
  else if (root === "release") result = await daemonRpc({ method: "releaseAccess", lease_id: args[1], outcome: "success", mutation_hint: "possible" });
  else if (root === "codex-hook") result = await codexHook(args[1]);
  else if (root === "report-commit") result = await daemonRpc({
    method: "reportCommit", project_id: args[1], commit_oid: args[2], branch: args[3]
  });
  else {
    // A non-command first argument is the ergonomic `resync <ssh-target>` form.
    if (args.slice(1).some(value => value === "-h" || value === "--help")) { helpFor(["peer", "sync"]); return; }
    const identity = await readIdentity().catch(() => null);
    if (!identity?.projectId) throw new Error(`unknown command ${root}; run \`resync --help\``);
    const peer = await import("./peer.js");
    result = await peer.peerSync([root, ...args.slice(1)]);
  }
  print(result);
}
