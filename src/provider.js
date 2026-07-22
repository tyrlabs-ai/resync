import { readFile } from "node:fs/promises";
import { hostname } from "node:os";
import { canonicalOrigin, generateDeviceIdentity, storeCredential } from "./credentials.js";
import { loadConfig, statePaths, writeJson } from "./state.js";

export const DEFAULT_PROVIDER = process.env.RESYNC_DEFAULT_PROVIDER || "https://resync-hosted-production.up.railway.app";

export async function discover(provider) {
  const origin = canonicalOrigin(provider || DEFAULT_PROVIDER);
  const response = await fetch(new URL("/.well-known/resync", origin), { headers: { accept: "application/json" } });
  if (!response.ok) throw new Error(`provider discovery failed (${response.status})`);
  const document = await response.json();
  if (document.protocol !== "resync.v1" || !document.endpoints) throw new Error("provider does not advertise a compatible resync.v1 service");
  const endpoints = Object.fromEntries(Object.entries(document.endpoints).map(([key, value]) => [key, new URL(value, origin).toString()]));
  return { ...document, origin, endpoints };
}

export async function providerFetch(url, token, options = {}) {
  const headers = { accept: "application/json", ...(options.body ? { "content-type": "application/json" } : {}), ...options.headers };
  if (token) headers.authorization = `Bearer ${token}`;
  const response = await fetch(url, { ...options, headers, body: options.body == null || typeof options.body === "string" ? options.body : JSON.stringify(options.body) });
  const text = await response.text();
  let value = null;
  if (text) {
    try { value = JSON.parse(text); }
    catch { value = { error: text.slice(0, 500) }; }
  }
  if (!response.ok) throw new Error(value?.error || `provider request failed (${response.status})`);
  return value;
}

export async function enrollWithToken({ provider, bootstrapToken, deviceName = hostname() }) {
  const discovery = await discover(provider);
  const key = await generateDeviceIdentity(deviceName);
  let enrollment;
  try {
    enrollment = await providerFetch(discovery.endpoints.enroll, bootstrapToken, {
      method: "POST", body: { device_name: deviceName, public_key: key.publicKey }
    });
  } catch (error) {
    // Compatibility with preview hosts that already issued a device token.
    const catalog = await providerFetch(discovery.endpoints.catalog, bootstrapToken);
    enrollment = { device: { id: null, name: deviceName, project_ids: catalog.projects.map(item => item.project_id) }, token: bootstrapToken };
  }
  await providerFetch(discovery.endpoints.catalog, enrollment.token);
  await storeProviderLogin({ discovery, enrollment, key });
  return { provider: discovery.origin, device: enrollment.device, authMethods: discovery.auth_methods };
}

export async function storeProviderLogin({ discovery, enrollment, key }) {
  await storeCredential(discovery.origin, enrollment.token);
  const config = await loadConfig();
  delete config.server;
  delete config.token;
  config.activeProvider = discovery.origin;
  config.providers ||= {};
  config.providers[discovery.origin] = {
    origin: discovery.origin,
    serviceId: discovery.service_id,
    deviceId: enrollment.device.id,
    deviceName: enrollment.device.name,
    publicKeyPath: key.publicKeyPath,
    loggedInAt: new Date().toISOString()
  };
  await writeJson(statePaths().config, config, 0o600);
}
