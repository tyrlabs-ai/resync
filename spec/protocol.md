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
  "auth_methods": ["bootstrap-token", "join-ticket"],
  "endpoints": {
    "enroll": "/v1/auth/enroll",
    "catalog": "/v1/catalog",
    "projects": "/v1/projects",
    "join_tickets": "/v1/projects/{project_id}/join-tickets",
    "redeem_join_ticket": "/v1/join-tickets/redeem"
  }
}
```

The CLI resolves relative endpoints against the canonical service origin. Provider-specific browser or OAuth methods may be added to `auth_methods`; an open-source client MUST NOT require provider-specific code when the standard methods are available.

### `POST /v1/auth/enroll`

Authenticated with an operator bootstrap token or an existing device credential allowed to enroll another device. The request supplies a device name and an Ed25519 public key. The response returns a distinct `device_id`, its own bearer credential, and authorized project IDs. The new machine's private key is never transferred.

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

The catalog is compact discovery metadata. `advertised_heads` contains every branch below `refs/heads/*` and is empty for an empty repository. `default_branch` selects an initial checkout only and MUST NOT limit synchronization. Clients MUST verify advertised refs through Git.

### `POST /v1/projects`

Creates a new synchronization universe for the authenticated account and automatically authorizes the calling device. An existing Git history may seed multiple project IDs intentionally.

### `POST /v1/projects/{project_id}/join-tickets`

Creates a random, single-use, short-lived capability scoped to exactly one service, account, and project. The calling device must already belong to the project and have enrollment permission.

### `POST /v1/join-tickets/redeem`

The new machine submits the ticket, its newly generated public key, and a device name. The service atomically consumes the ticket, enrolls the device only into the ticket's project, and returns that device's own credential. Reuse, expiration, project substitution, and service-origin substitution are errors.

### Git smart HTTP

Repositories are exposed below `/git/<project-id>.git`. The reference host accepts HTTP Basic username `resync` and a device token as the password; an admin bootstrap token is also accepted by the single-tenant reference deployment. Read and write access is checked per project. Every branch below `refs/heads/*` may be updated; each is independently fast-forward-only and non-deletable. Other ref namespaces are rejected.

### Administrative endpoints

`POST /v1/admin/projects` creates a project and bare repository. `POST /v1/admin/devices` creates a scoped device token. `GET /v1/admin/status` reports accounts/projects/devices and storage metadata without hashes. These use `Authorization: Bearer <admin token>` and are hosting-provider extensions, not required client protocol.

## Local daemon IPC

The daemon listens on a mode-`0600` Unix socket and exchanges newline-delimited JSON.

```json
{"method":"acquireAccess","project_id":"prj_example","session_id":"s1","call_id":"c1","observed_generation":3,"access_hint":"unknown"}
```

A grant contains `lease_id`, `workspace_generation`, `branch_oid`, `reconciliation`, an optional path summary, and an optional model-visible message. A blocked response contains `state`, `model_message`, and `recovery_actions`.

Release uses `releaseAccess` with the lease ID and outcome. Other V1 methods are `status`, `sync`, `publish`, and `ping`. A successful `ping` reports the daemon PID and a SHA-256 `binaryId` for the running executable. A client whose own executable fingerprint differs MUST replace the stale per-user daemon before sending an operation that depends on current reconciliation behavior. IPC access is local authorization; repository canonicalization and subscription checks scope every request.

## Credential storage contract

Non-secret provider configuration belongs under the user's configuration directory. Device private keys and bearer credentials belong in an interchangeable credential store keyed by canonical service origin and device ID.

Implementations SHOULD prefer an operating-system keychain or configured external credential helper. A headless fallback MAY use files readable only by the operating-system user, but MUST describe that boundary honestly and MUST NOT claim that a key stored beside encrypted data adds protection. Environment credentials MAY be used without persistence in CI.

Private device keys and long-lived account/device credentials MUST NOT be copied during peer synchronization. Only a one-time join ticket crosses the existing SSH channel. When the receiving machine already has a device credential for the same provider account, it SHOULD redeem the ticket as that authenticated device so the provider adds the project authorization without replacing the credential or losing prior projects. Future project encryption keys are separate data-plane material and may be wrapped to the device public key only after enrollment authorization succeeds.

## SSH peer bootstrap

Running `resync <ssh-target>` inside an enrolled checkout means “enroll the peer in this project and converge it with the same hosted Git repository.” It is not folder mirroring.

The initiating client obtains a join ticket, starts or bootstraps the open-source remote helper over SSH, makes that helper the stable user-local CLI, and asks it to redeem the ticket. Linux bootstrap binaries MUST be statically linked so the initiating machine's libc cannot impose a newer runtime requirement on the peer. A target without an explicit path materializes into a deterministic user-local checkout directory keyed by project ID. Existing target paths are accepted only when their empty identity or exact `(service origin, project_id)` is compatible. An existing directory without Git metadata MAY be adopted only when its tracked files exactly match a commit reachable from the hosted project; untracked files are preserved and an unknown or modified snapshot fails closed. Different project IDs fail closed.
