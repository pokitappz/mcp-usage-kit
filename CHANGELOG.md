# Changelog

All notable changes to this project will be documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Renamed the facade and component crates under the `mcp-usage` namespace.
- Per-crate package metadata, license files, and READMEs.
- Static project site with an interactive billing example.
- Continuous integration, GitHub Pages deployment, and trusted publishing
  workflows.
- Terminal-delivery billing semantics for MCP 2026-07-28.
- MRTR and durable task attribution without request-count overbilling.
- Tower authentication, caching, body bounds, metrics, and usage recording.
- Buffered log and optional Stripe Billing Meter Events exporters.
- Redis, Valkey, and PostgreSQL durable-task attribution with hashed keys,
  expiry, first-writer preservation, and atomic completion claims.
- Low-cardinality Prometheus output and optional OpenTelemetry observable
  counters.
- Provider-neutral function and composite exporters.
- Versioned machine-readable usage-accounting conformance vectors.
- Compiling Axum, Hyper, and `rmcp` integration examples.
