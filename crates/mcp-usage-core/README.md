# mcp-usage-core

Pure billing-attribution semantics for MCP 2026-07-28. This crate performs no
I/O, starts no tasks, and has no transport dependency. It classifies terminal
delivery, MRTR continuations, durable tasks, errors, cache delivery, quotas, and
spend caps.

Most applications should depend on the `mcp-usage-kit` facade. Use this crate
directly when you need only the deterministic protocol and pricing engine.

[API documentation](https://docs.rs/mcp-usage-core) | [Conformance vectors](conformance/v1/cases.json)

Licensed under Apache-2.0.
