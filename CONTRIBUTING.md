# Contributing

Contributions are welcome. Please open an issue before starting a large API or
protocol-semantics change so the approach can be discussed first.

## Development

Install Rust 1.88 or newer, clone the repository, and run:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-targets --all-features --locked
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps --locked
```

Changes to billing semantics should include focused unit tests and, when the
behavior crosses the HTTP boundary, an integration test. Security-sensitive
code must fail closed for billing: ambiguity, truncation, or missing attribution
must not create a charge.

## Pull requests

Keep each pull request focused, explain the user-visible behavior, and update
the changelog when the change affects consumers. By submitting a contribution,
you agree that it is licensed under the Apache License, Version 2.0.
