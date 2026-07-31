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

echo "Release tag matches workspace version $versions"
