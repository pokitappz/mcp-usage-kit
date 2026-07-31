#!/usr/bin/env bash
set -euo pipefail

publish_with_retry() {
  local package="$1"
  local attempt=1
  local max_attempts=12

  while ! cargo publish --package "$package" --locked; do
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
