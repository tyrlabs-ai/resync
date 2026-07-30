# RepoSync (`resync`)

RepoSync is an open protocol and native Rust daemon for macOS and Linux that keeps all Git branches synchronized across coding agents and machines. It continuously fetches every branch, then rebases committed, staged, and unstaged local work on the currently checked-out branch at cooperative tool-call boundaries. It never changes a managed worktree in the middle of a participating tool call.

This repository is the public half of the system. It contains the normative V1 specification, protocol types, CLI, daemon, Git transaction engine, and conformance tests. The hosted service is optional: any provider implementing the protocol can supply catalog coordination and a protected Git remote.

## Status

This is a working V1 engineering preview. The transaction engine preserves ordinary tracked files, executable modes, symlinks, staged versus unstaged state, and non-colliding untracked files. It intentionally rejects force-pushed history and untracked-path collisions. Submodules, Git LFS, sparse checkout, merge-heavy unpublished history, intent-to-add entries, arbitrary writers, and FUSE are outside the current guarantee.

## Quick start

Requires Git 2.39+. Install a prebuilt native binary through npm (Node.js 18+ is used only by this installer/launcher):

```sh
npm install -g @tyrlabs-ai/resync
resync auth login https://your-resync-host.example --token-command 'pass show resync/bootstrap'
cd /absolute/path/to/checkout
resync init --name my-project
```

Initialization installs the project-local RepoSync skill and Git/Codex adapters,
then starts the per-user daemon automatically. The same is true for `join`,
`fork`, and SSH peer enrollment. `install-adapters` and `daemon` are repair and
diagnostic commands, not normal setup steps.

Every command level supports both help spellings and lists its direct commands and options:

```sh
resync -h
resync auth --help
resync project init -h
```

An ordinary Git clone has no RepoSync identity. Run `resync project init` to give that clone an independent hosted project, `resync project join PROJECT_ID` to join any existing project owned by the registered device's account, or `resync project fork` to separate an enrolled checkout while preserving its Git history.

## Repository policy

Enrollment creates `.resync.toml` as shared repository policy. Commit this file
so every enrolled checkout validates publications consistently. It does not
store provider credentials, checkout identity, or machine-local paths.

Each `[[validation]]` entry defines one command as an argument array:

```toml
version = 1

[[validation]]
command = ["cargo", "fmt", "--check"]

[[validation]]
command = ["cargo", "test", "--locked"]
```

Immediately before pushing a commit, RepoSync checks out that exact commit in a
temporary detached worktree and runs the commands in declaration order. A
failed command blocks that publication attempt and leaves it queued for retry.
An absent validation list permits publication without running a command.

To enroll another SSH machine in the current project:

```sh
resync yolopods-t3-peer
# equivalent to: resync peer sync yolopods-t3-peer
```

For an unregistered target, the initiating machine creates a five-minute, one-time join ticket and bootstraps this open-source package over SSH when necessary. The bootstrapped binary becomes the peer's stable user-local `resync` command, and its executable fingerprint replaces any stale per-user daemon before enrollment continues. The peer generates its own key and credential, joins the same `(service, project_id)`, and receives a distinct checkout ID. Use `host:/path` or `--into /path` to select a remote destination; otherwise RepoSync uses its user-local checkout directory. A target already registered to the same provider account uses `resync project join PROJECT_ID` locally and needs neither SSH nor a ticket.

Managed SSH environments can export `RESYNC_PEER_PROJECT_PATH` to make a bare
`resync <ssh-target>` enroll their active project checkout. RepoSync also
recognizes the equivalent `YOLOPODS_PROJECT_PATH` value exported by Yolopods.
If that path is a source snapshot without Git metadata, RepoSync adopts it only
when all tracked files exactly match a commit in the hosted history. Untracked
files are preserved; a modified or unknown snapshot is refused without leaving
partial Git metadata behind.

In another terminal (or an agent adapter), bracket every participating tool call:

```sh
lease_json="$(resync acquire PROJECT_ID SESSION_ID CALL_ID)"
# Run exactly one agent tool call.
resync release LEASE_ID
```

Ordinary `git commit` is the publication trigger; no RepoSync command is needed afterward. Use `resync sync PROJECT_ID` for an explicit compatibility-mode barrier and `resync publish PROJECT_ID` only to retry or flush publication manually. The daemon's Unix socket is mode `0600`; its state defaults to `~/.local/state/resync` and can be redirected with `RESYNC_STATE_DIR`.

Propagation is symmetric: a commit made in any enrolled checkout is published
to its branch and fetched by every running machine daemon. An idle checkout is
reconciled immediately in the background. If a participating tool call is
active, installation waits for its lease to end and completes before another
call is admitted.

Enrollment automatically installs the bundled RepoSync skill at `.agents/skills/reposync`, merges project-local `PreToolUse` and `PostToolUse` entries into `.codex/hooks.json`, and installs a chainable, signal-only Git `post-commit` hook. `install-adapters` remains available as a repair command. A hook starts the daemon if necessary, so a commit notification is not lost merely because no daemon process was already running. Codex project hooks run only for trusted projects and new or changed hooks require review; open `/hooks` in Codex and inspect the definitions before trusting them. The pre-tool hook denies a local tool call if the access barrier cannot be reached, while the post-tool hook releases the exact `tool_use_id` lease. Hosted tool paths that Codex does not expose to local hooks do not touch the worktree and are outside this lease boundary.

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
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
cargo build --release
```

The runtime, protocol implementation, daemon, and tests are Rust 1.88+. The npm package contains only a small JavaScript launcher that selects the matching native package for Linux x64/ARM64 or macOS Intel/Apple Silicon. Linux release binaries are statically linked with musl so they do not inherit the build runner's glibc requirement. Tagged releases build all four binaries on native GitHub-hosted runners; macOS artifacts are intentionally unsigned and not notarized.

Apache-2.0 licensed. The protocol is designed to permit independent clients and hosting implementations.
