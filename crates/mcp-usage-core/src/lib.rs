//! Billing-attribution semantics for MCP 2026-07-28.
//!
//! This crate answers one question: **given an MCP request and the response it
//! got, what is owed?** It performs no I/O, spawns nothing, knows nothing about
//! HTTP, and never touches a billing provider. Keeping attribution separate makes
//! it possible to test the billing rules without a network.
//!
//! # Why request counts are insufficient
//!
//! Before 2026-07-28, metering an MCP server was counting requests. That revision
//! made counting requests wrong in three separate ways:
//!
//! - **Multi Round-Trip Requests.** A server that needs user input mid-call now
//!   returns an interim result and the client *retries the whole call*. One
//!   logical `tools/call` becomes N HTTP requests, each carrying
//!   `Mcp-Method: tools/call`. Counting requests bills N for one unit of work.
//! - **Tasks.** A long job returns a handle and the client polls `tasks/get`
//!   until it finishes. A ten-minute job on a two-second poll interval is ~300
//!   requests for one unit of work.
//! - **Caching.** `tools/list` and friends carry `ttlMs` and `cacheScope` so they
//!   can stop reaching the origin. Billing discovery traffic charges for the act
//!   of connecting.
//!
//! The fix is to stop counting requests. See [`charge`] for the rule and the
//! reasoning; see `docs/spec-2026-07-28-findings.md` in the repository root for
//! the specification citations behind every claim here.
//!
//! # Shape of a call site
//!
//! ```
//! use mcp_usage_core::{Call, Charge, Method, PriceBook, name, peek, version};
//!
//! // 1. Refuse to price on headers a legacy origin will never validate.
//! let trust = version::assess(Some("2026-07-28"));
//! assert!(trust.is_trusted());
//!
//! // 2. Classify from headers alone. No body, no allocation on the common path.
//! let method = Method::parse("tools/call");
//! let tool = name::decode("get_weather").expect("valid header value");
//!
//! // 3. Decide once the response is known.
//! let body = serde_json::json!({ "result": { "resultType": "complete" } });
//! let charge = mcp_usage_core::decide(
//!     &Call::new(method, Some(tool.into_owned())),
//!     &peek::response(&body),
//!     &PriceBook::flat(1),
//! );
//!
//! assert!(matches!(charge, Charge::Billable(_)));
//! assert_eq!(charge.units(), 1);
//! ```
//!
//! # Where the state lives
//!
//! Nowhere in here. Durable tasks require the caller to retain two small pieces
//! of state: pre-priced [`TaskAttribution`] keyed by `taskId` and the task IDs
//! already billed. Resolving the price when the task is created avoids
//! persisting a tool name, prompt name, resource URI, or extension method text.
//! This crate accepts the former in [`decide_with_task_attribution`] and surfaces
//! the latter as [`Billable::idempotency_key`]. Enforcement belongs to the
//! caller, which is the only layer that knows whether it runs beside the origin
//! or as a shared proxy.

#![forbid(unsafe_code)]
#![warn(missing_docs, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

pub mod charge;
pub mod limits;
pub mod method;
pub mod name;
pub mod peek;
pub mod price;
pub mod version;

pub use charge::{
    Billable, Call, Charge, FreeReason, TaskAttribution, TaskOriginKind, decide,
    decide_with_task_attribution, decide_with_task_origin,
};
pub use limits::{LimitDecision, LimitReason, Limits, Usage, assess_limits};
pub use method::Method;
pub use peek::{RequestPeek, ResponsePeek, ResultType, TaskPeek, TaskStatus};
pub use price::PriceBook;
pub use version::{HeaderTrust, ProtocolVersion, TrustFailure, validate_header};
