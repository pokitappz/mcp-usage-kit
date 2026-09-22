# Release guide

All six crates share one version and are released together in dependency order.

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

1. Update `workspace.package.version` in `Cargo.toml` **and** the five internal
   requirements in `[workspace.dependencies]`. They are separate literals, not
   derived from that value, which is why `scripts/check-release-version.sh`
   exists: a stale one is invisible to `cargo package` and to every CI job, and
   then breaks a partially-completed publish with the earlier crates already
   released and unpublishable again.
2. Update `bindings/python/Cargo.toml` to the same version. The binding is
   excluded from the workspace, so `cargo metadata --no-deps` cannot see it and
   the release guard does not cover it; the Python CI job compares it
   separately.
3. Regenerate both `Cargo.lock` files.
4. Move the relevant changelog entries into a dated version.
5. Merge the release commit to `main` and wait for every required CI check.
6. Run the complete CI command set from `CONTRIBUTING.md`.
7. Run `cargo package --workspace --locked` and inspect each generated archive.
   Verification resolves the sibling crates from the archives built in the same
   run, so this works even when none of them are in the registry yet.

## First release

**All six crates have been published since v0.4.0**, so this section no longer
applies to an ordinary release; see "Later releases" below. It is kept for
reference, and for any crate added to the workspace later, which would be
unpublished and would need this path for its own first version.

`mcp-usage-edge` needed it at v0.4.0. It was added to the workspace and to
`scripts/publish-crates.sh` after v0.3.1 but not to the release workflow's
"Refuse a first release" step, which still listed five crates. Tagging a release
in that state would have passed the guard, published the five that existed,
irreversibly, and then failed authenticating the sixth, because trusted
publishing needs a publisher that can only be configured against a crate that
already exists. **If you add a crate to `scripts/publish-crates.sh`, add it to
that guard in the same commit.** Confirm the current state with:

```sh
for c in mcp-usage-core mcp-usage-export mcp-usage-tower mcp-usage-store \
         mcp-usage-kit mcp-usage-edge; do
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
regular API token. A new crate cannot be published on its own if it depends on
a sibling at the version being released, because that version is not in the
registry yet - publish the whole workspace, which orders the crates and waits
for the index between them:

```sh
cargo publish --workspace --locked --dry-run   # rehearse first
cargo publish --workspace --locked
```

That is how v0.4.0 went out. `mcp-usage-edge` could not have been published
alone beforehand: at 0.3.1 its `ControlPlaneTenantStore` no longer matched the
published `mcp-usage-kit 0.3.1` `TenantStore` trait, and at 0.4.0 it needed a
`mcp-usage-kit` that did not exist yet.

Afterward, configure a trusted publisher for the new crate on crates.io with:

- GitHub owner: `pokitappz`
- Repository: `mcp-usage-kit`
- Workflow: `release.yml`
- Environment: `release`

v0.4.0 has a git tag but **no GitHub Release**, because the crates were already
published by hand and `release.yml` fires on `release: published` - creating one
would have sent the workflow to republish versions that already exist. Pushing a
plain tag does not trigger it.

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
