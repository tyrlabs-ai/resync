# RepoSync (`resync`)

RepoSync is an open protocol and Linux-first daemon that keeps all Git branches synchronized across coding agents and machines. It continuously fetches every branch, then rebases committed, staged, and unstaged local work on the currently checked-out branch at cooperative tool-call boundaries. It never changes a managed worktree in the middle of a participating tool call.

This repository is the public half of the system. It contains the normative V1 specification, protocol types, CLI, daemon, Git transaction engine, and conformance tests. The hosted service is optional: any provider implementing the protocol can supply catalog coordination and a protected Git remote.

## Status

This is a working V1 engineering preview. The transaction engine preserves ordinary tracked files, executable modes, symlinks, staged versus unstaged state, and non-colliding untracked files. It intentionally rejects force-pushed history and untracked-path collisions. Submodules, Git LFS, sparse checkout, merge-heavy unpublished history, intent-to-add entries, arbitrary writers, FUSE, and secret synchronization are outside the current guarantee.

## Quick start

Requires Node.js 22+ and Git 2.39+.

```sh
npm install -g .
resync auth login https://your-resync-host.example --token-command 'pass show resync/bootstrap'
cd /absolute/path/to/checkout
resync project init --name my-project
```

Initialization installs the Git/Codex adapters and starts the per-user daemon
automatically. The same is true for `join`, `fork`, and SSH peer enrollment.

Every command level supports both help spellings and lists its direct commands and options:

```sh
resync -h
resync auth --help
resync project init -h
```

An ordinary Git clone has no RepoSync identity. Run `resync project init` to give that clone an independent hosted project, `resync project join PROJECT_ID` to join an authorized existing project, or `resync project fork` to separate an enrolled checkout while preserving its Git history.

To enroll another SSH machine in the current project:

```sh
resync yolopods-t3-peer
# equivalent to: resync peer sync yolopods-t3-peer
```

The initiating machine creates a five-minute, one-time join ticket and bootstraps this open-source package over SSH when necessary. The peer generates its own key and credential, joins the same `(service, project_id)`, and receives a distinct checkout ID. Use `host:/path` or `--into /path` to select a remote destination; otherwise RepoSync uses its user-local checkout directory.

In another terminal (or an agent adapter), bracket every participating tool call:

```sh
lease_json="$(resync acquire PROJECT_ID SESSION_ID CALL_ID)"
# Run exactly one agent tool call.
resync release LEASE_ID
```

Ordinary `git commit` is the publication trigger; no RepoSync command is needed afterward. Use `resync sync PROJECT_ID` for an explicit compatibility-mode barrier and `resync publish PROJECT_ID` only to retry or flush publication manually. The daemon's Unix socket is mode `0600`; its state defaults to `~/.local/state/resync` and can be redirected with `RESYNC_STATE_DIR`.

Enrollment automatically merges project-local `PreToolUse` and `PostToolUse` entries into `.codex/hooks.json` and installs a chainable, signal-only Git `post-commit` hook. `install-adapters` remains available as a repair command. A hook starts the daemon if necessary, so a commit notification is not lost merely because no daemon process was already running. Codex project hooks run only for trusted projects and new or changed hooks require review; open `/hooks` in Codex and inspect the definitions before trusting them. The pre-tool hook denies a local tool call if the access barrier cannot be reached, while the post-tool hook releases the exact `tool_use_id` lease. Hosted tool paths that Codex does not expose to local hooks do not touch the worktree and are outside this lease boundary.

## Correctness boundary

- Fetching never changes the visible worktree.
- A participating call holds a shared activity lease. Reconciliation holds an exclusive lease.
- Dirty tracked state is represented as two private synthetic commits, rebased in an isolated worktree, and installed only after the original workspace snapshot is reverified.
- A failed rebase leaves the active branch, index, and working tree unchanged and retains recovery refs under `refs/resync/recovery/`.
- Publication uses normal, non-forced pushes. A rejected push triggers fetch/rebase/revalidate/retry.
- Every `refs/heads/*` branch is transported and protected independently. Fetching an inactive branch updates its remote-tracking ref without changing the worktree; reconciliation occurs when that branch is checked out.
- Detached processes, editors, and tools that bypass leases are unmanaged. Release builds belong in an immutable checkout or CI.

See [the normative specification](./spec/v1.md), [wire protocol](./spec/protocol.md), and [security model](./SECURITY.md).

## Agent instruction

Project owners can add this block to a managed repository's `AGENTS.md`:

```markdown
## RepoSync commit cadence

RepoSync propagates committed Git history. Commit after completing and testing
each coherent logical change, before starting the next independent change.
```

## Development

```sh
npm test
npm run check
```

Apache-2.0 licensed. The protocol is designed to permit independent clients and hosting implementations.
