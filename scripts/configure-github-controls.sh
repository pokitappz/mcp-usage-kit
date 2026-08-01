#!/usr/bin/env bash
set -euo pipefail

repository="${GITHUB_REPOSITORY:-pokitappz/mcp-usage-kit}"
reviewer="${RELEASE_REVIEWER:-pokitappz}"
reviewer_id="$(gh api "users/$reviewer" --jq .id)"

ruleset_id="$(
  gh api "repos/$repository/rulesets" --jq \
    '.[] | select(.name == "PR Only" or .name == "PR and required CI") | .id' \
    | head -n 1
)"
if [[ -z "$ruleset_id" ]]; then
  echo "The main-branch ruleset was not found" >&2
  exit 1
fi

gh api --method PUT "repos/$repository/rulesets/$ruleset_id" --input - <<JSON
{
  "name": "PR and required CI",
  "target": "branch",
  "enforcement": "active",
  "bypass_actors": [],
  "conditions": {
    "ref_name": {"include": ["refs/heads/main"], "exclude": []}
  },
  "rules": [
    {"type": "deletion"},
    {"type": "non_fast_forward"},
    {
      "type": "pull_request",
      "parameters": {
        "required_approving_review_count": 1,
        "dismiss_stale_reviews_on_push": true,
        "required_reviewers": [],
        "require_code_owner_review": true,
        "require_last_push_approval": true,
        "required_review_thread_resolution": true,
        "allowed_merge_methods": ["squash", "rebase"]
      }
    },
    {
      "type": "required_status_checks",
      "parameters": {
        "do_not_enforce_on_create": false,
        "strict_required_status_checks_policy": true,
        "required_status_checks": [
          {"context": "Format, lint, test, document, and package", "integration_id": 15368},
          {"context": "Rust 1.88 minimum version", "integration_id": 15368},
          {"context": "Check macos-latest", "integration_id": 15368},
          {"context": "Check windows-latest", "integration_id": 15368},
          {"context": "Dependency audit", "integration_id": 15368},
          {"context": "License, source, and ban checks", "integration_id": 15368},
          {"context": "Fuzz targets", "integration_id": 15368},
          {"context": "Redis and PostgreSQL integration", "integration_id": 15368},
          {"context": "Static site checks", "integration_id": 15368}
        ]
      }
    }
  ]
}
JSON

tag_ruleset_id="$(
  gh api "repos/$repository/rulesets" --jq \
    '.[] | select(.name == "Immutable release tags") | .id' \
    | head -n 1
)"
if [[ -z "$tag_ruleset_id" ]]; then
  gh api --method POST "repos/$repository/rulesets" --input - <<JSON
{
  "name": "Immutable release tags",
  "target": "tag",
  "enforcement": "active",
  "bypass_actors": [],
  "conditions": {
    "ref_name": {"include": ["refs/tags/v*"], "exclude": []}
  },
  "rules": [
    {"type": "deletion"},
    {"type": "non_fast_forward"}
  ]
}
JSON
else
  gh api --method PUT "repos/$repository/rulesets/$tag_ruleset_id" --input - <<JSON
{
  "name": "Immutable release tags",
  "target": "tag",
  "enforcement": "active",
  "bypass_actors": [],
  "conditions": {
    "ref_name": {"include": ["refs/tags/v*"], "exclude": []}
  },
  "rules": [
    {"type": "deletion"},
    {"type": "non_fast_forward"}
  ]
}
JSON
fi

gh api --method PUT "repos/$repository/environments/release" --input - <<JSON
{
  "wait_timer": 0,
  "prevent_self_review": false,
  "reviewers": [{"type": "User", "id": $reviewer_id}],
  "deployment_branch_policy": {
    "protected_branches": false,
    "custom_branch_policies": true
  }
}
JSON

existing_policy="$(
  gh api "repos/$repository/environments/release/deployment-branch-policies" \
    --jq '.branch_policies[] | select(.name == "v*") | .id' \
    | head -n 1
)"
if [[ -z "$existing_policy" ]]; then
  gh api --method POST \
    "repos/$repository/environments/release/deployment-branch-policies" \
    -f name='v*' -f type=tag
fi

echo "GitHub main, release-tag, and release-environment controls are active"
