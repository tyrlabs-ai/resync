#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 <version-or-tag>" >&2
  exit 2
fi

version="${1#v}"
tag="v${version}"

for executable in gh git install node npm sha256sum tar; do
  if ! command -v "$executable" >/dev/null 2>&1; then
    echo "required executable is unavailable: $executable" >&2
    exit 1
  fi
done

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"
git rev-parse --verify "${tag}^{commit}" >/dev/null

stage_dir="$(mktemp -d "${TMPDIR:-/tmp}/resync-npm-${version}.XXXXXX")"
cleanup() {
  if [[ -n "${stage_dir:-}" && -d "$stage_dir" ]]; then
    find "$stage_dir" -depth -delete
  fi
}
trap cleanup EXIT

source_dir="$stage_dir/source"
dist_dir="$stage_dir/dist"
mkdir -p "$source_dir" "$dist_dir"
git archive "$tag" | tar -x -C "$source_dir"

wrapper_version="$(cd "$source_dir" && node -p 'require("./package.json").version')"
if [[ "$wrapper_version" != "$version" ]]; then
  echo "tag ${tag} contains wrapper version ${wrapper_version}, expected ${version}" >&2
  exit 1
fi
for manifest in "$source_dir"/npm/*/package.json; do
  manifest_version="$(node -p "require('$manifest').version")"
  if [[ "$manifest_version" != "$version" ]]; then
    echo "$manifest contains version $manifest_version, expected $version" >&2
    exit 1
  fi
done

gh release view "$tag" --repo tyrlabs-ai/resync >/dev/null
gh release download "$tag" \
  --repo tyrlabs-ai/resync \
  --pattern 'resync-*' \
  --pattern SHA256SUMS \
  --dir "$dist_dir"
(cd "$dist_dir" && sha256sum --check SHA256SUMS)

mkdir -p \
  "$source_dir/npm/linux-x64/bin" \
  "$source_dir/npm/linux-arm64/bin" \
  "$source_dir/npm/darwin-x64/bin" \
  "$source_dir/npm/darwin-arm64/bin"
install -m 755 "$dist_dir/resync-x86_64-unknown-linux-gnu" "$source_dir/npm/linux-x64/bin/resync"
install -m 755 "$dist_dir/resync-aarch64-unknown-linux-gnu" "$source_dir/npm/linux-arm64/bin/resync"
install -m 755 "$dist_dir/resync-x86_64-apple-darwin" "$source_dir/npm/darwin-x64/bin/resync"
install -m 755 "$dist_dir/resync-aarch64-apple-darwin" "$source_dir/npm/darwin-arm64/bin/resync"

npm whoami >/dev/null

publish_package() {
  local package_dir="$1"
  local package_name
  local package_version
  package_name="$(cd "$package_dir" && node -p 'require("./package.json").name')"
  package_version="$(cd "$package_dir" && node -p 'require("./package.json").version')"

  if npm view "${package_name}@${package_version}" version >/dev/null 2>&1; then
    echo "already published: ${package_name}@${package_version}"
    return
  fi

  echo "publishing: ${package_name}@${package_version}"
  (cd "$package_dir" && npm publish --access public)
}

publish_package "$source_dir/npm/linux-x64"
publish_package "$source_dir/npm/linux-arm64"
publish_package "$source_dir/npm/darwin-x64"
publish_package "$source_dir/npm/darwin-arm64"
publish_package "$source_dir"

echo "npm release ${version} submitted successfully; new package names may take a few minutes to propagate"
