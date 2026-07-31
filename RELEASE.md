# Release guide

All five crates share one version and are released together in dependency order.

## Prepare a release

1. Confirm the five package names are still available for the first release.
2. Update `workspace.package.version` in `Cargo.toml` and regenerate
   `Cargo.lock`.
3. Move the relevant changelog entries from Unreleased into a dated version.
4. Run the complete CI command set from `CONTRIBUTING.md`.
5. Run `cargo package --workspace --locked` and inspect each generated archive.

## First release

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

Protect the GitHub `release` environment with the review rules appropriate for
the project.

## Later releases

Create and publish a GitHub release whose tag exactly matches the workspace
version, such as `v0.2.0`. The release workflow validates, packages, obtains a
short-lived crates.io token through OIDC, and publishes the crates in dependency
order. A release cannot be overwritten or removed from crates.io, so verify the
tag and generated packages before approving the environment.
