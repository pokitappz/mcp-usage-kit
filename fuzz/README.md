# Fuzz targets

Coverage-guided fuzzing for the parsers that read untrusted input: the
`Mcp-Name` base64 sentinel, the protocol-version guard, and the JSON body peeks.

These are a supplement, not the first line of defence. The same surface is
covered by property tests that run on stable in ordinary CI:

- `crates/mcp-usage-core/tests/properties.rs`
- `crates/mcp-usage-tower/src/properties.rs`

Those run on every build. Fuzzing goes deeper, but it is an activity rather than
a gate, so it lives here and is run deliberately.

## Why this is a separate workspace

libFuzzer requires a nightly toolchain. The main workspace builds and tests on
stable against a Rust 1.88 minimum, so this directory declares its own empty
`[workspace]` to keep nightly out of every ordinary build.

CI builds every target and smoke-runs each one briefly, so a target that stops
compiling is caught even though nobody fuzzes on every commit.

## Running

```sh
cargo install cargo-fuzz --locked
cargo +nightly fuzz list --fuzz-dir fuzz
cargo +nightly fuzz run name_sentinel --fuzz-dir fuzz -- -max_total_time=60
```

On macOS, `libfuzzer-sys` may fail to compile its bundled libFuzzer against the
Command Line Tools libc++ headers, with errors about `__countr_zero` in
`<bitset>`. That is a toolchain mismatch rather than a problem with these
targets; build them in a Linux container, or rely on the CI job, which runs on
Ubuntu.

Drop `-max_total_time` to run until interrupted. A crash is written to
`fuzz/artifacts/<target>/` and replayed with:

```sh
cargo +nightly fuzz run <target> fuzz/artifacts/<target>/<crash-file>
```

## What each target asserts

Each one checks a property whose violation would be a billing defect, not merely
a panic:

| Target | Property |
|---|---|
| `name_sentinel` | Decoding agrees with the sentinel test, so a tool cannot be priced under a name it does not have |
| `protocol_version` | A version is accepted only if it is a well-formed date at or after the revision that makes mirrored headers trustworthy, so a client cannot claim an old revision to dodge per-name pricing |
| `body_peek` | An error response never carries a result type, so a failed call cannot be read as a delivery |

## Adding a target

Add the source to `fuzz_targets/`, then a matching `[[bin]]` entry in
`fuzz/Cargo.toml`. Prefer asserting an invariant over merely calling the
function: a target that only checks for panics will not catch a parser that
quietly starts accepting something it should refuse.
