const globalOptions = [
  ["-h, --help", "Show help for the current command level"],
  ["--state-dir <path>", "Override the RepoSync state directory"],
  ["--version", "Print the CLI version"]
];

export const commandTree = {
  name: "resync",
  description: "Cooperative continuous Git synchronization across machines and agents.",
  usage: "resync [global options] <command>\n       resync [global options] <ssh-target>",
  options: globalOptions,
  examples: ["resync auth login https://provider.example --token-command 'pass show resync/bootstrap'", "resync project init --name api", "resync yolopods-t3-peer"],
  commands: {
    auth: {
      description: "Manage provider-neutral login credentials and device identity.",
      usage: "resync auth <command>",
      commands: {
        login: { description: "Enroll this machine with a provider.", usage: "resync auth login [provider] [options]", options: [["--token <token>", "Bootstrap or existing device token"], ["--token-command <command>", "Read the bootstrap token from a command's stdout"], ["--device-name <name>", "Name shown in the provider's device list"]], examples: ["resync auth login https://provider.example --token-command 'pass show resync/token'"] },
        status: { description: "List configured providers without revealing credentials.", usage: "resync auth status [provider]" },
        logout: { description: "Remove this machine's local provider credential and key.", usage: "resync auth logout [provider]", options: [["--all", "Log out of every configured provider"]] }
      }
    },
    project: {
      description: "Create, join, separate, synchronize, and inspect projects.", usage: "resync project <command>",
      commands: {
        init: { description: "Create a new synchronization universe for this Git repository.", usage: "resync project init [options]", options: [["--name <name>", "Hosted project name (defaults to directory name)"], ["--provider <url>", "Use a configured provider other than the active provider"]] },
        join: { description: "Join an authorized existing project with a distinct checkout ID.", usage: "resync project join <project-id> [options]", options: [["--path <path>", "Checkout path (defaults to the current Git worktree)"], ["--provider <url>", "Provider that owns the project"]] },
        fork: { description: "Create an independent project ID from the current Git repository.", usage: "resync project fork [options]", options: [["--name <name>", "Name for the independent hosted project"], ["--provider <url>", "Provider for the new project"]] },
        status: { description: "Show freshness, durability, project ID, and checkout ID.", usage: "resync project status [project]" },
        sync: { description: "Fetch and reconcile at an explicit barrier.", usage: "resync project sync [project]" },
        publish: { description: "Explicitly flush publication (commits publish automatically).", usage: "resync project publish [project]" },
        conflicts: { description: "List blocked reconciliation records.", usage: "resync project conflicts [project]" },
        recover: { description: "Recover interrupted reconciliation transactions.", usage: "resync project recover [project]" },
        leave: { description: "Stop managing a checkout without deleting its files.", usage: "resync project leave [project-id]" }
      }
    },
    device: {
      description: "Inspect and revoke independently enrolled machines.", usage: "resync device <command>",
      commands: {
        list: { description: "List devices visible to the active provider account.", usage: "resync device list [--provider <url>]", options: [["--provider <url>", "Provider to query"]] },
        revoke: { description: "Revoke a device without rotating other machines.", usage: "resync device revoke <device-id> [--provider <url>]", options: [["--provider <url>", "Provider to query"]] }
      }
    },
    peer: {
      description: "Enroll another machine in the same project over SSH.", usage: "resync peer <command>",
      commands: {
        sync: { description: "Create a one-time ticket and converge an SSH peer.", usage: "resync peer sync <ssh-target> [options]", options: [["--into <path>", "Explicit checkout path on the peer"], ["--name <name>", "Remote device name"], ["--no-bootstrap", "Require an already installed remote resync CLI"]] },
        accept: { description: "Internal remote-side ticket redemption helper.", usage: "resync peer accept [options]", options: [["--provider <url>", "Ticket issuer"], ["--ticket <ticket>", "Single-use enrollment capability"], ["--project <id>", "Expected project ID"], ["--into <path>", "Destination checkout path"], ["--device-name <name>", "Remote device name"]] }
      }
    },
    daemon: { description: "Run the per-user synchronization daemon (normally started automatically).", usage: "resync daemon", options: [["--poll-interval <duration>", "Catalog polling interval (default 10s)"]] },
    doctor: { description: "Check Git, Node, provider, daemon, identity, and credential state.", usage: "resync doctor" },
    "install-adapters": { description: "Repair or reinstall automatically managed lifecycle adapters.", usage: "resync install-adapters <project-id>" },
    acquire: { description: "Acquire one cooperative tool-call lease (adapter API).", usage: "resync acquire <project-id> [session-id] [call-id]" },
    release: { description: "Release one cooperative tool-call lease (adapter API).", usage: "resync release <lease-id>" }
  }
};

export function renderHelp(node = commandTree) {
  const rootLevel = node.name === "resync";
  const lines = [`${node.name ? `${node.name} — ` : ""}${node.description || ""}`.trim(), "", "Usage:", `  ${(node.usage || commandTree.usage).replaceAll("\n", "\n  ")}`];
  if (node.commands) {
    lines.push("", "Commands:");
    const width = Math.max(...Object.keys(node.commands).map(name => name.length));
    for (const [name, command] of Object.entries(node.commands)) lines.push(`  ${name.padEnd(width)}  ${command.description}`);
  }
  const options = [...(node.options || []), ...(rootLevel ? [] : [["-h, --help", "Show help for this command"]])];
  if (options.length) {
    lines.push("", "Options:");
    const width = Math.max(...options.map(([flag]) => flag.length));
    for (const [flag, description] of options) lines.push(`  ${flag.padEnd(width)}  ${description}`);
  }
  if (!rootLevel) {
    lines.push("", "Global options:");
    for (const [flag, description] of globalOptions) lines.push(`  ${flag.padEnd(22)}  ${description}`);
  }
  if (node.examples?.length) lines.push("", "Examples:", ...node.examples.map(example => `  ${example}`));
  return `${lines.join("\n")}\n`;
}

export function parseOptions(args, definitions = {}) {
  const options = {};
  const positional = [];
  for (let index = 0; index < args.length; index += 1) {
    const argument = args[index];
    if (argument === "--") { positional.push(...args.slice(index + 1)); break; }
    if (!argument.startsWith("-") || argument === "-") { positional.push(argument); continue; }
    const [name, inline] = argument.split("=", 2);
    if (name === "-h" || name === "--help") { options.help = true; continue; }
    const definition = definitions[name];
    if (!definition) throw new Error(`unknown option ${name}`);
    if (definition === "boolean") options[name.replace(/^--/, "").replaceAll("-", "_")] = true;
    else {
      const value = inline ?? args[++index];
      if (value == null || value.startsWith("--")) throw new Error(`${name} requires a value`);
      options[name.replace(/^--/, "").replaceAll("-", "_")] = value;
    }
  }
  return { options, positional };
}
