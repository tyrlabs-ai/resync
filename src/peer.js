import { hostname, homedir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { access, mkdir } from "node:fs/promises";
import { parseOptions } from "./help.js";
import { discover, providerFetch, storeProviderLogin } from "./provider.js";
import { generateDeviceIdentity, loadCredential } from "./credentials.js";
import { readIdentity, sameProject, writeIdentity } from "./identity.js";
import { fetchCatalog, materializeProject } from "./remote.js";
import { ensureProjectConfig } from "./config.js";
import { loadCatalog, loadConfig, writeJson, statePaths } from "./state.js";
import { git, run } from "./process.js";
import { rpc } from "./rpc.js";

const packageRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");

function shellQuote(value) {
  return `'${String(value).replaceAll("'", `'\\''`)}'`;
}

function parseSshTarget(value, explicitPath) {
  if (!value) throw new Error("peer sync requires an SSH target");
  if (explicitPath) return { host: value, path: explicitPath };
  const index = value.indexOf(":");
  if (index > 0 && !value.startsWith("ssh://")) return { host: value.slice(0, index), path: value.slice(index + 1) || null };
  return { host: value, path: null };
}

async function exists(path) {
  try { await access(path); return true; }
  catch (error) { if (error.code === "ENOENT") return false; throw error; }
}

async function persistProject(project, generation) {
  const catalog = await loadCatalog();
  catalog.projects = catalog.projects.filter(item => item.projectId !== project.projectId && item.localPath !== project.localPath);
  catalog.projects.push(project);
  if (generation != null) catalog.catalogGeneration = generation;
  await writeJson(statePaths().catalog, catalog);
}

export function ticketEndpoint(discovery, projectId) {
  const template = discovery.endpoints.join_tickets;
  if (!template) return new URL(`/v1/projects/${encodeURIComponent(projectId)}/join-tickets`, discovery.origin);
  return template
    .replace("{project_id}", encodeURIComponent(projectId))
    .replace(/%7Bproject_id%7D/i, encodeURIComponent(projectId));
}

export async function peerSync(args) {
  const parsed = parseOptions(args, { "--into": "string", "--name": "string", "--no-bootstrap": "boolean" });
  const target = parseSshTarget(parsed.positional[0], parsed.options.into);
  const identity = await readIdentity();
  if (!identity.projectId) throw new Error("current checkout is not enrolled; run `resync project init` first");
  const config = await loadConfig();
  const token = await loadCredential(identity.service) || (config.server === identity.service ? config.token : null);
  if (!token) throw new Error(`no credential for ${identity.service}`);
  const discovery = await discover(identity.service);

  let catalog = await fetchCatalog({ server: identity.service, token });
  let project = catalog.projects.find(item => item.project_id === identity.projectId);
  if (!project) throw new Error("current device is no longer authorized for this project");
  const localHead = (await git(identity.root, ["rev-parse", "HEAD"])).stdout.trim();
  if (project.advertised_head_oid !== localHead) {
    try { await rpc({ method: "publish", project_id: identity.projectId }); }
    catch (error) { throw new Error(`committed work is not published; start the daemon and run \`resync project publish\` before peer sync (${error.message})`); }
    catalog = await fetchCatalog({ server: identity.service, token });
    project = catalog.projects.find(item => item.project_id === identity.projectId);
    if (project?.advertised_head_oid !== localHead) throw new Error("hosted head did not advance to the current commit");
  }

  const ticket = await providerFetch(ticketEndpoint(discovery, identity.projectId), token, { method: "POST", body: { ttl_seconds: 300 } });
  const remoteRoot = ".local/share/resync/bootstrap/0.2.0";
  if (!parsed.options.no_bootstrap) {
    await run("rsync", [
      "--archive", "--mkpath", "--exclude=.git", "--exclude=node_modules", "--exclude=test", "-e", "ssh",
      `${packageRoot}/`, `${target.host}:${remoteRoot}/`
    ]);
  }
  const executable = parsed.options.no_bootstrap ? "resync" : `node \"$HOME/${remoteRoot}/bin/resync.js\"`;
  const remoteArguments = [
    "peer", "accept", "--provider", identity.service, "--ticket", ticket.ticket,
    "--project", identity.projectId, "--device-name", parsed.options.name || target.host,
    ...(target.path ? ["--into", target.path] : [])
  ].map(shellQuote).join(" ");
  const script = `set -eu\nexec ${executable} ${remoteArguments}\n`;
  const response = await run("ssh", [target.host, "sh", "-s"], { input: script });
  let remote;
  try { remote = JSON.parse(response.stdout); }
  catch { throw new Error(`peer returned an invalid response: ${response.stdout.slice(0, 500)}`); }
  if (remote.projectId !== identity.projectId || remote.service !== identity.service) throw new Error("peer enrolled into an unexpected project identity");
  const dirty = (await git(identity.root, ["status", "--porcelain"])).stdout.trim();
  return {
    projectId: identity.projectId,
    service: identity.service,
    localCheckoutId: identity.checkoutId,
    peer: target.host,
    peerPath: remote.localPath,
    peerCheckoutId: remote.checkoutId,
    committedHead: localHead,
    warning: dirty ? "local uncommitted work remains only on this checkout" : null
  };
}

export async function peerAccept(args) {
  const parsed = parseOptions(args, {
    "--provider": "string", "--ticket": "string", "--project": "string", "--into": "string", "--device-name": "string"
  });
  const { provider, ticket, project, into, device_name: deviceName } = parsed.options;
  if (!provider || !ticket || !project) throw new Error("peer accept requires --provider, --ticket, and --project");
  const discovery = await discover(provider);
  const key = await generateDeviceIdentity(deviceName || hostname());
  const enrollment = await providerFetch(discovery.endpoints.redeem_join_ticket, null, {
    method: "POST", body: { ticket, device_name: deviceName || hostname(), public_key: key.publicKey }
  });
  await storeProviderLogin({ discovery, enrollment, key });
  const catalog = await fetchCatalog({ server: discovery.origin, token: enrollment.token });
  const catalogProject = catalog.projects.find(item => item.project_id === project);
  if (!catalogProject) throw new Error("join ticket did not authorize the expected project");
  let destination = into;
  if (destination?.startsWith("~/")) destination = join(homedir(), destination.slice(2));
  destination = destination ? resolve(destination) : join(homedir(), ".local", "share", "resync", "checkouts", project, catalogProject.name);

  if (await exists(destination)) {
    const current = await readIdentity(destination).catch(() => null);
    if (current?.projectId && !sameProject(current, { service: discovery.origin, projectId: project })) {
      throw new Error(`target path belongs to ${current.service} / ${current.projectId}; refusing to relink it`);
    }
  } else await mkdir(dirname(destination), { recursive: true });

  const localProject = await materializeProject({ catalogProject, localPath: destination, token: enrollment.token });
  const identity = await writeIdentity(localProject.localPath, { service: discovery.origin, projectId: project });
  Object.assign(localProject, { service: discovery.origin, checkoutId: identity.checkoutId });
  await ensureProjectConfig(localProject.localPath, localProject.syncBranch);
  await persistProject(localProject, catalog.catalog_generation);
  return { projectId: project, service: discovery.origin, checkoutId: identity.checkoutId, localPath: localProject.localPath };
}
