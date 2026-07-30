# Security

Report vulnerabilities privately to the operator of the deployment in use. Do not include repository contents, tokens, or credentials in a public issue.

RepoSync providers receive Git repository contents and metadata. The local
daemon mutates enrolled worktrees and runs commands listed in the repository's
publication policy, so enroll only repositories you trust.

Approving a project or umbrella workspace's Codex hooks trusts its stable
RepoSync dispatcher. That dispatcher follows future locally installed RepoSync
versions without changing the approved hook definition, so review the RepoSync
installation and upgrade source as part of that approval.

Device and admin tokens are bearer credentials. RepoSync can delegate device
credential storage to `RESYNC_CREDENTIAL_HELPER`; otherwise it uses a local
mode-`0600` file. Hosted deployments require TLS outside localhost.

RepoSync never logs file contents by design. Debug output can include paths, commit IDs, command failures, and remote URLs; scrub those before sharing diagnostics.
