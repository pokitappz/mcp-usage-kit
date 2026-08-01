//! Property tests for the parsers that see untrusted input.
//!
//! Every function here is reached from a header or a body an attacker controls,
//! and every one of them is total by contract: malformed input produces an
//! error or a conservative classification, never a panic and never an
//! unbounded amount of work. These properties assert that contract against
//! generated input rather than against the handful of cases a person thinks to
//! write down.
//!
//! For coverage-guided fuzzing of the same surface, see `fuzz/README.md`.

use mcp_usage_core::peek::{self, ResultType, TaskStatus};
use mcp_usage_core::{Method, name, version};
use proptest::prelude::*;
use serde_json::{Value, json};

/// Strings biased toward the shapes these parsers actually have to survive:
/// sentinel markers, dates, separators, and non-ASCII.
fn hostile_text() -> impl Strategy<Value = String> {
    prop_oneof![
        any::<String>(),
        "[-=?0-9a-zA-Z/+:.]{0,64}",
        "=\\?base64\\?[A-Za-z0-9+/=]{0,64}\\?=",
        "[0-9]{4}-[0-9]{2}-[0-9]{2}",
        "\\PC{0,64}",
    ]
}

proptest! {
    /// `Mcp-Name` decoding is total: any header value either decodes or reports
    /// why, and a decoded value never claims to still be wearing the sentinel.
    #[test]
    fn decoding_a_name_never_panics(raw in hostile_text()) {
        match name::decode(&raw) {
            Ok(decoded) => {
                // A borrowed result means nothing was decoded, so the input was
                // not wearing the complete sentinel.
                if matches!(decoded, std::borrow::Cow::Borrowed(_)) {
                    prop_assert!(!name::is_sentinel(&raw));
                } else {
                    prop_assert!(name::is_sentinel(&raw));
                }
            }
            Err(_) => prop_assert!(name::is_sentinel(&raw)),
        }
    }

    /// Encoding then decoding returns the original value for any input.
    #[test]
    fn names_survive_the_sentinel_round_trip(value in any::<String>()) {
        let encoded = name::encode(&value);
        prop_assert!(name::is_sentinel(&encoded));
        let decoded = name::decode(&encoded).unwrap();
        prop_assert_eq!(decoded.as_ref(), value.as_str());
    }

    /// Version validation is total, and never trusts anything it cannot order.
    #[test]
    fn validating_a_protocol_version_never_panics(raw in hostile_text()) {
        let verdict = version::validate_header(Some(&raw));
        // Acceptance implies a well-formed date at or after the revision that
        // makes mirrored headers trustworthy. Anything else must be refused,
        // because an intermediary must not assume in the client's favour.
        if verdict.is_ok() {
            prop_assert!(version::ProtocolVersion::parse(&raw).is_some());
            prop_assert!(raw.as_str() >= version::HEADER_VALIDATION_SINCE);
        }
    }

    /// Method parsing is total and round-trips through its wire form.
    #[test]
    fn methods_round_trip_through_their_wire_form(raw in hostile_text()) {
        let method = Method::parse(&raw);
        prop_assert_eq!(method.as_str(), raw.as_str());
        // Classification helpers must agree with the taxonomy for any input.
        if matches!(method, Method::Other(_)) {
            prop_assert!(!method.is_discovery());
            prop_assert!(!method.is_task_drive());
            prop_assert!(!method.supports_mrtr());
            prop_assert!(!method.is_cacheable());
        }
    }

    /// Task status parsing is total and never invents a terminal state.
    #[test]
    fn unknown_task_statuses_are_never_terminal(raw in hostile_text()) {
        let status = TaskStatus::parse(&raw);
        if matches!(status, TaskStatus::Unknown(_)) {
            prop_assert!(!status.is_terminal());
            prop_assert!(!status.delivered(), "an unknown status must not bill");
        }
        prop_assert_eq!(status.to_string(), raw);
    }
}

/// Arbitrary JSON, including the shapes the peeks actually look for.
fn hostile_json() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::from),
        any::<i64>().prop_map(Value::from),
        ".{0,32}".prop_map(Value::from),
    ];
    leaf.prop_recursive(4, 32, 4, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..4).prop_map(Value::from),
            prop::collection::hash_map(
                prop_oneof![
                    Just("params".to_owned()),
                    Just("result".to_owned()),
                    Just("error".to_owned()),
                    Just("resultType".to_owned()),
                    Just("status".to_owned()),
                    Just("taskId".to_owned()),
                    Just("inputResponses".to_owned()),
                    Just("requestState".to_owned()),
                    ".{0,8}",
                ],
                inner,
                0..6,
            )
            .prop_map(|map| Value::Object(map.into_iter().collect())),
        ]
    })
}

proptest! {
    /// Both body peeks are total over arbitrary JSON.
    #[test]
    fn peeking_at_arbitrary_json_never_panics(body in hostile_json()) {
        let request = peek::request(&body);
        // A continuation is exactly the presence of either marker under params.
        let params = body.get("params");
        let expected = params.is_some_and(|params| {
            params.get("inputResponses").is_some_and(|v| !v.is_null())
                || params.get("requestState").is_some_and(|v| !v.is_null())
        });
        prop_assert_eq!(request.is_continuation, expected);

        let response = peek::response(&body);
        // An error response never carries a result type, so it can never bill.
        if response.is_error {
            prop_assert_eq!(response.result_type, ResultType::Absent);
            prop_assert!(response.task.is_none());
        }
    }

    /// A result this build does not understand must never look deliverable.
    #[test]
    fn unrecognized_result_types_are_never_complete(discriminant in ".{0,24}") {
        let body = json!({ "result": { "resultType": discriminant } });
        let peeked = peek::response(&body);
        prop_assert!(!peeked.is_error);
        match peeked.result_type {
            ResultType::Complete => prop_assert_eq!(discriminant.as_str(), "complete"),
            ResultType::InputRequired => prop_assert_eq!(discriminant.as_str(), "input_required"),
            ResultType::Task => prop_assert_eq!(discriminant.as_str(), "task"),
            ResultType::Unknown(other) => prop_assert_eq!(other, discriminant),
            ResultType::Absent => prop_assert!(false, "a present discriminant cannot be absent"),
        }
    }
}
