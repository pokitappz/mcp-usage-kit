//! Property tests for the edge parsers that see untrusted input.
//!
//! These live inside the crate because the functions they exercise are not
//! public: the SSE terminal-response reader and the request inspector are
//! internal, but both are reached directly from bytes an attacker controls.
//!
//! The contract under test is that each is total. Malformed input yields `None`
//! or a conservative classification, never a panic, and never a charge that the
//! bytes do not justify.
//!
//! For coverage-guided fuzzing of the same surface, see `fuzz/README.md`.

#![cfg(test)]

use proptest::prelude::*;
use serde_json::json;

use crate::cache::inspect_request;
use crate::layer::testing::terminal_response;
use mcp_usage_core::{Call, Method};

/// Bytes biased toward the shapes the SSE reader has to survive: event
/// separators, `data:` prefixes, comments, and partial JSON.
fn hostile_stream() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        any::<Vec<u8>>(),
        prop::collection::vec(
            prop_oneof![
                Just(b"data: ".to_vec()),
                Just(b"data:".to_vec()),
                Just(b"\n".to_vec()),
                Just(b"\r\n".to_vec()),
                Just(b": keepalive".to_vec()),
                Just(br#"{"jsonrpc":"2.0","id":1,"result":{"resultType":"complete"}}"#.to_vec()),
                Just(br#"{"error":{"code":-32020}}"#.to_vec()),
                Just(b"{".to_vec()),
                Just(b"\0".to_vec()),
                ".{0,16}".prop_map(String::into_bytes),
            ],
            0..24,
        )
        .prop_map(|parts| parts.concat()),
    ]
}

fn content_type() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("application/json".to_owned()),
        Just("text/event-stream".to_owned()),
        Just("text/event-stream; charset=utf-8".to_owned()),
        Just("Application/JSON".to_owned()),
        Just("application/jsonp".to_owned()),
        Just(String::new()),
        ".{0,24}",
    ]
}

proptest! {
    /// Reading a terminal response is total over arbitrary bytes.
    #[test]
    fn reading_a_terminal_response_never_panics(
        media in content_type(),
        bytes in hostile_stream(),
    ) {
        let terminal = terminal_response(&media, &bytes);
        // Anything recognized as terminal must be a JSON object carrying a
        // result or an error, since that is the whole basis for charging.
        if let Some(value) = terminal {
            prop_assert!(
                value.get("result").is_some() || value.get("error").is_some(),
                "a terminal response must carry a result or an error: {value}"
            );
        }
    }

    /// A media type the meter does not recognize is never read as terminal, so
    /// an origin cannot smuggle a charge through an unexpected content type.
    #[test]
    fn unrecognized_media_types_are_never_terminal(bytes in hostile_stream()) {
        for media in ["application/jsonp", "text/plain", "", "application/octet-stream"] {
            prop_assert!(terminal_response(media, &bytes).is_none());
        }
    }

    /// Inspecting a request body is total over arbitrary bytes.
    #[test]
    fn inspecting_a_request_never_panics(
        bytes in any::<Vec<u8>>(),
        named in any::<bool>(),
    ) {
        let call = Call::new(
            Method::ToolsList,
            named.then(|| "tool".to_owned()),
        );
        let metadata = inspect_request(&call, &bytes);
        // A continuation must never be cacheable: MRTR results are forbidden
        // from the cache, and that gate is the same body peek.
        if metadata.is_continuation {
            prop_assert!(metadata.cache_key.is_none());
        }
    }

    /// A cache key is only ever derived when the body agrees with the headers,
    /// which is what stops one call's result from being served for another.
    #[test]
    fn cache_keys_require_header_and_body_agreement(
        header_name in "[a-z]{1,8}",
        body_name in "[a-z]{1,8}",
    ) {
        let call = Call::new(Method::ResourcesRead, Some(header_name.clone()));
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "resources/read",
            "params": { "uri": body_name },
        });
        let metadata = inspect_request(&call, body.to_string().as_bytes());
        prop_assert_eq!(metadata.cache_key.is_some(), header_name == body_name);
    }
}
