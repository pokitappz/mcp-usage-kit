# Release guide

All five crates share one version and are released together in dependency order.

A repository administrator must apply the checked-in GitHub control baseline
once, and after any intentional CI job-name change:

```sh
bash scripts/configure-github-controls.sh
```

The default release reviewer and code owner is `@pokitappz`. Set
`RELEASE_REVIEWER` to another GitHub login when appropriate. The script is
idempotent and requires an authenticated `gh` session with repository
Administration write permission.

## Prepare a release

1. Update `workspace.package.version` in `Cargo.toml` and regenerate
   `Cargo.lock`. That single value also drives the internal dependency
   requirements, which are declared once in `[workspace.dependencies]`;
   `scripts/check-release-version.sh` fails the build if any of them drift,
   because a stale one is invisible until it breaks a partially-completed
   publish.
2. Move the relevant changelog entries from Unreleased into a dated version.
3. Merge the release commit to `main` and wait for every required CI check.
4. Run the complete CI command set from `CONTRIBUTING.md`.
5. Run `cargo package --workspace --locked` and inspect each generated archive.
   Verification resolves the sibling crates from the archives built in the same
   run, so this works even when none of them are in the registry yet.

## First release

**As of v0.2.0 none of the five crates has been published**, so the next release
is the first one and must take this path. Confirm with:

```sh
for c in mcp-usage-core mcp-usage-export mcp-usage-tower mcp-usage-store mcp-usage-kit; do
  cargo info "$c" >/dev/null 2>&1 && echo "$c is published" || echo "$c is unpublished"
done
```

Run that from outside this workspace. Inside it, `cargo info` resolves the local
path and reports every crate as present regardless of the registry.

Do **not** create a GitHub release for the first version. `release.yml` obtains
its token through OIDC trusted publishing, which requires a trusted publisher
that can only be configured on a crate that already exists; the workflow
refuses to run for an unpublished crate rather than failing halfway through.

crates.io requires the first version of each new crate to be published with a
regular API token. Publish in this order, allowing the registry index to expose
each dependency before publishing the next crate:

```sh
cargo publish -p mcp-usage-core --locked
cargo publish -p mcp-usage-export --locked
cargo publish -p mcp-usage-tower --locked
cargo publish -p mcp-usage-store --locked
cargo publish -p mcp-usage-kit --locked
```

Afterward, configure a trusted publisher for each crate on crates.io with:

- GitHub owner: `pokitappz`
- Repository: `mcp-usage-kit`
- Workflow: `release.yml`
- Environment: `release`

The GitHub `release` environment requires an explicit reviewer and accepts only
tags matching `v*`. Release tags are immutable. The workflow also verifies that
the tagged commit is reachable from the current `origin/main`; matching the
workspace version alone is not sufficient.

## Later releases

Once every crate exists on crates.io and has a trusted publisher, create and
publish a GitHub release whose tag exactly matches the workspace version, such
as `v0.3.0`. The release workflow validates, packages, obtains a short-lived
crates.io token through OIDC, and publishes the crates in dependency order. The release commit must already be merged to `main`, and the tag cannot
be moved or deleted after creation. A release cannot be overwritten or removed
from crates.io, so verify the tag and generated packages before approving the
environment.
