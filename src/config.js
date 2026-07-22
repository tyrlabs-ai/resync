import { readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";

export const DEFAULT_PROJECT_CONFIG = `version = 1

[sync]
branch = "main"
poll_interval = "10s"
preserve_index = true
conflict_behavior = "pause-writes"

[publish]
automatic = true
max_retries = 5
`;

export async function ensureProjectConfig(projectPath, branch = "main") {
  const path = join(projectPath, ".resync.toml");
  try { await readFile(path); return path; }
  catch (error) { if (error.code !== "ENOENT") throw error; }
  await writeFile(path, DEFAULT_PROJECT_CONFIG.replace('branch = "main"', `branch = ${JSON.stringify(branch)}`));
  return path;
}

export async function readProjectConfig(projectPath) {
  let source;
  try { source = await readFile(join(projectPath, ".resync.toml"), "utf8"); }
  catch (error) { if (error.code === "ENOENT") return { branch: "main", validations: [] }; throw error; }
  const branch = source.match(/^branch\s*=\s*"([^"]+)"/m)?.[1] || "main";
  const commands = [...source.matchAll(/^command\s*=\s*\[(.+)\]\s*$/gm)].map(match =>
    [...match[1].matchAll(/"((?:[^"\\]|\\.)*)"/g)].map(item => JSON.parse(`"${item[1]}"`))
  );
  return { branch, validations: commands };
}
