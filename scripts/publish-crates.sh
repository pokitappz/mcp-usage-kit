#!/usr/bin/env bash
set -euo pipefail

version="$(cargo metadata --no-deps --format-version 1 | jq -r '[.packages[].version] | unique | .[0]')"
log="$(mktemp)"
trap 'rm -f "$log"' EXIT

# crates.io never accepts a version twice, so a release that failed partway
# could not be re-run: the first crate it reached was already published and
# every retry of it failed. Skipping what is already there makes a re-run of
# the same tag finish the job. Anything other than a clear yes or no from the
# registry stops the run rather than guessing.
already_published() {
  local package="$1"
  local status
  status="$(curl -sS -o /dev/null -w '%{http_code}' \
    -A 'mcp-usage-kit release (https://github.com/pokitappz/mcp-usage-kit)' \
    "https://crates.io/api/v1/crates/$package/$version")"
  case "$status" in
    200) return 0 ;;
    404) return 1 ;;
    *)
      echo "crates.io answered $status when asked whether $package $version exists" >&2
      exit 1
      ;;
  esac
}

publish_with_retry() {
  local package="$1"
  local attempt=1
  local max_attempts=12

  if already_published "$package"; then
    echo "$package $version is already on crates.io; skipping."
    return 0
  fi

  while ! cargo publish --package "$package" --locked 2>&1 | tee "$log"; do
    # A 403 is a missing trusted publisher, not a lagging index, and no
    # amount of waiting fixes it. v0.5.0 spent two minutes retrying one.
    if grep -q '403 Forbidden' "$log"; then
      echo "crates.io refused the token for $package." >&2
      echo "Configure its trusted publisher; see 'Trusted publishers' in RELEASE.md." >&2
      return 1
    fi

    if [[ "$attempt" -ge "$max_attempts" ]]; then
      echo "Could not publish $package after $max_attempts attempts" >&2
      return 1
    fi

    attempt=$((attempt + 1))
    echo "Publish attempt for $package failed. Waiting for the registry index before attempt $attempt."
    sleep 10
  done
}

publish_with_retry mcp-usage-core
publish_with_retry mcp-usage-export
publish_with_retry mcp-usage-tower
publish_with_retry mcp-usage-store
publish_with_retry mcp-usage-kit
# The deployable sidecar, published so `cargo install mcp-usage-edge` works.
# It depends on mcp-usage-kit, so it has to follow it in this order.
publish_with_retry mcp-usage-edge
