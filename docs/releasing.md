# Releasing RepoSync

RepoSync uses a deliberate two-stage release process.

## 1. Create the GitHub release

1. Update the Rust crate, npm wrapper, and all four npm platform package
   versions to the same value.
2. Run formatting, linting, and the full test suite.
3. Commit and push the release change.
4. Create and push the matching annotated `vX.Y.Z` tag.
5. Wait for the `Release` workflow to build all four native binaries, verify
   the static Linux artifacts, publish checksums, and create the GitHub
   release.

The tag workflow does not publish to npm.

## 2. Publish to npm manually

Use an npm maintainer account with account-level 2FA enabled. Authenticate
interactively; never put a publishing token in the repository or GitHub
Actions.

```sh
npm login --auth-type=web
./scripts/publish-npm-release.sh vX.Y.Z
```

The script:

- extracts the exact tagged source rather than the working tree;
- verifies that every npm manifest matches the requested version;
- downloads the GitHub release binaries and validates `SHA256SUMS`;
- assembles the four platform packages;
- skips package versions that are already visible on npm; and
- publishes platform packages before the wrapper package.

Approve npm's passkey/security-key prompts as they appear. Newly created
package names can take a few minutes to become visible through registry
metadata.

After publication, verify a clean install in a temporary directory:

```sh
smoke_dir="$(mktemp -d)"
cd "$smoke_dir"
npm init -y
npm install @tyrlabs-ai/resync@X.Y.Z
./node_modules/.bin/resync --version
```

## Railway deployment

The hosted provider is deployed separately and manually. After `railway up`,
verify `/healthz`, provider discovery capabilities, both RepoSync daemons, and
durable queue convergence before declaring the release complete.
