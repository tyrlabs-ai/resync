import { spawn } from "node:child_process";

export function run(program, args, options = {}) {
  return new Promise((resolve, reject) => {
    const child = spawn(program, args, {
      cwd: options.cwd,
      env: { ...process.env, ...options.env },
      stdio: [options.input == null ? "ignore" : "pipe", "pipe", "pipe"]
    });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", chunk => { stdout += chunk; });
    child.stderr.on("data", chunk => { stderr += chunk; });
    if (options.input != null) child.stdin.end(options.input);
    child.on("error", reject);
    child.on("exit", code => {
      const result = { code, stdout, stderr };
      if (code === 0 || options.allowFailure) resolve(result);
      else {
        const detail = stderr.trim() || stdout.trim() || `exit ${code}`;
        const error = new Error(`${program} ${args.join(" ")} failed: ${detail}`);
        error.result = result;
        reject(error);
      }
    });
  });
}

export async function git(cwd, args, options = {}) {
  return run("git", args, { ...options, cwd });
}
