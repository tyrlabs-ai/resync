---
name: reposync
description: Set up and work in RepoSync-managed Git projects, including installing the CLI, authenticating with a provider, initializing, joining, or forking project identity, enrolling SSH peers, verifying synchronization, and following the required commit cadence. Use when starting or diagnosing RepoSync, when a repository contains .resync.toml, or when moving work between machines or agents.
---

# RepoSync

Use RepoSync to propagate committed Git history between cooperating checkouts while preserving a stable hosted project identity. `.resync.toml` is committed publication policy, not proof that a particular checkout is enrolled; use `resync doctor` to verify local identity and daemon state.

## Start a project

1. Confirm that `resync` and Git 2.39 or newer are available. Install a release with `npm install -g @tyrlabs-ai/resync` (Node.js is only the native-package launcher), or build a source checkout with `cargo install --path .`.
2. Work from the intended Git repository. Inspect `git status` before changing configuration, preserve existing work, and exclude credentials and runtime state from Git.
3. Run `resync auth status`. If no suitable provider is configured, enroll with `resync auth login [provider] --token-command '<credential command>'`. Prefer a credential helper or token command over putting credentials directly on the command line.
4. Choose the identity operation deliberately:
   - Run `resync init --name <name>` for a new synchronization universe, even when another repository has identical Git history.
   - Run `resync join <project-id>` only when this checkout should share an existing provider and project ID. Each joined checkout receives its own checkout ID and device key.
   - Run `resync fork --name <name>` when an enrolled checkout should retain its Git history but become an independent RepoSync project.
5. Let enrollment create `.resync.toml`, install the project-local RepoSync skill plus Codex and Git lifecycle hooks, and start or reconnect the per-user daemon. Commit `.resync.toml` so every checkout uses the same publication validations. Do not run `install-adapters` or `daemon` during normal setup; those are repair and diagnostic commands.
6. Review and trust newly installed project hooks, then verify setup with `resync doctor` and `resync status`.

Use `resync <ssh-target>` to enroll another SSH machine into the same project through a one-time ticket. Use `host:/path` or `--into <path>` when the remote checkout location must be explicit.

Use `resync -h` and the `-h` or `--help` option at any command level to inspect the installed CLI rather than guessing an option.

## Reconciliation mode

If RepoSync reports reconciliation mode, the workspace remains available but
publication is paused. Reconcile the current branch with the latest RepoSync
remote-tracking branch, then run `resync project sync`. Do not repeatedly retry
automatic reconciliation after ordinary file edits; the explicit sync command
verifies the result and leaves reconciliation mode only on success.

## RepoSync commit cadence

RepoSync propagates committed Git history. Commit after completing and testing
each coherent logical change, before starting the next independent change. Keep
unrelated changes in separate commits, preserve pre-existing user work, and do
not present an untested checkpoint as a completed logical change.
