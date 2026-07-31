use std::collections::HashMap;

use mcp_usage_core::{Call, Charge, FreeReason, Method, PriceBook, decide_with_task_origin, peek};
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Suite {
    schema_version: u64,
    cases: Vec<Case>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Case {
    id: String,
    method: String,
    name: Option<String>,
    flat_units: u64,
    #[serde(default)]
    named_units: HashMap<String, u64>,
    response: Value,
    task_origin: Option<Origin>,
    expected: Expected,
}

#[derive(Debug, Deserialize)]
struct Origin {
    method: String,
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Expected {
    kind: String,
    units: Option<u64>,
    reason: Option<String>,
    idempotency_key: Option<String>,
}

#[test]
fn version_one_vectors_match_the_reference_engine() {
    let suite: Suite = serde_json::from_str(include_str!("../conformance/v1/cases.json"))
        .expect("the checked-in conformance suite must be valid JSON");
    assert_eq!(suite.schema_version, 1);
    assert!(suite.cases.len() >= 10);

    for case in suite.cases {
        let call = Call::new(Method::parse(&case.method), case.name);
        let mut prices = PriceBook::flat(case.flat_units);
        for (name, units) in case.named_units {
            prices = prices.with_name(name, units);
        }
        let origin = case
            .task_origin
            .map(|origin| Call::new(Method::parse(&origin.method), origin.name));
        let response = peek::response(&case.response);
        let charge = decide_with_task_origin(&call, &response, &prices, origin.as_ref());

        match (case.expected.kind.as_str(), charge) {
            ("billable", Charge::Billable(billable)) => {
                assert_eq!(Some(billable.units), case.expected.units, "{}", case.id);
                assert_eq!(
                    billable.idempotency_key, case.expected.idempotency_key,
                    "{}",
                    case.id
                );
            }
            ("free", Charge::Free(reason)) => {
                assert_eq!(
                    Some(reason_name(reason)),
                    case.expected.reason.as_deref(),
                    "{}",
                    case.id
                );
            }
            (expected, actual) => panic!("{}: expected {expected}, got {actual:?}", case.id),
        }
    }
}

const fn reason_name(reason: FreeReason) -> &'static str {
    match reason {
        FreeReason::Discovery => "discovery",
        FreeReason::InterimInputRequired => "interim_input_required",
        FreeReason::TaskCreated => "task_created",
        FreeReason::TaskInProgress => "task_in_progress",
        FreeReason::TaskNotDelivered => "task_not_delivered",
        FreeReason::TaskDrive => "task_drive",
        FreeReason::MissingTaskAttribution => "missing_task_attribution",
        FreeReason::MissingTaskId => "missing_task_id",
        FreeReason::Subscription => "subscription",
        FreeReason::ProtocolError => "protocol_error",
        FreeReason::UnrecognizedResult => "unrecognized_result",
    }
}
