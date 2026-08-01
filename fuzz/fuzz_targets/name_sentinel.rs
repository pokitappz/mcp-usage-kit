//! `Mcp-Name` arrives as a header value and may wear a base64 sentinel. The
//! price book is keyed on the decoded value, so this runs before pricing on
//! attacker-supplied bytes.
#![no_main]

use libfuzzer_sys::fuzz_target;
use mcp_usage_core::name;

fuzz_target!(|data: &[u8]| {
    let Ok(raw) = std::str::from_utf8(data) else {
        return;
    };
    match name::decode(raw) {
        // Decoding must agree with the sentinel test, or a value could be
        // priced under a name it does not actually have.
        Ok(decoded) => {
            let borrowed = matches!(decoded, std::borrow::Cow::Borrowed(_));
            assert_eq!(borrowed, !name::is_sentinel(raw));
        }
        Err(_) => assert!(name::is_sentinel(raw)),
    }
});
