# Releasing RepoSync

RepoSync uses a deliberate two-stage release process.

> **Deployment invariant:** Linux machines, including Yolopods, must be
> upgraded from the static artifact produced by the tagged GitHub `Release`
> workflow. Do not deploy a local `cargo build --release` binary: it can inherit
> the builder's glibc requirement, and matching version text does not prove a
> matching build. For a given architecture, every machine must run the exact
> release artifact and checksum.

## 1. Create the GitHub release

1. Update the Rust crate, npm wrapper, and all four npm platform package
   versions to the same value.
2. Run formatting, linting, and the full test suite.
3. Commit and push the release change.
4. Create and push the matching annotated `vX.Y.Z` tag.
5. Wait for the `Release` workflow to build all four native binaries, verify
   the static Linux artifacts, publish checksums, and create the GitHub
   release.

The workflow in [`.github/workflows/release.yml`](../.github/workflows/release.yml)
is the canonical cross-machine builder. Local release builds are suitable for
development and tests only.

## 2. Deploy the exact release build

Download the target artifact and checksums from GitHub, then verify them before
running the upgrade. For Linux x86-64:

```sh
release_dir="$(mktemp -d)"
gh release download vX.Y.Z --repo tyrlabs-ai/resync --dir "$release_dir"
(cd "$release_dir" && sha256sum --check --ignore-missing SHA256SUMS)
file "$release_dir/resync-x86_64-unknown-linux-gnu"
! readelf -l "$release_dir/resync-x86_64-unknown-linux-gnu" | grep -q 'Requesting program interpreter'
chmod 755 "$release_dir/resync-x86_64-unknown-linux-gnu"
"$release_dir/resync-x86_64-unknown-linux-gnu" upgrade
```

Copy that same verified artifact—not a rebuilt binary—to other machines of the
same architecture and run its `upgrade` command there. Use the corresponding
release artifact on a different architecture.

After every deployment, verify:

```sh
resync --version
resync doctor
sha256sum "$(readlink -f "$(command -v resync)")"
systemctl --user show resync.service --property=ExecStart
```

The version must match the tag, `doctor` must report one RepoSync build, and
machines with the same architecture must report the checksum from the same
entry in `SHA256SUMS`. Re-run `resync upgrade` to confirm convergence is
idempotent. Do not retain npm/global installs or older managed build
directories after successful verification.

The tag workflow does not publish to npm.

## 3. Publish to npm manually

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
