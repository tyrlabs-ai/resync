#!/usr/bin/env node
import { spawnSync } from "node:child_process";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";

const require = createRequire(import.meta.url);
const supported = new Map([
  ["darwin-arm64", "@tyrlabs-ai/resync-darwin-arm64"],
  ["darwin-x64", "@tyrlabs-ai/resync-darwin-x64"],
  ["linux-arm64", "@tyrlabs-ai/resync-linux-arm64"],
  ["linux-x64", "@tyrlabs-ai/resync-linux-x64"]
]);
const platform = `${process.platform}-${process.arch}`;
const packageName = supported.get(platform);
if (!packageName) {
  console.error(`resync: unsupported platform ${platform}`);
  process.exit(1);
}

let executable;
try {
  executable = join(dirname(require.resolve(`${packageName}/package.json`)), "bin", "resync");
} catch {
  console.error(`resync: native package ${packageName} is missing; reinstall @tyrlabs-ai/resync`);
  process.exit(1);
}
const result = spawnSync(executable, process.argv.slice(2), { stdio: "inherit" });
if (result.error) {
  console.error(`resync: ${result.error.message}`);
  process.exit(1);
}
process.exit(result.status ?? 1);
