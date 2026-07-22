---
name: reposync
description: Set up and operate Git projects with RepoSync, including installing the CLI, authenticating with a provider, choosing whether to initialize, join, or fork a project identity, running the daemon and agent adapters, enrolling SSH peers, and verifying synchronization. Use when starting a new coding project with RepoSync, adding RepoSync to an existing Git checkout, moving work between machines or agents, or diagnosing RepoSync project and daemon state.
---

# RepoSync

Use RepoSync to propagate committed Git history between cooperating checkouts while preserving a stable hosted project identity.

## Start a project

1. Confirm that `resync`, Node.js 22 or newer, and Git 2.39 or newer are available. If working from a RepoSync source checkout and the CLI is missing, install it with `npm install -g .` from that checkout.
2. Work from the intended Git repository. Inspect `git status` before changing configuration, preserve existing work, and exclude credentials and runtime state from Git.
3. Run `resync auth status`. If no suitable provider is configured, enroll with `resync auth login [provider] --token-command '<credential command>'`. Prefer a credential helper or token command over putting secrets directly on the command line.
4. Choose the identity operation deliberately:
   - Run `resync project init --name <name>` for a new synchronization universe, even when another repository has identical Git history.
   - Run `resync project join <project-id>` only when this checkout should share an existing provider and project ID. Each joined checkout receives its own checkout ID and device key.
   - Run `resync project fork --name <name>` when an enrolled checkout should retain its Git history but become an independent RepoSync project.
5. Record the project ID shown by `resync project status`, then run `resync install-adapters <project-id>` for participating Codex and Git lifecycle hooks. Review and trust newly installed project hooks before using them.
6. Ensure one current per-user daemon is running with `resync daemon`. It runs in the foreground; use the machine's process supervisor or a dedicated terminal. After updating the RepoSync CLI, restart the daemon so it uses the updated code.
7. Verify the setup with `resync doctor` and `resync project status`.

Use `resync <ssh-target>` to enroll another SSH machine into the same project through a one-time ticket. Use `host:/path` or `--into <path>` when the remote checkout location must be explicit.

Use `resync -h` and the `-h` or `--help` option at any command level to inspect the installed CLI rather than guessing an option.

## RepoSync commit cadence

RepoSync propagates committed Git history. Commit after completing and testing
each coherent logical change, before starting the next independent change.
