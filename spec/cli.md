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

lease
  acquire <resource-key> [--project PROJECT_ID] [--holder HOLDER_ID] [--ttl DURATION]
  get <resource-key> [--project PROJECT_ID]
  list [--project PROJECT_ID] [--prefix PREFIX]
  renew <lease-id> --holder HOLDER_ID [--ttl DURATION]
  release <lease-id> --holder HOLDER_ID

peer
  sync <ssh-target>
  accept                       # internal remote helper

daemon
doctor
upgrade                              # converge launcher, daemon, adapters, and caches to one build
install-adapters [PROJECT_ID]        # repair all local projects, or one selected project
workspace
  install-hooks [WORKSPACE_ROOT]     # install one explicit multi-project Codex dispatcher
```

`lease` operates provider-hosted, project-scoped named leases. It does not call
the local daemon and must not be confused with the top-level engineering-preview
compatibility commands `acquire` and `release`, which continue to bracket local
workspace access.

Named-lease acquisition is immediate and has no wait loop. When `--project` is
omitted, commands use the enrolled repository containing the current directory,
including its service origin and project ID. An explicit project ID may be used
outside a checkout only when it resolves through the local catalog. The client
must discover `project-named-leases-v1`, the named-leases endpoint, and a valid
TTL policy before creating local receipt state or sending a request. Unsupported
and unreachable providers are errors; RepoSync does not create a local fallback
claim.

`--ttl` uses the normal duration grammar, must resolve to whole seconds without
overflow, and must be within the advertised provider policy. Omitting it uses
the provider default. Resource keys and prefixes follow the portable grammar in
the protocol specification and are validated before transport.

The client generates a high-entropy holder secret and stores it only in a
mode-`0600` crash-safe receipt below RepoSync's per-user state directory. Before
sending acquire it records the pending request ID and proof, and an ambiguous
retry reuses them. Once `ACQUIRED` is validated, it records the active receipt
before deleting pending state or printing success. Receipt paths are derived
from hashes of provider/project/lease/holder identities, never directly from a
resource key or holder string.

When `--holder` is omitted, acquire generates an opaque holder ID and returns
that public ID with the mutation response. `renew` and `release` require the
matching holder ID so they can select the exact local receipt; neither command
accepts the holder secret in argv. Mutation request IDs persist across ambiguous
retries. Confirmed release or an authoritative provider loss may remove or
tombstone the receipt; a local clock deadline or transport error may not.

All successful output is pretty JSON on stdout. Expected provider outcomes are
also preserved as JSON on stdout while diagnostics go to stderr. `HELD` exits
with status `3`, allowing a scheduler to distinguish ordinary contention from
transport, configuration, or other failures without scraping prose. Other
errors exit nonzero. Examples:

```sh
resync lease acquire waykeeper/maps/01h-map/tickets/01h-ticket --ttl 15m
resync lease get waykeeper/maps/01h-map/tickets/01h-ticket
resync lease list --project prj_example --prefix waykeeper/
resync lease renew nls_example --holder worker-example --ttl 15m
resync lease release nls_example --holder worker-example
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

`upgrade` stages the current verified executable in a build-identity directory,
atomically repoints the stable launcher, restarts and verifies the daemon by
executable fingerprint, repairs every catalog adapter, removes obsolete
package-manager installations, and prunes old managed builds only after no
process references them. Repeating the command MUST make no further changes.

Examples:

```sh
resync auth login https://provider.example --token-command 'pass show resync/bootstrap'
resync project init --name api
resync project join prj_example
resync project fork --name isolated-experiment
resync workspace install-hooks /work/reposync
resync yolopods-t3-peer
resync peer sync builder:/work/api
```

`project join` discovers any project owned by the active device's provider
account. It is the normal way to materialize an account project on another
already-registered device and requires neither an invitation nor SSH. `peer
sync` is reserved for enrolling a target that has no device identity for the
account.

`workspace install-hooks` reads `.resync-workspace.toml` from the workspace
root. Each `[[project]]` entry names a relative child path and MAY assert its
RepoSync project ID. Paths outside the workspace, duplicate projects,
non-subscribed checkouts, and mismatched IDs are rejected before the parent
Codex hook file is changed. Project and workspace hooks invoke stable,
machine-local dispatchers whose executable targets are atomically updated
during RepoSync upgrades. A repair MUST leave an already-current hook definition
byte-for-byte unchanged. Generated hooks are local adapter state and MUST NOT be
committed merely to propagate machine-specific paths.

A peer bootstrap environment MAY export `RESYNC_PEER_PROJECT_PATH` to select
its active checkout when the caller does not pass an explicit path. The
Yolopods compatibility variable `YOLOPODS_PROJECT_PATH` has the same behavior.
