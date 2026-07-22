import { chmod, mkdir, open, readFile, rename } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { homedir } from "node:os";

export function stateDirectory() {
  return resolve(process.env.RESYNC_STATE_DIR || join(homedir(), ".local", "state", "resync"));
}

export function statePaths() {
  const root = stateDirectory();
  return {
    root,
    config: join(root, "config.json"),
    credentials: join(root, "credentials.json"),
    keys: join(root, "keys"),
    catalog: join(root, "catalog.json"),
    transactions: join(root, "transactions"),
    socket: process.env.RESYNC_SOCKET || join(root, "daemon.sock")
  };
}

export async function readJson(path, fallback) {
  try { return JSON.parse(await readFile(path, "utf8")); }
  catch (error) { if (error.code === "ENOENT") return structuredClone(fallback); throw error; }
}

export async function writeJson(path, value, mode = 0o600) {
  await mkdir(dirname(path), { recursive: true, mode: 0o700 });
  const temporary = `${path}.${process.pid}.tmp`;
  const handle = await open(temporary, "w", mode);
  try {
    await handle.writeFile(`${JSON.stringify(value, null, 2)}\n`);
    await handle.sync();
  } finally { await handle.close(); }
  await chmod(temporary, mode);
  await rename(temporary, path);
}

export async function loadConfig() {
  const config = await readJson(statePaths().config, { protocol: "resync.v1", activeProvider: null, providers: {} });
  // Read the engineering-preview shape without forcing users to log in again.
  if (config.server && !config.activeProvider) {
    const origin = new URL(config.server).origin;
    config.activeProvider = origin;
    config.providers = { [origin]: { origin, deviceId: null, deviceName: "legacy device", publicKeyPath: null } };
  }
  const { loadCredential } = await import("./credentials.js");
  const origin = config.activeProvider;
  const token = origin ? (await loadCredential(origin)) || config.token || null : null;
  return { ...config, server: origin, token };
}

export async function loadCatalog() {
  return readJson(statePaths().catalog, { catalogGeneration: 0, projects: [] });
}
