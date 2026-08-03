# RepoSync V1 protocol

The media/protocol identifier is `resync.v1`. JSON field names on the hosted API use `snake_case`; local implementation fields are not wire contracts.

## Hosted API

All endpoints use TLS in production and return JSON except Git smart-HTTP paths. Device calls use `Authorization: Bearer <device token>`.

### `GET /.well-known/resync`

Unauthenticated provider discovery document:

```json
{
  "protocol": "resync.v1",
  "service_id": "svc_example",
  "capabilities": [
    "account-wide-project-access-v1",
    "membership-extension-v1",
    "durable-publication-v1",
    "active-branch-heartbeat-v1",
    "project-named-leases-v1"
  ],
  "auth_methods": ["bootstrap-token", "join-ticket"],
  "endpoints": {
    "enroll": "/v1/auth/enroll",
    "catalog": "/v1/catalog",
    "projects": "/v1/projects",
    "join_tickets": "/v1/projects/{project_id}/join-tickets",
    "redeem_join_ticket": "/v1/join-tickets/redeem",
    "heartbeat": "/v1/heartbeat",
    "publications": "/v1/projects/{project_id}/publications",
    "named_leases": "/v1/projects/{project_id}/named-leases"
  },
  "named_lease_policy": {
    "default_ttl_seconds": 900,
    "min_ttl_seconds": 30,
    "max_ttl_seconds": 3600
  }
}
```

The CLI resolves relative endpoints against the canonical service origin. Provider-specific browser or OAuth methods may be added to `auth_methods`; an open-source client MUST NOT require provider-specific code when the standard methods are available.

### `POST /v1/auth/enroll`

Authenticated with an operator bootstrap token or an existing device credential allowed to enroll another device. The request supplies a device name and an Ed25519 public key. The response returns a distinct `device_id`, its own bearer credential, and every project ID owned by the account. The new machine's private key is never transferred.

`account-wide-project-access-v1` defines an account invariant: every active device registered to an account MUST be able to discover and use every project owned by that account. Creating a project MUST make it available to all existing account devices, registering a device MUST make all existing account projects available to it, and providers MUST repair legacy narrower membership state. Device revocation still removes all access.

### `GET /healthz`

Unauthenticated liveness/readiness response. It MUST NOT reveal tokens, source, or account details.

### `GET /v1/catalog`

```json
{
  "protocol": "resync.v1",
  "catalog_generation": 12,
  "projects": [{
    "project_id": "prj_example",
    "name": "example",
    "remote_url": "https://host/git/prj_example.git",
    "default_branch": "main",
    "advertised_heads": [
      {"name": "feature/api", "oid": "1111111111111111111111111111111111111111"},
      {"name": "main", "oid": "0123456789abcdef0123456789abcdef01234567"}
    ],
    "project_generation": 4
  }]
}
```

The catalog is compact discovery metadata and contains every project owned by the authenticated device's account. `advertised_heads` contains every branch below `refs/heads/*` and is empty for an empty repository. `default_branch` selects an initial checkout only and MUST NOT limit synchronization. Clients MUST verify advertised refs through Git.

### `POST /v1/projects`

Creates a new synchronization universe for the authenticated account and
automatically makes it available to every device registered to that account. An existing Git history may seed
multiple project IDs intentionally. The request SHOULD contain a persisted
`idempotency_key`; repeating the same key for the same account returns the
original project instead of creating a duplicate after a partial client crash.

### `POST /v1/projects/{project_id}/join-tickets`

Creates a random, single-use, short-lived capability for enrolling a machine that has no device credential for the account. The capability is bound to one service, account, and initiating project. The calling device must belong to the account and have enrollment permission. A machine already registered to the account does not need a ticket and joins any account project directly from its catalog.

### `POST /v1/join-tickets/redeem`

The unregistered machine submits the ticket, its newly generated public key,
and a device name. The service atomically consumes the ticket, registers the
device to the ticket's account, grants the account-wide project set, and returns
that device's own credential. Existing-device redemption MAY remain as a
compatibility operation, but it does not extend access because registered
devices already have every project in the account. Reuse, expiration, account
substitution, project substitution, and service-origin substitution are errors.

### `POST /v1/heartbeat`

The client submits one record per managed checkout containing `project_id`,
`checkout_id`, and `active_branch`. The response contains only membership, the
authoritative head for that exact active branch, and project generation. A
missing membership is returned as `ACCESS_LOST`. The payload and server work
MUST NOT grow with unrelated branch count.

### `POST /v1/projects/{project_id}/publications`

After object transfer and the protected ref update, the client submits an
immutable `(publication_id, attempt)` payload with `branch`, `candidate_oid`,
and `expected_remote_oid`. If the candidate is durably reachable from the
hosted branch, the provider persists and returns a replayable ACK:

```json
{
  "publication_id": "pub_example",
  "attempt": 1,
  "project_id": "prj_example",
  "branch": "main",
  "accepted_oid": "0123456789abcdef0123456789abcdef01234567",
  "remote_head": "0123456789abcdef0123456789abcdef01234567",
  "project_generation": 42,
  "durable": true
}
```

Duplicate immutable attempts return the stored ACK. Reusing an attempt ID with
different content is a protocol error. An incompatible hosted advance returns
HTTP `409` with `outcome: REMOTE_ADVANCED`; it is not an ACK. If the ref update
committed but ACK persistence did not, a retry reconstructs the ACK from branch
reachability.

### Project-scoped named leases

`project-named-leases-v1` is optional hosted coordination. A `NamedLease` is a
provider-persisted, time-limited, exclusive reservation of an opaque resource
key within one RepoSync project. It is not a Git lock and is wholly separate
from the machine-local `WorkspaceAccessLease` used by daemon IPC. Named leases
MUST NOT participate in workspace reconciliation, Git refs, or daemon state.

A provider advertises this feature only when discovery contains all three of:

- the `project-named-leases-v1` capability;
- an `endpoints.named_leases` template containing `{project_id}`; and
- a valid `named_lease_policy`, where
  `0 < min_ttl_seconds <= default_ttl_seconds <= max_ttl_seconds`.

Clients MUST NOT synthesize an endpoint for a provider that omits any of these
fields. An omitted request TTL selects the advertised default. Providers reject
out-of-range values rather than clamping them. Provider time is authoritative:
a lease is live only while `provider_now < expires_at`.

After substituting and URL-encoding the project ID, the base endpoint supports:

```text
POST <base>                              acquire
GET  <base>?resource_key=<key>           inspect one live lease
GET  <base>?prefix=<segment-prefix>      list a namespace's live leases
POST <base>/<lease_id>/renew             renew one exact acquisition
POST <base>/<lease_id>/release           release one exact acquisition
```

`resource_key` and `prefix` are mutually exclusive. A portable V1 client sends
one of them; providers may reject or paginate unfiltered listing. Prefixes are
complete valid segments ending in `/`, so `waykeeper/` never matches
`waykeeper2/...`. Percent escaping is transport encoding and is decoded before
validation.

Every mutation carries a client-generated `request_id`. Repeating the same
request ID with the same canonical payload for one authenticated device returns
the first status and body. Reusing it with different content returns HTTP `409`
and `REQUEST_ID_REUSED` without executing the second operation. Providers MUST
retain replay state through the maximum lease lifetime plus their documented
retry window.

Acquire request:

```json
{
  "request_id": "req_example",
  "resource_key": "waykeeper/maps/map-id/tickets/ticket-id",
  "holder_id": "worker-example",
  "holder_secret": "base64url-random-256-bit-value",
  "ttl_seconds": 900
}
```

The client creates the high-entropy `holder_secret`. The provider stores only a
cryptographic hash and never returns the secret or its hash. HTTP `201` returns
`ACQUIRED`; a replay of that request returns the original successful result:

```json
{
  "outcome": "ACQUIRED",
  "lease": {
    "lease_id": "nls_example",
    "project_id": "prj_example",
    "resource_key": "waykeeper/maps/map-id/tickets/ticket-id",
    "holder_device_id": "dev_example",
    "holder_id": "worker-example",
    "generation": 7,
    "acquired_at": "2026-08-03T12:00:00Z",
    "expires_at": "2026-08-03T12:15:00Z"
  }
}
```

At most one live lease exists for `(account_id, project_id, resource_key)`. A
competing acquire returns HTTP `409`, outcome `HELD`, and that live lease's
public fields. Each acquisition after release or expiry receives a new lease ID
and a generation greater than every prior generation for the same resource.
Generation is a fencing value for consumers able to enforce it; RepoSync does
not claim to fence arbitrary external side effects.

Renew and release bodies contain `request_id` and `holder_secret`; renew may
also contain `ttl_seconds`. Both authenticate as the original active device and
prove possession of the secret. Renewal sets `expires_at` from provider receipt
time rather than extending the old deadline. Operations address a lease ID, so
a late request can never modify a successor on the same resource. Handoff is a
release followed by a new acquisition and generation; leases are not
transferable.

Expected outcomes and statuses are stable:

| Outcome | HTTP | Meaning |
| --- | ---: | --- |
| `ACQUIRED` | 201 | A new lease was durably acquired. |
| `RENEWED` | 200 | The addressed lease was renewed. |
| `RELEASED` | 200 | The addressed lease was released. |
| `HELD` | 409 | Another live lease owns the resource. |
| `LEASE_EXPIRED` | 409 | Provider time passed the addressed lease's deadline. |
| `REQUEST_ID_REUSED` | 409 | The request ID was previously used with different content. |
| `NOT_HOLDER` | 403 | The device or holder proof does not own the lease. |
| `NOT_FOUND` | 404 | The addressed lease does not exist. |
| `INVALID_RESOURCE_KEY` | 400 | The decoded resource key or prefix is invalid. |
| `INVALID_TTL` | 400 | The requested TTL violates provider policy. |

The HTTP status accompanies rather than replaces the JSON outcome. Clients
MUST validate required IDs, timestamps, positive generation, and the requested
project/resource or lease identity before treating a response as authority.

All calls use the existing device bearer credential over TLS. The device's
account must own the project. Any active account device may inspect, list, or
attempt to acquire its projects; only the acquiring device plus holder secret
may renew or release. Revoking a device immediately releases its live leases.
Portable V1 has no administrative force-release operation.

Public lease projections may contain only resource key, lease ID, project ID,
generation, holder device ID/name, holder ID, and acquisition/expiry
timestamps. They never contain bearer credentials, holder secrets or hashes,
request IDs, account IDs, internal release reasons, or arbitrary metadata.
Resource keys and holder IDs are visible to every account device and therefore
must not contain secrets, personal data, prompts, or ticket contents.

V1 resource keys are compared byte-for-byte after validation and are never
normalized or treated as paths:

- the encoded value is 1–255 bytes;
- it is lowercase ASCII slash-separated segments;
- each segment is 1–63 bytes and matches `[a-z0-9][a-z0-9._-]*`;
- empty segments, leading/trailing `/`, `.` and `..`, whitespace, controls,
  uppercase, and decoded traversal-like values are invalid;
- the first segment is the consuming application's namespace; and
- keys use immutable IDs, not mutable names, slugs, paths, checkout IDs, or
  device names. The RepoSync project is already the outer namespace and is not
  repeated in the key.

Waykeeper ticket claims use
`<claims.namespace>/maps/<map-id>/tickets/<ticket-id>`. Provider outage or lack
of capability is reported distinctly; RepoSync does not create a repository or
local fallback claim.

### Git smart HTTP

Repositories are exposed below `/git/<project-id>.git`. The reference host accepts HTTP Basic username `resync` and a device token as the password; an admin bootstrap token is also accepted by the single-tenant reference deployment. Read and write access requires the device and project to belong to the same account. Every branch below `refs/heads/*` may be updated; each is independently fast-forward-only and non-deletable. Other ref namespaces are rejected.

### Administrative endpoints

`POST /v1/admin/projects` creates a project and bare repository. `POST /v1/admin/devices` creates an account-wide device token. `GET /v1/admin/status` reports accounts/projects/devices and storage metadata without hashes. These use `Authorization: Bearer <admin token>` and are hosting-provider extensions, not required client protocol.

## Local daemon IPC

The daemon listens on a mode-`0600` Unix socket and exchanges newline-delimited JSON.

```json
{"method":"acquireAccess","project_id":"prj_example","session_id":"s1","call_id":"c1","observed_generation":3,"access_hint":"unknown"}
```

A grant contains `lease_id`, `workspace_generation`, `branch_oid`, `reconciliation`, reconciliation-mode state, an optional path summary, and an optional model-visible message. Reconciliation, connectivity, authorization, and daemon failures MUST NOT turn the workspace-access adapter into a persistent denial surface; the adapter surfaces a warning and leaves the prior coherent workspace available.

Release uses `releaseAccess` with the lease ID and outcome. Other V1 methods are
`status`, `sync`, `publish`, `reportCommit`, `reportCheckout`, and `ping`. A
successful `ping` reports the daemon PID and a SHA-256 `binaryId` for the
running executable. A client whose own executable fingerprint differs MUST
replace the stale per-user daemon before sending an operation that depends on
current reconciliation behavior. IPC access is local authorization; repository
canonicalization and subscription checks scope every request.

The lease returned by `acquireAccess` is a local `WorkspaceAccessLease`. It
exists only to prevent reconciliation from mutating one checkout during an
observable tool call. It is not a hosted `NamedLease`, cannot claim a generic
resource, and is unaffected by `project-named-leases-v1`.

## Credential storage contract

Provider configuration belongs under the user's configuration directory.
Device private keys and bearer credentials belong in an
interchangeable credential store keyed by canonical service origin and device
ID. Joining a project already owned by the account uses that same
provider/device identity directly; it never rotates the active provider
credential as a side effect.

Implementations SHOULD prefer an operating-system keychain or configured external credential helper. A headless fallback MAY use files readable only by the operating-system user, but MUST describe that boundary honestly and MUST NOT claim that a key stored beside encrypted data adds protection. Environment credentials MAY be used without persistence in CI.

Private device keys and long-lived account/device credentials MUST NOT be copied during peer synchronization. A one-time join ticket crosses the existing SSH channel only when the receiver is not registered to the provider account. A receiver already registered to the account obtains the project from its catalog with `project join`; no ticket or SSH-mediated authorization is required.

## SSH peer bootstrap

Running `resync <ssh-target>` inside an enrolled checkout means “register an unmanaged peer to this account and converge it with the same hosted Git repository.” It is not folder mirroring. If the target is already registered, run `resync project join PROJECT_ID` on that target instead.

The initiating client obtains a join ticket, starts or bootstraps the open-source remote helper over SSH, makes that helper the stable user-local CLI, and asks it to redeem the ticket. Linux bootstrap binaries MUST be statically linked so the initiating machine's libc cannot impose a newer runtime requirement on the peer. A target without an explicit path materializes into a deterministic user-local checkout directory keyed by project ID. Existing target paths are accepted only when their empty identity or exact `(service origin, project_id)` is compatible. An existing directory without Git metadata MAY be adopted only when its tracked files exactly match a commit reachable from the hosted project; untracked files are preserved and an unknown or modified snapshot fails closed. Different project IDs fail closed.
