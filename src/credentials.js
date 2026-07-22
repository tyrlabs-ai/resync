import { generateKeyPairSync, randomUUID } from "node:crypto";
import { chmod, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { run } from "./process.js";
import { readJson, statePaths, writeJson } from "./state.js";

function canonicalOrigin(value) {
  const url = new URL(value);
  if (url.protocol !== "https:" && !(url.protocol === "http:" && ["localhost", "127.0.0.1", "::1"].includes(url.hostname))) {
    throw new Error("RepoSync providers must use HTTPS except on localhost");
  }
  return url.origin;
}

async function helper(operation, origin, value) {
  const command = process.env.RESYNC_CREDENTIAL_HELPER;
  if (!command) return null;
  const result = await run("/bin/sh", ["-c", command], {
    input: `${JSON.stringify({ operation, origin, value })}\n`, allowFailure: true
  });
  if (result.code !== 0) throw new Error(`credential helper failed: ${result.stderr.trim() || `exit ${result.code}`}`);
  if (operation !== "get") return true;
  const response = result.stdout.trim();
  return response ? JSON.parse(response).token || null : null;
}

export async function loadCredential(provider) {
  const origin = canonicalOrigin(provider);
  const external = await helper("get", origin);
  if (external != null) return external;
  if (process.env.RESYNC_TOKEN && (!process.env.RESYNC_PROVIDER || canonicalOrigin(process.env.RESYNC_PROVIDER) === origin)) {
    return process.env.RESYNC_TOKEN;
  }
  const credentials = await readJson(statePaths().credentials, { version: 1, providers: {} });
  return credentials.providers[origin]?.token || null;
}

export async function storeCredential(provider, token) {
  const origin = canonicalOrigin(provider);
  if (!token) throw new Error("cannot store an empty credential");
  if (await helper("store", origin, { token })) return;
  const credentials = await readJson(statePaths().credentials, { version: 1, providers: {} });
  credentials.providers[origin] = { token, updatedAt: new Date().toISOString() };
  await writeJson(statePaths().credentials, credentials, 0o600);
}

export async function eraseCredential(provider) {
  const origin = canonicalOrigin(provider);
  if (await helper("erase", origin)) return;
  const credentials = await readJson(statePaths().credentials, { version: 1, providers: {} });
  delete credentials.providers[origin];
  await writeJson(statePaths().credentials, credentials, 0o600);
}

export async function generateDeviceIdentity(name = "device") {
  const id = `key_${randomUUID()}`;
  const directory = statePaths().keys;
  await mkdir(directory, { recursive: true, mode: 0o700 });
  const privatePath = join(directory, `${id}.pem`);
  const publicPath = join(directory, `${id}.pub.pem`);
  const { privateKey, publicKey } = generateKeyPairSync("ed25519", {
    privateKeyEncoding: { type: "pkcs8", format: "pem" },
    publicKeyEncoding: { type: "spki", format: "pem" }
  });
  await writeFile(privatePath, privateKey, { mode: 0o600 });
  await writeFile(publicPath, publicKey, { mode: 0o644 });
  await chmod(privatePath, 0o600);
  return { name, privateKeyPath: privatePath, publicKeyPath: publicPath, publicKey };
}

export async function removeDeviceIdentity(publicKeyPath) {
  if (!publicKeyPath) return;
  await rm(publicKeyPath, { force: true });
  await rm(publicKeyPath.replace(/\.pub\.pem$/, ".pem"), { force: true });
}

export { canonicalOrigin };
