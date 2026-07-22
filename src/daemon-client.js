import { spawn } from "node:child_process";
import { closeSync, mkdirSync, openSync, readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { rpc } from "./rpc.js";
import { statePaths } from "./state.js";

const cli = resolve(dirname(fileURLToPath(import.meta.url)), "../bin/resync.js");

function delay(ms) { return new Promise(resolveDelay => setTimeout(resolveDelay, ms)); }

export async function daemonRpc(request) {
  try { return await rpc(request); }
  catch (error) {
    if (!/daemon unavailable/i.test(error.message)) throw error;
  }
  const env = { ...process.env };
  delete env.NODE_TEST_CONTEXT;
  const paths = statePaths();
  mkdirSync(paths.root, { recursive: true, mode: 0o700 });
  const logPath = resolve(paths.root, "daemon.log");
  const log = openSync(logPath, "a", 0o600);
  const child = spawn(process.execPath, [cli, "daemon"], {
    detached: true,
    stdio: ["ignore", log, log],
    env
  });
  closeSync(log);
  child.unref();
  let lastError;
  for (let attempt = 0; attempt < 50; attempt += 1) {
    await delay(100);
    try { return await rpc(request); }
    catch (error) { lastError = error; }
  }
  const detail = readFileSync(logPath, "utf8").trim().split("\n").slice(-5).join("\n");
  throw new Error(`${lastError?.message || "RepoSync daemon did not become ready"}${detail ? `; daemon log: ${detail}` : ""}`);
}
