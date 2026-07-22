# RepoSync V1 protocol

The media/protocol identifier is `resync.v1`. JSON field names on the hosted API use `snake_case`; local implementation fields are not wire contracts.

## Hosted API

All endpoints use TLS in production and return JSON except Git smart-HTTP paths. Device calls use `Authorization: Bearer <device token>`.

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
    "sync_branch": "main",
    "advertised_head_oid": "0123456789abcdef0123456789abcdef01234567",
    "project_generation": 4
  }]
}
```

The catalog is compact discovery metadata. `advertised_head_oid` MAY be null for an empty repository. Clients fetch only when a project generation or head changes and MUST verify the ref through Git.

### Git smart HTTP

Repositories are exposed below `/git/<project-id>.git`. The reference host accepts HTTP Basic username `resync` and a device token as the password; an admin bootstrap token is also accepted by the single-tenant reference deployment. Read and write access is checked per project. Only the configured synchronization branch may be updated, and it is fast-forward-only and non-deletable.

### Administrative endpoints

`POST /v1/admin/projects` creates a project and bare repository. `POST /v1/admin/devices` creates a scoped device token. `GET /v1/admin/status` reports accounts/projects/devices and storage metadata without hashes. These use `Authorization: Bearer <admin token>` and are hosting-provider extensions, not required client protocol.

## Local daemon IPC

The daemon listens on a mode-`0600` Unix socket and exchanges newline-delimited JSON.

```json
{"method":"acquireAccess","project_id":"prj_example","session_id":"s1","call_id":"c1","observed_generation":3,"access_hint":"unknown"}
```

A grant contains `lease_id`, `workspace_generation`, `branch_oid`, `reconciliation`, an optional path summary, and an optional model-visible message. A blocked response contains `state`, `model_message`, and `recovery_actions`.

Release uses `releaseAccess` with the lease ID and outcome. Other V1 methods are `status`, `sync`, `publish`, and `ping`. IPC access is local authorization; repository canonicalization and subscription checks scope every request.
