//! Classifying a request from its body, for clients older than the mirrored headers.
//!
//! Mirrored headers ([`crate::version::HEADER_VALIDATION_SINCE`]) let the meter price a
//! request without reading its body, but only clients on that revision send them. Every
//! earlier client - and, as of this writing, the default configuration of MCP Inspector -
//! sends none, so a header-only meter refuses the entire installed base.
//!
//! Reading the body instead is not the unsafe fallback [`crate::version`] warns about.
//! That warning is about believing *headers* on a revision where nothing obliges the
//! origin to check them against the body: there, a client can call an expensive tool
//! while claiming a cheap one. The body carries no such ambiguity. It is the exact
//! document the origin will execute, so a client cannot misdeclare its call without
//! also changing what it invokes.
//!
//! What this costs is throughput, not correctness: one bounded JSON parse per legacy
//! request. Requests that arrive with trusted headers never reach this module.

use crate::Method;
use serde_json::Value;
use std::fmt;

/// Why a legacy request could not be classified from its body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LegacyClassificationError {
    /// The body was not JSON.
    Malformed,
    /// A JSON-RPC batch. Several calls in one request cannot be priced as one, and
    /// batching was removed from the protocol in 2025-06-18.
    Batch,
    /// No usable `method` string.
    MissingMethod,
    /// A name-bearing method arrived without `params.name` or `params.uri`.
    MissingName(Method),
}

impl fmt::Display for LegacyClassificationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed => {
                f.write_str("request body is not JSON, and this client sent no trusted MCP headers")
            }
            Self::Batch => f.write_str(
                "JSON-RPC batches cannot be metered as a single call; send one request per call",
            ),
            Self::MissingMethod => f.write_str("request body has no JSON-RPC \"method\""),
            Self::MissingName(method) => write!(
                f,
                "{} requires params.name or params.uri in the body",
                method.as_str()
            ),
        }
    }
}

impl std::error::Error for LegacyClassificationError {}

/// Derive the method and name of a legacy request from its JSON-RPC body.
///
/// `name` mirrors what `Mcp-Name` would carry: `params.name` for `tools/call` and
/// `prompts/get`, `params.uri` for `resources/read`.
///
/// # Errors
///
/// Returns [`LegacyClassificationError`] for malformed JSON, batches, a missing method,
/// or a name-bearing method with no name in its params.
pub fn classify_body(body: &[u8]) -> Result<(Method, Option<String>), LegacyClassificationError> {
    let value: Value =
        serde_json::from_slice(body).map_err(|_| LegacyClassificationError::Malformed)?;
    if value.is_array() {
        return Err(LegacyClassificationError::Batch);
    }
    let method = value
        .get("method")
        .and_then(Value::as_str)
        .ok_or(LegacyClassificationError::MissingMethod)?;
    let method = Method::parse(method);

    if !method.carries_name() {
        return Ok((method, None));
    }
    // resources/read identifies its target by uri; the other two by name. Mcp-Name
    // mirrors whichever the method uses, so the fallback has to do the same.
    let name = value
        .get("params")
        .and_then(|params| {
            params
                .get("name")
                .or_else(|| params.get("uri"))
                .and_then(Value::as_str)
        })
        .ok_or_else(|| LegacyClassificationError::MissingName(method.clone()))?;
    Ok((method, Some(name.to_owned())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_call_is_priced_on_the_name_in_the_body() {
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"tools/call",
                        "params":{"name":"check_merchant","arguments":{"domain":"a.com"}}}"#;
        let (method, name) = classify_body(body).expect("classifies");
        assert_eq!(method, Method::ToolsCall);
        assert_eq!(name.as_deref(), Some("check_merchant"));
    }

    /// The attack the header path has to guard against does not exist here: the name is
    /// read from the same document the origin executes, so claiming a cheap tool means
    /// calling the cheap tool.
    #[test]
    fn the_body_name_is_the_one_the_origin_will_run() {
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"tools/call",
                        "params":{"name":"expensive_tool"}}"#;
        let (_, name) = classify_body(body).expect("classifies");
        assert_eq!(name.as_deref(), Some("expensive_tool"));
    }

    #[test]
    fn resources_read_is_named_by_uri() {
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"resources/read",
                        "params":{"uri":"file:///a.txt"}}"#;
        let (method, name) = classify_body(body).expect("classifies");
        assert_eq!(method, Method::ResourcesRead);
        assert_eq!(name.as_deref(), Some("file:///a.txt"));
    }

    #[test]
    fn listing_methods_need_no_name() {
        for raw in ["tools/list", "prompts/list", "resources/list"] {
            let body = format!(r#"{{"jsonrpc":"2.0","id":1,"method":"{raw}"}}"#);
            let (method, name) = classify_body(body.as_bytes()).expect("classifies");
            assert_eq!(method, Method::parse(raw));
            assert!(name.is_none(), "{raw} should carry no name");
        }
    }

    #[test]
    fn unknown_extension_methods_pass_through() {
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"vendor/thing"}"#;
        let (method, _) = classify_body(body).expect("classifies");
        assert_eq!(method, Method::parse("vendor/thing"));
    }

    #[test]
    fn a_name_bearing_method_without_a_name_is_refused() {
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{}}"#;
        assert!(matches!(
            classify_body(body),
            Err(LegacyClassificationError::MissingName(Method::ToolsCall))
        ));
    }

    #[test]
    fn batches_are_refused_rather_than_priced_as_one_call() {
        let body = br#"[{"jsonrpc":"2.0","id":1,"method":"tools/list"}]"#;
        assert_eq!(classify_body(body), Err(LegacyClassificationError::Batch));
    }

    #[test]
    fn malformed_and_methodless_bodies_are_distinguished() {
        assert_eq!(
            classify_body(b"not json"),
            Err(LegacyClassificationError::Malformed)
        );
        assert_eq!(
            classify_body(br#"{"jsonrpc":"2.0","id":1}"#),
            Err(LegacyClassificationError::MissingMethod)
        );
    }
}
