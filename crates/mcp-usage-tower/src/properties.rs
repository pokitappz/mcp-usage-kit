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
use serde_json::{Value, json};

use crate::cache::{inspect_request, inspect_whole_body, render_with_id};
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
                // A bare CR is its own terminator on the wire, and the reader
                // no longer rewrites it into an LF before splitting.
                Just(b"\r".to_vec()),
                Just(b"\r\r".to_vec()),
                Just(b"\n\n".to_vec()),
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

/// JSON-RPC request bodies, biased toward the shapes where the narrow parse
/// and the whole-body parse could disagree: absent versus null `id`, `params`
/// that is not an object, continuation markers that are present but null, and
/// a `taskId` that is not a string.
fn request_body() -> impl Strategy<Value = Vec<u8>> {
    let id = prop_oneof![
        Just(None),
        Just(Some(json!(null))),
        Just(Some(json!(1))),
        Just(Some(json!("abc"))),
        Just(Some(json!(1.5))),
        Just(Some(json!([1]))),
        Just(Some(json!({"a": 1}))),
    ];
    let params = prop_oneof![
        Just(None),
        Just(Some(json!(null))),
        Just(Some(json!([1, 2]))),
        Just(Some(json!("scalar"))),
        Just(Some(json!({}))),
        Just(Some(json!({"taskId": "task-1"}))),
        Just(Some(json!({"taskId": 7}))),
        Just(Some(json!({"taskId": null}))),
        Just(Some(json!({"inputResponses": null}))),
        Just(Some(json!({"inputResponses": []}))),
        Just(Some(json!({"requestState": {"cursor": "x"}}))),
        Just(Some(
            json!({"taskId": "t", "requestState": null, "extra": {"deep": [1]}})
        )),
    ];
    (id, params, any::<bool>()).prop_map(|(id, params, with_method)| {
        let mut body = serde_json::Map::new();
        body.insert("jsonrpc".to_owned(), json!("2.0"));
        if with_method {
            body.insert("method".to_owned(), json!("tools/call"));
        }
        if let Some(id) = id {
            body.insert("id".to_owned(), id);
        }
        if let Some(params) = params {
            body.insert("params".to_owned(), params);
        }
        Value::Object(body).to_string().into_bytes()
    })
}

/// The original reader, kept as the reference the fast one is checked against.
///
/// It normalizes every line ending by rewriting the text, then splits on blank
/// lines. Straightforward, and two full copies of the body to do it.
fn buffered_terminal_response(bytes: &[u8]) -> Option<Value> {
    let text = std::str::from_utf8(bytes)
        .ok()?
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let mut terminal = None;
    for event in text.split("\n\n") {
        let data = event
            .lines()
            .filter_map(|line| line.strip_prefix("data:").map(str::trim_start))
            .collect::<Vec<_>>()
            .join("\n");
        if data.is_empty() {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(&data)
            && (value.get("result").is_some() || value.get("error").is_some())
        {
            terminal = Some(value);
        }
    }
    terminal
}

/// Well-formed SSE streams assembled with one chosen line terminator.
///
/// `hostile_stream` is for totality; this is for agreement. The case that
/// separates a borrowed reader from a rewriting one is a `data` field split
/// across lines inside a single event, joined by `\r\n`: get the terminator
/// width wrong and a phantom blank line splits that event in two, so the
/// halves parse as nothing instead of as one response.
fn sse_stream() -> impl Strategy<Value = Vec<u8>> {
    let terminator = prop_oneof![Just("\n"), Just("\r\n"), Just("\r")];
    let line = prop_oneof![
        Just(r#"data: {"jsonrpc":"2.0","id":1,"result":{"resultType":"complete"}}"#),
        // Two halves of one response. Only joined into valid JSON when the
        // reader keeps them inside the same event.
        Just(r#"data: {"jsonrpc":"2.0","id":1,"#),
        Just(r#"data: "result":{"resultType":"complete"}}"#),
        Just(r#"data: {"error":{"code":-32020}}"#),
        Just("data: 0"),
        Just("data:"),
        Just("event: message"),
        Just("id: 7"),
        Just(": keepalive"),
        Just(""),
    ];
    (terminator, prop::collection::vec(line, 0..12))
        .prop_map(|(terminator, lines)| lines.join(terminator).into_bytes())
}

/// Bodies a cache can be holding, including the shapes that are not objects
/// and so cannot take an id at all.
fn cached_body() -> impl Strategy<Value = Value> {
    prop_oneof![
        Just(json!({"jsonrpc": "2.0", "id": 1, "result": {"resultType": "complete"}})),
        // No id to overwrite; one has to be added.
        Just(json!({"jsonrpc": "2.0", "result": {"tools": [{"name": "a"}]}})),
        // Keys that need escaping, and an id nested where it must be left alone.
        Just(json!({"a\"b": "c\\d\ne", "result": {"id": "inner"}, "id": null})),
        Just(json!({})),
        Just(json!({"id": {"was": "an object"}})),
        Just(json!([1, 2, 3])),
        Just(json!("scalar")),
        Just(json!(null)),
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

    /// The single-pass SSE reader must answer exactly what the original
    /// buffering one did. It skips normalizing line endings into a fresh
    /// `String` - twice the captured body in copies, per streamed response -
    /// which is only sound while the two agree on every shape.
    #[test]
    fn the_streaming_sse_reader_agrees_with_a_buffered_one(
        bytes in prop_oneof![hostile_stream(), sse_stream()],
    ) {
        prop_assert_eq!(
            terminal_response("text/event-stream", &bytes),
            buffered_terminal_response(&bytes)
        );
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

    /// The narrow parse taken by non-cacheable methods must answer exactly
    /// what parsing the whole body into a `Value` answers. The two are
    /// separate code paths precisely so the hot one can skip building a tree
    /// it does not read, which is only sound while they agree.
    #[test]
    fn the_narrow_request_parse_agrees_with_the_whole_body_parse(
        bytes in prop_oneof![request_body().boxed(), any::<Vec<u8>>().boxed()],
        method in prop_oneof![
            Just(Method::ToolsCall),
            Just(Method::TasksGet),
            Just(Method::TasksUpdate),
            Just(Method::TasksCancel),
            Just(Method::PromptsGet),
            Just(Method::SubscriptionsListen),
            Just(Method::Other("custom/thing".to_owned())),
        ],
        named in any::<bool>(),
    ) {
        prop_assume!(!method.is_cacheable());
        let call = Call::new(method, named.then(|| "tool".to_owned()));
        prop_assert_eq!(
            inspect_request(&call, &bytes),
            inspect_whole_body(&call, &bytes)
        );
    }

    /// Substituting the caller's id during serialization must produce exactly
    /// what cloning the cached body and overwriting the field produced. The
    /// substitution exists so a cache hit does not deep-copy a `tools/list`
    /// result to change one field, which is only sound while the two agree.
    #[test]
    fn rendering_a_cached_body_agrees_with_cloning_and_overwriting(
        body in cached_body(),
        id in prop_oneof![
            Just(None),
            Just(Some(json!(null))),
            Just(Some(json!(1))),
            Just(Some(json!("caller"))),
            Just(Some(json!({"nested": [1, 2]}))),
        ],
    ) {
        let expected = {
            let mut owned = body.clone();
            if let (Some(id), Some(object)) = (id.as_ref(), owned.as_object_mut()) {
                object.insert("id".to_owned(), id.clone());
            }
            owned
        };
        let rendered: Value = serde_json::from_str(&render_with_id(&body, id.as_ref()))
            .expect("the rendered body must be valid JSON");
        prop_assert_eq!(rendered, expected);
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
