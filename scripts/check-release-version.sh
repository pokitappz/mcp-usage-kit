#!/usr/bin/env bash
set -euo pipefail

if [[ -z "${RELEASE_TAG:-}" ]]; then
  echo "RELEASE_TAG is required" >&2
  exit 1
fi

versions="$(cargo metadata --no-deps --format-version 1 | jq -r '[.packages[].version] | unique | .[]')"
version_count="$(printf '%s\n' "$versions" | sed '/^$/d' | wc -l | tr -d ' ')"

if [[ "$version_count" != "1" ]]; then
  echo "Workspace packages do not share exactly one version:" >&2
  printf '%s\n' "$versions" >&2
  exit 1
fi

if [[ "$RELEASE_TAG" != "v$versions" ]]; then
  echo "Release tag $RELEASE_TAG does not match workspace version v$versions" >&2
  exit 1
fi

# The crates depend on each other, and `cargo publish` resolves those
# requirements against the registry rather than against the path. A requirement
# left behind at the previous version is invisible to `cargo package` and to
# every CI job, then fails partway through the dependency-ordered publish, with
# the earlier crates already released and unpublishable again. Catch it here.
stale_internal_requirements="$(
  cargo metadata --no-deps --format-version 1 |
    jq -r --arg version "$versions" '
      .packages[] as $package
      | $package.dependencies[]
      | select(.path != null and .req != ("^" + $version))
      | "\($package.name) -> \(.name) \(.req)"
    '
)"

if [[ -n "$stale_internal_requirements" ]]; then
  echo "Workspace dependency requirements do not match version $versions:" >&2
  printf '%s\n' "$stale_internal_requirements" >&2
  exit 1
fi

release_commitish="${RELEASE_COMMIT:-refs/tags/$RELEASE_TAG}"
release_base_ref="${RELEASE_BASE_REF:-refs/remotes/origin/main}"

if ! release_commit="$(git rev-parse --verify "${release_commitish}^{commit}" 2>/dev/null)"; then
  echo "Release commit is not available: $release_commitish" >&2
  exit 1
fi

if ! git rev-parse --verify "${release_base_ref}^{commit}" >/dev/null 2>&1; then
  echo "Release base is not available: $release_base_ref" >&2
  exit 1
fi

if ! git merge-base --is-ancestor "$release_commit" "$release_base_ref"; then
  echo "Release commit is not reachable from $release_base_ref" >&2
  exit 1
fi

echo "Release tag matches workspace version $versions and commit provenance is valid"
