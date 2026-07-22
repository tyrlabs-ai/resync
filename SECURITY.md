# Security model

Report vulnerabilities privately to the operator of the deployment in use. Do not include repository contents, tokens, or credentials in a public issue.

## Trust boundaries

- The local daemon is trusted to mutate subscribed worktrees and invoke Git. Its state directory and Unix socket must be accessible only to the owning user.
- A hosted service is trusted with ordinary Git contents, repository metadata, device membership, and availability. RepoSync V1 does not synchronize secrets and must not be used to track secret files.
- Repository validation commands are code execution. The preview runs them only when explicitly configured in the checked-out `.resync.toml`; operators should enroll only trusted repositories.
- Device and admin tokens are bearer credentials. Store them in the mode-`0600` RepoSync configuration, rotate on compromise, and terminate TLS at the hosting edge.

RepoSync never logs file contents by design. Debug output can include paths, commit IDs, command failures, and remote URLs; scrub those before sharing diagnostics.
