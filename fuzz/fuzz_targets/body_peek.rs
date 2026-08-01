//! The request and response peeks read attacker-influenced JSON to decide
//! whether work was delivered, which is the input to every charge.
#![no_main]

use libfuzzer_sys::fuzz_target;
use mcp_usage_core::peek::{self, ResultType};

fuzz_target!(|data: &[u8]| {
    let Ok(body) = serde_json::from_slice::<serde_json::Value>(data) else {
        return;
    };
    let _ = peek::request(&body);
    let response = peek::response(&body);
    // An error response delivered nothing, so it must never carry a result type
    // that could be read as a completed delivery.
    if response.is_error {
        assert_eq!(response.result_type, ResultType::Absent);
        assert!(response.task.is_none());
    }
});
