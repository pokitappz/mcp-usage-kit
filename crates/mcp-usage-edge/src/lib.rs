//! Metering sidecar for MCP servers written in any language.
//!
//! The library crates meter an MCP server from inside the process, which means
//! the server has to be written in Rust. This crate puts the same `MeterLayer`
//! in front of an arbitrary upstream over HTTP instead, so a `FastMCP` server in
//! Python, a TypeScript server, or anything else gets identical billing
//! semantics without changing a line of its code.
//!
//! The metering decisions still come from `mcp-usage-core`, unchanged. The only
//! thing this crate adds is transport.
//!
//! The binary wires these pieces together from a TOML file. The modules are
//! public so the same proxy can be embedded in an application that wants to
//! build the edge itself.

#![forbid(unsafe_code)]
#![warn(missing_docs, clippy::pedantic)]

pub mod config;
pub mod control_plane;
pub mod proxy;
pub mod quota;
