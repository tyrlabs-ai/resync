# RepoSync command-line contract

Every command level supports both `-h` and `--help`. Help at a level lists its direct subcommands, its own options, inherited global options, representative examples, and relevant safety behavior. Help exits successfully and never requires login or a running daemon.

The root grammar is:

```text
resync [global options] <command>
resync [global options] <ssh-target> [peer options]
```

Command groups:

```text
auth
  login [provider]
  status
  logout [provider]

project
  init
  join <project-id>
  fork
  status [project]
  sync [project]
  publish [project]
  conflicts [project]
  recover [project]

device
  list
  revoke <device-id>

peer
  sync <ssh-target>
  accept                       # internal remote helper

daemon
doctor
install-adapters <project>           # repair/reinstall; enrollment does this automatically
```

Compatibility aliases from the engineering preview MAY remain, but grouped commands are canonical. A non-command first positional value is treated as an SSH target only while inside an enrolled Git checkout; unknown command-like values outside that context produce help and an error.

`init`, `join`, `fork`, and peer enrollment install the project-local RepoSync
skill, `post-commit` and `post-checkout` lifecycle adapters, then ensure the
per-user daemon is running. `init` is idempotent for an already enrolled
checkout: it verifies membership, repairs adapters and daemon registration,
reconciles the active branch, and flushes pending work without changing
project, checkout, or device identity. An ordinary `git commit` durably queues
publication automatically. `project publish` is an explicit retry/flush
command and is not part of the normal workflow.

Examples:

```sh
resync auth login https://provider.example --token-command 'pass show resync/bootstrap'
resync project init --name api
resync project join prj_example
resync project fork --name isolated-experiment
resync yolopods-t3-peer
resync peer sync builder:/work/api
```

A peer bootstrap environment MAY export `RESYNC_PEER_PROJECT_PATH` to select
its active checkout when the caller does not pass an explicit path. The
Yolopods compatibility variable `YOLOPODS_PROJECT_PATH` has the same behavior.
