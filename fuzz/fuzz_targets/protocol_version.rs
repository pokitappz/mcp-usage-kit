//! The protocol-version header decides whether mirrored headers may be trusted
//! for pricing at all, so accepting a value it should not is a billing bypass.
#![no_main]

use libfuzzer_sys::fuzz_target;
use mcp_usage_core::version;

fuzz_target!(|data: &[u8]| {
    let Ok(raw) = std::str::from_utf8(data) else {
        return;
    };
    if version::validate_header(Some(raw)).is_ok() {
        assert!(version::ProtocolVersion::parse(raw).is_some());
        assert!(raw >= version::HEADER_VALIDATION_SINCE);
    }
});
